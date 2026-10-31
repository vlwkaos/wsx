//! Pi adapter: one `pi --mode rpc` process, JSONL commands on stdin, session events on stdout.
//! ref: docs/tasks.md "Pi adapter"; Pi docs rpc.md, rpc-commands.md, json.md

use super::agent::{AgentLine, Launch, TurnResult, UsageLimit};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use wsx_core::task::view::clip;
use wsx_core::task::TurnUsage;

/// Longest stdout line the host reads; a longer one (a huge tool result) is skipped.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
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
    "PI_CODING_AGENT_DIR",
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
    child: Child,
    stdin: ChildStdin,
    requests: u64,
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
        let mut command = launch.boundary.command("pi")?;
        command.args(pi_args(launch, session));
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
        command.env("GIT_EDITOR", "true");
        let mut child = command
            .current_dir(&launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("cannot start pi; is it on PATH?")?;
        let stdin = child.stdin.take().context("pi has no stdin")?;
        let stdout = child.stdout.take().context("pi has no stdout")?;
        let init = session.to_string();
        std::thread::spawn(move || {
            if events.send(wrap(Some(AgentLine::Init(init)))).is_err() {
                return;
            }
            let mut reader = BufReader::new(stdout);
            let mut run = Run {
                cost_base,
                ..Run::default()
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
                        if let Some(parsed) = run.parse(&value) {
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
        let mut line = serde_json::to_vec(&value)?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
            .context("cannot write to pi")
    }

    /// Abort the run, close stdin (Pi's orderly shutdown), then kill if it does not end
    /// within a few seconds.
    pub fn stop(mut self) {
        let _ = self.request(json!({ "type": "abort" }));
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Headless and reproducible: no extensions, MCP, themes or templates of the owner's
/// interactive Pi, and no global `AGENTS.md`. The project's own `AGENTS.md` is appended, as
/// Claude reads only project settings. Skills: only the lent ones.
fn pi_args(launch: &Launch, session: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = [
        "--mode",
        "rpc",
        "--session-id",
        session,
        "--model",
        &launch.model,
        "--no-extensions",
        "--no-mcp",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--no-skills",
        "--no-approve",
        "--append-system-prompt",
        &launch.instructions,
    ]
    .map(Into::into)
    .to_vec();
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

/// Drop the rest of an oversized line one buffer at a time, never holding it.
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

/// What the reader keeps over one run, from `agent_start` to `agent_settled`.
#[derive(Default)]
struct Run {
    /// The session's cost before this run, in micro USD.
    cost_base: u64,
    started: Option<Instant>,
    usage: TurnUsage,
    /// The last assistant message: its text, stop reason and error.
    text: String,
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
        self.text = message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "text")
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
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
        TurnResult {
            is_error: aborted || failed,
            subtype: match (aborted, failed) {
                (true, _) => "aborted".into(),
                (_, true) => "error".into(),
                _ => "success".into(),
            },
            limit: (failed && is_usage_limit(&run.error)).then_some(UsageLimit { resets_at: None }),
            text: if failed { run.error } else { run.text },
            structured: None,
            denials: Vec::new(),
            usage,
            background: 0,
        }
    }
}

/// A provider's refusal for a limit, after Pi's own retries: the host waits or switches
/// agents. Pi reports no reset time, so the host backs off. ref: docs/tasks.md "Usage limit"
fn is_usage_limit(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "429",
        "rate limit",
        "rate_limit",
        "usage limit",
        "quota",
        "insufficient credit",
    ]
    .iter()
    .any(|word| error.contains(word))
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
