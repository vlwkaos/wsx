//! Herdr protocol 20 process adapter.
//!
//! There is intentionally no tmux fallback.

mod client;
mod events;

pub use client::Client;
pub use events::{EventMonitor, EventSignal};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_COMMAND_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_RECORDS: usize = 100_000;
const MAX_ID_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 4 * 1024;
const MAX_READ_LINES: u32 = 10_000;
const MIN_VERSION: HerdrVersion = HerdrVersion {
    major: 0,
    minor: 8,
    patch: 2,
};

/// Herdr metadata tokens as represented by the protocol's token object.
pub type MetadataTokens = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticReport {
    pub binary: String,
    pub client_version: Option<String>,
    pub client_error: Option<String>,
    pub server: Option<Value>,
    pub server_error: Option<String>,
    pub integration_notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HerdrVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for HerdrVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

/// The protocol-20 `session_snapshot` payload.  Herdr keeps these collections
/// flat; `workspace_id` and `tab_id` are the associations between them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Snapshot {
    pub version: String,
    pub protocol: u64,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
    /// Preserved protocol layout records.  wsx does not interpret layouts.
    pub layouts: Vec<Value>,
    /// Preserved protocol agent records.  wsx uses `Pane::agent_status`.
    pub agents: Vec<Value>,
}

/// Protocol-20 `WorkspaceInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    #[serde(default)]
    pub tokens: MetadataTokens,
}

/// Protocol-20 `TabInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
}

/// Protocol-20 `PaneInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<PathBuf>,
    pub label: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    pub revision: u64,
    #[serde(default)]
    pub tokens: MetadataTokens,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorkspace {
    pub workspace_id: String,
    pub tab_id: String,
    pub root_pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedTab {
    pub tab_id: String,
    pub root_pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerStatus {
    Running { version: String, protocol: u64 },
    NotRunning,
}

struct ServerStartLock(File);

impl ServerStartLock {
    fn acquire() -> Result<Self> {
        let dir = dirs::cache_dir()
            .ok_or_else(|| anyhow!("could not resolve cache directory"))?
            .join("wsx");
        std::fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("herdr-start.lock"))?;
        let deadline = Instant::now() + SERVER_READY_TIMEOUT;
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error).context("could not lock Herdr server startup");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for another wsx Herdr server startup");
            }
            std::thread::sleep(SERVER_POLL_INTERVAL);
        }
    }
}

impl Drop for ServerStartLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Inspect the installed runtime without starting or enforcing compatibility.
pub fn diagnostics() -> DiagnosticReport {
    let binary = herdr_binary();
    let run = |args: &[OsString], operation: &str| {
        let mut command = herdr_command(args);
        crate::git::output_with_timeout_limit(
            &mut command,
            COMMAND_TIMEOUT,
            MAX_COMMAND_STREAM_BYTES,
        )
        .with_context(|| format!("could not run herdr {operation}"))
    };

    let (client_version, client_error) = match run(&["--version".into()], "--version") {
        Ok(output) if output.status.success() => (
            String::from_utf8(output.stdout)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            None,
        ),
        Ok(output) => (None, Some(command_failure_message(&output))),
        Err(error) => (None, Some(error.to_string())),
    };
    let (server, server_error) = match run(
        &["status".into(), "server".into(), "--json".into()],
        "status server --json",
    ) {
        Ok(output) if output.status.success() => match serde_json::from_slice(&output.stdout) {
            Ok(value) => (Some(value), None),
            Err(_) => (
                None,
                Some("Herdr server status returned invalid JSON".into()),
            ),
        },
        Ok(output) => (None, Some(command_failure_message(&output))),
        Err(error) => (None, Some(error.to_string())),
    };
    let integration_notice = run(
        &[
            "integration".into(),
            "status".into(),
            "--outdated-only".into(),
        ],
        "integration status --outdated-only",
    )
    .ok()
    .filter(|output| output.status.success())
    .and_then(|output| {
        let text = String::from_utf8_lossy(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        })
        .trim()
        .to_string();
        (!text.is_empty()).then_some(text)
    });

    DiagnosticReport {
        binary: binary.to_string_lossy().into_owned(),
        client_version,
        client_error,
        server,
        server_error,
        integration_notice,
    }
}

