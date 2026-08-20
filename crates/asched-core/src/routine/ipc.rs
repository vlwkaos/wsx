use super::{
    Capabilities, FireOutcome, Routine, RoutineError, RoutineErrorKind, RunRecord, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_RESPONSE_FRAME_BYTES: usize = 64 * 1024 * 1024;

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
    SetEnabled {
        revision: u64,
        name: String,
        enabled: bool,
    },
    Run {
        name: String,
    },
    Fire {
        kind: String,
        payload: Value,
        event_id: String,
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
        routine: Box<RoutineView>,
    },
    Runs {
        runs: Vec<RunRecord>,
    },
    Fire {
        outcome: FireOutcome,
    },
    Daemon {
        protocol: u32,
        pid: u32,
    },
    Ok {
        revision: Option<u64>,
    },
    Error {
        kind: RoutineErrorKind,
        message: String,
    },
}

impl Response {
    pub fn error(error: RoutineError) -> Self {
        let kind = error.kind();
        Self::Error {
            kind,
            message: error.to_string(),
        }
    }

    pub fn into_result(self) -> Result<Self, RoutineError> {
        match self {
            Self::Error { kind, message } => Err(RoutineError::RemoteDaemon { kind, message }),
            other => Ok(other),
        }
    }
}

pub fn send(socket: &Path, request: &Request) -> Result<Response, RoutineError> {
    send_inner(socket, request, Duration::from_secs(30), false)
}

pub(crate) fn send_with_timeout(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response, RoutineError> {
    send_inner(socket, request, timeout, true)
}

fn send_inner(
    socket: &Path,
    request: &Request,
    timeout: Duration,
    timeout_is_unavailable: bool,
) -> Result<Response, RoutineError> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| RoutineError::Unavailable(format!("{}: {e}", socket.display())))?;
    stream.set_read_timeout(Some(timeout))?;
    let mut data = serde_json::to_vec(request).map_err(|e| RoutineError::Corrupt(e.to_string()))?;
    data.push(b'\n');
    stream.write_all(&data)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let frame =
        read_response_frame(
            BufReader::new(stream),
            MAX_RESPONSE_FRAME_BYTES,
            |error| match error.kind() {
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    if timeout_is_unavailable =>
                {
                    RoutineError::Unavailable("daemon response timed out".into())
                }
                _ => error.into(),
            },
        )?;
    serde_json::from_slice(&frame)
        .map_err(|e| RoutineError::Corrupt(format!("invalid daemon response: {e}")))
}

fn read_response_frame(
    reader: impl BufRead,
    max_bytes: usize,
    map_io: impl FnOnce(std::io::Error) -> RoutineError,
) -> Result<Vec<u8>, RoutineError> {
    let mut frame = Vec::new();
    reader
        .take((max_bytes + 2) as u64)
        .read_until(b'\n', &mut frame)
        .map_err(map_io)?;
    if frame.is_empty() {
        return Err(RoutineError::Unavailable("daemon closed connection".into()));
    }
    if !frame.ends_with(b"\n") {
        return Err(RoutineError::Corrupt(
            "daemon response frame must end with a newline".into(),
        ));
    }
    if frame.len() > max_bytes + 1 {
        return Err(RoutineError::Corrupt(format!(
            "daemon response frame exceeds {max_bytes} bytes"
        )));
    }
    frame.pop();
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_v1_error_kinds_keep_the_existing_wire_strings() {
        let cases = [
            (RoutineErrorKind::Validation, "validation"),
            (RoutineErrorKind::Duplicate, "duplicate"),
            (RoutineErrorKind::NotFound, "not_found"),
            (RoutineErrorKind::Conflict, "conflict"),
            (RoutineErrorKind::ProjectCollision, "project_collision"),
            (RoutineErrorKind::AlreadyRunning, "already_running"),
            (RoutineErrorKind::ProtocolMismatch, "protocol_mismatch"),
            (RoutineErrorKind::Unavailable, "unavailable"),
            (RoutineErrorKind::Io, "io"),
            (RoutineErrorKind::Corrupt, "corrupt"),
        ];
        for (kind, wire) in cases {
            let response = Response::Error {
                kind,
                message: "detail".into(),
            };
            let json = serde_json::to_string(&response).unwrap();
            assert!(json.contains(&format!(r#""kind":"{wire}""#)));
            let decoded: Response = serde_json::from_str(&json).unwrap();
            assert!(matches!(decoded, Response::Error { kind: decoded, .. } if decoded == kind));
        }
    }

    #[test]
    fn daemon_error_category_and_message_cross_the_client_boundary() {
        let response: Response = serde_json::from_str(
            r#"{"result":"error","kind":"conflict","message":"stale revision"}"#,
        )
        .unwrap();
        assert!(matches!(
            response.into_result(),
            Err(RoutineError::RemoteDaemon {
                kind: RoutineErrorKind::Conflict,
                message,
            }) if message == "stale revision"
        ));
    }

    #[test]
    fn unknown_error_kind_is_rejected_as_an_invalid_closed_domain() {
        let result = serde_json::from_str::<Response>(
            r#"{"result":"error","kind":"future_kind","message":"detail"}"#,
        );
        assert!(result.is_err());
    }
}

#[cfg(test)]
#[path = "ipc_contract_tests.rs"]
mod ipc_contract_tests;
