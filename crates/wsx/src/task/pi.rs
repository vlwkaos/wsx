//! Pi adapter: one `pi --mode rpc` process, JSONL commands on stdin, session events on stdout.
//! ref: docs/tasks.md "Pi adapter"; Pi docs rpc.md, rpc-commands.md, json.md

use super::agent::{
    end_stream, read_json_lines, thinking_chars, turn_text, AgentLine, AgentProcess, Launch,
    TurnResult, UsageLimit,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use wsx_core::task::view::clip;
use wsx_core::task::TurnUsage;

const PROGRESS_CHARS: usize = 160;
/// Variables the agent keeps from the host, as the Claude adapter's, with Pi's own.
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
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
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

pub struct PiAgent {
    process: AgentProcess,
    requests: u64,
    /// Dropped after the process: the launch's folder goes with its agent.
    _home: LaunchHome,
}

/// Pi's own folder for one launch (`PI_CODING_AGENT_DIR`): only the owner's `models.json`, so
/// their Pi settings, packages, keys and extensions are not loaded. Pi adds its own caches
/// to it. ref: docs/design/local-model-fallback.md 6.1, 8 (W2)
struct LaunchHome(PathBuf);

static HOMES: AtomicU64 = AtomicU64::new(0);

impl LaunchHome {
    /// `source` is the owner's Pi folder; a missing `models.json` there leaves the folder empty.
    fn create(source: &Path) -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "wsx-pi-{}-{}",
            std::process::id(),
            HOMES.fetch_add(1, Ordering::Relaxed)
        ));
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        let home = Self(path);
        match std::fs::copy(source.join("models.json"), home.0.join("models.json")) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot copy Pi's models.json"),
        }
        Ok(home)
    }
}

impl Drop for LaunchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The owner's Pi folder: `PI_CODING_AGENT_DIR` of the host, else `~/.pi/agent`.
fn owner_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".pi/agent")))
        .unwrap_or_default()
}

/// The `extension` of the first registry agent that runs `model` on the Pi adapter. A file
/// that is gone fails the launch: a lean runner must not start as the plain one.
fn extension_of(model: &str) -> Result<Option<PathBuf>> {
    let Ok((config, _)) = wsx_core::config::global::GlobalConfig::load() else {
        return Ok(None);
    };
    let Some(path) = config
        .agents
        .into_iter()
        .find(|agent| {
            agent.adapter.as_deref() == Some("pi") && agent.model.as_deref() == Some(model)
        })
        .and_then(|agent| agent.extension)
    else {
        return Ok(None);
    };
    anyhow::ensure!(path.is_file(), "extension {} is not a file", path.display());
    Ok(Some(path))
}

impl PiAgent {
    /// Start the agent on conversation `session`: Pi opens it when it exists, else creates
    /// it. Pi reports no conversation ID in RPC mode, so the given one is the Init line.
    pub fn spawn<E: Send + 'static>(
        launch: &Launch,
        session: &str,
        cost_base: u64,
        events: Sender<E>,
        wrap: impl Fn(Option<AgentLine>) -> E + Send + 'static,
    ) -> Result<Self> {
        // ^ A worker's check or a manager's quiet hour outlasts Pi's default 5-minute cache, and
        // then the whole context is written again. ref: docs/design/manager-efficiency.md
        let source = owner_dir();
        let home = LaunchHome::create(&source)?;
        // ^ Best effort: Pi reports a folder it cannot write.
        let _ = std::fs::create_dir_all(source.join("sessions/wsx"));
        let dir = home.0.to_str().context("the Pi folder is not UTF-8")?;
        let (process, stdout) = AgentProcess::spawn(
            launch,
            "pi",
            pi_args(
                launch,
                session,
                extension_of(&launch.model)?.as_deref(),
                &source,
            ),
            KEPT_ENV,
            &[("PI_CACHE_RETENTION", "long"), ("PI_CODING_AGENT_DIR", dir)],
        )?;
        let init = session.to_string();
        std::thread::spawn(move || {
            if events.send(wrap(Some(AgentLine::Init(init)))).is_err() {
                return;
            }
            let read = read_stream(stdout, cost_base, |line| {
                events.send(wrap(Some(line))).is_ok()
            });
            end_stream(&events, &wrap, read);
        });
        Ok(Self {
            process,
            requests: 0,
            _home: home,
        })
    }

    /// Send one user message. During a run Pi takes it as a steer.
    pub fn send(&mut self, text: &str) -> Result<()> {
        self.request(json!({ "type": "prompt", "message": text, "streamingBehavior": "steer" }))
    }

    /// The next turns run on `model`, `provider/id`; the conversation stays. A refusal comes
    /// back as a progress line.
    pub fn set_model(&mut self, model: &str) -> Result<()> {
        let (provider, id) = model
            .split_once('/')
            .with_context(|| format!("a pi model is provider/id, not {model}"))?;
        self.request(json!({ "type": "set_model", "provider": provider, "modelId": id }))
    }

    fn request(&mut self, mut value: Value) -> Result<()> {
        self.requests += 1;
        value["id"] = json!(format!("wsx-{}", self.requests));
        self.process.write(&value)
    }

    /// Queue `abort` without waiting for Pi to read it, then stop: the kill timer starts at
    /// once, so a hung Pi cannot hold the host.
    pub fn stop(mut self) {
        let _ = self.request(json!({ "type": "abort" }));
    }
}

