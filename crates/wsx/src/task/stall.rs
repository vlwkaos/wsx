//! Stalls: an open task that stays in one state while its cause is gone or nothing moves.
//! A state change is an event already; this finds what produces none. The manager host looks
//! every `SWEEP` and reports each stall once. ref: docs/tasks.md "Manager" (Stalls)

use super::hosting;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use wsx_core::config::project::load_task_config;
use wsx_core::task::store::{self, TaskDir, LOG_FILE};
use wsx_core::task::{call_host, HostRequest, TaskRecord, TaskState, WaitOn};

pub(super) const SWEEP: Duration = Duration::from_secs(10);
/// A stall seen only in the present must last this long: two disk polls of the task host
/// (30 s each), and longer than a host's exec handoff or a land's rebase holds its socket.
pub(super) const GRACE: Duration = Duration::from_secs(60);
/// The longest silence of a worker that still runs: a background job's wait or one check
/// (`tasks.checkTimeoutSeconds`), plus one Bash call (Claude's 10 minute maximum).
const BASH_MAX_SECS: u64 = 10 * 60;
const PROBE: Duration = Duration::from_secs(2);
const TAIL_BYTES: u64 = 4096;
const LAST_ENTRY_CHARS: usize = 120;

pub(super) struct Stall {
    /// The same stall keeps its marker; progress or a new wait makes a new one.
    pub marker: String,
    /// `waiting on disk for 3 h 10 min; free 53.0 GB, tasks.minFreeGb is 3`.
    pub words: String,
}

/// The stall of an open task, if it has one now.
pub(super) fn find(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<Stall> {
    let waiting = record.waiting.as_ref();
    let hosted = match record.state {
        TaskState::Working | TaskState::Checking => true,
        TaskState::Waiting => waiting.is_some_and(|w| matches!(w.on, WaitOn::Disk | WaitOn::Limit)),
        _ => false,
    };
    if !hosted {
        return None;
    }
    let state = match waiting {
        Some(w) if record.state == TaskState::Waiting => format!("waiting on {}", w.on.as_str()),
        Some(w) => format!("{}, waiting on {}", record.state.as_str(), w.on.as_str()),
        None => record.state.as_str().to_string(),
    };
    let lasted = span(now_ms.saturating_sub(record.state_unix_ms.max(record.created_unix_ms)));
    let stall = |marker: String, evidence: String| Stall {
        marker,
        words: format!("{state} for {lasted}; {evidence}"),
    };
    let progress = progress_ms(dir);
    if !host_answers(dir) {
        return Some(stall(
            format!("host {progress}"),
            format!(
                "its host does not answer; {}",
                last_entry(dir, progress, now_ms)
            ),
        ));
    }
    let Some(waiting) = waiting else {
        return quiet(dir, record, progress, now_ms)
            .map(|evidence| stall(format!("quiet {progress}"), evidence));
    };
    // ^ What shows the wait's cause is gone: a disk wait queues only once there is space.
    let cause = match waiting.on {
        WaitOn::Disk => {
            let min_gb =
                load_task_config(&record.project_path).map_or(5, |settings| settings.min_free_gb);
            hosting::free_mb(&record.working_copy)
                .filter(|free_mb| *free_mb >= min_gb * 1024)
                .map(|free_mb| {
                    format!(
                        "free {:.1} GB, tasks.minFreeGb is {min_gb}",
                        free_mb as f64 / 1024.0
                    )
                })
        }
        WaitOn::Limit => waiting
            .until_ms
            .filter(|until_ms| *until_ms < now_ms)
            .map(|until_ms| format!("its session limit reset {} ago", span(now_ms - until_ms))),
        _ => None,
    };
    match queued_behind(&waiting.reason) {
        Some((project, id)) => {
            let holder = holder_stall(&record.project_id, project, id, now_ms)?;
            let queued = format!("queued behind {project} t{id}, {holder}");
            Some(stall(
                format!(
                    "{} {} behind {project} t{id}",
                    waiting.on.as_str(),
                    record.state_unix_ms
                ),
                cause.map_or(queued.clone(), |cause| format!("{cause}; {queued}")),
            ))
        }
        None => cause.map(|cause| {
            stall(
                format!("{} {}", waiting.on.as_str(), record.state_unix_ms),
                cause,
            )
        }),
    }
}

/// "no log or check output for 45 min; ..." when a running task is silent past its bound.
fn quiet(dir: &TaskDir, record: &TaskRecord, progress: u64, now_ms: u64) -> Option<String> {
    let timeout = load_task_config(&record.project_path)
        .map_or(record.check_timeout_secs, |settings| {
            settings.check_timeout_secs
        });
    let bound_ms = (timeout + BASH_MAX_SECS) * 1000;
    (now_ms.saturating_sub(progress) > bound_ms).then(|| {
        format!(
            "no log or check output for {}; {}",
            span(now_ms - progress),
            last_entry(dir, progress, now_ms)
        )
    })
}

/// Why the holder a task queues behind does not hold its slot as it should, or `None`.
/// A dead holder frees its `flock` at once, so what stalls a queue is a holder that holds
/// without running, whose host is gone or that makes no progress.
fn holder_stall(own_project_id: &str, project: &str, id: u32, now_ms: u64) -> Option<String> {
    let (dir, holder) = holder_record(own_project_id, project, id)?;
    if !matches!(holder.state, TaskState::Working | TaskState::Checking) {
        return Some(format!("which is {}", holder.state.as_str()));
    }
    if !host_answers(&dir) {
        return Some("whose host does not answer".into());
    }
    let progress = progress_ms(&dir);
    quiet(&dir, &holder, progress, now_ms).map(|evidence| format!("which has {evidence}"))
}

/// The record of `<project> t<id>`: this project first, else any project with that name.
fn holder_record(own_project_id: &str, project: &str, id: u32) -> Option<(TaskDir, TaskRecord)> {
    let mut ids = vec![own_project_id.to_string()];
    if let Ok(entries) = std::fs::read_dir(store::state_dir().join("tasks")) {
        ids.extend(
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str().map(str::to_string)),
        );
    }
    ids.iter().find_map(|project_id| {
        let dir = TaskDir::open(project_id, id).ok()?;
        let record = dir.read_record().ok()?;
        (record.project == project).then_some((dir, record))
    })
}

