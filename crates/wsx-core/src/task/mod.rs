//! Task records for manager mode: one worker, its working copy and its log.
//! ref: docs/tasks.md
//!
//! `store` owns the files under `$XDG_STATE_HOME/wsx/tasks/`. While a task's
//! host runs it is the only writer; the CLI writes only before the host starts
//! and after it has exited.

pub mod brief;
pub mod checks;
pub mod copy;
pub mod ledger;
pub mod manager;
pub mod owner;
pub mod report;
pub mod store;
pub mod view;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Largest prompt, steer or add, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Longest tool rule a grant holds.
pub const MAX_RULE_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Starting,
    Working,
    /// The brief is written; the task waits for the owner's `start` or a reply.
    Briefed,
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
            Self::Briefed => "briefed",
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

/// Whether the worker still writes the brief (read-only turns) or builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Brief,
    /// Records from before the brief phase build from the start.
    #[default]
    Build,
}

/// A host or a path an agent needs, or a tool rule: what a block asks for and what a
/// grant gives. ref: docs/tasks.md "Permissions"
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Access {
    Network { host: String },
    Write { path: String },
    Tool { rule: String },
}

impl Access {
    /// Refuse what a grant may never hold: a malformed host or path, a Bash or bare
    /// file-tool rule.
    pub fn check(&self) -> Result<(), String> {
        use crate::config::project::{tool_refusal, validate_host, validate_write_path};
        match self {
            Self::Network { host } => validate_host(host),
            Self::Write { path } => validate_write_path(path),
            Self::Tool { rule } => {
                if rule.is_empty()
                    || rule.len() > MAX_RULE_BYTES
                    || rule.chars().any(char::is_control)
                {
                    return Err(format!(
                        "a tool rule must be 1..{MAX_RULE_BYTES} bytes without control characters"
                    ));
                }
                tool_refusal(rule).map_or(Ok(()), |reason| Err(format!("{rule}: {reason}")))
            }
        }
    }

    /// "reach crates.io", "write ~/.cargo", "use WebSearch".
    pub fn describe(&self) -> String {
        match self {
            Self::Network { host } => format!("reach {host}"),
            Self::Write { path } => format!("write {path}"),
            Self::Tool { rule } => format!("use {rule}"),
        }
    }
}

/// Who acted: the owner, or the manager's agent (`WSX_TASK_ACTOR=manager`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    #[default]
    Owner,
    Manager,
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
    /// Tools beyond the fixed ones (`tasks.agent.tools`). Records from before the sandbox
    /// may hold `Bash(...)` rules; the launch drops them.
    pub tools: Vec<String>,
    #[serde(default)]
    pub phase: Phase,
    /// `tasks.sandbox` as it was when the task was created.
    #[serde(default)]
    pub sandbox: crate::model::workspace::TaskSandbox,
    /// Hosts, paths and tools granted to this task; the project's own are in `grants.json`.
    #[serde(default)]
    pub grants: Vec<Access>,
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
    /// `task new --title`; without one the title comes from the prompt (`view::title`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The `checks.jsonl` item whose fail started this task (docs/tasks.md "To check").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes: Option<String>,
    /// Who ran `task new`. A ready task the manager created waits for its review instead of
    /// landing by itself (docs/tasks.md "Landing").
    #[serde(default)]
    pub created_by: Actor,
    /// Why the task waits for something other than a person, such as free disk; the host
    /// resumes it by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Prompt,
    Steer,
    Add,
    /// A verdict note from before M4f; old logs only.
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
        /// What the sandbox or a permission stopped in the turn, for the owner to grant.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        asks: Vec<Access>,
    },
    /// A tool rule allowed before grants; old logs only.
    Allowed {
        tool: String,
    },
    /// `project`: kept for every task of the project (`grants.json`).
    Granted {
        access: Access,
        project: bool,
        by: Actor,
    },
    Revoked {
        access: Access,
        project: bool,
        by: Actor,
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
    /// Failed checks of `revision` went back to the worker as turn `attempt`.
    CheckRetry {
        revision: u32,
        attempt: u32,
        commands: Vec<String>,
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
    /// A per-task verdict from before M4f; old logs only, shown nowhere.
    Verified {
        #[serde(default)]
        revision: u32,
    },
    /// `land --skip-verify` from before M4f; old logs only.
    VerifySkipped {
        #[serde(default)]
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
    /// A brief turn returned brief `version` (saved in `brief.json`), with the worker's
    /// answer to the owner's reply when the turn was a talk-through.
    Brief {
        version: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        answer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        denials: Vec<String>,
    },
    /// The task builds without a brief: `by` is `flag` (`--no-brief`) or `config` (`tasks.brief: false`).
    BriefSkipped {
        by: String,
        actor: String,
    },
    /// The task waits for free disk before a turn or its checks (`tasks.minFreeGb`).
    DiskWait {
        free_mb: u64,
        min_gb: u64,
    },
    /// A preview of the task answers at `url` (docs/tasks.md "Preview").
    Preview {
        url: String,
        port: u16,
    },
    /// The worker ran checks with `wsx task check-run`; no revision is recorded.
    CheckRun {
        results: Vec<CheckResult>,
    },
    /// The build of brief `version` started; `prompt` is its rewritten prompt. `auto`: the
    /// brief had no open questions, so it started by itself.
    Started {
        version: u32,
        prompt: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auto: bool,
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

/// One request on a task host's control socket; one JSON line per connection.
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
    /// Add a grant and resume a blocked task. A project grant is already in `grants.json`.
    Grant {
        access: Access,
        project: bool,
        by: Actor,
    },
    Revoke {
        access: Access,
        project: bool,
        by: Actor,
    },
    Stop,
    /// Build the brief's rewritten prompt.
    Start,
    /// Land the ready revision. Fields of older callers (`skip_verify`) are ignored.
    Land {},
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

/// `call_host` for an action: a host that execs its replaced wsx binary has no socket for
/// a moment, so a connect that finds none is tried again for up to 3 s. Status probes use
/// `call_host` and stay instant. ref: docs/tasks.md "Host"
pub fn call_host_waiting(
    socket: &std::path::Path,
    request: &HostRequest,
    timeout: std::time::Duration,
) -> anyhow::Result<HostResponse> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match call_socket(socket, request, timeout) {
            Err(error)
                if std::time::Instant::now() < deadline
                    && error
                        .root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| {
                            matches!(
                                io.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::ConnectionRefused
                            )
                        }) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            answer => return answer,
        }
    }
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
    /// What the worker proved, each with the command or capture that shows it.
    pub evidence: Vec<Evidence>,
    /// Only what needs a person: look, feel, a real device, real data.
    pub human_verify: Vec<HumanVerify>,
    pub left_open: Vec<String>,
    pub notes: String,
    /// Durable project knowledge a later task needs; empty when there is none. The manager
    /// folds it into its notes on rotation. ref: docs/tasks.md "Memo"
    pub memo: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    pub choice: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub check: String,
    /// The command that was run, or the capture's path.
    pub proof: String,
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
        "required": ["status", "summary", "decisions", "evidence", "human_verify", "left_open", "notes", "memo"],
        "properties": {
            "status": { "type": "string", "enum": ["done", "blocked", "failed"] },
            "summary": text,
            "decisions": { "type": "array", "items": pair("choice", "reason") },
            "evidence": { "type": "array", "items": pair("check", "proof") },
            "human_verify": { "type": "array", "items": pair("check", "pass_if") },
            "left_open": { "type": "array", "items": text },
            "notes": text,
            "memo": text,
        },
    })
}
