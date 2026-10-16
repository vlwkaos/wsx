//! Claude worker adapter: one `claude -p` process with stream JSON on stdin and stdout.
//! ref: docs/tasks.md "Claude adapter"; knowledge "Headless Worker Transport"

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use wsx_core::config::project::tool_refusal;
use wsx_core::task::{brief, copy, store, worker_result_schema, Phase, TaskRecord, TurnUsage};

/// Longest stdout line the host reads; a longer one (a huge tool result) is skipped.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
const PROGRESS_CHARS: usize = 160;
/// Variables a worker keeps from the host. Everything else, `WSX_*` included, is dropped.
const KEPT_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    // ^ Network and toolchain setup a worker's own commands need.
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "no_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

pub enum AgentLine {
    /// The conversation ID the agent runs under.
    Init(String),
    Progress(String),
    Result(TurnResult),
}

pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub text: String,
    pub structured: Option<Value>,
    pub denials: Vec<String>,
    pub usage: TurnUsage,
}

pub struct ClaudeAgent {
    child: Child,
    stdin: ChildStdin,
}

/// What one agent process runs with. Every role gets its launch from `Launch::new`.
/// ref: docs/tasks.md "Permissions"
pub struct Launch {
    pub cwd: PathBuf,
    pub model: String,
    /// `--settings`: the sandbox and the file-tool rules of the role.
    pub settings: Value,
    /// `--allowedTools`: never a Bash rule; the sandbox bounds Bash.
    pub tools: Vec<String>,
    /// `--disallowedTools`
    pub disallowed: Vec<String>,
    /// JSON Schema of the final result, when the turn must end with structured output.
    pub schema: Option<Value>,
    pub instructions: String,
    /// Set on top of `KEPT_ENV`.
    pub env: Vec<(String, std::ffi::OsString)>,
    /// Put first on `PATH`, so the agent's commands find this wsx build.
    pub path_prefix: Option<PathBuf>,
}

/// What one role may write and reach. The OS enforces it on Bash; the file tools get
/// deny rules for `deny_edits` and the auto-mode classifier reviews the rest.
pub struct Boundary {
    pub cwd: PathBuf,
    pub writes: Vec<String>,
    pub deny_writes: Vec<String>,
    pub domains: Vec<String>,
    /// Commands that run outside the sandbox; each also needs an allowed tool rule.
    pub excluded: Vec<String>,
    pub deny_edits: Vec<PathBuf>,
}

/// Tools of every role. Edit and Write are not listed: auto mode accepts them in the
/// working directory and its classifier reviews them elsewhere, while a bare `Write`
/// rule would allow every path.
const BASE_TOOLS: [&str; 3] = ["Read", "Glob", "Grep"];
/// The manager's own commands, which leave the sandbox (`excludedCommands`).
pub const MANAGER_COMMAND: &str = "wsx task *";
/// Denied in a brief turn even when a project's settings allow them.
const BRIEF_DENIED_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

impl Launch {
    pub fn new(boundary: &Boundary, model: &str, tools: Vec<String>, instructions: String) -> Self {
        let mut allowed: Vec<String> = BASE_TOOLS.map(String::from).to_vec();
        for tool in tools {
            if !allowed.contains(&tool) {
                allowed.push(tool);
            }
        }
        Self {
            cwd: boundary.cwd.clone(),
            model: model.to_string(),
            settings: settings(boundary),
            tools: allowed,
            disallowed: Vec::new(),
            schema: None,
            instructions,
            env: Vec::new(),
            path_prefix: None,
        }
    }

    /// A brief turn reads only and returns a brief; a build turn edits and returns a result.
    pub fn worker(record: &TaskRecord) -> Result<Self> {
        let boundary = worker_boundary(record)?;
        // ^ Records from before the sandbox hold Bash rules and bare Edit and Write: drop them.
        let tools = record
            .tools
            .iter()
            .filter(|tool| tool_refusal(tool).is_none())
            .cloned()
            .collect();
        let mut launch = Self::new(&boundary, &record.model, tools, worker_instructions(record));
        launch.schema = Some(match record.phase {
            Phase::Brief => {
                launch.disallowed = BRIEF_DENIED_TOOLS.map(String::from).to_vec();
                brief::schema()
            }
            Phase::Build => worker_result_schema(),
        });
        Ok(launch)
    }
}

