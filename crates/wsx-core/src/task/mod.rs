//! Task records for manager mode: one worker, its working copy and its log.
//! ref: docs/tasks.md
//!
//! `store` owns the files under `$XDG_STATE_HOME/wsx/tasks/`. While a task's
//! host runs it is the only writer; the CLI writes only before the host starts
//! and after it has exited.

pub mod copy;
pub mod manager;
pub mod owner;
pub mod report;
pub mod store;
pub mod view;

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
    /// A landing rebase conflicted; the worker is resolving it.
    Conflict,
    /// A pull request carries the task (`pr` landing policy).
    InReview,
    /// The revision is on the default branch.
    Landed,
    Closed,
}

impl TaskState {
    /// Still part of the project's work: counted for overlap and listed by default.
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Landed | Self::Closed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Conflict => "conflict",
            Self::InReview => "in_review",
            Self::Landed => "landed",
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
    #[serde(default = "default_check_timeout_secs")]
    pub check_timeout_secs: u64,
    /// The adapter's own conversation ID, used to resume after a stop.
    pub agent_session: Option<String>,
    pub state: TaskState,
    pub revision: u32,
    /// Paths changed in `base..r<revision>`, capped; `paths_total` keeps the full count.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub paths_total: u32,
    /// The pull request of the `pr` landing policy, and the revision last pushed to it.
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub pr_revision: Option<u32>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Prompt,
    Steer,
    Add,
    /// The owner's note on a failed human verification, sent to the worker.
    Verify,
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
        /// Tool calls the agent refused during the turn; the worker may have continued without them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        denials: Vec<String>,
    },
    Checks {
        results: Vec<CheckResult>,
    },
    Revision {
        revision: u32,
        commit: String,
        #[serde(default)]
        base: String,
        #[serde(default)]
        paths_total: u32,
    },
    Stopped {
        reason: String,
    },
    Closed {
        discarded: bool,
    },
    /// A human verdict on one revision, or a pass carried over a rebase.
    Verified {
        revision: u32,
        result: Verdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carried_from: Option<u32>,
    },
    /// `land --skip-verify` landed a revision without a pass.
    VerifySkipped {
        revision: u32,
    },
    Conflict {
        onto: String,
        paths: Vec<String>,
    },
    /// A landing that started did not finish; the task keeps its state.
    LandStopped {
        reason: String,
    },
    Landed {
        commit: String,
    },
    /// A revision was pushed to the task's pull request.
    InReview {
        url: String,
        revision: u32,
    },
    /// The pull request was closed without a merge.
    PrClosed {
        url: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// True when the verdict that applies to `revision` is a pass.
pub fn passed(log: &[LogEntry], revision: u32) -> bool {
    matches!(
        verification(log, revision).map(|entry| &entry.event),
        Some(LogEvent::Verified {
            result: Verdict::Pass,
            ..
        })
    )
}

/// The verdict that applies to `revision`: the latest one recorded for it.
/// A new revision has none until someone verifies it or a pass is carried.
pub fn verification(log: &[LogEntry], revision: u32) -> Option<&LogEntry> {
    log.iter().rev().find(|entry| {
        matches!(&entry.event, LogEvent::Verified { revision: verified, .. } if *verified == revision)
    })
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
    pub cache_creation_tokens: u64,
    pub cost_micro_usd: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub unix_ms: u64,
    #[serde(flatten)]
    pub event: LogEvent,
}

fn default_check_timeout_secs() -> u64 {
    crate::model::workspace::DEFAULT_CHECK_TIMEOUT_SECS
}

pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// One line of `deploys.jsonl`: a deploy or release of one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRecord {
    pub kind: DeployKind,
    pub commit: String,
    pub exit: Option<i32>,
    pub unix_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployKind {
    Deploy,
    Release,
}

/// One request on a host's `host.sock`; one JSON line per connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HostRequest {
    Status,
    Steer {
        text: String,
    },
    Add {
        text: String,
    },
    Allow {
        tool: String,
    },
    Stop,
    Land {
        skip_verify: bool,
    },
    Verify {
        result: Verdict,
        note: Option<String>,
    },
    Close {
        discard: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResponse {
    pub ok: bool,
    pub state: TaskState,
    /// What happened, or why the request was refused.
    pub message: String,
}

const MAX_HOST_RESPONSE_BYTES: u64 = 64 * 1024;

/// Send one request to a running task host. An error means no host answered.
pub fn call_host(
    socket: &std::path::Path,
    request: &HostRequest,
    timeout: std::time::Duration,
) -> anyhow::Result<HostResponse> {
    call_socket(socket, request, timeout)
}

/// One JSON request and one JSON answer on a host's owner-only socket.
pub fn call_socket<Request: Serialize, Response: serde::de::DeserializeOwned>(
    socket: &std::path::Path,
    request: &Request,
    timeout: std::time::Duration,
) -> anyhow::Result<Response> {
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
