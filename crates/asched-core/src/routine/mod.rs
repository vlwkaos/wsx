//! Machine-local scheduled routines.
//!
//! Definitions are stored per canonical working directory. The daemon is the
//! only writer; application clients should use [`RoutineClient`]. The
//! versioned Unix-socket protocol remains available in [`ipc`] for diagnostics
//! and protocol-level integrations.

mod client;
mod cron;
pub mod daemon;
pub mod execution;
pub mod ipc;
pub mod store;

pub use client::{RoutineClient, STARTUP_FD_ENV};
pub use cron::{CronSchedule, LocalTime};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROJECT_CONFIG_VERSION: u32 = 2;
pub const RUNTIME_STATE_VERSION: u32 = 2;
pub const TRANSACTION_VERSION: u32 = 1;
/// Current project-config version. Retained as a source-compatibility alias.
pub const SCHEMA_VERSION: u32 = PROJECT_CONFIG_VERSION;
pub const PROTOCOL_VERSION: u32 = 3;
pub const MAX_RUNS: usize = 20;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_EVENT_RECEIPTS: usize = 4_096;

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
                CronSchedule::parse(expression)?;
            }
            Self::Event { kind } => {
                *kind = kind.trim().to_string();
                if kind.is_empty() {
                    return Err(RoutineError::Validation(
                        "event kind must not be empty".into(),
                    ));
                }
                if kind.chars().any(char::is_whitespace)
                    || kind.chars().any(char::is_control)
                    || !kind.contains('.')
                    || kind.split('.').any(str::is_empty)
                {
                    return Err(RoutineError::Validation(
                        "event kind must be a namespaced string without whitespace or control characters"
                            .into(),
                    ));
                }
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
// ^ [[Routine Scheduling and Run Lifecycle]]
pub struct Routine {
    pub name: String,
    pub trigger: Trigger,
    pub command: Vec<String>,
    pub prompt: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

impl<'de> Deserialize<'de> for Routine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredRoutine {
            name: String,
            #[serde(default)]
            trigger: Option<Trigger>,
            #[serde(default)]
            cron: Option<String>,
            command: Vec<String>,
            prompt: String,
            #[serde(default = "default_enabled")]
            enabled: bool,
        }
        let stored = StoredRoutine::deserialize(deserializer)?;
        let trigger = match (stored.trigger, stored.cron) {
            (Some(trigger), None) => trigger,
            (None, Some(cron)) => Trigger::Cron(cron),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "routine must contain exactly one trigger representation",
                ))
            }
            (None, None) => return Err(serde::de::Error::missing_field("trigger")),
        };
        Ok(Self {
            name: stored.name,
            trigger,
            command: stored.command,
            prompt: stored.prompt,
            enabled: stored.enabled,
        })
    }
}

