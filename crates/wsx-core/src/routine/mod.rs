//! Machine-local scheduled routines.
//!
//! Definitions are stored per canonical main repository. The daemon is the
//! only writer; application clients should use [`RoutineClient`]. The
//! versioned Unix-socket protocol remains available in [`ipc`] for diagnostics
//! and protocol-level integrations.

mod client;
mod cron;
pub mod daemon;
pub mod execution;
pub mod ipc;
pub mod store;

pub use client::RoutineClient;
pub use cron::{CronSchedule, LocalTime};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_RUNS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    pub name: String,
    pub cron: String,
    pub command: Vec<String>,
    pub prompt: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

impl Routine {
    pub fn validated(mut self) -> Result<Self, RoutineError> {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err(RoutineError::Validation("name must not be empty".into()));
        }
        if self.name.contains('/') || self.name.contains('\0') {
            return Err(RoutineError::Validation(
                "name must not contain '/' or NUL".into(),
            ));
        }
        CronSchedule::parse(&self.cron)?;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub routine: String,
    pub started_epoch: i64,
    pub finished_epoch: Option<i64>,
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
    fn routine_validation_rejects_ambiguous_boundaries() {
        for routine in [
            Routine {
                name: " ".into(),
                cron: "* * * * *".into(),
                command: vec!["x".into()],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                cron: "60 * * * *".into(),
                command: vec!["x".into()],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                cron: "* * * * *".into(),
                command: vec![],
                prompt: String::new(),
                enabled: true,
            },
            Routine {
                name: "x".into(),
                cron: "* * * * *".into(),
                command: vec!["{prompt}".into(), "{prompt}".into()],
                prompt: String::new(),
                enabled: true,
            },
        ] {
            assert!(routine.validated().is_err());
        }
    }
}