fn command_failure_message(output: &Output) -> String {
    String::from_utf8_lossy(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    })
    .lines()
    .find(|line| !line.trim().is_empty())
    .unwrap_or("Herdr command failed")
    .trim()
    .chars()
    .take(300)
    .collect()
}

/// Return the installed Herdr version after enforcing the v0.8.2 minimum.
pub fn version() -> Result<HerdrVersion> {
    let output = run_ok(&["--version".into()], "--version")?;
    let stdout =
        String::from_utf8(output.stdout).context("herdr --version returned non-UTF-8 output")?;
    let version = parse_version(&stdout).ok_or_else(|| anyhow!("could not parse herdr version"))?;
    if version < MIN_VERSION {
        bail!("herdr {version} is too old; require herdr 0.8.2+");
    }
    Ok(version)
}

/// Whether a usable Herdr v0.8.2+ protocol-20 server is already available.
pub fn is_available() -> bool {
    check_available().is_ok()
}

/// Validate both the executable version and an already-running server.
pub fn check_available() -> Result<HerdrVersion> {
    let version = version()?;
    validate_running_server(server_status()?)?;
    snapshot_with(&Client::local()?)?;
    Ok(version)
}

/// Start Herdr when it is explicitly stopped, then validate protocol 20.
// ^ [[Herdr Runtime]] Distribution, trust, protocol, and capability boundary.
pub fn ensure_available() -> Result<HerdrVersion> {
    let version = version()?;
    if matches!(server_status()?, ServerStatus::NotRunning) {
        let _lock = ServerStartLock::acquire()?;
        if matches!(server_status()?, ServerStatus::NotRunning) {
            await_server_start(spawn_server()?, SERVER_READY_TIMEOUT)?;
        }
    }
    validate_running_server(server_status()?)?;
    snapshot_with(&Client::local()?)?;
    Ok(version)
}

/// Request a snapshot over the local protocol socket without spawning a client process.
pub fn snapshot_with(client: &Client) -> Result<Snapshot> {
    let result = client.request("session.snapshot", serde_json::json!({}))?;
    let response = result_object(&result, "session.snapshot result")?;
    if required_str(response, "type", "snapshot type")? != "session_snapshot" {
        bail!("Herdr session.snapshot returned an unexpected response type");
    }
    parse_snapshot(
        response
            .get("snapshot")
            .ok_or_else(|| anyhow!("Herdr session.snapshot result has no snapshot"))?,
    )
}

/// Create a workspace and its first tab/root pane over the local protocol socket.
pub fn create_workspace_with(client: &Client, cwd: &Path, label: &str) -> Result<CreatedWorkspace> {
    validate_path(cwd, "workspace cwd")?;
    validate_text(label, "workspace label")?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("workspace cwd is not valid UTF-8"))?;
    let result = client.request(
        "workspace.create",
        serde_json::json!({"cwd": cwd, "focus": false, "label": label}),
    )?;
    let result = result_object(&result, "workspace.create result")?;
    if required_str(result, "type", "workspace.create result type")? != "workspace_created" {
        bail!("Herdr workspace.create returned an unexpected response type");
    }
    let workspace = required_object(result, "workspace", "workspace.create result")?;
    let tab = required_object(result, "tab", "workspace.create result")?;
    let root_pane = required_object(result, "root_pane", "workspace.create result")?;
    Ok(CreatedWorkspace {
        workspace_id: required_id(workspace, "workspace_id", "created workspace id")?,
        tab_id: required_id(tab, "tab_id", "created tab id")?,
        root_pane_id: required_id(root_pane, "pane_id", "created root pane id")?,
    })
}

