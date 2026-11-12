//! Routines: a project's commands that wsxd runs by cron or on an event.
//! wsxd's scheduler is the only writer of the store; clients send an [`Action`] through
//! wsxd's socket ([`request`]) and may read the store ([`store::load`]).
//! ref: docs/tasks.md "Routines"

pub mod cron;
pub mod local;
pub mod store;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Cron(String),
    Event { kind: String },
}

impl Trigger {
    pub fn validated(mut self) -> Result<Self, RoutineError> {
        match &mut self {
            Self::Cron(expression) => {
                *expression = expression.trim().to_string();
                cron::CronSchedule::parse(expression)?;
            }
            Self::Event { kind } => {
                *kind = kind.trim().to_string();
                validate_event_kind(kind)?;
            }
        }
        Ok(self)
    }
}

fn validate_event_kind(kind: &str) -> Result<(), RoutineError> {
    if kind.is_empty()
        || kind.chars().any(|c| c.is_whitespace() || c.is_control())
        || !kind.contains('.')
        || kind.split('.').any(str::is_empty)
    {
        return Err(RoutineError::validation(
            "event kind must be a namespaced string like task.done, without spaces",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub name: String,
    pub trigger: Trigger,
    pub command: Vec<String>,
    pub prompt: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Whether the manager posts a run's end to the project's chat. Stored only when off.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub post: bool,
    /// Non-zero exit codes that mean the run completed and found something: such a run is
    /// `Findings`, not `Failed`. Stored only when set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings_exit: Vec<u8>,
}

const fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Routine {
    pub fn validated(mut self) -> Result<Self, RoutineError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty()
            || self.name.contains('/')
            || self.name.chars().any(char::is_control)
        {
            return Err(RoutineError::validation(
                "name must not be empty or contain '/' or control characters",
            ));
        }
        self.trigger = self.trigger.validated()?;
        if self.command.is_empty() || self.command.iter().any(String::is_empty) {
            return Err(RoutineError::validation(
                "command must contain nonempty argv items",
            ));
        }
        if self.findings_exit.contains(&0) {
            return Err(RoutineError::validation(
                "findings exit codes are 1 to 255, not 0",
            ));
        }
        self.findings_exit.sort_unstable();
        self.findings_exit.dedup();
        if self.command.iter().filter(|arg| *arg == "{prompt}").count() > 1 {
            return Err(RoutineError::validation(
                "command may contain at most one exact {prompt} argument",
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    /// The command exited with one of the routine's `findings_exit` codes: it completed and
    /// found something.
    Findings,
    Failed,
    SpawnFailed,
    Cancelled,
    /// The run's process ended without its result: a crash, or the upgrade from asched.
    Interrupted,
}

impl RunStatus {
    /// The status in the words the lists show (the web client's routine list has the same).
    pub fn words(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Findings => "found something",
            Self::Failed => "failed",
            Self::SpawnFailed => "did not start",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// The run went to its end: a success, or one that found something.
    pub fn completed(self) -> bool {
        matches!(self, Self::Succeeded | Self::Findings)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunCause {
    #[default]
    Manual,
    Cron {
        scheduled_epoch_minute: i64,
    },
    Event {
        kind: String,
        event_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub routine: String,
    pub started_epoch: i64,
    pub finished_epoch: Option<i64>,
    #[serde(default)]
    pub cause: RunCause,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    /// The run's supervisor process while it runs, and its start token against a reused pid.
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub process_start: Option<String>,
    pub final_output: String,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

/// `exit 3`, or `no exit code`.
pub fn exit_words(exit: Option<i32>) -> String {
    exit.map_or("no exit code".into(), |code| format!("exit {code}"))
}

impl RunRecord {
    /// `routine <name> succeeded after 4 s`, `routine <name> failed (exit 3)`, `routine <name>
    /// found something (exit 1)`: the run's end as the manager's events say it, and what
    /// `task::chat::run_messages` finds the run's message by.
    pub fn result_line(&self) -> String {
        let end = self.finished_epoch.unwrap_or_default();
        match &self.status {
            RunStatus::Succeeded => format!(
                "routine {} succeeded after {} s",
                self.routine,
                end - self.started_epoch
            ),
            status => format!(
                "routine {} {} ({})",
                self.routine,
                match status {
                    RunStatus::Findings => "found something",
                    RunStatus::SpawnFailed => "could not start",
                    RunStatus::Cancelled => "was cancelled",
                    RunStatus::Interrupted => "was interrupted",
                    _ => "failed",
                },
                exit_words(self.exit_code),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FireOutcome {
    Handled { routines: Vec<RoutineFire> },
    Deduplicated,
    NoMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineFire {
    Started { name: String },
    AlreadyRunning { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineView {
    pub routine: Routine,
    pub running: bool,
    pub next_run_epoch: Option<i64>,
    pub latest_run: Option<RunRecord>,
    pub recent_runs: Vec<RunRecord>,
}

/// What a project's routines say at a glance: the routine pill's count and tone on the web's
/// project screen and Home, and the Workspace tree's `◈` mark. Of the enabled routines, how
/// many failed or found something on their last run.
/// ref: docs/web-client-tasks.md "Routines", docs/tasks.md "TUI"
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub total: usize,
    pub on: usize,
    pub failed: usize,
    pub found: usize,
}

impl Counts {
    pub fn of(views: &[RoutineView]) -> Self {
        let on = views.iter().filter(|view| view.routine.enabled);
        let status = |view: &&RoutineView| view.latest_run.as_ref().map(|run| run.status);
        Self {
            total: views.len(),
            on: on.clone().count(),
            failed: on
                .clone()
                .filter(|view| {
                    status(view).is_some_and(|status| {
                        !matches!(
                            status,
                            RunStatus::Succeeded | RunStatus::Running | RunStatus::Findings
                        )
                    })
                })
                .count(),
            found: on
                .filter(|view| status(view) == Some(RunStatus::Findings))
                .count(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    List,
    Add {
        revision: u64,
        routine: Routine,
    },
    Edit {
        revision: u64,
        old_name: String,
        routine: Routine,
    },
    Delete {
        revision: u64,
        name: String,
    },
    Run {
        name: String,
    },
    Cancel {
        name: String,
    },
    Logs {
        name: String,
    },
    Fire {
        kind: String,
        payload: serde_json::Value,
        event_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Routines {
        revision: u64,
        routines: Vec<RoutineView>,
    },
    Runs {
        runs: Vec<RunRecord>,
    },
    Fire {
        outcome: FireOutcome,
    },
    Ok {
        revision: Option<u64>,
    },
    Error(RoutineError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineErrorKind {
    Validation,
    Duplicate,
    NotFound,
    Conflict,
    AlreadyRunning,
    Unavailable,
    Io,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineError {
    pub kind: RoutineErrorKind,
    pub message: String,
}

impl RoutineError {
    pub fn new(kind: RoutineErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(RoutineErrorKind::Validation, message)
    }
}

impl std::fmt::Display for RoutineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RoutineError {}

impl From<std::io::Error> for RoutineError {
    fn from(error: std::io::Error) -> Self {
        Self::new(RoutineErrorKind::Io, error.to_string())
    }
}

/// Send `action` for `project` to `client`'s wsxd, which runs the scheduler.
pub fn request(
    client: &crate::runtime::Client,
    project: &Path,
    action: Action,
) -> Result<Response, RoutineError> {
    let request = crate::runtime::Request::Routine {
        project: project.to_path_buf(),
        action,
    };
    match client.call(&request) {
        Ok(crate::runtime::Response::Routine(Response::Error(error))) => Err(error),
        Ok(crate::runtime::Response::Routine(response)) => Ok(response),
        // ^ A wsxd from before revision 41 has no scheduler and refuses the request.
        Ok(crate::runtime::Response::Error(error)) => Err(RoutineError::new(
            RoutineErrorKind::Unavailable,
            format!(
                "wsxd has no routine scheduler ({}); run `wsx daemon restart`",
                error.message
            ),
        )),
        Ok(other) => Err(RoutineError::new(
            RoutineErrorKind::Corrupt,
            format!("unexpected wsxd answer: {other:?}"),
        )),
        Err(error) => Err(RoutineError::new(
            RoutineErrorKind::Unavailable,
            format!("wsxd at {}: {error}", client.socket().display()),
        )),
    }
}

/// Turn the routine `name` on or off through the scheduler: the routine as it is stored, with
/// `enabled` set. `revision`: the list's revision the caller saw, else the current one.
pub fn set_enabled(
    client: &crate::runtime::Client,
    project: &Path,
    name: &str,
    revision: Option<u64>,
    enabled: bool,
) -> Result<Response, RoutineError> {
    let Response::Routines {
        revision: current,
        routines,
    } = request(client, project, Action::List)?
    else {
        return Err(RoutineError::new(
            RoutineErrorKind::Corrupt,
            "the scheduler answered a list with something else",
        ));
    };
    let mut routine = routines
        .into_iter()
        .find(|view| view.routine.name == name)
        .map(|view| view.routine)
        .ok_or_else(|| RoutineError::validation(format!("routine '{name}' not found")))?;
    routine.enabled = enabled;
    let action = Action::Edit {
        revision: revision.unwrap_or(current),
        old_name: name.to_string(),
        routine,
    };
    request(client, project, action)
}
