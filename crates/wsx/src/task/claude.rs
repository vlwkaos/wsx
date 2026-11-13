//! Claude worker adapter: one `claude -p` process with stream JSON on stdin and stdout.
//! ref: docs/tasks.md "Claude adapter"; knowledge "Headless Worker Transport"

use super::agent::{
    end_stream, read_json_lines, thinking_chars, turn_text, AgentLine, AgentProcess, Launch,
    TurnResult, UsageLimit,
};
use super::boundary::Boundary;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use wsx_core::config::global::GlobalConfig;
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskWorkflow;
use wsx_core::task::policy::Role;
use wsx_core::task::view::clip;
use wsx_core::task::{now_unix_ms, store, worker_result_schema, TaskRecord, TurnUsage};

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

pub struct ClaudeAgent {
    process: AgentProcess,
    /// Control requests sent, for their ids.
    requests: u64,
}

const SKILLS_PLUGIN_DIR: &str = "skills-plugin";
pub(super) const SKILLS_PLUGIN_NAME: &str = "owner";

/// The agent account of the project at `project`: its profile's `CLAUDE_CONFIG_DIR`, None
/// for the host's own. A config wsx cannot read stops the start: a guess could pick the
/// wrong account. ref: docs/configuration.md "Profiles"
pub(super) fn config_root(project: &Path) -> Result<Option<PathBuf>> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(warning) = warning {
        anyhow::bail!("wsx config: {warning}");
    }
    config.claude_config_dir(project)
}

/// An agent runs with project and local settings only, which load no user skill: a plugin
/// in its folder links the owner's skills folder, `<config root>/skills`,
/// `$CLAUDE_CONFIG_DIR/skills` or `~/.claude/skills`. None when the owner has none.
/// `only` (`tasks.skillsOnly`) non-empty: `skills/` is a folder of links to just those skills;
/// one the owner lacks is skipped with a warning.
/// ref: docs/tasks.md "Manager", "Workflow"
pub(super) fn skills_plugin(
    folder: &Path,
    root: Option<&Path>,
    only: &[String],
) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::{symlink, DirBuilderExt};
    let Some(skills) = root
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
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
    if !only.is_empty() {
        // ^ A link from an earlier launch without `only` goes; a folder is rebuilt.
        match link.symlink_metadata() {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&link),
            Ok(_) => std::fs::remove_file(&link),
            Err(_) => Ok(()),
        }
        .with_context(|| format!("cannot replace {}", link.display()))?;
        std::fs::create_dir(&link).with_context(|| format!("cannot create {}", link.display()))?;
        for name in only {
            let source = skills.join(name);
            if source.is_dir() {
                symlink(&source, link.join(name))
                    .with_context(|| format!("cannot link {}", link.join(name).display()))?;
            } else {
                eprintln!(
                    "wsx: tasks.skillsOnly names {name}, not in {}",
                    skills.display()
                );
            }
        }
        return Ok(Some(plugin));
    }
    if link.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
        std::fs::remove_dir_all(&link)
            .with_context(|| format!("cannot replace {}", link.display()))?;
    }
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
    /// A worker edits its working copy and returns a result.
    /// `broker`: the task host's socket for `wsx task check-run` (broker.rs).
    /// `folder`: the task's folder, where the owner's skills plugin goes (`tasks.skills`).
    pub fn worker(record: &TaskRecord, broker: &Path, folder: &Path) -> Result<Self> {
        let settings = load_task_config(&record.project_path).map_err(anyhow::Error::msg)?;
        let root = config_root(&record.project_path)?;
        let skills = match settings.owner_skills {
            true => skills_plugin(
                folder,
                root.as_deref(),
                record.skills.as_ref().unwrap_or(&settings.skills_only),
            )?,
            false => None,
        };
        let env: Vec<(String, std::ffi::OsString)> = record
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.into()))
            .chain([(
                super::broker::BROKER_ENV.into(),
                broker.as_os_str().to_owned(),
            )])
            .chain(account_env(root.clone()))
            .collect();
        let policy = super::agent::policy(Role::Worker, &record.adapter, &record.model, &env);
        let boundary = Boundary::worker(record, folder, policy)?
            .adapter(&record.adapter, root.as_deref())
            .reach(&record.adapter, &record.model, &env)
            .broker(broker);
        let structured = record.adapter == "claude";
        let mut instructions = worker_instructions(record, &settings.workflow, skills.is_some());
        // ^ An adapter without structured output reports through the broker (host.rs
        // `reported`); the schema is the same. ref: docs/tasks.md "Agents"
        if !structured {
            instructions = instructions.replace(
                "with the Skill tool as `owner:<name>`",
                "in your skills list",
            );
            instructions.push_str(&format!(
                " End every turn by sending your result, one JSON object that matches this JSON \
                 Schema, on stdin to `wsx task result -` (for example with a quoted heredoc), \
                 then end the turn with no further tool call. A result it refuses names why: \
                 fix it and send it again. Schema: {}",
                worker_result_schema()
            ));
        }
        let mut launch = Self::new(&record.working_copy, boundary, &record.model, instructions);
        launch.effort = record.effort.clone();
        launch.plugin_dirs = skills.into_iter().collect();
        launch.schema = structured.then(worker_result_schema);
        launch.env = env;
        launch.path_prefix = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        Ok(launch)
    }
}