/// The settings every role shares, with its own boundary.
fn settings(boundary: &Boundary) -> Value {
    let mut sandbox = json!({
        "enabled": true,
        "failIfUnavailable": true,
        "autoAllowBashIfSandboxed": true,
        // ^ From `--settings` this also makes the sandbox admin-required, so a repository's
        // own settings cannot add hosts, write paths or excluded commands.
        "allowUnsandboxedCommands": false,
        "network": { "allowedDomains": boundary.domains },
        "filesystem": { "allowWrite": boundary.writes, "denyWrite": boundary.deny_writes },
    });
    if !boundary.excluded.is_empty() {
        sandbox["excludedCommands"] = json!(boundary.excluded);
    }
    json!({
        // ^ Agent commits carry no AI attribution; `attribution` replaced
        // `includeCoAuthoredBy`, which older builds still read.
        "attribution": { "commit": "", "pr": "" },
        "includeCoAuthoredBy": false,
        "sandbox": sandbox,
        // ^ `//` starts an absolute path in a permission rule; Edit rules cover every file tool.
        "permissions": {
            "deny": boundary
                .deny_edits
                .iter()
                .map(|path| format!("Edit(/{}/**)", path.display()))
                .collect::<Vec<_>>(),
        },
    })
}

/// A worker writes its working copy and the git paths its commits need in the main
/// repository; a brief turn writes nothing. ref: docs/tasks.md "Permissions"
fn worker_boundary(record: &TaskRecord) -> Result<Boundary> {
    let (common, admin) = copy::git_dirs(&record.working_copy)?;
    let default_ref = format!("refs/heads/{}", record.default_branch);
    let mut deny_writes = vec![
        common.join("hooks"),
        common.join("config"),
        common.join(&default_ref),
        common.join("logs").join(&default_ref),
    ];
    let mut writes: Vec<String> = Vec::new();
    match record.phase {
        Phase::Brief => deny_writes.insert(0, record.working_copy.clone()),
        Phase::Build => {
            writes.extend(
                [
                    common.join("objects"),
                    admin,
                    common.join("refs/heads/wsx"),
                    common.join("logs/refs/heads/wsx"),
                ]
                .iter()
                .map(|path| path.display().to_string()),
            );
            writes.extend(record.sandbox.allow_write.iter().cloned());
        }
    }
    let mut deny_edits = kept_from_edits(&record.project_path, Some(&record.working_copy));
    deny_edits.push(store::state_dir());
    Ok(Boundary {
        cwd: record.working_copy.clone(),
        writes,
        deny_writes: deny_writes
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        domains: record.sandbox.allowed_domains.clone(),
        excluded: Vec::new(),
        deny_edits,
    })
}

/// Folders no agent's file tools may write: the main checkout, wsx's config folders and
/// the project's task copies other than `own`.
pub fn kept_from_edits(main: &Path, own: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = vec![main.to_path_buf()];
    paths.extend(config_dirs());
    paths.extend(
        copy::task_copies(main)
            .into_iter()
            .filter(|path| Some(path.as_path()) != own),
    );
    paths
}

/// Where wsx keeps its configuration: the platform folder and the XDG one.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = dirs::config_dir().into_iter().collect();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")));
    dirs.extend(xdg);
    let mut dirs: Vec<PathBuf> = dirs.into_iter().map(|dir| dir.join("wsx")).collect();
    dirs.dedup();
    dirs
}

/// The same text in both phases: Claude records the system prompt of a conversation's
/// first request and reuses it on every resume (`--system-prompt-snapshot`), so the
/// phase is said in the user messages.
fn worker_instructions(record: &TaskRecord) -> String {
    format!(
        "You are a task worker in a separate working copy on branch {}. \
         Nobody reads your terminal: work without asking questions. \
         A task may start with a brief: while a message asks for the brief, only read, \
         search and run read-only commands, change no file, and return the brief in the \
         requested structure. When a message says the build starts, build it and commit \
         your changes with git before you finish. Build and test what you change. \
         Your commands run in a sandbox: they write only in this working copy, and a \
         host that is not allowed fails with a sandbox_violations note; when the task \
         needs that host, run the command again with the host listed. Keep file edits in \
         this working copy. \
         When you cannot continue, return status blocked and say what you need; when the \
         sandbox or a permission stops a host or a path you need, name that host or path.",
        record.branch
    )
}