/// Create a tab in an existing workspace over the local protocol socket.
pub fn create_tab_with(
    client: &Client,
    workspace_id: &str,
    cwd: &Path,
    label: &str,
) -> Result<CreatedTab> {
    validate_id(workspace_id, "workspace")?;
    validate_path(cwd, "tab cwd")?;
    validate_text(label, "tab label")?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow!("tab cwd is not valid UTF-8"))?;
    let result = client.request(
        "tab.create",
        serde_json::json!({
            "workspace_id": workspace_id,
            "cwd": cwd,
            "focus": false,
            "label": label
        }),
    )?;
    let result = result_object(&result, "tab.create result")?;
    if required_str(result, "type", "tab.create result type")? != "tab_created" {
        bail!("Herdr tab.create returned an unexpected response type");
    }
    let tab = required_object(result, "tab", "tab.create result")?;
    let root_pane = required_object(result, "root_pane", "tab.create result")?;
    Ok(CreatedTab {
        tab_id: required_id(tab, "tab_id", "created tab id")?,
        root_pane_id: required_id(root_pane, "pane_id", "created root pane id")?,
    })
}

pub fn rename_pane_with(client: &Client, pane_id: &str, label: &str) -> Result<()> {
    validate_id(pane_id, "pane")?;
    validate_text(label, "pane label")?;
    require_ok_result(
        &client.request(
            "pane.rename",
            serde_json::json!({"pane_id": pane_id, "label": label}),
        )?,
        "pane.rename",
    )
}

pub fn close_pane_with(client: &Client, pane_id: &str) -> Result<()> {
    validate_id(pane_id, "pane")?;
    require_ok_result(
        &client.request("pane.close", serde_json::json!({"pane_id": pane_id}))?,
        "pane.close",
    )
}

pub fn close_workspace_with(client: &Client, workspace_id: &str) -> Result<()> {
    validate_id(workspace_id, "workspace")?;
    require_ok_result(
        &client.request(
            "workspace.close",
            serde_json::json!({"workspace_id": workspace_id}),
        )?,
        "workspace.close",
    )
}

fn require_ok_result(result: &Value, operation: &str) -> Result<()> {
    let result = result_object(result, &format!("{operation} result"))?;
    if required_str(result, "type", &format!("{operation} result type"))? != "ok" {
        bail!("Herdr {operation} returned an unexpected response type");
    }
    Ok(())
}

/// Send literal text and, when requested, a separate Enter key.
pub fn send_text_with(client: &Client, pane_id: &str, text: &str, enter: bool) -> Result<()> {
    validate_id(pane_id, "pane")?;
    if text.is_empty() {
        bail!("pane text must not be empty");
    }
    client.request(
        "pane.send_text",
        serde_json::json!({"pane_id": pane_id, "text": text}),
    )?;
    if enter {
        client.request(
            "pane.send_keys",
            serde_json::json!({"pane_id": pane_id, "keys": ["enter"]}),
        )?;
    }
    Ok(())
}

pub fn agent_prompt_with(client: &Client, pane_id: &str, text: &str) -> Result<()> {
    validate_id(pane_id, "pane")?;
    validate_text(text, "agent prompt")?;
    client.request(
        "agent.prompt",
        serde_json::json!({"target": pane_id, "text": text}),
    )?;
    Ok(())
}

pub fn send_ctrl_c_with(client: &Client, pane_id: &str) -> Result<()> {
    validate_id(pane_id, "pane")?;
    client.request(
        "pane.send_keys",
        serde_json::json!({"pane_id": pane_id, "keys": ["ctrl+c"]}),
    )?;
    Ok(())
}

/// Read recent ANSI-preserved terminal output exactly as emitted by Herdr.
pub fn read_recent_ansi_with(client: &Client, pane_id: &str, lines: u32) -> Result<String> {
    validate_read(pane_id, lines)?;
    let result = client.request(
        "pane.read",
        serde_json::json!({
            "pane_id": pane_id,
            "source": "recent",
            "lines": lines,
            "format": "ansi",
            "strip_ansi": false
        }),
    )?;
    result
        .pointer("/read/text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Herdr pane.read result has no text"))
}

