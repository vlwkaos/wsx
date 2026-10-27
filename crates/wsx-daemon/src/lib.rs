//! Persistent wsx authority for project/session structure, PTYs, frames, leases, and plugins.
// ^ [[wsx Architecture]] Snapshots are authoritative; events only invalidate revisions.

#[cfg(unix)]
mod handoff;
mod plugins;
mod reporter;
mod state_store;
#[cfg(any(target_os = "macos", test))]
mod wake;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    ops::{Deref, DerefMut},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wsx_core::{
    config::global::GlobalConfig,
    integration::{claude_status, resume},
    runtime::*,
};
use wsx_terminal::{validate_launch, AgentTerminalObservation, TerminalRuntime};

mod agent_report;
mod changes;
mod dispatch;
mod exchange;
mod lease;
mod live_handoff;
mod recovery;
mod scanners;
mod server;
mod sessions;
mod stream;
mod validate;

use agent_report::*;
use changes::*;
use dispatch::*;
use exchange::*;
use lease::*;
use live_handoff::*;
pub use recovery::run_resume_supervisor;
use recovery::*;
use scanners::*;
use server::*;
pub use server::{run, run_handoff_import, run_routine_daemon};
use sessions::*;
use stream::*;
use validate::*;

const EVENT_LIMIT: usize = 1024;
const PLUGIN_EVENT_LIMIT: usize = 256;
const MAX_CLIENTS: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_VIEW_PANES: usize = 32;
const MAX_VIEW_CELLS: usize = 1_000_000;
const MAX_AGENT_EXCHANGES: usize = 256;
const LEASE_TTL: Duration = Duration::from_secs(3);
const TUI_PRESENCE_TTL: Duration = Duration::from_secs(3);
const AGENT_WAKE_LEASE_TTL: Duration = Duration::from_secs(30 * 60);
const AGENT_PRESENCE_TTL: Duration = Duration::from_secs(30);
const AGENT_PRESENCE_POLL: Duration = Duration::from_secs(1);
const CLAUDE_RECONCILE_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(target_os = "macos")]
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PRESENTATION_CADENCE: Duration = Duration::from_millis(4);
static NEXT_LEASE_GENERATION: AtomicU64 = AtomicU64::new(1);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
const PORT_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const FOREGROUND_SCAN_INTERVAL: Duration = Duration::from_millis(150);
const PORT_SCAN_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_PORT_SCAN_BYTES: u64 = 256 * 1024;
const RESUME_CLEAR_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const RESUME_CLEAR_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const RECOVERY_AGENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const RECOVERY_AGENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HANDOFF_RESTORE_WORKERS: usize = 4;
pub const RESUME_SUPERVISOR_ARG: &str = "__wsx_resume_supervisor";
pub const HANDOFF_IMPORT_ARG: &str = "__wsx_handoff_import";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchRecipe {
    command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
}

struct RecoveryLaunch {
    recipe: LaunchRecipe,
    resume_key: Option<String>,
    provider: Option<String>,
    await_readiness: bool,
}

struct RecoveryAttempt {
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: PathBuf,
    launch: RecoveryLaunch,
}

