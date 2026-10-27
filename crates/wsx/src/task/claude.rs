//! Claude worker adapter: one `claude -p` process with stream JSON on stdin and stdout.
//! ref: docs/tasks.md "Claude adapter"; knowledge "Headless Worker Transport"

use super::boundary::Boundary;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskWorkflow;
use wsx_core::task::view::clip;
use wsx_core::task::{worker_result_schema, TaskRecord, TurnUsage};

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

impl TurnResult {
    /// No result and no model call: Claude's own turn for a background-task notification
    /// (a job that ended or was killed, also on `--resume`), which answers no message.
    /// ref: docs/tasks.md "Claude adapter"
    pub fn answers_nothing(&self) -> bool {
        !self.is_error
            && self.structured.is_none()
            && self.usage.input_tokens == 0
            && self.usage.output_tokens == 0
    }
}

pub struct ClaudeAgent {
    child: Child,
    stdin: ChildStdin,
    /// Control requests sent, for their ids.
    requests: u64,
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
    /// Set on top of `KEPT_ENV`.
    pub env: Vec<(String, std::ffi::OsString)>,
    /// Put first on `PATH`, so the agent's commands find this wsx build.
    pub path_prefix: Option<PathBuf>,
    /// `--plugin-dir`: the manager's plugin of the owner's skills.
    pub plugin_dirs: Vec<PathBuf>,
}

const SKILLS_PLUGIN_DIR: &str = "skills-plugin";
pub(super) const SKILLS_PLUGIN_NAME: &str = "owner";

/// An agent runs with project and local settings only, which load no user skill: a plugin
/// in its folder links the owner's skills folder, `$CLAUDE_CONFIG_DIR/skills` or
/// `~/.claude/skills`. None when the owner has none. ref: docs/tasks.md "Manager", "Workflow"
pub(super) fn skills_plugin(folder: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::{symlink, DirBuilderExt};
    let Some(skills) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .map(|root| root.join("skills"))
        .filter(|skills| skills.is_dir())
    else {
        return Ok(None);
    };
    let plugin = folder.join(SKILLS_PLUGIN_DIR);
    let meta = plugin.join(".claude-plugin");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&meta)
        .with_context(|| format!("cannot create {}", meta.display()))?;
    std::fs::write(
        meta.join("plugin.json"),
        serde_json::json!({
            "name": SKILLS_PLUGIN_NAME,
            "description": "The owner's skills, lent to wsx agents",
        })
        .to_string(),
    )
    .with_context(|| format!("cannot write {}", meta.display()))?;
    let link = plugin.join("skills");
    match std::fs::read_link(&link) {
        Ok(target) if target == skills => return Ok(Some(plugin)),
        Ok(_) => std::fs::remove_file(&link)
            .with_context(|| format!("cannot replace {}", link.display()))?,
        Err(_) if link.symlink_metadata().is_ok() => {
            anyhow::bail!("{} is not a link to the owner's skills", link.display())
        }
        Err(_) => {}
    }
    symlink(&skills, &link).with_context(|| format!("cannot link {}", link.display()))?;
    Ok(Some(plugin))
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

    /// A worker edits its working copy and returns a result.
    /// `broker`: the task host's socket for `wsx task check-run` (broker.rs).
    /// `folder`: the task's folder, where the owner's skills plugin goes (`tasks.skills`).
    pub fn worker(record: &TaskRecord, broker: &Path, folder: &Path) -> Result<Self> {
        let settings = load_task_config(&record.project_path).map_err(anyhow::Error::msg)?;
        let skills = match settings.owner_skills {
            true => skills_plugin(folder)?,
            false => None,
        };
        let boundary = Boundary::worker(record, folder)?;
        let instructions = worker_instructions(record, &settings.workflow, skills.is_some());
        let mut launch = Self::new(&record.working_copy, boundary, &record.model, instructions);
        launch.plugin_dirs = skills.into_iter().collect();
        launch.schema = Some(worker_result_schema());
        launch.env = record
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.into()))
            .chain([(
                super::broker::BROKER_ENV.into(),
                broker.as_os_str().to_owned(),
            )])
            .collect();
        launch.path_prefix = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        Ok(launch)
    }
}