/// Headless and reproducible: no extensions, MCP, themes or templates of the owner's
/// interactive Pi, and no global `AGENTS.md`. The project's own `AGENTS.md` is appended, as
/// Claude reads only project settings. Skills: only the lent ones. A stand-in's one call has
/// no tools, no stored conversation and only its own words. `extension` is loaded with `-e`
/// (an explicit path still loads under `--no-extensions`). Conversations live in `owner`'s
/// `sessions/wsx`, since the launch's own folder goes away with it.
fn pi_args(
    launch: &Launch,
    session: &str,
    extension: Option<&Path>,
    owner: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--mode",
        "rpc",
        "--model",
        &launch.model,
        "--no-extensions",
        "--no-mcp",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--no-skills",
        "--no-approve",
    ]
    .map(Into::into)
    .to_vec();
    if let Some(path) = extension {
        args.extend(["-e".into(), path.into()]);
    }
    // ^ W1 hook: a tier 3 agent adds `--no-tools` here as a stand-in does, once `tier` is on
    // the registry entry (docs/design/local-model-fallback.md 3.4, 8).
    // ^ A small model read Pi's coding prompt and said it could only work with files.
    if launch.minimal {
        args.extend(["--no-session", "--no-tools", "--thinking", "off"].map(Into::into));
        args.extend(["--system-prompt".into(), (&launch.instructions).into()]);
        return args;
    }
    args.extend(["--session-dir".into(), owner.join("sessions/wsx").into()]);
    args.extend(["--session-id".into(), session.into()]);
    if !launch.effort.is_empty() {
        args.extend(["--thinking".into(), (&launch.effort).into()]);
    }
    args.extend([
        "--append-system-prompt".into(),
        (&launch.instructions).into(),
    ]);
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let file = launch.cwd.join(name);
        if file.is_file() {
            args.extend(["--append-system-prompt".into(), file.into_os_string()]);
            break;
        }
    }
    // ^ A Claude skills plugin holds the owner's skills folder as `skills`.
    for dir in &launch.plugin_dirs {
        args.extend(["--skill".into(), dir.join("skills").into_os_string()]);
    }
    args
}

/// Read one process's stdout to its end, handing each parsed line to `send` (false stops).
/// The reader thread and `stream_tests.rs` both run this.
pub(super) fn read_stream(
    stdout: impl Read,
    cost_base: u64,
    mut send: impl FnMut(AgentLine) -> bool,
) -> std::io::Result<()> {
    let mut run = Run {
        cost_base,
        ..Run::default()
    };
    read_json_lines(stdout, |value| run.parse(&value).is_none_or(&mut send))
}

/// What the reader keeps over one run, from `agent_start` to `agent_settled`.
#[derive(Default)]
struct Run {
    /// The session's cost before this run, in micro USD.
    cost_base: u64,
    started: Option<Instant>,
    usage: TurnUsage,
    /// The run's assistant text blocks in order; the last message's stop reason and error.
    said: Vec<String>,
    /// The characters of the run's assistant thinking.
    thought: usize,
    stop: String,
    error: String,
}

impl Run {
    fn parse(&mut self, value: &Value) -> Option<AgentLine> {
        match value.get("type")?.as_str()? {
            "agent_start" if self.started.is_none() => {
                self.started = Some(Instant::now());
                None
            }
            "message_end" if value.pointer("/message/role")? == "assistant" => {
                let message = value.get("message")?;
                self.note_message(message);
                message
                    .get("content")?
                    .as_array()?
                    .iter()
                    .rev()
                    .find_map(progress_text)
                    .map(|(text, said)| AgentLine::Progress { text, said })
            }
            "tool_execution_start" => {
                let name = value.get("toolName")?.as_str()?;
                let text = match value.get("args").and_then(key_argument) {
                    Some(argument) => format!("{name} {}", clip(&argument, PROGRESS_CHARS)),
                    None => name.to_string(),
                };
                Some(AgentLine::Progress { text, said: false })
            }
            "response" if value["success"] == false => Some(AgentLine::Progress {
                text: format!(
                    "pi refused {}: {}",
                    value["command"].as_str().unwrap_or("a command"),
                    value["error"].as_str().unwrap_or("no reason")
                ),
                said: false,
            }),
            "agent_settled" => Some(AgentLine::Result(Box::new(self.finish(value)))),
            _ => None,
        }
    }

