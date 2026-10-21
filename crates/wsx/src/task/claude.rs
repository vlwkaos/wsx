//! Claude worker adapter: one `claude -p` process with stream JSON on stdin and stdout.
//! ref: docs/tasks.md "Claude adapter"; knowledge "Headless Worker Transport"

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use wsx_core::config::project::tool_refusal;
use wsx_core::task::{
    brief, copy, store, worker_result_schema, Access, Phase, TaskRecord, TurnUsage,
};

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
    /// A tool result shows the sandbox stopped a host or a write; a path is as the
    /// command printed it.
    Stopped(Access),
    /// A command that listed these hosts ran without an error: they need no grant.
    Reached(Vec<String>),
}

pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub text: String,
    pub structured: Option<Value>,
    pub denials: Vec<String>,
    /// Paths of file-tool calls that were denied.
    pub denied_writes: Vec<String>,
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
    /// `--plugin-dir`: the manager's plugin of the owner's skills.
    pub plugin_dirs: Vec<PathBuf>,
}

/// What one role may write and reach. The OS enforces it on Bash; the file tools get
/// deny rules for `deny_edits` and the auto-mode classifier reviews the rest.
pub struct Boundary {
    pub cwd: PathBuf,
    pub writes: Vec<String>,
    pub deny_writes: Vec<String>,
    pub domains: Vec<String>,
    /// Unix sockets Bash may connect to (the manager's broker).
    pub unix_sockets: Vec<String>,
    pub deny_edits: Vec<PathBuf>,
    /// Tools denied by name in the settings, so the deny also binds subagents and a skill's
    /// `allowed-tools`. A rule without a path never reaches the sandbox's `denyWrite`.
    pub deny_tools: Vec<String>,
}

/// Tools of every role. Edit and Write are not listed: auto mode accepts them in the
/// working directory and its classifier reviews them elsewhere, while a bare `Write`
/// rule would allow every path.
const BASE_TOOLS: [&str; 3] = ["Read", "Glob", "Grep"];
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
            plugin_dirs: Vec::new(),
        }
    }

    /// A brief turn reads only and returns a brief; a build turn edits and returns a result.
    /// `project_grants` are the project's `grants.json`; the record holds the task's own.
    /// `broker`: the task host's socket for `wsx task check-run` (broker.rs).
    pub fn worker(record: &TaskRecord, project_grants: &[Access], broker: &Path) -> Result<Self> {
        let grants: Vec<&Access> = record.grants.iter().chain(project_grants).collect();
        let mut boundary = worker_boundary(record, &grants)?;
        boundary.unix_sockets = vec![broker.display().to_string()];
        // ^ Records from before the sandbox hold Bash rules and bare Edit and Write: drop them.
        let tools = record
            .tools
            .iter()
            .filter(|tool| tool_refusal(tool).is_none())
            .cloned()
            .chain(grants.iter().filter_map(|grant| match grant {
                Access::Tool { rule } => Some(rule.clone()),
                _ => None,
            }))
            .collect();
        let mut launch = Self::new(&boundary, &record.model, tools, worker_instructions(record));
        launch.schema = Some(match record.phase {
            Phase::Brief => {
                launch.disallowed = BRIEF_DENIED_TOOLS.map(String::from).to_vec();
                brief::schema()
            }
            Phase::Build => worker_result_schema(),
        });
        launch.env = vec![(
            super::broker::BROKER_ENV.into(),
            broker.as_os_str().to_owned(),
        )];
        launch.path_prefix = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
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
        // own settings cannot add hosts, write paths or sockets.
        "allowUnsandboxedCommands": false,
        "network": { "allowedDomains": boundary.domains },
        "filesystem": { "allowWrite": boundary.writes, "denyWrite": boundary.deny_writes },
    });
    if !boundary.unix_sockets.is_empty() {
        sandbox["network"]["allowUnixSockets"] = json!(boundary.unix_sockets);
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
                .map(|path| match path.is_dir() {
                    true => format!("Edit(/{}/**)", path.display()),
                    false => format!("Edit(/{})", path.display()),
                })
                .chain(boundary.deny_tools.iter().cloned())
                .collect::<Vec<_>>(),
        },
    })
}

/// A worker writes its working copy and the git paths its commits need in the main
/// repository; a brief turn writes nothing. ref: docs/tasks.md "Permissions"
fn worker_boundary(record: &TaskRecord, grants: &[&Access]) -> Result<Boundary> {
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
            writes.extend(grants.iter().filter_map(|grant| match grant {
                Access::Write { path } => Some(path.clone()),
                _ => None,
            }));
        }
    }
    let mut domains = record.sandbox.allowed_domains.clone();
    for grant in grants {
        if let Access::Network { host } = grant {
            if !domains.contains(host) {
                domains.push(host.clone());
            }
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
        domains,
        unix_sockets: Vec::new(),
        deny_edits,
        deny_tools: Vec::new(),
    })
}

