use super::{Capabilities, Routine, RoutineError, RunRecord, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub protocol: u32,
    pub project: PathBuf,
    pub action: Action,
}

impl Request {
    pub fn new(project: PathBuf, action: Action) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            project,
            action,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    List,
    Show {
        name: String,
    },
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
    Status,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineView {
    pub routine: Routine,
    pub capabilities: Capabilities,
    #[serde(default)]
    pub next_run_epoch: Option<i64>,
    pub latest_run: Option<RunRecord>,
    #[serde(default)]
    pub recent_runs: Vec<RunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Routines {
        revision: u64,
        routines: Vec<RoutineView>,
    },
    Routine {
        revision: u64,
        routine: RoutineView,
    },
    Runs {
        runs: Vec<RunRecord>,
    },
    Run {
        run: RunRecord,
    },
    Daemon {
        protocol: u32,
        pid: u32,
    },
    Ok {
        revision: Option<u64>,
    },
    Error {
        kind: String,
        message: String,
    },
}

impl Response {
    pub fn error(error: RoutineError) -> Self {
        let kind = match error {
            RoutineError::Validation(_) => "validation",
            RoutineError::Duplicate(_) => "duplicate",
            RoutineError::NotFound(_) => "not_found",
            RoutineError::Conflict { .. } => "conflict",
            RoutineError::ProjectCollision { .. } => "project_collision",
            RoutineError::AlreadyRunning(_) => "already_running",
            RoutineError::ProtocolMismatch { .. } => "protocol_mismatch",
            RoutineError::Unavailable(_) => "unavailable",
            RoutineError::Io(_) => "io",
            RoutineError::Corrupt(_) => "corrupt",
        }
        .to_string();
        Self::Error {
            kind,
            message: error.to_string(),
        }
    }

    pub fn into_result(self) -> Result<Self, RoutineError> {
        match self {
            Self::Error { message, .. } => Err(RoutineError::Unavailable(message)),
            other => Ok(other),
        }
    }
}

pub fn send(socket: &Path, request: &Request) -> Result<Response, RoutineError> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| RoutineError::Unavailable(format!("{}: {e}", socket.display())))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut data = serde_json::to_vec(request).map_err(|e| RoutineError::Corrupt(e.to_string()))?;
    data.push(b'\n');
    stream.write_all(&data)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        return Err(RoutineError::Unavailable("daemon closed connection".into()));
    }
    serde_json::from_str(&line)
        .map_err(|e| RoutineError::Corrupt(format!("invalid daemon response: {e}")))
}