/// Claude records the system prompt of a conversation's first request and reuses it on
/// every resume (`--system-prompt-snapshot`): what changes is said in the user messages.
fn worker_instructions(record: &TaskRecord, workflow: &TaskWorkflow, skills: bool) -> String {
    // ^ No skill is named: the owner's skills are whatever their folder holds.
    let skills = match skills {
        true => format!(
            "The owner's skills are yours with the Skill tool as `{SKILLS_PLUGIN_NAME}:<name>`: \
             at the start, look at them and follow any whose description fits the work, such \
             as a development workflow, a simplification or a test skill. "
        ),
        false => String::new(),
    };
    format!(
        "You are a task worker in a separate working copy on branch {}. \
         Nobody reads your terminal: work without asking questions. \
         Build what the prompt asks and commit your changes with git before you finish. \
         Build and test what you change. {skills}{build} \
         Prove what you can: each check a command, a test or a capture shows goes to \
         `evidence` with that command or the capture's path. Anything a test, an assert or a \
         capture can show is yours to prove, also a state you must stage in a fixture (a \
         dirty checkout, a failed deploy, an error); never put a command in `human_verify`. \
         `human_verify` keeps only how it looks or feels, and an item about a screen comes \
         with a capture of that state; an item without a capture is only for a real device \
         or the owner's real data, and its `where` says so. Each item says in plain words \
         what to look at, `where` (a URL, a page path or the screen), `capture` (the path \
         of an image of it in this working copy) and `pass_if`. \
         Before you finish a build, put in the result's memo a short, lean memo of durable \
         project knowledge a later task needs, a few lines at most: when you learned a \
         project convention, a gotcha or a decision and its reason, the memo must say it. \
         Leave the memo empty only when you learned none. \
         {checks}\
         You run inside wsx's boundary and nothing asks you for permission: write where \
         the work needs, listen on 127.0.0.1, open a PTY. The boundary refuses the owner's \
         secrets, wsx's config and state, other tasks' copies and the default branch's ref \
         (wsx lands). Keep your work in this working copy. \
         When you cannot continue, return status blocked and say what you need; when the \
         boundary refuses something the task needs, name it. \
         {steps}Write the summary and the report by this rule. {presentation}",
        record.branch,
        steps = steps_note(record),
        presentation = wsx_core::integration::PRESENTATION.trim_end(),
        build = workflow.build,
        checks = check_run_note(record),
    )
}

