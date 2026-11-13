//! Stalls: an open task that stays in one state while its cause is gone or nothing moves.
//! A state change is an event already; this finds what produces none. The manager host looks
//! every `SWEEP` and reports each stall once. ref: docs/tasks.md "Manager" (Stalls)

use super::status::{self, span};
use super::store::{self, TaskDir, LOG_FILE};
use super::{call_host, view, HostRequest, LogEvent, TaskRecord, TaskState, WaitOn};
use crate::config::project::load_task_config;
use crate::model::workspace::DEFAULT_MANAGER_REVIEW_SECS;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

pub const SWEEP: Duration = Duration::from_secs(10);
/// A stall seen only in the present must last this long: two disk polls of the task host
/// (30 s each), and longer than a host's exec handoff or a land's rebase holds its socket.
const GRACE: Duration = Duration::from_secs(60);
/// `GRACE`, or the seconds in `WSX_TEST_STALL_GRACE_SECS`: for the task scenario only, which
/// would otherwise wait a minute per stall. ref: docs/tasks.md "Manager" (Stalls)
pub fn grace() -> Duration {
    std::env::var("WSX_TEST_STALL_GRACE_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .map_or(GRACE, Duration::from_secs)
}
/// A wait for another task to land that lasts this long is a stall.
const AFTER_STALL_SECS: u64 = 24 * 60 * 60;
/// The longest silence of a worker that still runs: a background job's wait or one check
/// (`tasks.checkTimeoutSeconds`), plus one Bash call (Claude's 10 minute maximum).
const BASH_MAX_SECS: u64 = 10 * 60;
const PROBE: Duration = Duration::from_secs(2);
const LAST_ENTRY_CHARS: usize = 120;

pub struct Stall {
    /// The same stall keeps its marker; progress or a new wait makes a new one.
    pub marker: String,
    /// `waiting on disk for 3 h 10 min; free 53.0 GB, tasks.minFreeGb is 3`.
    pub words: String,
    /// The cause alone, for the owner: never a quoted log line.
    pub why: String,
}

/// The stall of an open task, if it has one now.
pub fn find(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<Stall> {
    running(dir, record, now_ms)
        .or_else(|| manager_wait(dir, record, now_ms))
        .or_else(|| after_wait(record, now_ms))
}

/// A wait for another task (`wsx task after`) still open after `AFTER_STALL_SECS`: reported
/// once. The wait ends by itself when that task lands or closes.
/// ref: docs/tasks.md "After another task"
fn after_wait(record: &TaskRecord, now_ms: u64) -> Option<Stall> {
    let after = record.after.as_ref()?;
    let waited = now_ms.saturating_sub(after.since_unix_ms);
    (waited >= AFTER_STALL_SECS * 1000).then(|| Stall {
        marker: format!("after {}", after.since_unix_ms),
        words: format!(
            "{} for {}; waiting for #{} to land",
            record.state.as_str(),
            span(waited),
            after.task
        ),
        why: format!("waiting for #{} to land", after.task),
    })
}

/// A task the manager must act on (ready for its review, or waiting on `plan`) that has had
/// no action of the manager for `tasks.manager.reviewMinutes` since it entered the state. An
/// owner wait is none, and neither is a task the manager parked with `wsx task after`: that
/// is its review, and `after_wait` reports a long wait.
/// ref: docs/tasks.md "Manager" (Stalls)
fn manager_wait(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<Stall> {
    let settings = load_task_config(&record.project_path).ok();
    let limit_ms = settings
        .as_ref()
        .map_or(DEFAULT_MANAGER_REVIEW_SECS, |s| s.manager_review_secs)
        * 1000;
    if limit_ms == 0
        || record.after.is_some()
        || !record.manager_reviews(settings.is_none_or(|s| s.land_auto))
    {
        return None;
    }
    let entered = record.state_unix_ms.max(record.created_unix_ms);
    // ^ A manager's `add`, steer, land or close changes the state, so the state's start counts.
    // A `message` entry does not count: the log keeps no author, and the host writes its own
    // (a land notice). `check_run` and `hold` count; `check_run` has no author either, but the
    // worker's turn is over while the task waits, so a late one is rare.
    let acted = dir
        .last_entry(|entry| {
            matches!(
                entry.event,
                LogEvent::CheckRun { .. } | LogEvent::Hold { .. }
            )
            .then_some(entry.unix_ms)
        })
        .ok()
        .flatten()
        .unwrap_or(0);
    (now_ms.saturating_sub(entered.max(acted)) >= limit_ms).then(|| {
        let state = match &record.waiting {
            Some(w) if record.state == TaskState::Waiting => {
                format!("waiting on {}", w.on.as_str())
            }
            _ => record.state.as_str().to_string(),
        };
        Stall {
            marker: format!("manager {entered}"),
            words: format!(
                "{state} for {}; the manager has not reviewed it",
                span(now_ms.saturating_sub(entered))
            ),
            why: "the manager has not reviewed it".into(),
        }
    })
}

/// The stall of an open task the host runs or queues (a disk or limit wait), if it has one now.
fn running(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<Stall> {
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
    let stall = |marker: String, evidence: String, why: String| Stall {
        marker,
        words: format!("{state} for {lasted}; {evidence}"),
        why,
    };
    let progress = progress_ms(dir);
    if !host_answers(dir) {
        return Some(stall(
            format!("host {progress}"),
            format!(
                "its host does not answer; {}",
                last_entry(dir, progress, now_ms)
            ),
            "its host does not answer".into(),
        ));
    }
    let Some(waiting) = waiting else {
        return quiet(dir, record, progress, now_ms).map(|evidence| {
            let why = format!("no output for {}", span(now_ms.saturating_sub(progress)));
            stall(format!("quiet {progress}"), evidence, why)
        });
    };
    // ^ What shows the wait's cause is gone: a disk wait queues only once there is space.
    let cause = match waiting.on {
        WaitOn::Disk => {
            let min_gb =
                load_task_config(&record.project_path).map_or(5, |settings| settings.min_free_gb);
            store::free_mb(&record.working_copy)
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
            .map(|until_ms| format!("its usage limit reset {} ago", span(now_ms - until_ms))),
        // ^ The host stops the run at its time limit and answers: a wait past it is lost.
        WaitOn::CheckRun => {
            waiting
                .until_ms
                .filter(|until_ms| *until_ms < now_ms)
                .map(|until_ms| {
                    format!(
                        "its check-run passed its time limit {} ago",
                        span(now_ms - until_ms)
                    )
                })
        }
        _ => None,
    };
    match status::queue(&waiting.reason).and_then(|queue| status::task_label(queue.holder)) {
        Some((project, id)) => {
            let holder = holder_stall(&record.project_id, project, id, now_ms)?;
            let queued = format!("queued behind {project} t{id}, {holder}");
            let why = format!("queued behind {project} t{id}, which does not move");
            Some(stall(
                format!(
                    "{} {} behind {project} t{id}",
                    waiting.on.as_str(),
                    record.state_unix_ms
                ),
                cause.map_or(queued.clone(), |cause| format!("{cause}; {queued}")),
                why,
            ))
        }
        None => cause.map(|cause| {
            stall(
                format!("{} {}", waiting.on.as_str(), record.state_unix_ms),
                cause.clone(),
                cause,
            )
        }),
    }
}

/// Why an open task is stuck now, for a page that reads it once: `find`, after `GRACE` with no
/// write and past a limit's reset, as a read has no sweep that saw it before.
/// ref: docs/web-client-tasks.md "Task sheet"
pub fn stuck(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<String> {
    let stall = running(dir, record, now_ms)?;
    let until = record.waiting.as_ref().and_then(|waiting| waiting.until_ms);
    let settled = progress_ms(dir).max(until.unwrap_or(0));
    (now_ms.saturating_sub(settled) > grace().as_millis() as u64).then_some(stall.why)
}

/// How long a turn may run with no output before it counts as hung: one check
/// (`check_timeout_secs`, `tasks.checkTimeoutSeconds`) plus one Bash call. The manager's
/// stall sweep reports a worker silent this long, and each host ends its own turn by it
/// (`turn_quiet_ms`). ref: docs/tasks.md "Agents"
pub fn quiet_after_ms(check_timeout_secs: u64) -> u64 {
    (check_timeout_secs + BASH_MAX_SECS) * 1000
}

/// `quiet_after_ms` for a host's own turn, or the seconds in the file `WSX_TEST_TURN_QUIET_FILE`
/// names while it exists: for the task scenario only, whose hung turns would otherwise hold 10
/// minutes, and only around them, since its other fixtures keep a turn quiet on a gate longer
/// than that; the sweep keeps the real bound, so no fixture turn reads as a stall.
pub fn turn_quiet_ms(check_timeout_secs: u64) -> u64 {
    std::env::var_os("WSX_TEST_TURN_QUIET_FILE")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|secs| secs.trim().parse::<u64>().ok())
        .map_or(quiet_after_ms(check_timeout_secs), |secs| secs * 1000)
}

/// "no log or check output for 45 min; ..." when a running task is silent past its bound.
fn quiet(dir: &TaskDir, record: &TaskRecord, progress: u64, now_ms: u64) -> Option<String> {
    let timeout = load_task_config(&record.project_path)
        .map_or(record.check_timeout_secs, |settings| {
            settings.check_timeout_secs
        });
    let bound_ms = quiet_after_ms(timeout);
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

fn host_answers(dir: &TaskDir) -> bool {
    call_host(&dir.socket_path(), &HostRequest::Status, PROBE).is_ok()
}

/// The newest write to the task's log or a check's output, in unix ms.
pub fn progress_ms(dir: &TaskDir) -> u64 {
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
    let entry = dir
        .last_entry(|entry| serde_json::to_value(entry).ok())
        .ok()
        .flatten()
        .unwrap_or_default();
    match entry["kind"].as_str() {
        Some(kind) => {
            let text = entry["text"].as_str().or(entry["reason"].as_str());
            let text = text.map_or(String::new(), |text| {
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                format!(" \"{}\"", view::clip(&text, LAST_ENTRY_CHARS))
            });
            format!("its last write {ago} ago; its log ends with {kind}{text}")
        }
        None => format!("its last write {ago} ago"),
    }
}
