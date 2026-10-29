//! Task records for manager mode: one worker, its working copy and its log.
//! ref: docs/tasks.md
//!
//! `store` owns the files under `$XDG_STATE_HOME/wsx/tasks/`. While a task's
//! host runs it is the only writer; the CLI writes only before the host starts
//! and after it has exited.

pub mod build;
pub mod checks;
pub mod copy;
pub mod ledger;
pub mod manager;
pub mod owner;
pub mod report;
pub mod roles;
pub mod store;
pub mod view;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Largest prompt, steer or add, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// An agent turn runs, also one that resolves a landing conflict.
    Working,
    /// Nothing runs until what `TaskRecord::waiting` names happens.
    Waiting,
    Checking,
    Ready,
    Failed,
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
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Checking => "checking",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Landed => "landed",
            Self::Closed => "closed",
        }
    }
}

/// What a `waiting` task waits for. ref: docs/tasks.md "Store"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOn {
    /// An answer: the worker asked, was stopped, or its host restarted during a turn.
    /// ^ `permission` is a record from before wsx's own boundary replaced the grants.
    #[serde(alias = "permission")]
    Owner,
    /// `tasks.minFreeGb` of free disk; the host resumes by itself.
    Disk,
    /// A `--plan` task's first result is its plan: the manager reads it and answers with an
    /// `add` (go, or what to change). ref: docs/tasks.md "Workflow"
    Plan,
    /// The owner works in the working copy; nothing runs until `handback`.
    /// ref: docs/tasks.md "By hand"
    Hand,
    /// The machine's check slot: a `checking` task, or a worker's `check-run`, waits for the
    /// check runs ahead of it. ref: docs/tasks.md "Checks, revisions and report"
    CheckSlot,
    /// The agent's usage limit: the host continues its session at `until_ms`, one task at a
    /// time. ref: docs/tasks.md "Claude adapter"
    Limit,
}

impl WaitOn {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Disk => "disk",
            Self::Plan => "plan",
            Self::Hand => "hand",
            Self::CheckSlot => "check_slot",
            Self::Limit => "limit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub on: WaitOn,
    /// What it waits for, in plain words.
    pub reason: String,
    /// When a wait on `limit` resumes, in unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
}

/// Who acted: the owner, the manager's agent (`WSX_TASK_ACTOR=manager`) or a role's
/// (`role:<name>`). ref: docs/tasks.md "Roles"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Actor {
    #[default]
    Owner,
    Manager,
    Role(String),
}

impl From<Actor> for String {
    fn from(actor: Actor) -> Self {
        match actor {
            Actor::Owner => "owner".into(),
            Actor::Manager => "manager".into(),
            Actor::Role(name) => format!("role:{name}"),
        }
    }
}

impl TryFrom<String> for Actor {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        match text.as_str() {
            "owner" => Ok(Self::Owner),
            "manager" => Ok(Self::Manager),
            _ => match text.strip_prefix("role:") {
                Some(name) if !name.is_empty() => Ok(Self::Role(name.into())),
                _ => Err(format!("unknown actor {text}")),
            },
        }
    }
}

impl Actor {
    /// The actor a host's agent runs as, from `WSX_TASK_ACTOR`; anything else is the owner.
    pub fn from_env() -> Self {
        std::env::var(owner::ACTOR_ENV)
            .ok()
            .and_then(|text| Self::try_from(text).ok())
            .unwrap_or_default()
    }

