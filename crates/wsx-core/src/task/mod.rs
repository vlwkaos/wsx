//! Task records for manager mode: one worker, its working copy and its log.
//! ref: docs/tasks.md
//!
//! `store` owns the files under `$XDG_STATE_HOME/wsx/tasks/`. While a task's
//! host runs it is the only writer; the CLI writes only before the host starts
//! and after it has exited.

pub mod copy;
pub mod store;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Largest prompt, steer or add, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Starting,
    Working,
    Blocked,
    Checking,
    Ready,
    Failed,
    Stopped,
    Closed,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Closed => "closed",
        }
    }
}

/// `task.json`: what the task is and where it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u32,
    pub project: String,
    pub project_id: String,
    pub project_path: PathBuf,
    pub default_branch: String,
    pub base: String,
    pub branch: String,
    pub working_copy: PathBuf,
    pub pane_id: Option<u64>,
    pub adapter: String,
    pub model: String,
    pub tools: Vec<String>,
    pub checks: Vec<String>,
    /// The adapter's own conversation ID, used to resume after a stop.
    pub agent_session: Option<String>,
    pub state: TaskState,
    pub revision: u32,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Prompt,
    Steer,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub command: String,
    pub exit: Option<i32>,
    pub timed_out: bool,
    /// The start of stdout, then of stderr, bounded.
    pub output: String,
}

/// One line of `log.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogEvent {
    Created {
        base: String,
        branch: String,
    },
    Message {
        mode: MessageKind,
        text: String,
    },
    Agent {
        text: String,
    },
    Blocked {
        denials: Vec<String>,
    },
    Allowed {
        tool: String,
    },
    Result {
        status: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
    },
    Checks {
        results: Vec<CheckResult>,
    },
    Revision {
        revision: u32,
        commit: String,
    },
    Stopped {
        reason: String,
    },
    Closed {
        discarded: bool,
    },
}

impl LogEvent {
    /// Agent progress may be dropped when the log is full; every other event is kept.
    pub(crate) fn is_progress(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// What one turn cost, as the adapter reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnUsage {
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_micro_usd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub unix_ms: u64,
    #[serde(flatten)]
    pub event: LogEvent,
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// One request on a host's `host.sock`; one JSON line per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HostRequest {
    Status,
    Steer { text: String },
    Add { text: String },
    Allow { tool: String },
    Stop,
    Close { discard: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResponse {
    pub ok: bool,
    pub state: TaskState,
    /// What happened, or why the request was refused.
    pub message: String,
}

const MAX_HOST_RESPONSE_BYTES: u64 = 64 * 1024;

/// Send one request to a running host. An error means no host answered.
pub fn call_host(
    socket: &std::path::Path,
    request: &HostRequest,
    timeout: std::time::Duration,
) -> anyhow::Result<HostResponse> {
    use anyhow::Context;
    use std::io::{Read, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("no task host answers on {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut bytes = Vec::new();
    stream
        .take(MAX_HOST_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("the task host did not answer in time")?;
    if bytes.len() as u64 > MAX_HOST_RESPONSE_BYTES {
        anyhow::bail!("the task host answer is larger than {MAX_HOST_RESPONSE_BYTES} bytes");
    }
    serde_json::from_slice(&bytes).context("the task host answer is not valid JSON")
}

/// The structured result a worker returns at the end of each turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerResult {
    pub status: String,
    pub summary: String,
    pub decisions: Vec<Decision>,
    pub human_verify: Vec<HumanVerify>,
    pub left_open: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    pub choice: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HumanVerify {
    pub check: String,
    pub pass_if: String,
}

/// JSON Schema given to the adapter for `WorkerResult`.
pub fn worker_result_schema() -> serde_json::Value {
    let text = serde_json::json!({ "type": "string" });
    let pair = |a: &str, b: &str| {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [a, b],
            "properties": { a: text, b: text },
        })
    };
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["status", "summary", "decisions", "human_verify", "left_open", "notes"],
        "properties": {
            "status": { "type": "string", "enum": ["done", "blocked", "failed"] },
            "summary": text,
            "decisions": { "type": "array", "items": pair("choice", "reason") },
            "human_verify": { "type": "array", "items": pair("check", "pass_if") },
            "left_open": { "type": "array", "items": text },
            "notes": text,
        },
    })
}
