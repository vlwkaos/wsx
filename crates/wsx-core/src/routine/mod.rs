//! Routines: a project's commands that wsxd runs by cron, at one minute, or on an event.
//! wsxd's scheduler is the only writer of the store; clients send an [`Action`] through
//! wsxd's socket ([`request`]) and may read the store ([`store::load`]).
//! ref: docs/tasks.md "Routines"

pub mod cron;
pub mod store;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Cron(String),
    Event {
        kind: String,
    },
    /// One run at this epoch minute; a missed minute runs at the next tick, once.
    At {
        epoch_minute: i64,
    },
}

impl Trigger {
    pub fn validated(mut self) -> Result<Self, RoutineError> {
        match &mut self {
            Self::Cron(expression) => {
                *expression = expression.trim().to_string();
                cron::CronSchedule::parse(expression)?;
            }
            Self::At { epoch_minute } if *epoch_minute <= 0 => {
                return Err(RoutineError::validation(
                    "at must be a positive epoch minute",
                ));
            }
            Self::At { .. } => {}
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
