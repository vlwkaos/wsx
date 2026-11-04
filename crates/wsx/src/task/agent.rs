//! What a task host and a manager host need from any headless agent: one process per
//! conversation, messages in, progress and turn results out. Each adapter (claude.rs) maps
//! its own stream to these types; the hosts never read an adapter's wire format.
//! ref: docs/tasks.md "Agents"

use super::boundary::Boundary;
use super::claude::ClaudeAgent;
use super::pi::PiAgent;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use wsx_core::model::workspace::TaskConfig;
use wsx_core::task::{store, usage, TurnUsage};

/// Ping turns in a row, without a real turn between: three keep a conversation warm for
/// about 3 h 40 min and cost far less than the one rewrite they save.
/// ref: docs/tasks.md "Context rotation"
pub const KEEP_WARM_PINGS: u32 = 3;
/// How long Claude's prompt cache lives: a ping after that writes everything again.
pub const PROMPT_CACHE_MS: u64 = 3_600_000;
/// The ping turn's message: the agent answers it with one word.
pub const KEEP_WARM_TEXT: &str = "wsx keep-alive: the host is waiting and keeps this \
conversation's cache warm. Answer with the one word ok. Use no tool and return no result; \
nothing about the task changes.";

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

/// The words of a turn: every text block the agent said, in order, joined with a blank line.
/// A block of `-` or nothing is dropped, so a turn that says only `-` has no words and stays
/// quiet. With no block seen, the stream's own `fallback` (Claude's `result`) is the text.
/// ref: docs/tasks.md "Manager"
pub fn turn_text(blocks: &[String], fallback: String) -> String {
    if blocks.is_empty() {
        return fallback;
    }
    let said: Vec<&str> = blocks
        .iter()
        .map(|block| block.trim())
        .filter(|block| !wsx_core::task::manager::says_nothing(block))
        .collect();
    said.join("\n\n")
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
    /// Claude's `--effort`, Pi's `--thinking`; empty passes none. ref: docs/tasks.md "Agents"
    pub effort: String,
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
    /// A stand-in's one call: no tools and no stored conversation.
    pub minimal: bool,
}