impl ClaudeAgent {
    /// Start the agent. `resume` names a conversation that already exists;
    /// otherwise `session` becomes the new conversation's ID.
    pub fn spawn<E: Send + 'static>(
        launch: &Launch,
        session: &str,
        resume: bool,
        events: Sender<E>,
        wrap: impl Fn(Option<AgentLine>) -> E + Send + 'static,
    ) -> Result<Self> {
        let mut command = Command::new("claude");
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--setting-sources",
                "project,local",
                "--permission-mode",
                "auto",
                "--model",
                &launch.model,
                "--append-system-prompt",
                &launch.instructions,
                "--settings",
                &launch.settings.to_string(),
            ])
            .args(if resume {
                ["--resume", session]
            } else {
                ["--session-id", session]
            });
        if let Some(schema) = &launch.schema {
            command.args(["--json-schema", &schema.to_string()]);
        }
        if !launch.tools.is_empty() {
            command.arg("--allowedTools").args(&launch.tools);
        }
        if !launch.disallowed.is_empty() {
            command.arg("--disallowedTools").args(&launch.disallowed);
        }
        command.env_clear();
        for name in KEPT_ENV {
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
        let mut child = command
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("cannot start claude; is it on PATH?")?;
        let stdin = child.stdin.take().context("claude has no stdin")?;
        let stdout = child.stdout.take().context("claude has no stdout")?;
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                match (&mut reader)
                    .take(MAX_LINE_BYTES)
                    .read_until(b'\n', &mut line)
                {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !line.ends_with(b"\n") && line.len() as u64 >= MAX_LINE_BYTES => {
                        if !matches!(skip_line(&mut reader), Ok(true)) {
                            break;
                        }
                    }
                    Ok(_) => {
                        if let Some(parsed) = parse_line(&line) {
                            if events.send(wrap(Some(parsed))).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            let _ = events.send(wrap(None));
        });
        Ok(Self { child, stdin })
    }

    /// Send one user message. During a turn Claude takes it as a steer.
    pub fn send(&mut self, text: &str) -> Result<()> {
        let mut line = serde_json::to_vec(&serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        }))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
            .context("cannot write to claude")
    }

    /// Interrupt, then kill if it does not end within a few seconds.
    pub fn stop(mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: signalling our own child.
        unsafe { libc::kill(pid, libc::SIGINT) };
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drop the rest of an oversized line one buffer at a time, never holding it.
/// Returns false at end of input.
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

fn parse_line(line: &[u8]) -> Option<AgentLine> {
    let value: Value = serde_json::from_slice(line).ok()?;
    match value.get("type")?.as_str()? {
        "system" if value.get("subtype")?.as_str()? == "init" => Some(AgentLine::Init(
            value.get("session_id")?.as_str()?.to_string(),
        )),
        "assistant" => value
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .rev()
            .find_map(progress_text)
            .map(AgentLine::Progress),
        "result" => Some(AgentLine::Result(TurnResult {
            is_error: value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            subtype: str_field(&value, "subtype"),
            text: str_field(&value, "result"),
            structured: value.get("structured_output").cloned(),
            denials: value
                .get("permission_denials")
                .and_then(Value::as_array)
                .map(|denials| denials.iter().map(denial_text).collect())
                .unwrap_or_default(),
            usage: turn_usage(&value),
        })),
        _ => None,
    }
}

fn turn_usage(value: &Value) -> TurnUsage {
    let count = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    TurnUsage {
        duration_ms: count("/duration_ms"),
        input_tokens: count("/usage/input_tokens"),
        output_tokens: count("/usage/output_tokens"),
        cache_read_tokens: count("/usage/cache_read_input_tokens"),
        cache_creation_tokens: count("/usage/cache_creation_input_tokens"),
        cost_micro_usd: value
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map_or(0, |cost| (cost * 1_000_000.0).round() as u64),
    }
}

fn str_field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `Bash(rm -rf x)` style text for one denied tool call.
fn denial_text(denial: &Value) -> String {
    let tool = denial
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    match denial.get("tool_input").and_then(key_argument) {
        Some(argument) => format!("{tool}({})", clip(&argument)),
        None => tool.to_string(),
    }
}

fn key_argument(input: &Value) -> Option<String> {
    ["command", "file_path", "path", "pattern", "url"]
        .iter()
        .find_map(|key| input.get(key)?.as_str().map(str::to_string))
}

fn progress_text(item: &Value) -> Option<String> {
    match item.get("type")?.as_str()? {
        "tool_use" => {
            let name = item.get("name")?.as_str()?;
            Some(match item.get("input").and_then(key_argument) {
                Some(argument) => format!("{name} {}", clip(&argument)),
                None => name.to_string(),
            })
        }
        "text" => {
            let text = item.get("text")?.as_str()?.trim();
            (!text.is_empty()).then(|| clip(text.lines().next().unwrap_or(text)))
        }
        _ => None,
    }
}

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= PROGRESS_CHARS {
        line.to_string()
    } else {
        let mut clipped: String = line.chars().take(PROGRESS_CHARS).collect();
        clipped.push('…');
        clipped
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
