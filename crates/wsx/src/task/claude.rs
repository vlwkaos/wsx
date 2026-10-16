//! Claude worker adapter: one `claude -p` process with stream JSON on stdin and stdout.
//! ref: docs/tasks.md "Claude adapter"; knowledge "Headless Worker Transport"

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
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

pub struct ClaudeAgent {
    child: Child,
    stdin: ChildStdin,
}

fn worker_instructions(record: &TaskRecord) -> String {
    format!(
        "You are a task worker in a separate working copy on branch {}. \
         Nobody reads your terminal: work without asking questions. \
         Commit your changes with git before you finish. \
         When you cannot continue, return status blocked and say what you need.",
        record.branch
    )
}

impl ClaudeAgent {
    /// Start the agent. `resume` names a conversation that already exists;
    /// otherwise `session` becomes the new conversation's ID.
    pub fn spawn<E: Send + 'static>(
        record: &TaskRecord,
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
                "default",
                "--model",
                &record.model,
                "--json-schema",
                &worker_result_schema().to_string(),
                "--append-system-prompt",
                &worker_instructions(record),
                // ^ Worker commits carry no AI attribution; `attribution` replaced
                // `includeCoAuthoredBy`, which older builds still read.
                "--settings",
                r#"{"attribution":{"commit":"","pr":""},"includeCoAuthoredBy":false}"#,
            ])
            .args(if resume {
                ["--resume", session]
            } else {
                ["--session-id", session]
            });
        if !record.tools.is_empty() {
            command.arg("--allowedTools").args(&record.tools);
        }
        command.env_clear();
        for name in KEPT_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .current_dir(&record.working_copy)
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