    fn note_message(&mut self, message: &Value) {
        let count = |name: &str| {
            message
                .pointer(&format!("/usage/{name}"))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let usage = &mut self.usage;
        usage.input_tokens += count("input");
        usage.output_tokens += count("output");
        usage.cache_read_tokens += count("cacheRead");
        usage.cache_creation_tokens += count("cacheWrite");
        usage.context_tokens = count("input") + count("cacheRead") + count("cacheWrite");
        usage.calls += 1;
        let cost = message
            .pointer("/usage/cost/total")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map_or(0, |cost| (cost * 1_000_000.0).round() as u64);
        usage.cost_micro_usd += cost;
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            usage.model = model.to_string();
        }
        self.said.extend(
            message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "text")
                .filter_map(|item| item["text"].as_str())
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
        );
        self.thought += message
            .get("content")
            .and_then(Value::as_array)
            .map_or(0, |content| thinking_chars(content));
        self.stop = message["stopReason"].as_str().unwrap_or_default().into();
        self.error = message["errorMessage"].as_str().unwrap_or_default().into();
    }

    /// One settled run is one turn result.
    fn finish(&mut self, settled: &Value) -> TurnResult {
        let run = std::mem::take(self);
        self.cost_base = run.cost_base + run.usage.cost_micro_usd;
        let aborted = settled["aborted"] == true || run.stop == "aborted";
        let failed = run.stop == "error";
        let mut usage = run.usage;
        usage.duration_ms = run
            .started
            .map_or(0, |started| started.elapsed().as_millis() as u64);
        usage.session_cost_micro_usd = self.cost_base;
        usage.adapter = "pi".into();
        TurnResult {
            is_error: aborted || failed,
            subtype: match (aborted, failed) {
                (true, _) => "aborted".into(),
                (_, true) => "error".into(),
                _ => "success".into(),
            },
            limit: failed.then(|| refusal(&run.error)).flatten(),
            text: if failed {
                run.error
            } else {
                turn_text(&run.said, String::new())
            },
            structured: None,
            denials: Vec::new(),
            usage,
            background: 0,
            thought: run.thought,
        }
    }
}

/// How long an account without credit, without a valid login or without the model counts as
/// limited: nothing tells when the owner fixes it, so a later turn tries again.
const UNAVAILABLE_SECS: u64 = 6 * 3600;

/// A provider's refusal after Pi's own retries, which the host waits out or moves away from.
/// A rate limit or an unreachable server backs off; missing credit, a refused login and an
/// unknown model will not pass soon. ref: docs/tasks.md "Pi adapter"
fn refusal(error: &str) -> Option<UsageLimit> {
    let error = error.to_ascii_lowercase();
    let says = |words: &[&str]| words.iter().any(|word| error.contains(word));
    if says(&[
        "credit balance",
        "insufficient credit",
        "insufficient_quota",
        "billing",
        "authentication",
        "invalid x-api-key",
        "no api key",
        "permission_error",
        "not_found_error",
        "model_not_found",
        "model not found",
        "unknown model",
    ]) || error.starts_with("401")
        || error.starts_with("403")
    {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        return Some(UsageLimit {
            resets_at: Some(now + UNAVAILABLE_SECS),
        });
    }
    says(&[
        "429",
        "rate limit",
        "rate_limit",
        "usage limit",
        "quota",
        "econnrefused",
        "fetch failed",
        "connection error",
    ])
    .then_some(UsageLimit { resets_at: None })
}

fn key_argument(input: &Value) -> Option<String> {
    ["command", "path", "file_path", "pattern", "url"]
        .iter()
        .find_map(|key| input.get(key)?.as_str().map(str::to_string))
}

fn progress_text(item: &Value) -> Option<(String, bool)> {
    match item.get("type")?.as_str()? {
        "text" => {
            let text = item.get("text")?.as_str()?.trim();
            (!text.is_empty()).then(|| (clip(text, PROGRESS_CHARS), true))
        }
        _ => None,
    }
}