    /// The role's name, or none for the owner and the manager.
    pub fn role(&self) -> Option<&str> {
        match self {
            Self::Role(name) => Some(name),
            _ => None,
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
    pub checks: Vec<String>,
    /// `tasks.workflow.steps` as they were when the task was created, and the index of the
    /// step the worker is on. ref: docs/tasks.md "Workflow"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<crate::model::workspace::WorkflowStep>,
    #[serde(default)]
    pub step: u32,
    /// `tasks.env` as it was when the task was created: the worker's and the checks'.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
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
    /// `task new --files` and `task add --files`: the globs of the paths it expects to
    /// change; overlaps use them with `paths` (docs/tasks.md "Store").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// `task new --title`; without one the title comes from the prompt (`view::title`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The `checks.jsonl` item whose fail started this task (docs/tasks.md "To check").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixes: Option<String>,
    /// Who ran `task new`. A ready task the manager created waits for its review
    /// (docs/tasks.md "Landing").
    #[serde(default)]
    pub created_by: Actor,
    /// Why the owner tries this task before it lands. A held task never lands by itself.
    /// ref: docs/tasks.md "Hold and trial"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    /// `task new --into`: the open task this one lands into, never the default branch.
    /// ref: docs/tasks.md "Parent tasks"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// `task new --plan`: the first result is a plan, with no checks; set until it arrives.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan: bool,
    /// Set while the state is `waiting`, and on `check_slot` while checks wait for the slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
    /// When the task entered its current state; 0 in a record from before this field (read as
    /// `created_unix_ms`). Unlike `updated_unix_ms`, a record save does not renew it.
    #[serde(default)]
    pub state_unix_ms: u64,
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
    /// How long the check ran, in ms (docs/tasks.md "Checks, revisions and report").
    #[serde(default)]
    pub ms: u64,
    /// How long its run waited for the check slot before it, in ms.
    #[serde(default)]
    pub queued_ms: u64,
    /// The result came from an earlier run on the same tree; `ms` is that run's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stored: bool,
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
    Result {
        status: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        /// Tool calls the agent refused during the turn; the worker may have continued without them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        denials: Vec<String>,
        /// The workflow step the turn ran in, and what started it (docs/tasks.md "Usage").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<TurnTrigger>,
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
    /// The task waits for free disk before a turn or its checks (`tasks.minFreeGb`).
    DiskWait {
        free_mb: u64,
        min_gb: u64,
    },
    /// The worker's context reached `tasks.agent.rotateTokens`: after its handoff notes the
    /// next turn starts a fresh conversation (docs/tasks.md "Context rotation").
    Rotated {
        tokens: u64,
        /// Set when an idle gap, not the size, rotated it: the prompt cache was gone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The agent's usage limit ended a turn: it continues at `until_ms` (unix ms).
    LimitWait {
        until_ms: u64,
    },
    /// The checks wait for the machine's check slot (docs/tasks.md "Checks, revisions and report").
    CheckQueued {
        reason: String,
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
    /// The owner took the task over by hand; `dropped` queued messages were not sent.
    Hand {
        by: Actor,
        #[serde(default, skip_serializing_if = "is_zero")]
        dropped: u32,
    },
    /// The owner gave the task back; `commits` were made since the last revision.
    Handback {
        by: Actor,
        commits: u32,
    },
    /// Child task `child` landed `commits` on this task's branch.
    ChildLanded {
        child: u32,
        commits: u32,
    },
    /// The task is held for the owner to try (`reason`), or no longer (`None`).
    Hold {
        reason: Option<String>,
        by: Actor,
    },
    /// Revision `revision` at `commit` goes to the live site for the owner to try.
    Trial {
        revision: u32,
        commit: String,
    },
    /// The live site no longer runs this task's trial.
    TrialEnded {
        reason: String,
    },
}

/// What started a worker turn, logged on its `result`. ref: docs/tasks.md "Usage"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTrigger {
    /// The task's first turn, or a workflow step's start.
    Step,
    /// An `add` or a steer with no running turn, or messages queued until a turn ended.
    Message,
    /// Failed checks sent back to the worker.
    CheckRetry,
    /// Human Verify items the worker proves itself.
    Verify,
    /// The answer of a `check-run` that ended after the worker's result.
    CheckRun,
    /// A landing's rebase conflict.
    Conflict,
    /// The one turn that asks for a result the last turn left out.
    ResultRetry,
    /// The turn after a usage limit reset.
    Limit,
    /// The first turn of a fresh conversation after a rotation.
    Rotation,
    /// A turn that waited for free disk.
    Disk,
    /// The turn a host handed over when it exec'd a replaced binary.
    Restart,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl LogEvent {
    /// Agent progress may be dropped when the log is full; every other event is kept.
    pub(crate) fn is_progress(&self) -> bool {
        matches!(self, Self::Agent { .. })
    }
}

/// What one turn cost, as the adapter reported it. ref: docs/tasks.md "Claude adapter"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnUsage {
    pub duration_ms: u64,
    /// Token counts summed over every model call of the turn.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// This turn's cost: the difference of `session_cost_micro_usd` from the previous turn.
    pub cost_micro_usd: u64,
    /// Claude's `total_cost_usd`: the process's running total, plus the session's earlier
    /// spend when the process resumed it (Claude Code 2.1.277 and later).
    /// ref: https://code.claude.com/docs/en/agent-sdk/cost-tracking
    pub session_cost_micro_usd: u64,
    /// The context of the turn's last model call (input, cache read and cache write tokens).
    pub context_tokens: u64,
    /// The model of the turn's last model call.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Main-conversation model calls in the turn (distinct `assistant` message ids).
    #[serde(skip_serializing_if = "is_zero")]
    pub calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub unix_ms: u64,
    #[serde(flatten)]
    pub event: LogEvent,
}

impl TaskRecord {
    /// Whether the task waits for `on`.
    pub fn waits_on(&self, on: WaitOn) -> bool {
        self.waiting
            .as_ref()
            .is_some_and(|waiting| waiting.on == on)
    }

    /// Neither a turn nor checks run and nothing waits: a child task lands on such a parent
    /// only. ref: docs/tasks.md "Parent tasks"
    pub fn at_rest(&self) -> bool {
        matches!(self.state, TaskState::Ready | TaskState::Failed)
    }

    /// A ready task waits for the manager's review when the manager or a role created it (the
    /// creator reviews it), or when the
    /// project lands by itself (`tasks.land.auto`) and checks prove it: an owner task never
    /// lands by itself. A held one waits for the owner, as does any other ready task. A plan
    /// waits for the manager. ref: docs/tasks.md "Landing"
    pub fn manager_reviews(&self, land_auto: bool) -> bool {
        let reviewed = self.created_by != Actor::Owner || (land_auto && !self.checks.is_empty());
        (self.state == TaskState::Ready && self.hold.is_none() && reviewed)
            || self.waits_on(WaitOn::Plan)
    }
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
    /// The task whose trial this deploy put on the live site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<u32>,
    /// The end of a failed command's output (stdout, then stderr).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// `tasks/<project-id>/trial.json`: the one task whose unlanded revision the live site runs
/// for the owner to try. `commit` is known once a rebase onto the default branch passed its
/// checks. ref: docs/tasks.md "Hold and trial"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial {
    pub task: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub by: Actor,
    pub unix_ms: u64,
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
        /// More expected path globs (`task add --files`), already checked.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    Stop,
    /// Land the ready revision. `tried`: the owner passed its trial, so its Human Verify
    /// items are not added to "To check".
    Land {
        #[serde(default)]
        tried: bool,
        /// A batch land checked a tree with this task on top of the ones before it: when
        /// the rebase gives this tree, it lands without its own checks. ref: docs/tasks.md "Batch land"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch_tree: Option<String>,
    },
    /// Hold the task for the owner to try (`reason`), or release it (`None`).
    Hold {
        reason: Option<String>,
        by: Actor,
    },
    /// Put the ready revision on the live site for the owner to try; the trial slot is
    /// already claimed for this task.
    Trial,
    Close {
        discard: bool,
    },
    /// The owner takes the task over: the running turn stops and no agent turn starts.
    Hand,
    /// The owner's commits become the next revision and its checks run.
    Handback,
    /// A child task landed `commits` on this task's branch: they become the next revision
    /// and its checks run. ref: docs/tasks.md "Parent tasks"
    ChildLanded {
        child: u32,
        commits: u32,
    },
    /// Another task landed files this one shares: the text joins the worker's next turn,
    /// and no turn starts for it. ref: docs/tasks.md "Landing"
    Notice {
        text: String,
    },
    /// Start the batch land of `tasks`, this one first, outside the caller's boundary.
    /// ref: docs/tasks.md "Batch land"
    LandBatch {
        tasks: Vec<u32>,
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
/// a moment, so a connect that finds none, or a request the host closed unanswered as it
/// exec'd, is tried again for up to 3 s. Status probes use `call_host` and stay instant.
/// ref: docs/tasks.md "Host"
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
                                    | std::io::ErrorKind::UnexpectedEof
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
    // ^ A host answers every request it takes; none at all means it ended before this one.
    if bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the task host closed without an answer",
        )
        .into());
    }
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
    /// Only how it looks or feels, with a capture; without one, a real device or real data.
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
    /// Where a person looks: a URL, a page path such as `/#/p/wsx/checks`, or a screen name.
    #[serde(rename = "where")]
    pub place: String,
    /// A capture of it: the path of an image or a short MP4 video in the working copy, kept
    /// with the task at the land.
    pub capture: String,
    /// A video capture's thumbnail image in the working copy, or empty.
    pub poster: String,
    pub pass_if: String,
}

impl HumanVerify {
    /// An item the worker proves, not a person: it names a command (a code span), or it has
    /// no capture and its `where` does not name a real device or real data.
    /// ref: docs/tasks.md "To check"
    pub fn is_for_worker(&self) -> bool {
        let place = self.place.to_lowercase();
        self.check.contains('`')
            || self.pass_if.contains('`')
            || (self.capture.trim().is_empty()
                && !place.contains("real device")
                && !place.contains("real data"))
    }
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
            "human_verify": { "type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["check", "where", "capture", "poster", "pass_if"],
                "properties": { "check": text, "where": text, "capture": text, "poster": text, "pass_if": text },
            } },
            "left_open": { "type": "array", "items": text },
            "notes": text,
            "memo": text,
        },
    })
}