#[derive(Debug, Clone, Serialize)]
struct PersistedPane {
    #[serde(flatten)]
    pane: Pane,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery: Option<LaunchRecipe>,
    #[serde(default, skip_serializing_if = "is_false")]
    recovery_quarantined: bool,
}
impl<'de> Deserialize<'de> for PersistedPane {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(flatten)]
            pane: Pane,
            #[serde(default)]
            recovery: Option<serde_json::Value>,
            #[serde(default)]
            recovery_quarantined: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        let had_recovery = wire.recovery.is_some();
        let recovery = wire
            .recovery
            .map(serde_json::from_value)
            .transpose()
            .unwrap_or(None);
        Ok(Self {
            pane: wire.pane,
            recovery_quarantined: wire.recovery_quarantined || (had_recovery && recovery.is_none()),
            recovery,
        })
    }
}
fn is_false(value: &bool) -> bool {
    !value
}
impl Deref for PersistedPane {
    type Target = Pane;
    fn deref(&self) -> &Self::Target {
        &self.pane
    }
}
impl DerefMut for PersistedPane {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pane
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Persisted {
    next_id: u64,
    projects: Vec<Project>,
    worktrees: Vec<Worktree>,
    sessions: Vec<Session>,
    panes: Vec<PersistedPane>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    agent_exchanges: Vec<AgentExchange>,
}
impl Default for Persisted {
    fn default() -> Self {
        Self {
            next_id: 1,
            projects: Vec::new(),
            worktrees: Vec::new(),
            sessions: Vec::new(),
            panes: Vec::new(),
            agent_exchanges: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Lease {
    client_id: u64,
    generation: u64,
    expires_at: Instant,
}

#[derive(Clone)]
struct TuiPresenceLease {
    target_binary_id: String,
    target_daemon_revision: u32,
    max_protocol: u32,
    follows: bool,
    expires_at: Instant,
}

#[derive(Clone, Copy)]
enum LeaseAccess {
    Client(u64),
    Stream { client_id: u64, generation: u64 },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StreamLease {
    pane_id: PaneId,
    client_id: u64,
    generation: u64,
}

struct RuntimeAgentAuthority {
    pane_id: PaneId,
    generation: Option<String>,
}

impl RuntimeAgentAuthority {
    fn new(pane_id: PaneId, generation: Option<String>) -> Self {
        Self {
            pane_id,
            generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    Unexpected,
    Replacement,
    Handoff,
    Intentional,
}

impl StopReason {
    fn marker(self) -> &'static str {
        match self {
            Self::Unexpected => "unexpected",
            Self::Replacement | Self::Handoff => "replacement",
            Self::Intentional => "intentional",
        }
    }
}

#[derive(Clone, Copy)]
struct ClaudeReconciliation {
    event_state: AgentState,
    event_at: Instant,
    terminal_revision: Option<u64>,
    terminal: Option<claude_status::TerminalEvidence>,
    interrupt_requested: bool,
    interrupt_revision: Option<u64>,
}

impl ClaudeReconciliation {
    fn effective(self, now: Instant) -> AgentState {
        claude_status::reconcile(
            self.event_state,
            now.saturating_duration_since(self.event_at),
            self.terminal,
            self.interrupt_requested,
        )
    }
}

struct State {
    persisted: Persisted,
    revision: u64,
    runtimes: HashMap<PaneId, Arc<TerminalRuntime>>,
    runtime_generations: HashMap<PaneId, String>,
    agent_wake_leases: HashMap<PaneId, Instant>,
    agent_presence_leases: HashMap<PaneId, Instant>,
    agent_wake_tokens: HashMap<PaneId, String>,
    claude_reconciliations: HashMap<PaneId, ClaudeReconciliation>,
    terminal_operation_locks: HashMap<PaneId, Arc<Mutex<()>>>,
    listening_ports: HashMap<PaneId, Vec<u16>>,
    foreground_jobs: HashSet<PaneId>,
    listener_scan_complete: bool,
    leases: HashMap<PaneId, Lease>,
    transferred_streams: VecDeque<StreamLease>,
    events: VecDeque<Event>,
    plugins: Vec<PluginManifest>,
    plugin_events: VecDeque<(String, String)>,
    tui_clients: HashMap<u64, TuiPresenceLease>,
    legacy_tui_until: Option<Instant>,
    replacement_target: Option<String>,
    replacement_daemon_revision: Option<u32>,
    replacement_protocol: Option<u32>,
    replacement_executable: Option<PathBuf>,
    handoff_in_progress: bool,
    handoff_started: Option<Instant>,
    /// The last rolled-back handoff, for the lifecycle status.
    last_handoff_error: Option<HandoffError>,
    stop_reason: Option<StopReason>,
    persistence_dirty: bool,
    recovery_in_progress: bool,
    stopping: bool,
    /// Exchange rounds that the PTY is taking now. A live handoff waits for none.
    deliveries_in_flight: usize,
}

impl State {
    /// Saves `persisted`, then makes it the in-memory copy. A failed save changes nothing.
    fn persist(&mut self, path: &Path, persisted: Persisted) -> io::Result<()> {
        save_state(path, &persisted)?;
        self.persisted = persisted;
        Ok(())
    }

    /// Drops what a pane's process observed or was granted while it ran: its runtime
    /// generation, wake and presence leases, Claude reconciliation, foreground job and
    /// listeners. A pane whose process exited keeps its runtime entry, leases and operation
    /// lock, so `record_terminal_exit` calls only this. A dead pane must stop reporting a live
    /// foreground job or listener; the periodic scan skips exited runtimes and never clears them.
    fn forget_pane_observations(&mut self, pane_id: PaneId) {
        self.runtime_generations.remove(&pane_id);
        clear_agent_wake(self, pane_id);
        self.agent_presence_leases.remove(&pane_id);
        self.claude_reconciliations.remove(&pane_id);
        self.foreground_jobs.remove(&pane_id);
        self.listening_ports.remove(&pane_id);
    }

    /// Ends the terminal lease and the transferred-stream records of a pane, so a stream that
    /// belonged to the old runtime has no authority over its successor.
    fn forget_pane_leases(&mut self, pane_id: PaneId) {
        self.leases.remove(&pane_id);
        self.transferred_streams
            .retain(|stream| stream.pane_id != pane_id);
    }

    /// Removes every trace of a closed pane and returns its runtime for the caller to
    /// terminate after it drops the state lock. `agent_presence_leases` was missing from
    /// the close paths. The entry could not matter: `expire_agent_presence` skips a pane
    /// that is gone and drops its entry within the TTL.
    fn forget_pane(&mut self, pane_id: PaneId) -> Option<Arc<TerminalRuntime>> {
        self.forget_pane_observations(pane_id);
        self.forget_pane_leases(pane_id);
        self.terminal_operation_locks.remove(&pane_id);
        self.runtimes.remove(&pane_id)
    }

    /// Adds to the bounded event ring that `poll` serves.
    fn push_event(&mut self, event: Event) {
        self.events.push_back(event);
        while self.events.len() > EVENT_LIMIT {
            self.events.pop_front();
        }
    }

    /// Adds to the bounded queue of events that plugins subscribe to.
    fn push_plugin_event(&mut self, name: &str, revision: u64, id: u64) {
        if self.plugin_events.len() == PLUGIN_EVENT_LIMIT {
            self.plugin_events.pop_front();
        }
        self.plugin_events.push_back((
            name.into(),
            serde_json::json!({"event":name,"revision":revision,"id":id}).to_string(),
        ));
    }

    /// Records `terminal.changed` for a PTY read. Pollers act on a changed revision or any new
    /// event and never read the entity, so a burst of reads for one pane moves the newest
    /// event to the new revision instead of filling the ring and evicting other events.
    /// The first event stays as it is: `poll` takes the first revision as the ring floor.
    fn push_terminal_changed(&mut self, revision: u64, pane_id: PaneId) {
        if self.events.len() >= 2 {
            if let Some(Event::Changed {
                revision: newest,
                entity,
                id,
            }) = self.events.back_mut()
            {
                if entity == "terminal.changed" && *id == pane_id.0 {
                    *newest = revision;
                    return;
                }
            }
        }
        self.push_event(Event::Changed {
            revision,
            entity: "terminal.changed".into(),
            id: pane_id.0,
        });
    }
}

struct Daemon {
    state: Mutex<State>,
    mutation_lock: Mutex<()>,
    changed: Condvar,
    plugin_changed: Condvar,
    active_clients: Arc<AtomicUsize>,
    review_jobs: plugins::review::Registry,
    epoch: u64,
    binary_id: String,
    started_unix_ms: u64,
    recovered_from_backup: bool,
    next_runtime_generation: AtomicU64,
    state_path: PathBuf,
    lifecycle_path: PathBuf,
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ^ docs/daemon-discovery.md: the state and its singleton lock live beside the socket, where
// a client waits for the lock, so two sockets are two daemons that never share a state file.
fn state_path(socket: &Path) -> PathBuf {
    socket.with_file_name("state.json")
}

fn lifecycle_marker_path(socket: &Path) -> PathBuf {
    socket.with_extension("lifecycle")
}

fn write_lifecycle_marker(path: &Path, reason: &str) -> io::Result<()> {
    let temporary = path.with_extension(format!("lifecycle.tmp.{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        writeln!(file, "{reason}")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn secure_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsx state directory",
        ));
    }
    Ok(())
}
fn prepare_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_socket()
                || metadata.uid() != unsafe { libc::geteuid() } =>
        {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe existing socket path",
            ))
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
fn load_state_with_status(path: &Path) -> io::Result<(Persisted, bool)> {
    let loaded = state_store::load(path, validate_persisted)?;
    let mut state = loaded.state;
    for pane in &mut state.panes {
        pane.exited = true;
    }
    let now = unix_time_millis();
    for exchange in &mut state.agent_exchanges {
        if !exchange.is_terminal() {
            exchange.state = AgentExchangeState::Interrupted;
            exchange.updated_unix_ms = now;
            exchange.revision = exchange.revision.saturating_add(1);
        }
    }
    Ok((state, loaded.recovered_from_backup))
}

#[cfg(test)]
fn load_state(path: &Path) -> io::Result<Persisted> {
    load_state_with_status(path).map(|(state, _)| state)
}

fn save_state(path: &Path, state: &Persisted) -> io::Result<()> {
    state_store::save(path, state, validate_persisted)
}

fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ u64::from(std::process::id())
}
fn api(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}
fn io_api(error: io::Error) -> ApiError {
    api("io", error.to_string())
}
fn terminal_api(error: impl std::fmt::Display) -> ApiError {
    api("terminal", error.to_string())
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