pub fn agent_read_with(client: &Client, pane_id: &str, lines: u32) -> Result<String> {
    validate_read(pane_id, lines)?;
    let result = client.request(
        "agent.read",
        serde_json::json!({
            "target": pane_id,
            "source": "recent_unwrapped",
            "lines": lines,
            "format": "text",
            "strip_ansi": true
        }),
    )?;
    result
        .pointer("/read/text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("Herdr agent.read result has no text"))
}

fn validate_read(pane_id: &str, lines: u32) -> Result<()> {
    validate_id(pane_id, "pane")?;
    if lines == 0 || lines > MAX_READ_LINES {
        bail!("read line count must be between 1 and {MAX_READ_LINES}");
    }
    Ok(())
}

/// Attach a terminal in the foreground with all three standard streams inherited.
pub fn attach_terminal_foreground(terminal_id: &str) -> Result<()> {
    validate_id(terminal_id, "terminal")?;
    let args: [OsString; 3] = ["terminal".into(), "attach".into(), terminal_id.into()];
    let status = Command::new(herdr_binary())
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("could not start herdr terminal attach")?;
    if !status.success() {
        bail!("herdr terminal attach failed");
    }
    Ok(())
}

fn api_socket_path() -> Result<PathBuf> {
    static CACHE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        if !path.is_absolute() {
            bail!("HERDR_SOCKET_PATH must be absolute");
        }
        return Ok(path);
    }
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some(path) = cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
    {
        return Ok(path);
    }
    let output = run_ok(
        &["status".into(), "server".into(), "--json".into()],
        "status server --json",
    )?;
    let stdout = String::from_utf8(output.stdout)
        .context("herdr status server --json returned non-UTF-8 output")?;
    let value: Value = serde_json::from_str(&stdout)
        .map_err(|_| anyhow!("herdr status server --json returned invalid JSON"))?;
    if value.get("running").and_then(Value::as_bool) != Some(true) {
        bail!("Herdr server is not running");
    }
    let path = value
        .get("socket")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| anyhow!("Herdr server status has no absolute socket path"))?;
    *cache.lock().unwrap_or_else(|error| error.into_inner()) = Some(path.clone());
    Ok(path)
}

fn server_status() -> Result<ServerStatus> {
    let output = run_ok(
        &["status".into(), "server".into(), "--json".into()],
        "status server --json",
    )?;
    let stdout = String::from_utf8(output.stdout)
        .context("herdr status server --json returned non-UTF-8 output")?;
    parse_server_status(&stdout)
}

fn parse_server_status(stdout: &str) -> Result<ServerStatus> {
    let value: Value = serde_json::from_str(stdout)
        .map_err(|_| anyhow!("herdr status server --json returned invalid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("herdr server status is malformed"))?;
    let running = object
        .get("running")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("herdr server status has no running flag"))?;
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("herdr server status has no status field"))?;
    match (running, status) {
        (false, "not_running") => Ok(ServerStatus::NotRunning),
        (true, "running") => {
            let version = object
                .get("version")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("running Herdr server status has no version"))?;
            validate_label(version, "Herdr server version")?;
            let protocol = object
                .get("protocol")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("running Herdr server status has no protocol"))?;
            Ok(ServerStatus::Running {
                version: version.to_owned(),
                protocol,
            })
        }
        _ => bail!("herdr server status is inconsistent"),
    }
}

fn validate_running_server(status: ServerStatus) -> Result<()> {
    let ServerStatus::Running { version, protocol } = status else {
        bail!("Herdr server is not running");
    };
    let parsed_version = parse_version(&version)
        .ok_or_else(|| anyhow!("could not parse running Herdr server version"))?;
    if parsed_version < MIN_VERSION {
        bail!("running Herdr {parsed_version} is too old; require Herdr 0.8.2+");
    }
    if protocol != 20 {
        bail!("running Herdr {version} uses unsupported protocol {protocol}; require protocol 20");
    }
    Ok(())
}