/// The holder a slot's reason names: "queued for disk behind demo t3 and 2 more".
/// ref: crates/wsx/src/task/slot.rs `try_take`
fn queued_behind(reason: &str) -> Option<(&str, u32)> {
    let (_, holder) = reason.split_once(" behind ")?;
    let holder = match holder.rsplit_once(" and ") {
        Some((holder, more)) if more.ends_with(" more") => holder,
        _ => holder,
    };
    let (project, id) = holder.rsplit_once(" t")?;
    Some((project, id.parse().ok()?))
}

fn host_answers(dir: &TaskDir) -> bool {
    call_host(&dir.socket_path(), &HostRequest::Status, PROBE).is_ok()
}

/// The newest write to the task's log or a check's output, in unix ms.
fn progress_ms(dir: &TaskDir) -> u64 {
    let mtime = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |at| at.as_millis() as u64)
    };
    let checks = std::fs::read_dir(&dir.path).into_iter().flatten().flatten();
    checks
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("check-") && name.ends_with(".log")
        })
        .map(|entry| mtime(&entry.path()))
        .chain([mtime(&dir.file(LOG_FILE))])
        .max()
        .unwrap_or(0)
}

/// "its last write 2 h ago; its log ends with agent \"cargo build\"".
fn last_entry(dir: &TaskDir, progress: u64, now_ms: u64) -> String {
    let ago = span(now_ms.saturating_sub(progress));
    let line = tail_line(&dir.file(LOG_FILE)).unwrap_or_default();
    let entry: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    match entry["kind"].as_str() {
        Some(kind) => {
            let text = entry["text"].as_str().or(entry["reason"].as_str());
            let text = text.map_or(String::new(), |text| {
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                format!(
                    " \"{}\"",
                    wsx_core::task::view::clip(&text, LAST_ENTRY_CHARS)
                )
            });
            format!("its last write {ago} ago; its log ends with {kind}{text}")
        }
        None => format!("its last write {ago} ago"),
    }
}

fn tail_line(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    tail.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
}

/// "45 min", "3 h 10 min", "2 d 4 h".
fn span(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "less than a minute".into(),
        1..60 => format!("{minutes} min"),
        60..1440 => format!("{} h {} min", minutes / 60, minutes % 60),
        _ => format!("{} d {} h", minutes / 1440, minutes % 1440 / 60),
    }
}