/// What no agent's file tools may write: the main checkout, wsx's config folders and the
/// project's task copies other than a worker's own.
pub fn kept_from_edits(main: &Path, worker_copy: Option<&Path>) -> Vec<PathBuf> {
    // ^ Claude merges Edit deny rules into the sandbox, where a deny beats `allowWrite`: a rule
    // on the whole main checkout would stop a worker's commits through its `.git`. A worker's
    // rules name the checkout's entries other than `.git` instead.
    let mut paths = match worker_copy {
        Some(_) => std::fs::read_dir(main)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| entry.file_name() != ".git")
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default(),
        None => vec![main.to_path_buf()],
    };
    paths.sort();
    paths.extend(config_dirs());
    paths.extend(
        copy::task_copies(main)
            .into_iter()
            .filter(|path| Some(path.as_path()) != worker_copy),
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
         Prove what you can: each check a command, a test or a capture shows goes to \
         `evidence` with that command or the capture's path. Put in `human_verify` only \
         what needs a person: how it looks or feels, a real device, real data. \
         Before you finish a build, put in the result's memo a short, lean memo of durable \
         project knowledge a later task needs, a few lines at most: when you learned a \
         project convention, a gotcha or a decision and its reason, the memo must say it. \
         Leave the memo empty only when you learned none. \
         {checks}\
         Your commands run in a sandbox: they write only in this working copy, and a \
         host that is not allowed fails with a sandbox_violations note; when the task \
         needs that host, run the command again with the host listed. Keep file edits in \
         this working copy. \
         When you cannot continue, return status blocked and say what you need; when the \
         sandbox or a permission stops a host or a path you need, name that host or path.",
        record.branch,
        checks = check_run_note(record),
    )
}

/// The project's checks by number, and how the worker runs them before it reports done.
fn check_run_note(record: &TaskRecord) -> String {
    if record.checks.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = record
        .checks
        .iter()
        .enumerate()
        .map(|(index, check)| format!("{}. `{check}`", index + 1))
        .collect();
    format!(
        "After your result wsx runs the project's checks: {}. Before you report done, run \
         `wsx task check-run` (all) or `wsx task check-run 2 3` (by number): the task host runs \
         them outside your sandbox exactly as it will after your result, so a test that needs \
         a terminal or a browser works there. Fix what fails first. ",
        listed.join(" ")
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
        for dir in &launch.plugin_dirs {
            command.arg("--plugin-dir").arg(dir);
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
            let mut listed = HashMap::new();
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
                        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                            continue;
                        };
                        let lines = parse_line(&value)
                            .into_iter()
                            .chain(access_lines(&value, &mut listed));
                        for parsed in lines {
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

fn parse_line(value: &Value) -> Option<AgentLine> {
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
            subtype: str_field(value, "subtype"),
            text: str_field(value, "result"),
            structured: value.get("structured_output").cloned(),
            denials: denials(value).map(denial_text).collect(),
            denied_writes: denials(value)
                .filter(|denial| {
                    FILE_TOOLS.contains(
                        &denial
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
                })
                .filter_map(|denial| denial.pointer("/tool_input/file_path")?.as_str())
                .map(str::to_string)
                .collect(),
            usage: turn_usage(value),
        })),
        _ => None,
    }
}

const FILE_TOOLS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];
/// How macOS and Linux refuse a write the sandbox does not allow.
const REFUSED_WRITE: [&str; 2] = ["Operation not permitted", "Read-only file system"];

fn denials(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("permission_denials")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// What the sandbox stopped, read from tool results. `listed` keeps the hosts a pending
/// command named in `allowed_domains`, by tool use, until its result.
/// ref: docs/tasks.md "Permissions"
fn access_lines(value: &Value, listed: &mut HashMap<String, Vec<String>>) -> Vec<AgentLine> {
    let items = value
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let mut lines = Vec::new();
    match value.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            for item in items.filter(|item| item["type"] == "tool_use") {
                let hosts: Vec<String> = item
                    .pointer("/input/allowed_domains")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|host| host.as_str().map(str::to_string))
                    .collect();
                if let (Some(id), false) = (item["id"].as_str(), hosts.is_empty()) {
                    listed.insert(id.to_string(), hosts);
                }
            }
        }
        Some("user") => {
            for item in items.filter(|item| item["type"] == "tool_result") {
                let text = match &item["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let failed = item["is_error"].as_bool().unwrap_or(false);
                if let Some(hosts) = item["tool_use_id"]
                    .as_str()
                    .and_then(|id| listed.remove(id))
                {
                    if !failed {
                        lines.push(AgentLine::Reached(hosts));
                    }
                }
                lines.extend(
                    violation_hosts(&text).map(|host| AgentLine::Stopped(Access::Network { host })),
                );
                lines.extend(
                    refused_paths(&text).map(|path| AgentLine::Stopped(Access::Write { path })),
                );
            }
        }
        _ => {}
    }
    lines
}

/// Hosts in `<sandbox_violations>` lines such as `deny network-outbound example.com:443 (...)`.
fn violation_hosts(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split("<sandbox_violations>")
        .skip(1)
        .filter_map(|block| block.split("</sandbox_violations>").next())
        .flat_map(str::lines)
        .filter_map(|line| {
            line.split("network-outbound ")
                .nth(1)?
                .split_whitespace()
                .next()
        })
        .map(|target| match target.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host.to_string(),
            _ => target.to_string(),
        })
}

/// Paths in lines such as `touch: /a/b: Operation not permitted`, `Unable to create
/// '/a/b.lock': Operation not permitted` (the last absolute path before the error) or zsh's
/// `operation not permitted: /a/b`.
fn refused_paths(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines().filter_map(|line| {
        // ^ zsh names the path after the error: `operation not permitted: /a/b`.
        if let Some(path) = line
            .split_once("operation not permitted: ")
            .map(|(_, path)| path.trim())
            .filter(|path| path.starts_with('/') && path.len() > 1)
        {
            return Some(path.to_string());
        }
        let at = REFUSED_WRITE.iter().find_map(|error| line.find(error))?;
        let before = line[..at].trim_end_matches([':', ' ']);
        let (start, _) = before.char_indices().rfind(|(index, c)| {
            *c == '/' && (*index == 0 || before[..*index].ends_with([' ', '\'', '"', '`']))
        })?;
        let path = before[start..].trim_end_matches(['\'', '"', '`']);
        (path.len() > 1).then(|| path.to_string())
    })
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