fn spawn_server() -> Result<Child> {
    let mut command = Command::new(herdr_binary());
    command
        .arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and does not access parent memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command
        .spawn()
        .context("could not start headless Herdr server")
}

fn reap_server_on_exit(mut child: Child) -> Result<()> {
    std::thread::Builder::new()
        .name("wsx-herdr-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("could not start Herdr server process reaper")?;
    Ok(())
}

fn await_server_start(mut child: Child, timeout: Duration) -> Result<()> {
    if let Err(error) = wait_for_server_with_timeout(&mut child, timeout) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    reap_server_on_exit(child)
}

fn wait_for_server_with_timeout(child: &mut Child, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("could not inspect headless Herdr server startup")?
        {
            bail!("headless Herdr server exited before readiness with {status}");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for headless Herdr server readiness");
        }
        match server_status() {
            Ok(ServerStatus::Running { version, protocol }) => {
                return validate_running_server(ServerStatus::Running { version, protocol });
            }
            Ok(ServerStatus::NotRunning) => {}
            Err(error) => return Err(error).context("could not inspect Herdr server readiness"),
        }
        std::thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn parse_snapshot(value: &Value) -> Result<Snapshot> {
    let snapshot: Snapshot = serde_json::from_value(value.clone())
        .map_err(|_| anyhow!("herdr session snapshot is malformed"))?;
    if snapshot.protocol != 20 {
        bail!(
            "unsupported Herdr protocol {}; require protocol 20",
            snapshot.protocol
        );
    }
    let record_count = snapshot
        .workspaces
        .len()
        .saturating_add(snapshot.tabs.len())
        .saturating_add(snapshot.panes.len())
        .saturating_add(snapshot.layouts.len())
        .saturating_add(snapshot.agents.len());
    if record_count > MAX_SNAPSHOT_RECORDS {
        bail!("herdr session snapshot has too many records");
    }

    let mut workspace_ids = HashSet::new();
    for workspace in &snapshot.workspaces {
        validate_id(&workspace.workspace_id, "workspace id")?;
        validate_label(&workspace.label, "workspace label")?;
        if !workspace_ids.insert(workspace.workspace_id.as_str()) {
            bail!("herdr session snapshot has duplicate workspace ids");
        }
    }

    let mut tab_workspaces = HashMap::new();
    for tab in &snapshot.tabs {
        validate_id(&tab.tab_id, "tab id")?;
        validate_id(&tab.workspace_id, "tab workspace id")?;
        validate_label(&tab.label, "tab label")?;
        if !workspace_ids.contains(tab.workspace_id.as_str()) {
            bail!("herdr session snapshot tab references an unknown workspace");
        }
        if tab_workspaces
            .insert(tab.tab_id.as_str(), tab.workspace_id.as_str())
            .is_some()
        {
            bail!("herdr session snapshot has duplicate tab ids");
        }
    }

    let mut pane_ids = HashSet::new();
    let mut terminal_ids = HashSet::new();
    for pane in &snapshot.panes {
        validate_id(&pane.pane_id, "pane id")?;
        validate_id(&pane.terminal_id, "pane terminal id")?;
        validate_id(&pane.workspace_id, "pane workspace id")?;
        validate_id(&pane.tab_id, "pane tab id")?;
        if !pane_ids.insert(pane.pane_id.as_str()) {
            bail!("herdr session snapshot has duplicate pane ids");
        }
        if !terminal_ids.insert(pane.terminal_id.as_str()) {
            bail!("herdr session snapshot has duplicate terminal ids");
        }
        let Some(tab_workspace_id) = tab_workspaces.get(pane.tab_id.as_str()) else {
            bail!("herdr session snapshot pane references an unknown tab");
        };
        if *tab_workspace_id != pane.workspace_id {
            bail!("herdr session snapshot pane and tab reference different workspaces");
        }
        if let Some(label) = &pane.label {
            validate_label(label, "pane label")?;
        }
        if let Some(cwd) = &pane.cwd {
            validate_path(cwd, "pane cwd")?;
        }
    }
    Ok(snapshot)
}

fn required_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    object
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{owner} has no {field}"))
}

fn result_object<'a>(value: &'a Value, owner: &str) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| anyhow!("herdr {owner} is malformed"))
}