impl Routine {
    pub fn validated(mut self) -> Result<Self, RoutineError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(RoutineError::Validation("name must not be empty".into()));
        }
        if self.name.contains('/') || self.name.chars().any(char::is_control) {
            return Err(RoutineError::Validation(
                "name must not contain '/' or control characters".into(),
            ));
        }
        self.trigger = self.trigger.validated()?;
        if self.command.is_empty() || self.command.iter().any(|arg| arg.is_empty()) {
            return Err(RoutineError::Validation(
                "command must contain nonempty argv items".into(),
            ));
        }
        if self
            .command
            .iter()
            .filter(|arg| arg.as_str() == "{prompt}")
            .count()
            > 1
        {
            return Err(RoutineError::Validation(
                "command may contain at most one exact {prompt} argument".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    SpawnFailed,
    Cancelled,
    Interrupted,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub routine: String,
    pub started_epoch: i64,
    pub finished_epoch: Option<i64>,
    #[serde(default)]
    pub cause: RunCause,
    /// Retained in memory for source compatibility; v2 readers should use `cause`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_epoch_minute: Option<i64>,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub pid: Option<i32>,
    /// OS process-start token used to reject a recycled PID/process group.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub can_edit: bool,
    pub can_delete: bool,
    pub can_run: bool,
    pub can_cancel: bool,
    pub can_rename: bool,
    pub can_toggle_enabled: bool,
}

impl Capabilities {
    pub fn for_running(running: bool) -> Self {
        Self {
            can_edit: true,
            can_delete: true,
            can_run: !running,
            can_cancel: running,
            can_rename: !running,
            can_toggle_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineErrorKind {
    Validation,
    Duplicate,
    NotFound,
    Conflict,
    ProjectCollision,
    AlreadyRunning,
    ProtocolMismatch,
    Unavailable,
    Io,
    Corrupt,
}

impl std::fmt::Display for RoutineErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = serde_json::to_value(self).map_err(|_| std::fmt::Error)?;
        formatter.write_str(value.as_str().ok_or(std::fmt::Error)?)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RoutineError {
    #[error("invalid routine: {0}")]
    Validation(String),
    #[error("routine '{0}' already exists")]
    Duplicate(String),
    #[error("routine '{0}' not found")]
    NotFound(String),
    #[error("stale config revision: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("project identity collision: expected {expected}, stored {stored}")]
    ProjectCollision { expected: PathBuf, stored: PathBuf },
    #[error("routine '{0}' is already running")]
    AlreadyRunning(String),
    #[error("protocol mismatch: client {client}, daemon {daemon}")]
    ProtocolMismatch { client: u32, daemon: u32 },
    #[error("daemon unavailable: {0}")]
    Unavailable(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("invalid stored data: {0}")]
    Corrupt(String),
    #[error("routine daemon {kind}: {message}")]
    RemoteDaemon {
        kind: RoutineErrorKind,
        message: String,
    },
}

impl RoutineError {
    pub fn kind(&self) -> RoutineErrorKind {
        match self {
            Self::Validation(_) => RoutineErrorKind::Validation,
            Self::Duplicate(_) => RoutineErrorKind::Duplicate,
            Self::NotFound(_) => RoutineErrorKind::NotFound,
            Self::Conflict { .. } => RoutineErrorKind::Conflict,
            Self::ProjectCollision { .. } => RoutineErrorKind::ProjectCollision,
            Self::AlreadyRunning(_) => RoutineErrorKind::AlreadyRunning,
            Self::ProtocolMismatch { .. } => RoutineErrorKind::ProtocolMismatch,
            Self::Unavailable(_) => RoutineErrorKind::Unavailable,
            Self::Io(_) => RoutineErrorKind::Io,
            Self::Corrupt(_) => RoutineErrorKind::Corrupt,
            Self::RemoteDaemon { kind, .. } => *kind,
        }
    }
}

impl From<std::io::Error> for RoutineError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_cron_and_typed_event_routines_share_one_serialization_boundary() {
        let legacy = r#"
name = "legacy"
cron = "0 9 * * *"
command = ["/bin/true"]
prompt = ""
enabled = true
"#;
        let cron: Routine = toml::from_str(legacy).unwrap();
        assert_eq!(cron.trigger, Trigger::Cron("0 9 * * *".into()));

        let event = Routine {
            name: "event".into(),
            trigger: Trigger::Event {
                kind: "filesystem.changed".into(),
            },
            command: vec!["/bin/true".into()],
            prompt: String::new(),
            enabled: true,
        };
        let encoded = toml::to_string(&event).unwrap();
        let decoded: Routine = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, event);
        assert!(!encoded.contains("cron"));
    }

    #[test]
    fn routine_validation_rejects_ambiguous_boundaries() {
        for routine in [
            Routine {
                name: " ".into(),
                trigger: Trigger::Cron("* * * * *".into()),
                command: vec!["x".into()],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                trigger: Trigger::Cron("60 * * * *".into()),
                command: vec!["x".into()],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                trigger: Trigger::Cron("* * * * *".into()),
                command: vec![],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                trigger: Trigger::Cron("* * * * *".into()),
                command: vec!["{prompt}".into(), "{prompt}".into()],
                prompt: String::new(),
                enabled: true,
            },
        ] {
            assert!(routine.validated().is_err());
        }
    }

    #[test]
    fn event_trigger_validation_rejects_empty_namespace_segments() {
        for kind in [".", ".changed", "source.", "source..changed"] {
            assert!(matches!(
                (Trigger::Event { kind: kind.into() }).validated(),
                Err(RoutineError::Validation(_))
            ));
        }
    }

    #[test]
    fn given_ansi_and_control_characters_in_name_when_validated_then_each_is_rejected() {
        let rejected = [
            "\u{1b}[31mdanger",
            "line\nbreak",
            "tab\tname",
            "delete\u{7f}",
        ]
        .into_iter()
        .all(|name| {
            matches!(
                (Routine {
                    name: name.into(),
                    trigger: Trigger::Cron("* * * * *".into()),
                    command: vec!["/bin/true".into()],
                    prompt: String::new(),
                    enabled: true,
                })
                .validated(),
                Err(RoutineError::Validation(_))
            )
        });

        assert!(rejected);
    }
}