/// `CLAUDE_CONFIG_DIR` over the host's own, when the project's profile sets one.
pub(super) fn account_env(root: Option<PathBuf>) -> Option<(String, std::ffi::OsString)> {
    root.map(|root| ("CLAUDE_CONFIG_DIR".into(), root.into_os_string()))
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
    // ^ 42% of worker spend is reads, one call at a time, each paying for the whole context
    // again (docs/design/manager-efficiency.md). Only Claude has subagents here. The
    // foreground: a background subagent's result came after a plan's result
    // (docs/design/manager-reliability.md "F").
    let search = match record.adapter.as_str() {
        "claude" => {
            "A search across many files goes to a subagent in the foreground (the Agent tool, \
             type Explore, `run_in_background: false`; `model: sonnet`, or `model: haiku` for a \
             plan's code reading), and only its findings, with file:line, come back to you. "
        }
        _ => "",
    };
    format!(
        "You are a task worker in a separate working copy on branch {}. \
         Nobody reads your terminal: work without asking questions. \
         Build what the prompt asks and commit your changes with git before you finish. \
         Build and test what you change. {skills}{build} \
         Every model call reads your whole context again, so use few calls: put the searches \
         and reads you already know you need in one message, not one after another, and read \
         line ranges, not whole files. {search}Never poll: a background job tells you when it \
         ends. Wait for or stop each background job before your result: one that ends after \
         it starts another turn. Stop a job only by the pid or process group you started, \
         never with `pkill -f` or `killall`: the boundary lets you signal only your own \
         processes. \
         Prove what you can: each check a command, a test or a capture shows goes to \
         `evidence` with that command or the capture's path, and a number or a cause in the \
         summary names its source. Anything a test, an assert or a \
         capture can show is yours to prove, also a state you must stage in a fixture (a \
         dirty checkout, a failed deploy, an error); never put a command in `human_verify`. \
         `human_verify` keeps only how it looks or feels, and an item about a screen comes \
         with a capture of that state; an item without a capture is only for a real device \
         or the owner's real data, and its `where` says so. Each item says in plain words \
         what to look at, `where` (a URL, a page path or the screen), `capture` (the path \
         of an image of it in this working copy, or of a short MP4 video of at most 25 MiB), \
         `poster` (a video's thumbnail image in this working copy, else empty) and `pass_if`. \
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
         boundary refuses something the task needs, name it. When your turn must end while \
         something outside goes on (a run you started) and you ask nobody anything, return \
         status paused with what runs and what is left: the manager continues you. \
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
         a local run waits for the machine's one check slot, often 20 to 90 minutes, and a \
         `ci:` scenario runs on CI, 20 minutes or more. Commit first and run it in the \
         foreground: a run that must wait or runs on CI answers `queued` at once; \
         then do not wait or poll, finish the step and return its result, and the host sends \
         the answer as your next turn. A check that already ran on the \
         same commit, with nothing uncommitted, answers from its stored result. Fix what \
         fails in code you changed. A check that fails in code you did not touch is not \
         yours: do not run it again, name it in your summary. After a rebase or a conflict \
         fix run no check-run: the checks after your result prove the rebased tree. \
         `wsx task check-run <scenario> --part <part>` reruns one part of a scenario that has \
         parts (`web`: `chat`, `tasks`) after a fix: its pass is not a full pass, so run the \
         scenario whole once more before your result. ",
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
        let (process, stdout) = AgentProcess::spawn(
            launch,
            "claude",
            claude_args(launch, session, resume),
            KEPT_ENV,
            &[],
            std::process::Stdio::inherit(),
        )?;
        std::thread::spawn(move || {
            // ^ The account's newest limit state, for the usage views; a failed write only
            // leaves the last one. ref: docs/tasks.md "Usage"
            let limit = |info: &Value| {
                let _ = store::note_rate_limit("claude", info, now_unix_ms());
                // ^ A call went through: a limit on record has lifted early.
                if rate_allows(info) {
                    store::clear_limit("claude");
                }
            };
            let read = read_stream(stdout, cost_base, limit, |line| {
                events.send(wrap(Some(line))).is_ok()
            });
            end_stream(&events, &wrap, read);
        });
        Ok(Self {
            process,
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
        self.process.write(value)
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
    if !launch.effort.is_empty() {
        args.extend(["--effort".into(), (&launch.effort).into()]);
    }
    if let Some(schema) = &launch.schema {
        args.extend(["--json-schema".into(), schema.to_string().into()]);
    }
    for dir in &launch.plugin_dirs {
        args.extend(["--plugin-dir".into(), dir.clone().into_os_string()]);
    }
    args
}

/// Read one process's stdout to its end, handing each parsed line to `send` (false stops)
/// and each `rate_limit_info` to `limit`. The reader thread and `stream_tests.rs` both run this.
pub(super) fn read_stream(
    stdout: impl Read,
    cost_base: u64,
    mut limit: impl FnMut(&Value),
    mut send: impl FnMut(AgentLine) -> bool,
) -> std::io::Result<()> {
    let mut turns = Turns {
        cost_base,
        ..Turns::default()
    };
    read_json_lines(stdout, |value| {
        if value["type"] == "rate_limit_event" {
            limit(&value["rate_limit_info"]);
        }
        parse_line(&value, &mut turns).is_none_or(&mut send)
    })
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
    /// The turn's main-conversation calls so far, and the last one's message id.
    calls: u32,
    call_id: Option<String>,
    /// `resetsAt` and `rateLimitType` of the last rejected `rate_limit_info` before the result.
    resets_at: Option<u64>,
    window: Option<String>,
    /// The live background jobs of the last `background_tasks_changed`, its `ambient` ones
    /// (Claude's own) left out. None are announced at startup, so a process starts at 0.
    background: usize,
    /// The turn's main-conversation text blocks so far, in order: `result` holds the last one.
    said: Vec<String>,
    /// The characters of the turn's main-conversation thinking so far.
    thought: usize,
    /// A `Skill` call of the simplify skill came in the turn.
    simplified: bool,
}

impl Turns {
    /// A main-conversation `assistant` line carries the usage of its own model call; a
    /// subagent's (`parent_tool_use_id` set) is another context. Claude writes one line per
    /// content block, each with its call's message id: a line without one is its own call.
    fn note_call(&mut self, value: &Value) {
        if is_subagent(value) {
            return;
        }
        let id = value
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if id.is_none() || id != self.call_id {
            self.calls += 1;
        }
        self.call_id = id;
        if let Some(usage) = value.pointer("/message/usage") {
            self.context = Some(context_of(usage));
        }
        if let Some(model) = value.pointer("/message/model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
    }

    /// A rejected `rate_limit_info`, on a `rate_limit_event` or an API error's `assistant`
    /// line, names when the limit resets.
    fn note_limit(&mut self, info: Option<&Value>) {
        let Some(info) = info.filter(|info| info["status"] == "rejected") else {
            return;
        };
        // ^ The top-level `resetsAt` and `rateLimitType` are the representative claim's (Claude
        // Code copies the `anthropic-ratelimit-unified-representative-claim` and `-reset`
        // headers), which can be a window that still allows calls: a 7-day reset days ahead
        // was taken for a 5-hour limit. The binding window is one of `unifiedWindows` with
        // `status: rejected`; with several, the limit lifts when the last of them resets.
        let binding = info["unifiedWindows"].as_object().and_then(|windows| {
            windows
                .iter()
                .filter(|(_, window)| window["status"] == "rejected")
                .filter_map(|(name, window)| window["resetsAt"].as_u64().map(|at| (at, name)))
                .max()
        });
        if let Some((at, name)) = binding {
            self.resets_at = Some(at);
            self.window = Some(name.clone());
            return;
        }
        if let Some(at) = info.get("resetsAt").and_then(Value::as_u64) {
            self.resets_at = Some(at);
        }
        if let Some(window) = info.get("rateLimitType").and_then(Value::as_str) {
            self.window = Some(window.to_string());
        }
    }
}

fn is_subagent(value: &Value) -> bool {
    value
        .get("parent_tool_use_id")
        .is_some_and(|parent| !parent.is_null())
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
        "system" if value.get("subtype")?.as_str()? == "background_tasks_changed" => {
            turns.background = value
                .get("tasks")?
                .as_array()?
                .iter()
                .filter(|task| task["ambient"] != true)
                .count();
            None
        }
        "rate_limit_event" => {
            turns.note_limit(value.get("rate_limit_info"));
            None
        }
        "assistant" => {
            turns.note_call(value);
            turns.note_limit(value.pointer("/api_error_params/rate_limit_info"));
            // ^ The limit's own words are no progress: the host says when it resumes.
            if value["api_error"] == "usage_limit_reached" {
                return None;
            }
            let content = value.pointer("/message/content")?.as_array()?;
            turns.simplified |= content.iter().any(runs_simplify);
            // ^ An API error's words are the result's reason, not a reply; a subagent's are no
            // reply of this conversation.
            if !is_subagent(value) && !value["isApiErrorMessage"].as_bool().unwrap_or(false) {
                turns.said.extend(
                    content
                        .iter()
                        .filter(|item| item["type"] == "text")
                        .filter_map(|item| item["text"].as_str())
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_string),
                );
                turns.thought += thinking_chars(content);
            }
            content
                .iter()
                .rev()
                .find_map(progress_text)
                .map(|(text, said)| AgentLine::Progress { text, said })
        }
        "control_response" if value.pointer("/response/subtype")?.as_str()? == "error" => {
            Some(AgentLine::Progress {
                text: format!(
                    "claude refused a control request: {}",
                    value
                        .pointer("/response/error")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason")
                ),
                said: false,
            })
        }
        "result" => {
            let is_error = value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let said = std::mem::take(&mut turns.said);
            // ^ An error's `result` is its reason, whatever was said before it.
            let text = if is_error {
                str_field(value, "result")
            } else {
                turn_text(&said, str_field(value, "result"))
            };
            Some(AgentLine::Result(Box::new(TurnResult {
                limit: usage_limit(value, turns.resets_at.take(), turns.window.take()),
                background: turns.background,
                thought: std::mem::take(&mut turns.thought),
                simplified: std::mem::take(&mut turns.simplified),
                is_error,
                subtype: str_field(value, "subtype"),
                text,
                structured: value.get("structured_output").cloned(),
                denials: value
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(denial_text)
                    .collect(),
                usage: turn_usage(value, turns),
            })))
        }
        _ => None,
    }
}

/// A `tool_use` of the `Skill` tool that loads the simplify skill, as `simplify` or as the
/// owner's plugin skill. ref: docs/tasks.md "Workflow"
fn runs_simplify(item: &Value) -> bool {
    item["type"] == "tool_use"
        && item["name"] == "Skill"
        && item
            .pointer("/input/skill")
            .and_then(Value::as_str)
            .is_some_and(|skill| {
                skill == "simplify" || skill == format!("{SKILLS_PLUGIN_NAME}:simplify")
            })
}

/// The task folder holds the skills plugin with the simplify skill: the worker was lent it.
pub(super) fn lends_simplify(folder: &Path) -> bool {
    folder
        .join(SKILLS_PLUGIN_DIR)
        .join("skills/simplify")
        .is_dir()
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
    turns.call_id = None;
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
        adapter: "claude".into(),
        calls: std::mem::take(&mut turns.calls),
    }
}

/// A result that Claude's usage limit ended, with its reset from the stream or its words.
fn usage_limit(
    result: &Value,
    resets_at: Option<u64>,
    window: Option<String>,
) -> Option<UsageLimit> {
    (result["api_error"] == "usage_limit_reached").then(|| UsageLimit {
        resets_at: resets_at
            .or_else(|| text_reset(&str_field(result, "result"), now_unix_ms() / 1000)),
        window,
    })
}

/// The reset in Claude's own words, "You've hit your session limit · resets 12:10am
/// (Asia/Seoul)", as the next such time after `now` (unix s). Only a time of day in this
/// machine's zone is read: anything else gives none, and the host backs off.
fn text_reset(text: &str, now: u64) -> Option<u64> {
    let (_, rest) = text.rsplit_once("resets ")?;
    let (clock, zone) = rest.trim_end().strip_suffix(')')?.split_once(" (")?;
    if Some(zone) != local_zone().as_deref() {
        return None;
    }
    let (clock, pm) = match clock.strip_suffix("pm") {
        Some(clock) => (clock, true),
        None => (clock.strip_suffix("am")?, false),
    };
    let (hour, minute) = clock.split_once(':').unwrap_or((clock, "0"));
    let (hour, minute) = (hour.parse::<i32>().ok()?, minute.parse::<i32>().ok()?);
    if !(1..=12).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    let mut tm = wsx_core::routine::local::local_tm(now as i64)?;
    tm.tm_hour = hour % 12 + if pm { 12 } else { 0 };
    tm.tm_min = minute;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    // SAFETY: `tm` is a valid, initialized value for the call.
    let at = u64::try_from(unsafe { libc::mktime(&mut tm) }).ok()?;
    Some(if at <= now { at + 86_400 } else { at })
}

/// This machine's IANA zone name: `TZ`, else the target of `/etc/localtime`.
fn local_zone() -> Option<String> {
    let zone = match std::env::var("TZ") {
        Ok(zone) => zone,
        Err(_) => std::fs::read_link("/etc/localtime")
            .ok()?
            .to_string_lossy()
            .into_owned(),
    };
    let zone = zone.trim_start_matches(':');
    Some(
        zone.split_once("zoneinfo/")
            .map_or(zone, |(_, name)| name)
            .to_string(),
    )
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

fn progress_text(item: &Value) -> Option<(String, bool)> {
    match item.get("type")?.as_str()? {
        "tool_use" => {
            let name = item.get("name")?.as_str()?;
            let line = match item.get("input").and_then(key_argument) {
                Some(argument) => format!("{name} {}", clip(&argument, PROGRESS_CHARS)),
                None => name.to_string(),
            };
            Some((line, false))
        }
        "text" => {
            let text = item.get("text")?.as_str()?.trim();
            (!text.is_empty()).then(|| (clip(text, PROGRESS_CHARS), true))
        }
        _ => None,
    }
}

/// How often one host on the machine asks Claude whether a usage limit on record still holds
/// (`WSX_TEST_LIMIT_PROBE_SECS` for `scripts/test-task.py`), and how long that call may take:
/// less than the period, so probes never overlap. ref: docs/tasks.md "Agents"
const LIMIT_PROBE_EVERY_MS: u64 = 30_000;
const LIMIT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

fn limit_probe_every_ms() -> u64 {
    std::env::var("WSX_TEST_LIMIT_PROBE_SECS")
        .ok()
        .and_then(|secs| secs.parse::<u64>().ok())
        .map_or(LIMIT_PROBE_EVERY_MS, |secs| secs * 1000)
}

/// Claude's rate limit event says the account takes calls.
fn rate_allows(info: &Value) -> bool {
    matches!(
        info["status"].as_str(),
        Some("allowed") | Some("allowed_warning")
    )
}

/// While Claude's usage limit is on record, ask the provider with one tiny call, on the model
/// the limit refused, whether it still holds: the provider's answer, not the stored reset time,
/// decides every wait. An allowed answer removes the record and every waiting host resumes; a
/// rejected one leaves it. Every waiting host calls this at each look; it runs in its own
/// thread, at most once per period on the machine, and not for a reset that comes within the
/// period. ref: docs/tasks.md "Agents"
pub fn probe_limit() {
    let now = now_unix_ms();
    let every = limit_probe_every_ms();
    let Some(limit) = store::account_limit("claude", now).filter(|l| l.until_ms > now + every)
    else {
        return;
    };
    if !store::claim_limit_probe("claude", now, every) {
        return;
    }
    let model = limit.model.unwrap_or_else(|| "haiku".into());
    std::thread::spawn(move || run_limit_probe(&model));
}

fn run_limit_probe(model: &str) {
    use wsx_core::process::{run_bounded, Bound, Options, Stderr};
    let mut command = std::process::Command::new("claude");
    // ^ No project folder: no CLAUDE.md or settings, no tools asked for.
    command
        .current_dir(store::state_dir())
        .env_clear()
        .envs(
            KEPT_ENV
                .iter()
                .filter_map(|name| Some((*name, std::env::var_os(name)?))),
        )
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        .args(["--model", model, "--setting-sources", ""])
        .arg("Reply with the word ok.")
        .stdin(std::process::Stdio::null());
    let options = Options {
        timeout: LIMIT_PROBE_TIMEOUT,
        stdout: Bound::Tail(256 * 1024),
        stderr: Stderr::Null,
        reap: true,
    };
    let Ok(run) = run_bounded(&mut command, &options) else {
        return;
    };
    let now = now_unix_ms();
    let mut allowed = false;
    for line in run.stdout.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("rate_limit_event") => {
                let info = &value["rate_limit_info"];
                let _ = store::note_rate_limit("claude", info, now);
                allowed |= rate_allows(info);
            }
            Some("result") => allowed |= value["is_error"] == false,
            _ => {}
        }
    }
    if allowed {
        store::clear_limit("claude");
    }
}
