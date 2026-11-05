//! What a task host and a manager host need from any headless agent: one process per
//! conversation, messages in, progress and turn results out. Each adapter (claude.rs) maps
//! its own stream to these types; the hosts never read an adapter's wire format.
//! ref: docs/tasks.md "Agents"

use super::boundary::Boundary;
use super::claude::ClaudeAgent;
use super::pi::PiAgent;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Sender, SyncSender, TrySendError};
use std::time::{Duration, Instant};
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
    /// The characters of thinking text in the turn's main conversation: some of an answer can
    /// sit there, which no reader shows. ref: docs/tasks.md "Reply text"
    pub thought: usize,
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

/// Thinking shorter than this is no answer.
const THOUGHT_MIN_CHARS: usize = 100;

/// The characters of the `thinking` blocks among a message's content blocks.
pub fn thinking_chars(content: &[Value]) -> usize {
    content
        .iter()
        .filter(|item| item["type"] == "thinking")
        .filter_map(|item| item["thinking"].as_str())
        .map(|text| text.trim().chars().count())
        .sum()
}

impl TurnResult {
    /// The turn thought far more than it said (twice its words, at least `THOUGHT_MIN_CHARS`;
    /// a `-` or empty reply has none): the answer may be in the thinking. Both adapters count
    /// the thinking, so one rule serves both; an error result is judged before (`on_result`). ref: docs/tasks.md "Reply text"
    pub fn answered_in_thinking(&self) -> bool {
        let said = if wsx_core::task::manager::says_nothing(&self.text) {
            0
        } else {
            self.text.trim().chars().count()
        };
        self.thought >= THOUGHT_MIN_CHARS && self.thought >= 2 * said
    }

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