/// The workflow steps in order: each turn names its step.
fn steps_note(record: &TaskRecord) -> String {
    if record.steps.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = record.steps.iter().map(|step| step.name.as_str()).collect();
    format!(
        "The task runs in steps, one turn each: {}. Each turn names its step: do that step \
         only, then return status done as the turn's last call: nothing runs after it. When \
         a step needs a decision that is the owner's, return status blocked with the \
         question; otherwise decide yourself and go on. ",
        names.join(", ")
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
    // ^ The scenarios run only by name or number, never after a result (host.rs check_run).
    let scenarios = wsx_core::config::project::load_task_config(&record.project_path)
        .map(|settings| settings.scenarios)
        .unwrap_or_default();
    let scenarios = match scenarios.is_empty() {
        true => String::new(),
        false => format!(
            "The project's end-to-end scenarios run only when you name them, by name or by \
             number: {}. ",
            scenarios
                .iter()
                .enumerate()
                .map(|(at, (name, command))| {
                    format!("{}. `{name}` (`{command}`)", record.checks.len() + at + 1)
                })
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    format!(
        "After your result wsx runs all of the project's checks once: {}. {scenarios}\
         `wsx task check-run 2 3` runs checks or scenarios by number or name: the task host \
         runs them outside your boundary exactly as it will after your result, so a test \
         that needs a terminal or a browser works there. Run only the checks your change touches, never the full set: \
         each run waits for the machine's one check slot and can take 20 minutes or more, so \
         start it with run_in_background (or a 600000 ms timeout) and never return your \
         result while it has no answer. A check that already ran on the \
         same commit, with nothing uncommitted, answers from its stored result. Fix what \
         fails in code you changed. A check that fails in code you did not touch is not \
         yours: do not run it again, name it in your summary. ",
        listed.join(" ")
    )
}

impl ClaudeAgent {
    /// Start the agent. `resume` names a conversation that already exists;
    /// otherwise `session` becomes the new conversation's ID.
    /// `cost_base` is the session's last `session_cost_micro_usd`, which a resumed process
    /// counts on from.
    pub fn spawn<E: Send + 'static>(
        launch: &Launch,
        session: &str,
        resume: bool,
        cost_base: u64,
        events: Sender<E>,
        wrap: impl Fn(Option<AgentLine>) -> E + Send + 'static,
    ) -> Result<Self> {
        // ^ The host's boundary confines the agent; the adapter only adds its own argv.
        let mut command = launch.boundary.command("claude")?;
        command.args(claude_args(launch, session, resume));
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
            let mut turns = Turns {
                cost_base,
                ..Turns::default()
            };
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
                        if let Some(parsed) = parse_line(&value, &mut turns) {
                            if events.send(wrap(Some(parsed))).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            let _ = events.send(wrap(None));
        });
        Ok(Self {
            child,
            stdin,
            requests: 0,
        })
    }

    /// Send one user message. During a turn Claude takes it as a steer.
    pub fn send(&mut self, text: &str) -> Result<()> {
        self.write(&json!({
            "type": "user",
            "message": { "role": "user", "content": text },
        }))
    }

    /// The next turns run on `model`; the conversation stays. The SDK's `setModel` sends
    /// this control request; a refusal comes back as a progress line.
    /// ref: https://code.claude.com/docs/en/agent-sdk/typescript ("setModel")
    pub fn set_model(&mut self, model: &str) -> Result<()> {
        self.requests += 1;
        self.write(&json!({
            "type": "control_request",
            "request_id": format!("wsx-{}", self.requests),
            "request": { "subtype": "set_model", "model": model },
        }))
    }

    fn write(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(value)?;
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

/// Claude's argv: stream JSON both ways, no prompt for any tool (`bypassPermissions`: the
/// boundary decides), and only project and local settings.
fn claude_args(launch: &Launch, session: &str, resume: bool) -> Vec<std::ffi::OsString> {
    // ^ Agent commits carry no AI attribution; `attribution` replaced `includeCoAuthoredBy`,
    // which older builds still read.
    let settings =
        json!({ "attribution": { "commit": "", "pr": "" }, "includeCoAuthoredBy": false });
    let mut args: Vec<std::ffi::OsString> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--setting-sources",
        "project,local",
        "--permission-mode",
        "bypassPermissions",
        "--model",
        &launch.model,
        "--append-system-prompt",
        &launch.instructions,
        "--settings",
        &settings.to_string(),
        if resume { "--resume" } else { "--session-id" },
        session,
    ]
    .map(Into::into)
    .to_vec();
    if let Some(schema) = &launch.schema {
        args.extend(["--json-schema".into(), schema.to_string().into()]);
    }
    for dir in &launch.plugin_dirs {
        args.extend(["--plugin-dir".into(), dir.clone().into_os_string()]);
    }
    args
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

/// What the reader keeps across the lines of one process.
/// ref: https://code.claude.com/docs/en/headless (stream-json `assistant` and `result`)
#[derive(Default)]
struct Turns {
    /// The session's `total_cost_usd` at its previous result, in micro USD.
    cost_base: u64,
    /// The context and model of the turn's last model call so far.
    context: Option<u64>,
    model: String,
}

impl Turns {
    /// A main-conversation `assistant` line carries the usage of its own model call; a
    /// subagent's (`parent_tool_use_id` set) is another context.
    fn note_call(&mut self, value: &Value) {
        if value
            .get("parent_tool_use_id")
            .is_some_and(|parent| !parent.is_null())
        {
            return;
        }
        if let Some(usage) = value.pointer("/message/usage") {
            self.context = Some(context_of(usage));
        }
        if let Some(model) = value.pointer("/message/model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
    }
}

/// Input, cache read and cache write tokens of one `usage` object.
fn context_of(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .filter_map(|name| usage.get(name).and_then(Value::as_u64))
    .sum()
}

fn parse_line(value: &Value, turns: &mut Turns) -> Option<AgentLine> {
    match value.get("type")?.as_str()? {
        "system" if value.get("subtype")?.as_str()? == "init" => Some(AgentLine::Init(
            value.get("session_id")?.as_str()?.to_string(),
        )),
        "assistant" => {
            turns.note_call(value);
            value
                .pointer("/message/content")?
                .as_array()?
                .iter()
                .rev()
                .find_map(progress_text)
                .map(AgentLine::Progress)
        }
        "control_response" if value.pointer("/response/subtype")?.as_str()? == "error" => {
            Some(AgentLine::Progress(format!(
                "claude refused a control request: {}",
                value
                    .pointer("/response/error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason")
            )))
        }
        "result" => Some(AgentLine::Result(TurnResult {
            is_error: value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            subtype: str_field(value, "subtype"),
            text: str_field(value, "result"),
            structured: value.get("structured_output").cloned(),
            denials: value
                .get("permission_denials")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(denial_text)
                .collect(),
            usage: turn_usage(value, turns),
        })),
        _ => None,
    }
}

/// The result's `usage` sums every model call of the turn and `total_cost_usd` counts the
/// whole process: the turn's cost is the difference, its context the last call's.
fn turn_usage(value: &Value, turns: &mut Turns) -> TurnUsage {
    let count = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    let total = value
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .map(|cost| (cost * 1_000_000.0).round() as u64);
    // ^ A total below the base: the process did not restore the session's spend (a crash,
    // an older CLI), so it counts from zero.
    let cost = total.map_or(0, |total| {
        total.checked_sub(turns.cost_base).unwrap_or(total)
    });
    turns.cost_base = total.unwrap_or(turns.cost_base);
    TurnUsage {
        duration_ms: count("/duration_ms"),
        input_tokens: count("/usage/input_tokens"),
        output_tokens: count("/usage/output_tokens"),
        cache_read_tokens: count("/usage/cache_read_input_tokens"),
        cache_creation_tokens: count("/usage/cache_creation_input_tokens"),
        cost_micro_usd: cost,
        session_cost_micro_usd: turns.cost_base,
        // ^ No `assistant` usage seen (an older CLI): the sum is all there is.
        context_tokens: turns
            .context
            .take()
            .unwrap_or_else(|| value.get("usage").map_or(0, context_of)),
        model: std::mem::take(&mut turns.model),
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
        Some(argument) => format!("{tool}({})", clip(&argument, PROGRESS_CHARS)),
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
                Some(argument) => format!("{name} {}", clip(&argument, PROGRESS_CHARS)),
                None => name.to_string(),
            })
        }
        "text" => {
            let text = item.get("text")?.as_str()?.trim();
            (!text.is_empty()).then(|| clip(text, PROGRESS_CHARS))
        }
        _ => None,
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
