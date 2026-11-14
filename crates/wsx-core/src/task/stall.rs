//! Stalls: an open task that stays in one state while its cause is gone or nothing moves.
//! A state change is an event already; this finds what produces none. The manager host looks
//! every `SWEEP` and reports each stall once. ref: docs/tasks.md "Manager" (Stalls)

use super::status::{self, span};
use super::store::{self, TaskDir};
use super::{call_host, view, HostRequest, LogEvent, TaskRecord, TaskState, WaitOn};
use crate::config::project::load_task_config;
use crate::model::workspace::DEFAULT_MANAGER_REVIEW_SECS;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

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
pub(super) const PROBE: Duration = Duration::from_secs(2);
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
    let progress = progress_ms(dir, record);
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
        return Turn::overdue(&dir.path, now_ms).map(|turn| {
            let why = format!("no output for {}", span(now_ms - turn.output_ms));
            let evidence = format!("its {}", silent(dir, &turn, now_ms));
            stall(format!("quiet {}", turn.output_ms), evidence, why)
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
    let settled = progress_ms(dir, record).max(until.unwrap_or(0));
    (now_ms.saturating_sub(settled) > grace().as_millis() as u64).then_some(stall.why)
}

/// How long a turn may run with no output before its host ends it: one check
/// (`check_timeout_secs`, `tasks.checkTimeoutSeconds`) plus one Bash call, or the seconds in the
/// file `WSX_TEST_TURN_QUIET_FILE` names while it exists (for the task scenario only, around its
/// hung turns: its other fixtures keep a turn quiet on a gate longer than that). The host puts
/// it in the turn's record (`Turn`). ref: docs/tasks.md "Agents"
pub fn turn_quiet_ms(check_timeout_secs: u64) -> u64 {
    std::env::var_os("WSX_TEST_TURN_QUIET_FILE")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|secs| secs.trim().parse::<u64>().ok())
        .map_or((check_timeout_secs + BASH_MAX_SECS) * 1000, |secs| {
            secs * 1000
        })
}

/// The turn record's file in a task's, the manager's or a chat's folder.
pub const TURN_FILE: &str = "turn.json";

/// The one record of an agent turn, for workers, the manager and chats alike: its host writes
/// it at the start, at each line of the agent and at the end, as `turn.json` in its folder (the
/// next turn overwrites it). The host ends a turn whose `deadline_ms` passes (`hung_turn`); the
/// stall sweep, `stuck` and `wsx task why` read the same record, so liveness has one clock and
/// one bound. ref: docs/tasks.md "Agents" (Turn record)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub started_ms: u64,
    /// `<adapter>/<model>` that runs the turn.
    pub agent: String,
    /// The agent's last line, or the turn's start.
    pub output_ms: u64,
    /// The quiet bound the turn started with (`turn_quiet_ms`).
    pub quiet_ms: u64,
    /// `output_ms` plus `quiet_ms`: past it with no line, the host ends the turn.
    pub deadline_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<TurnEnd>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub unix_ms: u64,
    /// The result's first line, the error, or what ended the turn (a stop, a close).
    pub reason: String,
}

impl Turn {
    /// A turn that starts now on `agent` in `folder`, ended by its host after `quiet_ms` with
    /// no line from the agent.
    pub fn start(folder: &Path, agent: String, quiet_ms: u64) -> Self {
        let now = super::now_unix_ms();
        let turn = Self {
            started_ms: now,
            agent,
            output_ms: now,
            quiet_ms,
            deadline_ms: now + quiet_ms,
            ended: None,
        };
        turn.save(folder);
        turn
    }

    /// A line of the agent: the quiet clock starts again. Only the agent's own lines call it,
    /// so what the manager or the host writes never moves the deadline.
    pub fn output(&mut self, folder: &Path) {
        self.output_ms = super::now_unix_ms();
        self.deadline_ms = self.output_ms + self.quiet_ms;
        self.save(folder);
    }

    /// End the turn with `reason`; the record stays until the next turn.
    pub fn end(mut self, folder: &Path, reason: &str) {
        self.ended = Some(TurnEnd {
            unix_ms: super::now_unix_ms(),
            reason: view::clip(reason.lines().next().unwrap_or_default(), LAST_ENTRY_CHARS),
        });
        self.save(folder);
    }

    /// How long until the deadline.
    pub fn left(&self) -> Duration {
        Duration::from_millis(self.deadline_ms.saturating_sub(super::now_unix_ms()))
    }

    /// How long the agent has written no line.
    pub fn quiet(&self) -> Duration {
        Duration::from_millis(super::now_unix_ms().saturating_sub(self.output_ms))
    }

    /// The record in `folder`, if a turn ran there.
    pub fn read(folder: &Path) -> Option<Self> {
        let bytes = std::fs::read(folder.join(TURN_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The open turn in `folder` past its deadline: its host did not end it (an older host,
    /// or one whose loop does not run).
    pub fn overdue(folder: &Path, now_ms: u64) -> Option<Self> {
        Self::read(folder).filter(|turn| turn.ended.is_none() && now_ms > turn.deadline_ms)
    }

    /// ^ A failed write leaves the readers the last record; the host keeps its own in memory.
    fn save(&self, folder: &Path) {
        let written = serde_json::to_vec(self)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| store::write_atomic(&folder.join(TURN_FILE), &bytes));
        if let Err(error) = written {
            eprintln!("cannot write the turn record: {error:#}");
        }
    }
}

/// "turn on claude/opus is past its quiet bound of 30 min; its last output 2 h ago; ...".
fn silent(dir: &TaskDir, turn: &Turn, now_ms: u64) -> String {
    format!(
        "turn on {} is past its quiet bound of {}; {}",
        turn.agent,
        span(turn.quiet_ms),
        last_entry(dir, turn.output_ms, now_ms)
    )
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
    Turn::overdue(&dir.path, now_ms).map(|turn| format!("whose {}", silent(&dir, &turn, now_ms)))
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

/// The newest output of the task's worker, in unix ms: the last agent, blocked, result, checks
/// or check-run entry of the log, or the state's start (a new turn). An entry the manager or
/// the host wrote (a message of any kind, `after`, `hold`, a land notice) is no progress: it
/// would restart the quiet clock of a hung worker. ref: docs/tasks.md "Manager" (Stalls)
pub fn progress_ms(dir: &TaskDir, record: &TaskRecord) -> u64 {
    let output = dir
        .last_entry(|entry| {
            matches!(
                entry.event,
                LogEvent::Agent { .. }
                    | LogEvent::Blocked { .. }
                    | LogEvent::Result { .. }
                    | LogEvent::Checks { .. }
                    | LogEvent::CheckRun { .. }
            )
            .then_some(entry.unix_ms)
        })
        .ok()
        .flatten()
        .unwrap_or(0);
    output.max(record.state_unix_ms.max(record.created_unix_ms))
}

/// "its last output 2 h ago; its log ends with agent \"cargo build\"".
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
            format!("its last output {ago} ago; its log ends with {kind}{text}")
        }
        None => format!("its last output {ago} ago"),
    }
}