impl Launch {
    pub fn new(cwd: &Path, boundary: Boundary, model: &str, instructions: String) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            model: model.to_string(),
            effort: String::new(),
            boundary,
            schema: None,
            instructions,
            env: Vec::new(),
            path_prefix: None,
            plugin_dirs: Vec::new(),
            minimal: false,
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

/// Whose limit and credit a turn runs on: Claude's account, or a Pi provider's
/// (`anthropic/claude-opus-5-5` is `pi-anthropic`). Agents on one account share a limit; a local
/// model through Pi does not share one with a paid provider. ref: docs/tasks.md "Agents"
pub fn account(adapter: &str, model: &str) -> String {
    match model.split_once('/') {
        Some((provider, _)) if adapter != "claude" => format!("{adapter}-{provider}"),
        _ => adapter.to_string(),
    }
}

/// The first of `names` (the global `[[agents]]`) with a headless adapter and a model, and no
/// limit on record on its account. `minimal` picks among the stand-ins, else among the others.
/// A name the registry does not hold is skipped. ref: docs/tasks.md "Agents"
pub fn fallback(names: &[String], now_ms: u64, minimal: bool) -> Option<Choice> {
    if names.is_empty() {
        return None;
    }
    let (config, _) = wsx_core::config::global::GlobalConfig::load().ok()?;
    names.iter().find_map(|name| {
        let entry = config.agents.iter().find(|agent| &agent.name == name)?;
        if entry.minimal != minimal {
            return None;
        }
        let adapter = entry.adapter.clone()?;
        let model = entry.model.clone()?;
        store::limited_until(&account(&adapter, &model), now_ms)
            .is_none()
            .then(|| Choice {
                name: name.clone(),
                adapter,
                model,
            })
    })
}

/// The first available of `names` when `account`'s limit until `until_ms` is past
/// `maxWaitMinutes`, counted from the first limit of its row on record, so a backoff that keeps
/// failing becomes long; a shorter one is waited out. ref: docs/tasks.md "Agents"
pub fn fallback_past_wait(
    names: &[String],
    max_wait_secs: u64,
    account: &str,
    until_ms: u64,
    now_ms: u64,
) -> Option<Choice> {
    // ^ A record without its start (an older one) counts from now.
    let since_ms =
        store::account_limit(account, now_ms).map_or(now_ms, |limit| limit.since_ms.min(now_ms));
    (until_ms.saturating_sub(since_ms) > max_wait_secs * 1000)
        .then(|| fallback(names, now_ms, false))
        .flatten()
}

/// Where a worker on (`adapter`, `model`) goes when its account refuses turns until `until_ms`:
/// its own agent at once when that is free, else a fallback past `maxWaitMinutes`, else
/// nowhere: it waits. ref: docs/tasks.md "Agents"
pub fn worker_next(
    settings: &TaskConfig,
    adapter: &str,
    model: &str,
    until_ms: u64,
    now_ms: u64,
) -> Option<Choice> {
    let own = account(&settings.adapter, &settings.model);
    let current = account(adapter, model);
    if current != own && store::limited_until(&own, now_ms).is_none() {
        return Some(Choice {
            name: settings.adapter.clone(),
            adapter: settings.adapter.clone(),
            model: settings.model.clone(),
        });
    }
    fallback_past_wait(
        &settings.fallback,
        settings.max_wait_secs,
        &current,
        until_ms,
        now_ms,
    )
}

/// The budget of an agent that has none set.
const DEFAULT_BUDGET_USD_PER_DAY: u32 = 10;

/// Count a turn's cost on its account. Past the day's budget the account counts as limited
/// until midnight, and the message says so. Claude's plan has no such bill, and a local model
/// costs nothing. ref: docs/tasks.md "Agents"
pub fn note_spend(adapter: &str, model: &str, micro_usd: u64, now_ms: u64) -> Option<String> {
    if adapter == "claude" || micro_usd == 0 {
        return None;
    }
    let account = account(adapter, model);
    // ^ A budget belongs to the account: the smallest any of its agents sets.
    let budget = wsx_core::config::global::GlobalConfig::load()
        .ok()
        .and_then(|(config, _)| {
            config
                .agents
                .iter()
                .filter(|agent| match (&agent.adapter, &agent.model) {
                    (Some(adapter), Some(model)) => self::account(adapter, model) == account,
                    _ => false,
                })
                .filter_map(|agent| agent.budget_usd_per_day)
                .min()
        })
        .unwrap_or(DEFAULT_BUDGET_USD_PER_DAY);
    let today = usage::local_midnight_ms(now_ms);
    let spent = match store::add_spend(&account, today, micro_usd) {
        Ok(spent) => spent,
        Err(error) => return Some(format!("cannot record the spend: {error:#}")),
    };
    if spent < u64::from(budget) * 1_000_000 {
        return None;
    }
    let midnight = usage::local_midnight_ms(today + 26 * 3_600_000);
    match store::mark_limited(&account, midnight, now_ms, false) {
        Ok(()) => Some(format!(
            "{account} spent {} of its {budget} USD for today: limited until midnight",
            usage::usd(spent)
        )),
        Err(error) => Some(format!("cannot record the budget: {error:#}")),
    }
}

/// Longest stdout line the host reads; a longer one (a huge tool result) is skipped.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Hand each JSON line of an agent's stdout to `each` (false stops), to the end of the stream.
/// A line over `MAX_LINE_BYTES` is dropped in buffer-sized steps, never held; a line that is
/// no JSON is skipped.
pub(super) fn read_json_lines(stdout: impl Read, mut each: impl FnMut(Value) -> bool) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        match (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(_) if !line.ends_with(b"\n") && line.len() as u64 >= MAX_LINE_BYTES => {
                if !matches!(skip_line(&mut reader), Ok(true)) {
                    return;
                }
            }
            Ok(_) => {
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    if !each(value) {
                        return;
                    }
                }
            }
        }
    }
}

/// Drop the rest of an oversized line one buffer at a time. Returns false at end of input.
fn skip_line(reader: &mut impl BufRead) -> std::io::Result<bool> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(false);
        }
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                reader.consume(end + 1);
                return Ok(true);
            }
            None => {
                let length = buffer.len();
                reader.consume(length);
            }
        }
    }
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