fn required_str<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<&'a str> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{owner} is missing or invalid"))
}

fn required_id(
    object: &serde_json::Map<String, Value>,
    field: &str,
    owner: &str,
) -> Result<String> {
    let value = required_str(object, field, owner)?;
    validate_id(value, owner)?;
    Ok(value.to_owned())
}

fn validate_id(value: &str, owner: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{owner} must not be empty");
    }
    if value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        bail!("{owner} is invalid");
    }
    Ok(())
}

fn validate_text(value: &str, owner: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{owner} must not be empty");
    }
    validate_label(value, owner)
}

fn validate_label(value: &str, owner: &str) -> Result<()> {
    if value.len() > MAX_LABEL_BYTES || value.chars().any(char::is_control) {
        bail!("{owner} is invalid");
    }
    Ok(())
}

fn validate_path(path: &Path, owner: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{owner} must not be empty");
    }
    Ok(())
}

fn herdr_binary() -> OsString {
    resolve_herdr_binary(
        std::env::var_os("WSX_HERDR_BIN"),
        std::env::current_exe().ok().as_deref(),
        Path::is_file,
    )
}

fn resolve_herdr_binary(
    override_bin: Option<OsString>,
    current_exe: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> OsString {
    if let Some(override_bin) = override_bin.filter(|value| !value.is_empty()) {
        return override_bin;
    }
    if let Some(parent) = current_exe.and_then(Path::parent) {
        let adjacent = parent.join(format!("herdr{}", std::env::consts::EXE_SUFFIX));
        if is_file(&adjacent) {
            return adjacent.into_os_string();
        }
    }
    OsString::from("herdr")
}

fn herdr_command(args: &[OsString]) -> Command {
    let mut command = Command::new(herdr_binary());
    command.args(args).stdin(Stdio::null());
    command
}

fn run_ok(args: &[OsString], operation: &str) -> Result<Output> {
    let mut command = herdr_command(args);
    let output = crate::git::output_with_timeout_limit(
        &mut command,
        COMMAND_TIMEOUT,
        MAX_COMMAND_STREAM_BYTES,
    )
    .with_context(|| format!("could not run herdr {operation}"))?;
    if output.status.success() {
        return Ok(output);
    }
    let message = String::from_utf8_lossy(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    })
    .lines()
    .find(|line| !line.trim().is_empty())
    .unwrap_or("herdr command failed")
    .trim()
    .chars()
    .take(200)
    .collect::<String>();
    bail!("herdr {operation} failed: {message}")
}

