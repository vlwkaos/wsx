//! What a task host and a manager host need from any headless agent: one process per
//! conversation, messages in, progress and turn results out. Each adapter (claude.rs) maps
//! its own stream to these types; the hosts never read an adapter's wire format.
//! ref: docs/tasks.md "Agents"

use super::boundary::Boundary;
use super::claude::ClaudeAgent;
use super::pi::PiAgent;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use wsx_core::task::TurnUsage;

pub enum AgentLine {
    /// The conversation ID the agent runs under.
    Init(String),
    Progress {
        text: String,
        /// The agent's own words, not a tool call.
        said: bool,
    },
    Result(Box<TurnResult>),
}

pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub text: String,
    /// The turn's structured result, when the launch asked for one.
    pub structured: Option<Value>,
    pub denials: Vec<String>,
    pub usage: TurnUsage,
    /// The agent's usage limit ended the turn, before any model call.
    pub limit: Option<UsageLimit>,
    /// Background jobs still running when the turn ended: the agent starts its own turn
    /// when each ends. ref: docs/tasks.md "Claude adapter"
    pub background: usize,
}

/// The agent's provider refused the turn for its usage limit.
/// ref: docs/tasks.md "Usage limit"
pub struct UsageLimit {
    /// When the limit resets, in unix seconds, when the provider said.
    pub resets_at: Option<u64>,
}

impl TurnResult {
    /// No result and no model call: the agent's own turn for a background-task notification
    /// (a job that ended or was killed, also on a resume), which answers no message.
    pub fn answers_nothing(&self) -> bool {
        !self.is_error
            && self.structured.is_none()
            && self.usage.input_tokens == 0
            && self.usage.output_tokens == 0
    }
}

/// What one agent process runs with. Every role gets its launch from `Launch::new`.
/// ref: docs/tasks.md "Permissions"
pub struct Launch {
    pub cwd: PathBuf,
    pub model: String,
    /// The role's OS boundary over the whole agent process tree; the agent itself prompts
    /// for nothing.
    pub boundary: Boundary,
    /// JSON Schema of the final result, when the turn must end with structured output.
    pub schema: Option<Value>,
    pub instructions: String,
    /// Set on top of the adapter's kept environment.
    pub env: Vec<(String, std::ffi::OsString)>,
    /// Put first on `PATH`, so the agent's commands find this wsx build.
    pub path_prefix: Option<PathBuf>,
    /// `--plugin-dir`: the manager's plugin of the owner's skills.
    pub plugin_dirs: Vec<PathBuf>,
}

impl Launch {
    pub fn new(cwd: &Path, boundary: Boundary, model: &str, instructions: String) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            model: model.to_string(),
            boundary,
            schema: None,
            instructions,
            env: Vec::new(),
            path_prefix: None,
            plugin_dirs: Vec::new(),
        }
    }
}

/// One running agent process of a configured adapter (`tasks.agent.adapter`,
/// `tasks.manager.adapter`). ref: docs/tasks.md "Agents"
pub enum Agent {
    Claude(ClaudeAgent),
    Pi(PiAgent),
}

impl Agent {
    /// Start `adapter` on conversation `session`; `resume` says it exists already.
    /// `cost_base` is the session's last `session_cost_micro_usd`.
    pub fn spawn<E: Send + 'static>(
        adapter: &str,
        launch: &Launch,
        session: &str,
        resume: bool,
        cost_base: u64,
        events: Sender<E>,
        wrap: impl Fn(Option<AgentLine>) -> E + Send + 'static,
    ) -> Result<Self> {
        Ok(match adapter {
            "claude" => Self::Claude(ClaudeAgent::spawn(
                launch, session, resume, cost_base, events, wrap,
            )?),
            "pi" => Self::Pi(PiAgent::spawn(launch, session, cost_base, events, wrap)?),
            other => bail!("no task adapter {other}"),
        })
    }

    /// Send one user message; during a turn the agent takes it as a steer.
    pub fn send(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Claude(agent) => agent.send(text),
            Self::Pi(agent) => agent.send(text),
        }
    }

    /// The next turns run on `model`; the conversation stays.
    pub fn set_model(&mut self, model: &str) -> Result<()> {
        match self {
            Self::Claude(agent) => agent.set_model(model),
            Self::Pi(agent) => agent.set_model(model),
        }
    }

    pub fn stop(self) {
        match self {
            Self::Claude(agent) => agent.stop(),
            Self::Pi(agent) => agent.stop(),
        }
    }
}

/// A registry agent a host can run headless.
pub struct Choice {
    pub name: String,
    pub adapter: String,
    pub model: String,
}

/// The first of `names` (the global `[[agents]]`) with a headless adapter and a model,
/// another adapter than `current`'s, and no usage limit on record. A name the registry does
/// not hold is skipped. ref: docs/tasks.md "Agents"
pub fn fallback(names: &[String], current: &str, now_ms: u64) -> Option<Choice> {
    if names.is_empty() {
        return None;
    }
    let (config, _) = wsx_core::config::global::GlobalConfig::load().ok()?;
    names.iter().find_map(|name| {
        let entry = config.agents.iter().find(|agent| &agent.name == name)?;
        let adapter = entry.adapter.clone().filter(|adapter| adapter != current)?;
        let model = entry.model.clone()?;
        wsx_core::task::store::limited_until(&adapter, now_ms)
            .is_none()
            .then(|| Choice {
                name: name.clone(),
                adapter,
                model,
            })
    })
}

/// A random version 4 UUID for a new conversation.
pub fn new_session_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