    /// Stop the agent's whole process tree and reap it; dropping it does the same.
    pub fn stop(self) {
        match self {
            Self::Claude(agent) => drop(agent),
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
/// A read error ends the stream too and comes back, so the reader can say why.
pub(super) fn read_json_lines(
    stdout: impl Read,
    mut each: impl FnMut(Value) -> bool,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        match (&mut reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut line)?
        {
            0 => return Ok(()),
            length if !line.ends_with(b"\n") && length as u64 >= MAX_LINE_BYTES => {
                if !skip_line(&mut reader)? {
                    return Ok(());
                }
            }
            _ => {
                if let Ok(value) = serde_json::from_slice::<Value>(&line) {
                    if !each(value) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// The reader thread's end: a read error is said first, so the log tells why the stream
/// ended. The host then drops the agent, which stops its whole tree and reaps it.
pub(super) fn end_stream<E>(
    events: &Sender<E>,
    wrap: &impl Fn(Option<AgentLine>) -> E,
    read: std::io::Result<()>,
) {
    if let Err(error) = read {
        let text = format!("cannot read the agent's output: {error}; the host stops it");
        let _ = events.send(wrap(Some(AgentLine::Progress { text, said: false })));
    }
    let _ = events.send(wrap(None));
}

/// Set on every agent, unique to its process: the processes of its tree that left its
/// process group (Claude's Bash tool starts each shell in its own session) still carry it.
const TREE_ENV: &str = "WSX_AGENT_TREE";
/// Lines that wait for a slow agent's stdin before `write` refuses the next one.
const INPUT_LINES: usize = 64;
/// How long a stopped agent has to end after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// The wsxd argument of a resumable session's supervisor, which a stop never signals.
/// ref: crates/wsx-daemon/src/lib.rs `RESUME_SUPERVISOR_ARG`
#[cfg(target_os = "macos")]
const RESUME_SUPERVISOR_ARG: &[u8] = b"__wsx_resume_supervisor";
static SPAWNED: AtomicU64 = AtomicU64::new(0);

/// One agent process. It leads its own process group, a thread writes its stdin so a hung
/// agent cannot block the host, and dropping it stops its whole tree and reaps it.
/// ref: docs/tasks.md "Agents"
pub(super) struct AgentProcess {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    /// `WSX_AGENT_TREE=<value>` as it stands in each tree process's environment.
    marker: Vec<u8>,
    program: &'static str,
}

impl AgentProcess {
    /// Start `program` with `args` in the launch's boundary, folder and environment: `kept`
    /// names the host variables it keeps, `env` adds the adapter's own.
    pub(super) fn spawn(
        launch: &Launch,
        program: &'static str,
        args: Vec<OsString>,
        kept: &[&str],
        env: &[(&str, &str)],
    ) -> Result<(Self, ChildStdout)> {
        // ^ The host's boundary confines the agent; the adapter only adds its own argv.
        let mut command = launch.boundary.command(program)?;
        command.args(args).env_clear();
        for name in kept {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if let Some(prefix) = &launch.path_prefix {
            let mut paths = vec![prefix.clone()];
            paths.extend(
                std::env::var_os("PATH")
                    .iter()
                    .flat_map(std::env::split_paths),
            );
            command.env("PATH", std::env::join_paths(paths)?);
        }
        for (name, value) in &launch.env {
            command.env(name, value);
        }
        // ^ No terminal to edit in: `git rebase --continue` and `git commit` keep their message.
        command.env("GIT_EDITOR", "true");
        for (name, value) in env {
            command.env(name, value);
        }
        let tree = format!(
            "{}-{}",
            std::process::id(),
            SPAWNED.fetch_add(1, Ordering::Relaxed)
        );
        command.env(TREE_ENV, &tree);
        // ^ Its own group: a stop reaches the tool processes in it and never the host.
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot start {program}; is it on PATH?"))?;
        let mut process = Self {
            child,
            input: None,
            marker: format!("{TREE_ENV}={tree}").into_bytes(),
            program,
        };
        let mut stdin = process
            .child
            .stdin
            .take()
            .context("the agent has no stdin")?;
        let stdout = process
            .child
            .stdout
            .take()
            .context("the agent has no stdout")?;
        let (input, lines) = sync_channel::<Vec<u8>>(INPUT_LINES);
        // ^ Ends when the sender is dropped, which closes stdin, or when a write fails.
        std::thread::spawn(move || {
            for line in lines {
                if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                    return;
                }
            }
        });
        process.input = Some(input);
        Ok((process, stdout))
    }

    /// Queue one JSON line for the agent's stdin; never waits for the agent to read it.
    pub(super) fn write(&self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        let input = self.input.as_ref().context("the agent is stopped")?;
        match input.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!(
                "{} does not read its input: {INPUT_LINES} lines wait",
                self.program
            ),
            Err(TrySendError::Disconnected(_)) => bail!("cannot write to {}", self.program),
        }
    }

    /// The agent's process group, and each process of its tree outside it. The leader is not
    /// reaped yet, so its group ID is still its own.
    fn signal(&self, signal: libc::c_int) {
        let leader = self.child.id() as libc::pid_t;
        // SAFETY: plain signals to the agent's own group and to processes that carry its mark.
        unsafe { libc::killpg(leader, signal) };
        for pid in marked(&self.marker) {
            unsafe { libc::kill(pid, signal) };
        }
    }
}

/// Close stdin, SIGTERM the tree, give the leader `STOP_GRACE` to end, SIGKILL what is left,
/// then reap the leader.
impl Drop for AgentProcess {
    fn drop(&mut self) {
        drop(self.input.take());
        let leader = self.child.id() as libc::pid_t;
        self.signal(libc::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while !exited(leader) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.signal(libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// The leader has ended. `WNOWAIT` leaves it a zombie, so its pid and group cannot be
/// reused before the SIGKILL.
fn exited(pid: libc::pid_t) -> bool {
    // SAFETY: a zeroed siginfo_t is valid, and waitid writes only into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0 {
        return true;
    }
    #[cfg(target_os = "macos")]
    let ended = info.si_pid;
    // SAFETY: waitid filled the struct.
    #[cfg(not(target_os = "macos"))]
    let ended = unsafe { info.si_pid() };
    ended != 0
}

/// The processes, other than this host, whose environment holds `marker`. A wsxd session
/// supervisor is never one. Another user's process does not answer `KERN_PROCARGS2`.
#[cfg(target_os = "macos")]
fn marked(marker: &[u8]) -> Vec<libc::pid_t> {
    let me = std::process::id() as libc::pid_t;
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
    let mut pids: Vec<libc::pid_t> = vec![0; count + 64];
    let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer holds `bytes` bytes.
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(listed.max(0) as usize);
    let mut args = vec![0u8; arg_max()];
    let mut found = Vec::new();
    for pid in pids.into_iter().filter(|pid| *pid > 1 && *pid != me) {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size = args.len();
        // SAFETY: `args` holds `size` bytes, and sysctl writes at most that and sets `size`.
        let read = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                args.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if read == 0 && in_tree(&args[..size], marker) {
            found.push(pid);
        }
    }
    found
}

#[cfg(not(target_os = "macos"))]
fn marked(_marker: &[u8]) -> Vec<libc::pid_t> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn arg_max() -> usize {
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: KERN_ARGMAX is one int.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 && value > 0 {
        value as usize
    } else {
        1 << 20
    }
}

/// `KERN_PROCARGS2` is argc, the exec path, NUL padding, the arguments, then the environment,
/// each NUL-ended.
#[cfg(target_os = "macos")]
fn in_tree(args: &[u8], marker: &[u8]) -> bool {
    let Some((argc, rest)) = args.split_first_chunk::<4>() else {
        return false;
    };
    let argc = i32::from_ne_bytes(*argc).max(0) as usize;
    let mut strings = rest
        .split(|byte| *byte == 0)
        .filter(|text| !text.is_empty());
    let _path = strings.next();
    let supervisor = strings
        .by_ref()
        .take(argc)
        .any(|arg| arg == RESUME_SUPERVISOR_ARG);
    !supervisor && strings.any(|variable| variable == marker)
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