/// Parse the protocol command envelope and return its exact `result` member.
fn parse_version(text: &str) -> Option<HerdrVersion> {
    for word in text.split(|c: char| c.is_whitespace() || c == ',') {
        let mut parts = word.trim_start_matches('v').split('.');
        let (Some(major), Some(minor), Some(patch)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(major), Ok(minor)) = (major.parse(), minor.parse()) else {
            continue;
        };
        let patch = patch
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let Ok(patch) = patch.parse() else { continue };
        return Some(HerdrVersion {
            major,
            minor,
            patch,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn binary_resolution_prefers_override_then_adjacent_then_path() {
        let current = Path::new("/opt/wsx/bin/wsx");
        let adjacent =
            Path::new("/opt/wsx/bin").join(format!("herdr{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(
            resolve_herdr_binary(Some("/trusted/herdr".into()), Some(current), |_| true),
            OsString::from("/trusted/herdr")
        );
        assert_eq!(
            resolve_herdr_binary(None, Some(current), |path| path == adjacent),
            adjacent.as_os_str()
        );
        assert_eq!(
            resolve_herdr_binary(Some(OsString::new()), Some(current), |_| false),
            OsString::from("herdr")
        );
    }

    #[test]
    fn server_status_requires_consistent_explicit_state() {
        assert_eq!(
            parse_server_status(r#"{"status":"not_running","running":false}"#).unwrap(),
            ServerStatus::NotRunning
        );
        assert_eq!(
            parse_server_status(
                r#"{"status":"running","running":true,"version":"0.8.2","protocol":20}"#
            )
            .unwrap(),
            ServerStatus::Running {
                version: "0.8.2".into(),
                protocol: 20,
            }
        );
        assert!(parse_server_status(r#"{"status":"running","running":false}"#).is_err());
        assert!(parse_server_status(r#"{"status":"running","running":true}"#).is_err());
    }

    #[test]
    fn incompatible_running_server_is_not_accepted() {
        let error = validate_running_server(ServerStatus::Running {
            version: "0.9.0".into(),
            protocol: 21,
        })
        .unwrap_err();
        assert!(error.to_string().contains("unsupported protocol 21"));
        assert!(validate_running_server(ServerStatus::Running {
            version: "0.8.1".into(),
            protocol: 20,
        })
        .unwrap_err()
        .to_string()
        .contains("too old"));
        assert!(validate_running_server(ServerStatus::NotRunning).is_err());
    }

    #[test]
    fn failed_server_readiness_is_bounded_killed_and_reaped() {
        let child = Command::new("sh")
            .args(["-c", "sleep 10"])
            .spawn()
            .expect("spawn bounded readiness fixture");
        let pid = child.id() as libc::pid_t;
        let error = await_server_start(child, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    fn snapshot_preserves_flat_protocol_associations() {
        let snapshot = parse_snapshot(&serde_json::json!({
            "version": "0.8.2",
            "protocol": 20,
            "workspaces": [{"workspace_id": "ws-1", "label": "main"}],
            "tabs": [{"tab_id": "tab-1", "workspace_id": "ws-1", "label": "agent"}],
            "panes": [{
                "pane_id": "pane-1",
                "terminal_id": "term-1",
                "workspace_id": "ws-1",
                "tab_id": "tab-1",
                "cwd": "/work",
                "label": null,
                "agent_status": "working",
                "revision": 4
            }],
            "layouts": [],
            "agents": []
        }))
        .unwrap();
        assert_eq!(snapshot.version, "0.8.2");
        assert_eq!(snapshot.protocol, 20);
        assert_eq!(snapshot.workspaces[0].workspace_id, "ws-1");
        assert_eq!(snapshot.tabs[0].workspace_id, "ws-1");
        assert_eq!(snapshot.panes[0].tab_id, "tab-1");
        assert_eq!(snapshot.panes[0].cwd, Some(PathBuf::from("/work")));
        assert_eq!(snapshot.panes[0].revision, 4);
    }

    #[test]
    fn protocol_snapshot_requires_string_version_and_protocol_20() {
        let numeric_version = serde_json::json!({
            "version": 12, "protocol": 20, "workspaces": [], "tabs": [],
            "panes": [], "layouts": [], "agents": []
        });
        assert!(parse_snapshot(&numeric_version).is_err());

        let wrong_protocol = serde_json::json!({
            "version": "0.8.2", "protocol": 19, "workspaces": [], "tabs": [],
            "panes": [], "layouts": [], "agents": []
        });
        assert!(parse_snapshot(&wrong_protocol)
            .unwrap_err()
            .to_string()
            .contains("protocol 20"));
    }

    #[test]
    fn snapshot_rejects_duplicate_ids_and_broken_references() {
        let duplicate_workspaces = serde_json::json!({
            "version": "0.8.2", "protocol": 20,
            "workspaces": [
                {"workspace_id": "same", "label": "one"},
                {"workspace_id": "same", "label": "two"}
            ],
            "tabs": [], "panes": [], "layouts": [], "agents": []
        });
        assert!(parse_snapshot(&duplicate_workspaces)
            .unwrap_err()
            .to_string()
            .contains("duplicate workspace"));

        let unknown_tab = serde_json::json!({
            "version": "0.8.2", "protocol": 20,
            "workspaces": [{"workspace_id": "ws", "label": "one"}],
            "tabs": [],
            "panes": [{
                "pane_id": "pane", "terminal_id": "terminal",
                "workspace_id": "ws", "tab_id": "missing", "cwd": "/work",
                "label": null, "agent_status": "idle", "revision": 1
            }],
            "layouts": [], "agents": []
        });
        assert!(parse_snapshot(&unknown_tab)
            .unwrap_err()
            .to_string()
            .contains("unknown tab"));

        let duplicate_terminals = serde_json::json!({
            "version": "0.8.2", "protocol": 20,
            "workspaces": [{"workspace_id": "ws", "label": "one"}],
            "tabs": [{"tab_id": "tab", "workspace_id": "ws", "label": "one"}],
            "panes": [
                {
                    "pane_id": "pane-1", "terminal_id": "terminal",
                    "workspace_id": "ws", "tab_id": "tab", "cwd": "/work",
                    "label": null, "agent_status": "idle", "revision": 1
                },
                {
                    "pane_id": "pane-2", "terminal_id": "terminal",
                    "workspace_id": "ws", "tab_id": "tab", "cwd": "/work",
                    "label": null, "agent_status": "idle", "revision": 1
                }
            ],
            "layouts": [], "agents": []
        });
        assert!(parse_snapshot(&duplicate_terminals)
            .unwrap_err()
            .to_string()
            .contains("duplicate terminal"));
    }

    #[test]
    fn pane_read_rejects_unbounded_line_counts() {
        assert!(validate_read("pane-1", 0).is_err());
        assert!(validate_read("pane-1", MAX_READ_LINES + 1).is_err());
    }

    #[test]
    fn mutation_requests_use_typed_protocol_shapes() {
        let dir = std::env::current_dir()
            .unwrap()
            .join(".work/tests")
            .join(format!("wsx-herdr-mutations-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("herdr.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let expected = [
                "workspace.create",
                "tab.create",
                "pane.rename",
                "pane.close",
                "workspace.close",
            ];
            for method in expected {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(request["method"], method);
                let params = request["params"].as_object().unwrap();
                let result = match method {
                    "workspace.create" => {
                        assert_eq!(params["cwd"], "/work");
                        assert_eq!(params["label"], "wsx:main");
                        serde_json::json!({
                            "type": "workspace_created",
                            "workspace": {"workspace_id": "w1"},
                            "tab": {"tab_id": "w1:t1"},
                            "root_pane": {"pane_id": "w1:p1"}
                        })
                    }
                    "tab.create" => {
                        assert_eq!(params["workspace_id"], "w1");
                        assert_eq!(params["cwd"], "/work");
                        serde_json::json!({
                            "type": "tab_created",
                            "tab": {"tab_id": "w1:t2"},
                            "root_pane": {"pane_id": "w1:p2"}
                        })
                    }
                    "pane.rename" => {
                        assert_eq!(params["pane_id"], "w1:p2");
                        assert_eq!(params["label"], "agent");
                        serde_json::json!({"type": "ok"})
                    }
                    "pane.close" => {
                        assert_eq!(params["pane_id"], "w1:p2");
                        serde_json::json!({"type": "ok"})
                    }
                    "workspace.close" => {
                        assert_eq!(params["workspace_id"], "w1");
                        serde_json::json!({"type": "ok"})
                    }
                    _ => unreachable!(),
                };
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"id": request["id"], "result": result})
                )
                .unwrap();
            }
        });

        let client = Client::new(path).unwrap();
        let workspace = create_workspace_with(&client, Path::new("/work"), "wsx:main").unwrap();
        assert_eq!(workspace.root_pane_id, "w1:p1");
        let tab = create_tab_with(&client, "w1", Path::new("/work"), "agent").unwrap();
        assert_eq!(tab.root_pane_id, "w1:p2");
        rename_pane_with(&client, "w1:p2", "agent").unwrap();
        close_pane_with(&client, "w1:p2").unwrap();
        close_workspace_with(&client, "w1").unwrap();
        server.join().unwrap();
        let _ = fs::remove_dir_all(dir);
    }
}
