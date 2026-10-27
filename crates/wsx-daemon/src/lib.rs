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

fn recover_runtimes(daemon: &Arc<Daemon>, resume_agents: bool) -> io::Result<()> {
    let mut attempts = prepare_recovery_attempts(daemon, resume_agents)?;
    attempts.sort_by_key(|attempt| attempt.launch.resume_key.is_some());
    let mut attempted_sessions = HashSet::new();

    for attempt in attempts {
        let RecoveryAttempt {
            pane_id,
            terminal_id,
            cwd,
            launch,
        } = attempt;
        let (stopping, pane_exists) = {
            let state = lock(&daemon.state);
            (
                state.stopping,
                state.persisted.panes.iter().any(|pane| pane.id == pane_id),
            )
        };
        if stopping {
            break;
        }
        if !pane_exists {
            continue;
        }
        let had_resume = launch.resume_key.is_some();
        let launch = deduplicate_recovery_launch(launch, &attempted_sessions);
        if had_resume && launch.resume_key.is_none() {
            clear_pending_recovery_agent(daemon, pane_id)?;
        }
        let launch = match prepare_recovery_launch(launch, &cwd) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {error}", pane_id.0);
                clear_pending_recovery_agent(daemon, pane_id)?;
                continue;
            }
        };
        let runtime = match spawn_runtime(daemon, pane_id, terminal_id, &cwd, &launch.recipe) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {}", pane_id.0, error.message);
                clear_pending_recovery_agent(daemon, pane_id)?;
                continue;
            }
        };
        let generation = {
            let mut state = lock(&daemon.state);
            if !state.persisted.panes.iter().any(|pane| pane.id == pane_id) {
                drop(state);
                runtime.terminate();
                continue;
            }
            let generation = state.runtime_generations.get(&pane_id).cloned();
            state.runtimes.insert(pane_id, Arc::clone(&runtime));
            if runtime.exited() {
                record_terminal_exit(daemon, &mut state, pane_id);
            } else if let Some(pane) = state
                .persisted
                .panes
                .iter_mut()
                .find(|pane| pane.id == pane_id)
            {
                pane.exited = false;
            }
            generation
        };
        let Some(resume_key) = launch.resume_key else {
            continue;
        };
        attempted_sessions.insert(resume_key);
        if !launch.await_readiness {
            continue;
        }
        let Some(provider) = launch.provider.as_deref() else {
            continue;
        };
        let Some(generation) = generation else {
            continue;
        };
        if wait_for_recovered_agent(
            daemon,
            pane_id,
            &generation,
            provider,
            RECOVERY_AGENT_READY_TIMEOUT,
        ) == RecoveryReadiness::TimedOut
        {
            eprintln!(
                "wsxd recovery pane {}: {provider} did not become ready within {} seconds; opening a shell",
                pane_id.0,
                RECOVERY_AGENT_READY_TIMEOUT.as_secs()
            );
            replace_timed_out_recovery_with_shell(
                daemon,
                pane_id,
                terminal_id,
                &cwd,
                launch.recipe.rows,
                launch.recipe.cols,
                &runtime,
            )?;
        }
    }
    let state = lock(&daemon.state);
    save_state(&daemon.state_path, &state.persisted)
}

fn prepare_recovery_attempts(
    daemon: &Arc<Daemon>,
    resume_agents: bool,
) -> io::Result<Vec<RecoveryAttempt>> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let session_worktrees = persisted
        .sessions
        .iter()
        .map(|session| (session.id, session.worktree_id))
        .collect::<HashMap<_, _>>();
    let worktree_paths = persisted
        .worktrees
        .iter()
        .map(|worktree| (worktree.id, worktree.path.clone()))
        .collect::<HashMap<_, _>>();
    let mut attempts = Vec::new();
    for pane in &mut persisted.panes {
        pane.exited = true;
        if pane.recovery_quarantined {
            continue;
        }
        if pane.recovery.is_none() {
            pane.recovery = Some(default_launch_recipe());
        }
        let Some(saved_recipe) = pane.recovery.clone() else {
            continue;
        };
        if let Err(error) = validate_recipe(&saved_recipe) {
            eprintln!("wsxd recovery pane {}: {error}", pane.id.0);
            pane.recovery = None;
            pane.recovery_quarantined = true;
            pane.agent = None;
            continue;
        }
        let launch = recovery_launch(pane.agent.as_mut(), &saved_recipe, resume_agents);
        // ^ [[Session Model]] A persisted identity authorizes the queued resume plan.
        // It remains detached until the replacement runtime reports its own generation.
        if launch.resume_key.is_some() {
            if let Some(agent) = pane.agent.as_mut() {
                detach_agent(agent);
            }
        } else {
            pane.agent = None;
        }
        let Some(cwd) = session_worktrees
            .get(&pane.session_id)
            .and_then(|worktree_id| worktree_paths.get(worktree_id))
            .cloned()
        else {
            eprintln!("wsxd recovery pane {}: worktree is absent", pane.id.0);
            pane.agent = None;
            continue;
        };
        attempts.push(RecoveryAttempt {
            pane_id: pane.id,
            terminal_id: pane.terminal_id,
            cwd,
            launch,
        });
    }
    state.persist(&daemon.state_path, persisted)?;
    Ok(attempts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryReadiness {
    Ready,
    Failed,
    Cancelled,
    TimedOut,
}

fn wait_for_recovered_agent(
    daemon: &Daemon,
    pane_id: PaneId,
    generation: &str,
    provider: &str,
    timeout: Duration,
) -> RecoveryReadiness {
    let deadline = Instant::now() + timeout;
    let mut state = lock(&daemon.state);
    loop {
        if state.stopping || !state.persisted.panes.iter().any(|pane| pane.id == pane_id) {
            return RecoveryReadiness::Cancelled;
        }
        if state.runtime_generations.get(&pane_id).map(String::as_str) != Some(generation) {
            return RecoveryReadiness::Failed;
        }
        if state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.agent.as_ref())
            .is_some_and(|agent| agent.attached && agent.provider == provider)
        {
            return RecoveryReadiness::Ready;
        }
        let now = Instant::now();
        if now >= deadline {
            return RecoveryReadiness::TimedOut;
        }
        let wait = (deadline - now).min(RECOVERY_AGENT_POLL_INTERVAL);
        let (next, _) = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
    }
}

fn clear_pending_recovery_agent(daemon: &Daemon, pane_id: PaneId) -> io::Result<()> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
        pane.agent = None;
    }
    state.persist(&daemon.state_path, persisted)?;
    Ok(())
}

fn replace_timed_out_recovery_with_shell(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: &Path,
    rows: u16,
    cols: u16,
    runtime: &Arc<TerminalRuntime>,
) -> io::Result<()> {
    {
        let mut state = lock(&daemon.state);
        let owns_runtime = state
            .runtimes
            .get(&pane_id)
            .is_some_and(|current| Arc::ptr_eq(current, runtime));
        if !owns_runtime {
            return Ok(());
        }
        let mut persisted = state.persisted.clone();
        let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) else {
            return Ok(());
        };
        pane.agent = None;
        pane.exited = true;
        state.persist(&daemon.state_path, persisted)?;
        state.runtimes.remove(&pane_id);
        state.runtime_generations.remove(&pane_id);
    }
    runtime.terminate();
    let recipe = shell_launch_recipe(rows, cols);
    let replacement = Arc::new(
        spawn_runtime(daemon, pane_id, terminal_id, cwd, &recipe)
            .map_err(|error| io::Error::other(error.message))?,
    );
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
        pane.exited = replacement.exited();
        if let Err(error) = state.persist(&daemon.state_path, persisted) {
            state.runtime_generations.remove(&pane_id);
            drop(state);
            replacement.terminate();
            return Err(error);
        }
        state.runtimes.insert(pane_id, replacement);
    } else {
        state.runtime_generations.remove(&pane_id);
        drop(state);
        replacement.terminate();
    }
    Ok(())
}

fn spawn_cold_recovery(daemon: &Arc<Daemon>, resume_agents: bool) -> thread::JoinHandle<()> {
    {
        let mut state = lock(&daemon.state);
        state.recovery_in_progress = true;
    }
    let daemon = Arc::clone(daemon);
    thread::spawn(move || {
        if let Err(error) = recover_runtimes(&daemon, resume_agents) {
            eprintln!("wsxd recovery: {error}");
        }
        let mut state = lock(&daemon.state);
        state.recovery_in_progress = false;
        daemon.changed.notify_all();
    })
}

fn recovery_launch(
    agent: Option<&mut AgentInfo>,
    saved_recipe: &LaunchRecipe,
    resume_agents: bool,
) -> RecoveryLaunch {
    if !resume_agents {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    }
    let Some(agent) = agent else {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    let session_ref = agent.session_ref.clone().or_else(|| {
        agent
            .conversation_id
            .as_ref()
            .and_then(|value| AgentSessionRef::id(value.clone()))
    });
    let Some(session_ref) = session_ref else {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    agent.session_ref = Some(session_ref.clone());
    let provider = agent.provider.clone();
    let await_readiness = agent.capabilities.lifecycle;
    let Some(plan) = resume::plan(&provider, &session_ref) else {
        return RecoveryLaunch {
            recipe: shell_launch_recipe(saved_recipe.rows, saved_recipe.cols),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    RecoveryLaunch {
        recipe: LaunchRecipe {
            command: plan.argv,
            initial_input: None,
            rows: saved_recipe.rows,
            cols: saved_recipe.cols,
        },
        resume_key: Some(plan.dedupe_key),
        provider: Some(provider),
        await_readiness,
    }
}

fn deduplicate_recovery_launch(
    launch: RecoveryLaunch,
    resumed_sessions: &HashSet<String>,
) -> RecoveryLaunch {
    if launch
        .resume_key
        .as_ref()
        .is_some_and(|key| resumed_sessions.contains(key))
    {
        RecoveryLaunch {
            recipe: shell_launch_recipe(launch.recipe.rows, launch.recipe.cols),
            resume_key: None,
            provider: None,
            await_readiness: false,
        }
    } else {
        launch
    }
}

fn shell_launch_recipe(rows: u16, cols: u16) -> LaunchRecipe {
    launch_recipe(Vec::new(), None, rows, cols).unwrap_or_else(|_| default_launch_recipe())
}

fn prepare_recovery_launch(
    mut launch: RecoveryLaunch,
    cwd: &Path,
) -> Result<RecoveryLaunch, String> {
    if launch.resume_key.is_none() {
        return Ok(launch);
    }
    let executable = launch
        .recipe
        .command
        .first()
        .ok_or_else(|| "native resume command is empty".to_string())?;
    let resolved = resolve_executable(cwd, executable)
        .ok_or_else(|| format!("native resume executable is unavailable: {executable}"))?;
    launch.recipe.command[0] = resolved
        .to_str()
        .ok_or_else(|| "native resume executable path is not UTF-8".to_string())?
        .to_string();
    let supervisor = std::env::current_exe()
        .map_err(|error| format!("resolve wsxd resume supervisor: {error}"))?;
    let supervisor = supervisor
        .to_str()
        .ok_or_else(|| "wsxd resume supervisor path is not UTF-8".to_string())?;
    let mut command = vec![supervisor.to_string(), RESUME_SUPERVISOR_ARG.to_string()];
    command.extend(launch.recipe.command);
    launch.recipe.command = command;
    Ok(launch)
}

fn resolve_executable(cwd: &Path, executable: &str) -> Option<PathBuf> {
    let executable_path = Path::new(executable);
    let candidates = if executable_path.components().count() > 1 {
        vec![if executable_path.is_absolute() {
            executable_path.to_path_buf()
        } else {
            cwd.join(executable_path)
        }]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| {
                        if directory.is_absolute() {
                            directory.join(executable_path)
                        } else {
                            cwd.join(directory).join(executable_path)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates.into_iter().find(|candidate| {
        fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

/// Main screen, no kitty keyboard flags, no modifyOtherKeys, no mouse or focus reports, no
/// bracketed paste, normal cursor keys and keypad, a visible cursor.
const AGENT_MODES_RESET: &[u8] = b"\x1b[?1049l\x1b[=0;1u\x1b[>4;0m\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\
\x1b[?1l\x1b>\x1b[?25h";

pub fn run_resume_supervisor(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    let executable = arguments.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "resume supervisor command is empty",
        )
    })?;
    let mut command = Command::new(executable);
    command.args(arguments);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let signals = InteractiveSignalGuard::ignore()?;
    reset_child_interactive_signals(&mut command);
    let status = command.status()?;
    // ^ An agent that quits can leave its alternate screen and its keyboard and mouse modes
    // on; the shell would then get CSI u codes for typed keys and draw on a screen that
    // goes away. Leave them as a fresh terminal has them.
    // ref: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ (CSI = 0 ; 1 u sets no flags)
    let mut out = io::stdout();
    let _ = out.write_all(AGENT_MODES_RESET).and_then(|()| out.flush());
    if !status.success() {
        eprintln!("wsxd resumed agent exited with {status}; opening a shell");
    }
    // ^ The shell is what the pane is for; a record wsxd could not clear (a stale runtime
    // generation, a full disk) must not leave the pane as a dead screen.
    if let Err(error) = transition_resumed_agent_to_shell() {
        eprintln!("wsxd could not clear the resumed agent: {error}; opening a shell");
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    let mut shell = Command::new(shell);
    // ^ A repeated Ctrl+C that ends the agent must not kill this supervisor before
    // the shell exists, so the default dispositions return only at exec.
    reset_child_interactive_signals(&mut shell);
    let error = shell.exec();
    drop(signals);
    Err(error)
}

fn transition_resumed_agent_to_shell() -> io::Result<()> {
    let pane_id = std::env::var("WSX_PANE_ID")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "resume pane ID is missing"))?
        .parse::<PaneId>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let runtime_generation = std::env::var(WSX_RUNTIME_GENERATION_ENV).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "resume runtime generation is missing",
        )
    })?;
    let next_runtime_generation =
        format!("{:016x}:{:016x}", epoch(), u64::from(std::process::id()));
    let request = Request::AgentClear {
        pane_id,
        runtime_generation,
        next_runtime_generation: next_runtime_generation.clone(),
    };
    let client = Client::local();
    // ^ [[Session Model]] A recovered agent can exit before startup publishes the wsxd socket.
    let deadline = Instant::now() + RESUME_CLEAR_RETRY_TIMEOUT;
    let response = loop {
        match client.call(&request) {
            Ok(response) => break response,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(RESUME_CLEAR_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    };
    match response {
        Response::Ack { .. } => {
            std::env::set_var(WSX_RUNTIME_GENERATION_ENV, next_runtime_generation);
            Ok(())
        }
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd returned an unexpected agent clear response",
        )),
    }
}

struct InteractiveSignalGuard {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl InteractiveSignalGuard {
    fn ignore() -> io::Result<Self> {
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            // SAFETY: setting a process signal disposition is async-signal-safe;
            // the prior disposition is restored before this supervisor execs a shell.
            let prior = unsafe { libc::signal(signal, libc::SIG_IGN) };
            if prior == libc::SIG_ERR {
                for (restore_signal, disposition) in previous.drain(..).rev() {
                    // SAFETY: restoring a disposition returned by libc::signal.
                    unsafe { libc::signal(restore_signal, disposition) };
                }
                return Err(io::Error::last_os_error());
            }
            previous.push((signal, prior));
        }
        Ok(Self { previous })
    }
}

impl Drop for InteractiveSignalGuard {
    fn drop(&mut self) {
        for (signal, disposition) in self.previous.drain(..).rev() {
            // SAFETY: restoring a disposition returned by libc::signal.
            unsafe { libc::signal(signal, disposition) };
        }
    }
}

fn reset_child_interactive_signals(command: &mut Command) {
    // SAFETY: pre_exec only calls async-signal-safe libc::signal operations.
    unsafe {
        command.pre_exec(|| {
            for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

fn resume_agents_on_restore() -> bool {
    match GlobalConfig::load() {
        Ok((config, None)) => config.resume_agents_on_restore,
        Ok((_, Some(warning))) => {
            eprintln!("wsxd agent restoration disabled: {warning}");
            false
        }
        Err(error) => {
            eprintln!("wsxd agent restoration disabled: {error:#}");
            false
        }
    }
}

pub fn run() -> io::Result<()> {
    run_daemon(None)
}

pub fn run_routine_daemon(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected routine daemon argument",
        ));
    }
    let root = asched_core::RegistryStore::default_root().map_err(io::Error::other)?;
    asched_core::routine::daemon::serve_from_startup_env(root).map_err(io::Error::other)
}

pub fn run_handoff_import(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    let socket = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "handoff socket is missing"))?;
    let token = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "handoff token is missing"))?;
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected handoff import argument",
        ));
    }
    run_daemon(Some(handoff::receive(&socket, &token)?))
}

fn new_state(persisted: Persisted) -> State {
    State {
        persisted,
        revision: 1,
        runtimes: HashMap::new(),
        runtime_generations: HashMap::new(),
        agent_wake_leases: HashMap::new(),
        agent_presence_leases: HashMap::new(),
        agent_wake_tokens: HashMap::new(),
        claude_reconciliations: HashMap::new(),
        terminal_operation_locks: HashMap::new(),
        listening_ports: HashMap::new(),
        foreground_jobs: HashSet::new(),
        listener_scan_complete: false,
        leases: HashMap::new(),
        transferred_streams: VecDeque::new(),
        events: VecDeque::new(),
        plugins: plugins::discover(),
        plugin_events: VecDeque::new(),
        tui_clients: HashMap::new(),
        legacy_tui_until: None,
        replacement_target: None,
        replacement_daemon_revision: None,
        replacement_protocol: None,
        replacement_executable: None,
        handoff_in_progress: false,
        handoff_started: None,
        last_handoff_error: None,
        stop_reason: None,
        persistence_dirty: false,
        recovery_in_progress: false,
        deliveries_in_flight: 0,
        stopping: false,
    }
}

fn run_daemon(import: Option<handoff::Received>) -> io::Result<()> {
    install_shutdown_signals()?;
    raise_descriptor_limit();
    let socket = default_socket_path();
    let state_path = state_path(&socket);
    secure_parent(&socket)?;

    let (singleton_lock, persisted, recovered_from_backup, import) = match import {
        Some(handoff::Received {
            manifest,
            singleton_lock,
            pane_fds,
            stream,
        }) => {
            validate_persisted(&manifest.persisted)?;
            (
                fs::File::from(singleton_lock),
                manifest.persisted,
                false,
                Some((manifest.panes, manifest.exited_panes, pane_fds, stream)),
            )
        }
        None => {
            let singleton_lock = acquire_singleton_lock(&state_path.with_extension("lock"))?;
            prepare_socket(&socket)?;
            let (persisted, recovered) = load_state_with_status(&state_path)?;
            (singleton_lock, persisted, recovered, None)
        }
    };

    let daemon = Arc::new(Daemon {
        state: Mutex::new(new_state(persisted)),
        mutation_lock: Mutex::new(()),
        changed: Condvar::new(),
        plugin_changed: Condvar::new(),
        active_clients: Arc::new(AtomicUsize::new(0)),
        review_jobs: plugins::review::Registry::default(),
        epoch: epoch(),
        binary_id: binary_identity(&std::env::current_exe()?)?,
        started_unix_ms: unix_time_millis(),
        recovered_from_backup,
        next_runtime_generation: AtomicU64::new(1),
        state_path,
        lifecycle_path: lifecycle_marker_path(&socket),
    });

    let cold_start = import.is_none();
    let mut import_stream = None;
    if let Some((panes, exited_panes, pane_fds, mut stream)) = import {
        if panes.len() != pane_fds.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff pane and descriptor counts differ",
            ));
        }
        restore_handoff_runtimes(&daemon, panes, exited_panes, pane_fds)?;
        handoff::report_restored(&mut stream)?;
        handoff::wait_publish(&mut stream)?;
        prepare_socket(&socket)?;
        import_stream = Some(stream);
    }

    // ^ docs/agent-reporting.md: stage before readiness, publish only under ownership.
    let mut reporter = Some(reporter::Publication::prepare(
        &socket,
        &std::env::current_exe()?,
    )?);
    let listener = bind_owner_only(&socket)?;
    listener.set_nonblocking(true)?;
    let mut listener = Some(listener);
    let plugin_dispatcher = match spawn_plugin_dispatcher(&daemon) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };

    if let Some(mut stream) = import_stream {
        write_lifecycle_marker(&daemon.lifecycle_path, "ready")?;
        handoff::report_ready(&mut stream)?;
        handoff::wait_commit(&mut stream)?;
        // ^ docs/daemon-live-upgrade.md "A successor that dies": scripts/runtime-smoke.py
        // ends a successor here, after the commit and before `owned`, which no outside
        // signal can time. Read only by a successor, never by a daemon that starts cold.
        if std::env::var_os("WSX_HANDOFF_FAULT").is_some_and(|fault| fault == "after_commit") {
            std::process::exit(70);
        }
        // Commit is irreversible. A publication failure must not kill imported PTYs.
        if let Err(error) = reporter::publish_pending(&mut reporter) {
            eprintln!("wsxd reporter publication after handoff commit: {error}; retrying under successor ownership");
        }
        let runtimes = lock(&daemon.state)
            .runtimes
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in runtimes {
            runtime.assume_handoff_ownership();
            runtime.activate_handoff();
        }
        let _ = handoff::report_owned(&mut stream);
    } else {
        reporter::publish_pending(&mut reporter)?;
        write_lifecycle_marker(&daemon.lifecycle_path, "ready")?;
    }

    // ^ [[Session Model]] Cold recovery begins only after lifecycle hooks can reach
    // the canonical socket. Agent resumes are queued by recover_runtimes.
    let mut cold_recovery =
        cold_start.then(|| spawn_cold_recovery(&daemon, resume_agents_on_restore()));
    let mut reporter_retry_at = Instant::now() + Duration::from_secs(1);
    let mut next_presence_poll = Instant::now() + AGENT_PRESENCE_POLL;
    let port_scanner = spawn_port_scanner(&daemon);
    let foreground_scanner = spawn_foreground_scanner(&daemon);
    let claude_reconciler = spawn_claude_reconciler(&daemon);
    let wake_controller = spawn_wake_controller(&daemon);

    while !advance_replacement(&daemon) {
        retry_dirty_persistence(&daemon);
        // ^ docs/agent-reporting.md: retain staged publication after an
        // irreversible commit and retry only while this daemon owns the runtime.
        if reporter.is_some() && Instant::now() >= reporter_retry_at {
            if reporter::publish_pending(&mut reporter).is_ok() {
                eprintln!("wsxd reporter publication recovered");
            }
            reporter_retry_at = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= next_presence_poll {
            expire_agent_presence(&daemon, Instant::now());
            next_presence_poll = Instant::now() + AGENT_PRESENCE_POLL;
        }
        if let Some(target) = take_ready_handoff(&daemon) {
            match perform_live_handoff(&daemon, &socket, &singleton_lock, &mut listener, target) {
                Ok(()) => break,
                Err(error) => eprintln!("wsxd handoff: {error}"),
            }
        }
        let Some(active_listener) = listener.as_ref() else {
            // ^ Only a rollback that could not bind the socket again leaves it unbound.
            if let Err(error) = rebind_listener(&socket, &mut listener) {
                eprintln!("wsxd socket: {error}");
                thread::sleep(Duration::from_secs(1));
            }
            continue;
        };
        match active_listener.accept() {
            Ok((stream, _)) => {
                if daemon.active_clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                    daemon.active_clients.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let daemon = Arc::clone(&daemon);
                let clients = Arc::clone(&daemon.active_clients);
                thread::spawn(move || {
                    let _guard = ClientGuard(clients);
                    if let Err(error) = serve_client(stream, daemon) {
                        eprintln!("wsxd client: {error}");
                    }
                });
            }
            Err(error) if accept_error_is_transient(&error) => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                {
                    let mut state = lock(&daemon.state);
                    state.stopping = true;
                    daemon.changed.notify_all();
                }
                if let Some(recovery) = cold_recovery.take() {
                    let _ = recovery.join();
                }
                cleanup(&daemon, &socket);
                let _ = plugin_dispatcher.join();
                let _ = port_scanner.join();
                let _ = foreground_scanner.join();
                let _ = claude_reconciler.join();
                let _ = wake_controller.join();
                return Err(error);
            }
        }
    }
    if let Some(recovery) = cold_recovery.take() {
        let _ = recovery.join();
    }
    cleanup(&daemon, &socket);
    let _ = plugin_dispatcher.join();
    let _ = port_scanner.join();
    let _ = foreground_scanner.join();
    let _ = claude_reconciler.join();
    let _ = wake_controller.join();
    Ok(())
}

#[derive(Clone)]
struct HandoffTarget {
    binary_id: String,
    version: String,
    protocol: u32,
    daemon_revision: u32,
    executable: PathBuf,
}

fn take_ready_handoff(daemon: &Daemon) -> Option<HandoffTarget> {
    // ^ The accept loop calls this on every pass. A handoff starts between mutations, so
    // while one runs this pass is skipped and the loop keeps accepting connections.
    let _mutation = match daemon.mutation_lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let mut state = lock(&daemon.state);
    // ^ A delivery releases the mutation lock during its PTY write; its PTY must not move
    // to the successor before the outcome is recorded.
    if state.stopping || state.handoff_in_progress || state.deliveries_in_flight > 0 {
        return None;
    }
    let binary_id = state.replacement_target.clone()?;
    let executable = state.replacement_executable.clone()?;
    if !handoff_blockers(&mut state, &binary_id).is_empty() {
        return None;
    }
    let target = HandoffTarget {
        version: binary_identity_version(&binary_id)?.to_string(),
        binary_id,
        protocol: state.replacement_protocol?,
        daemon_revision: state.replacement_daemon_revision?,
        executable,
    };
    // ^ docs/daemon-live-upgrade.md: mark the whole transition before any client is
    // told, so attached clients wait for the successor or the rollback.
    if let Err(error) = write_lifecycle_marker(
        &daemon.lifecycle_path,
        &format!("{HANDOFF_MARKER_PREFIX}{}", unix_time_millis()),
    ) {
        eprintln!("wsxd handoff marker: {error}");
        return None;
    }
    state.handoff_in_progress = true;
    state.handoff_started = Some(Instant::now());
    daemon.changed.notify_all();
    Some(target)
}

fn perform_live_handoff(
    daemon: &Arc<Daemon>,
    socket: &Path,
    singleton_lock: &fs::File,
    listener: &mut Option<UnixListener>,
    target: HandoffTarget,
) -> io::Result<()> {
    if let Err(error) = validate_handoff_executable(&target.executable, &target.binary_id) {
        rollback_handoff(daemon, socket, listener, &[], "validate", &error);
        return Err(error);
    }
    let runtimes = {
        let state = lock(&daemon.state);
        let mut runtimes = state
            .runtimes
            .iter()
            .map(|(pane_id, runtime)| (*pane_id, Arc::clone(runtime)))
            .collect::<Vec<_>>();
        runtimes.sort_by_key(|(pane_id, _)| *pane_id);
        runtimes
    };
    let operation_locks = runtimes
        .iter()
        .map(|(pane_id, _)| terminal_operation_lock(daemon, *pane_id))
        .collect::<Vec<_>>();
    let _operations = operation_locks
        .iter()
        .map(|operation_lock| lock(operation_lock))
        .collect::<Vec<_>>();

    // ^ A pane whose program exited has no reader to pause and no process to hand over: it
    // goes as its screen alone (docs/daemon-live-upgrade.md "Exited panes"). A pane that
    // exits during the pause is one of them.
    let mut paused = Vec::new();
    let mut live = Vec::new();
    let mut exited = Vec::new();
    for (pane_id, runtime) in &runtimes {
        if runtime.exited() {
            exited.push((*pane_id, Arc::clone(runtime)));
            continue;
        }
        if let Err(error) = runtime.pause_handoff_reader() {
            if runtime.exited() {
                exited.push((*pane_id, Arc::clone(runtime)));
                continue;
            }
            let error = io::Error::other(format!("pane {}: {error}", pane_id.0));
            rollback_handoff(daemon, socket, listener, &paused, "pause", &error);
            return Err(error);
        }
        paused.push(Arc::clone(runtime));
        live.push((*pane_id, Arc::clone(runtime)));
    }

    let mut duplicate_fds = Vec::<std::os::fd::OwnedFd>::new();
    let mut source_ownership = Vec::<(Arc<TerminalRuntime>, libc::pid_t)>::new();
    let mut step = "export";
    let result = (|| {
        let (persisted, generations) = {
            let state = lock(&daemon.state);
            save_state(&daemon.state_path, &state.persisted)?;
            (state.persisted.clone(), state.runtime_generations.clone())
        };
        let export = |pane_id: &PaneId, runtime: &TerminalRuntime| {
            runtime
                .handoff_state()
                .map(|state| handoff::PaneHandoff {
                    state,
                    runtime_generation: generations.get(pane_id).cloned(),
                })
                .map_err(|error| io::Error::other(format!("pane {}: {error}", pane_id.0)))
        };
        let mut pane_handoffs = Vec::with_capacity(live.len());
        for (pane_id, runtime) in &live {
            pane_handoffs.push(export(pane_id, runtime)?);
            let fd = runtime
                .duplicate_handoff_fd()
                .map_err(|error| io::Error::other(format!("pane {}: {error}", pane_id.0)))?;
            duplicate_fds.push(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        }
        let exited_handoffs = exited
            .iter()
            .map(|(pane_id, runtime)| export(pane_id, runtime))
            .collect::<io::Result<Vec<_>>>()?;
        let manifest = handoff::manifest(
            persisted,
            pane_handoffs,
            exited_handoffs,
            target.version.clone(),
            target.protocol,
            target.daemon_revision,
        );
        step = "spawn";
        let handoff_path = handoff::socket_path(socket, daemon.epoch)?;
        let handoff_listener = handoff::bind(&handoff_path)?;
        let token = format!(
            "{:016x}{:016x}",
            daemon.epoch,
            unix_time_millis() ^ u64::from(std::process::id())
        );
        let mut child = handoff::spawn_import(&target.executable, &handoff_path, &token)?;
        let pane_fds = duplicate_fds
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        step = "send";
        let mut stream = match handoff::accept_and_send(
            handoff_listener,
            &mut child,
            &handoff_path,
            &token,
            &manifest,
            singleton_lock.as_raw_fd(),
            &pane_fds,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                handoff::cleanup_child(&mut child);
                return Err(error);
            }
        };

        drop(listener.take());
        let _ = fs::remove_file(socket);
        step = "publish";
        handoff::request_publish(&mut stream)?;
        step = "ready";
        if let Err(error) = handoff::wait_ready(&mut stream) {
            handoff::cleanup_child(&mut child);
            return Err(error);
        }
        step = "ownership";
        for (_, runtime) in &live {
            match runtime.prepare_source_handoff() {
                Ok(process_group) => source_ownership.push((Arc::clone(runtime), process_group)),
                Err(error) => {
                    handoff::cleanup_child(&mut child);
                    return Err(io::Error::other(error.to_string()));
                }
            }
        }
        step = "commit";
        if let Err(error) = handoff::commit(&mut stream) {
            handoff::cleanup_child(&mut child);
            return Err(error);
        }
        step = "owned";
        handoff::wait_owned(&mut stream, &mut child)?;
        {
            let mut state = lock(&daemon.state);
            state.stopping = true;
            state.stop_reason = Some(StopReason::Handoff);
            daemon.changed.notify_all();
            daemon.plugin_changed.notify_all();
        }
        Ok(())
    })();

    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            for (runtime, process_group) in source_ownership {
                runtime.rollback_source_handoff(process_group);
            }
            rollback_handoff(daemon, socket, listener, &paused, step, &error);
            Err(error)
        }
    }
}

/// Back to serving after a failed handoff. The step and the error stay in the lifecycle
/// status as `last_handoff_error`, because the daemon's stderr goes nowhere
/// (docs/daemon-live-upgrade.md "A failed handoff").
// ^ docs/daemon-live-upgrade.md "A stuck handoff": nothing here may leave the handoff open.
// A daemon that kept `handoff_in_progress` after a failed rebind or marker write answered
// every request with `daemon_replacing` for ever while its sessions ran unseen. A socket it
// could not bind again is bound by the main loop (`rebind_listener`).
fn rollback_handoff(
    daemon: &Daemon,
    socket: &Path,
    listener: &mut Option<UnixListener>,
    paused: &[Arc<TerminalRuntime>],
    step: &str,
    error: &io::Error,
) {
    let mut failures = vec![error.to_string()];
    if let Err(rebind_error) = rebind_listener(socket, listener) {
        failures.push(format!("bind the socket again: {rebind_error}"));
    }
    for runtime in paused {
        runtime.resume_handoff_reader();
    }
    if let Err(marker_error) = write_lifecycle_marker(&daemon.lifecycle_path, "ready") {
        failures.push(format!("write the lifecycle marker: {marker_error}"));
    }
    let mut state = lock(&daemon.state);
    state.last_handoff_error = Some(HandoffError {
        unix_ms: unix_time_millis(),
        step: step.to_string(),
        error: failures.join("; rollback could not "),
    });
    if let Some(started) = state.handoff_started.take() {
        let paused_for = started.elapsed();
        for lease in state.leases.values_mut() {
            lease.expires_at += paused_for;
        }
    }
    state.handoff_in_progress = false;
    state.replacement_target = None;
    state.replacement_daemon_revision = None;
    state.replacement_protocol = None;
    state.replacement_executable = None;
    daemon.changed.notify_all();
}

/// Bind the canonical socket again when a handoff gave it up.
fn rebind_listener(socket: &Path, listener: &mut Option<UnixListener>) -> io::Result<()> {
    if listener.is_none() {
        prepare_socket(socket)?;
        let restored = bind_owner_only(socket)?;
        restored.set_nonblocking(true)?;
        *listener = Some(restored);
    }
    Ok(())
}

fn restore_handoff_runtimes(
    daemon: &Arc<Daemon>,
    panes: Vec<handoff::PaneHandoff>,
    exited_panes: Vec<handoff::PaneHandoff>,
    pane_fds: Vec<OwnedFd>,
) -> io::Result<()> {
    let persisted_ids = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for pane in panes.iter().chain(&exited_panes) {
        if !persisted_ids.contains(&pane.state.pane_id) || !seen.insert(pane.state.pane_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff pane is absent from persisted state or duplicated",
            ));
        }
    }
    // ^ An exited pane comes back as its last screen, without a process
    // (docs/daemon-live-upgrade.md "Exited panes").
    let exited = exited_panes
        .into_iter()
        .map(|pane| {
            let pane_id = pane.state.pane_id;
            TerminalRuntime::exited_from_handoff(
                pane.state,
                runtime_notify(daemon, pane_id, pane.runtime_generation.clone()),
            )
            .map(|runtime| (pane_id, pane.runtime_generation, Arc::new(runtime)))
            .map_err(|error| {
                io::Error::other(format!("restore exited pane {}: {error}", pane_id.0))
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    {
        let mut state = lock(&daemon.state);
        for (pane_id, generation, runtime) in exited {
            if let Some(generation) = generation {
                state.runtime_generations.insert(pane_id, generation);
            }
            state.runtimes.insert(pane_id, runtime);
        }
    }
    if panes.is_empty() {
        return Ok(());
    }

    let worker_count = panes.len().min(HANDOFF_RESTORE_WORKERS);
    let mut buckets: Vec<Vec<(handoff::PaneHandoff, OwnedFd)>> =
        (0..worker_count).map(|_| Vec::new()).collect();
    for (index, pair) in panes.into_iter().zip(pane_fds).enumerate() {
        buckets[index % worker_count].push(pair);
    }

    let restored = thread::scope(|scope| -> io::Result<Vec<_>> {
        let handles = buckets
            .into_iter()
            .map(|bucket| {
                let daemon = Arc::clone(daemon);
                scope.spawn(move || -> io::Result<Vec<_>> {
                    let mut restored = Vec::with_capacity(bucket.len());
                    for (pane, fd) in bucket {
                        let pane_id = pane.state.pane_id;
                        let runtime = unsafe {
                            TerminalRuntime::from_handoff_fd(
                                fd.into_raw_fd(),
                                pane.state,
                                runtime_notify(&daemon, pane_id, pane.runtime_generation.clone()),
                            )
                        }
                        .map_err(|error| {
                            io::Error::other(format!("restore handoff pane {}: {error}", pane_id.0))
                        })?;
                        runtime.protect_handoff_import();
                        restored.push((pane_id, pane.runtime_generation, Arc::new(runtime)));
                    }
                    Ok(restored)
                })
            })
            .collect::<Vec<_>>();
        let mut restored = Vec::new();
        for handle in handles {
            let worker = handle
                .join()
                .map_err(|_| io::Error::other("handoff restore worker panicked"))??;
            restored.extend(worker);
        }
        Ok(restored)
    })?;

    let mut state = lock(&daemon.state);
    for (pane_id, generation, runtime) in restored {
        if let Some(generation) = generation {
            state.runtime_generations.insert(pane_id, generation);
            if state
                .persisted
                .panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| agent.attached && agent.presence_id.is_some())
            {
                state.agent_presence_leases.insert(pane_id, Instant::now());
            }
        }
        state.runtimes.insert(pane_id, runtime);
    }
    Ok(())
}

extern "C" fn request_signal_shutdown(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

fn install_shutdown_signals() -> io::Result<()> {
    // ^ crates/wsx-core/src/runtime/client.rs detaches automatic starts before exec.
    // Ignore terminal hangup here as well so direct starts keep host-owned PTYs alive.
    if unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    let previous = unsafe {
        libc::signal(
            libc::SIGTERM,
            request_signal_shutdown as *const () as libc::sighandler_t,
        )
    };
    if previous == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ^ A GUI terminal passes launchd's soft limit of 256 descriptors on to wsxd. Each pane
// holds about five and each connection two, so agents reporting from many panes at once
// ran out of descriptors. Raise the soft limit toward the hard one, at most to macOS's
// OPEN_MAX (10240), which macOS requires and which keeps panes, that inherit it, from a
// Linux hard limit of millions. Failure keeps the inherited limit.
fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let target = limit.rlim_max.min(10240);
    if limit.rlim_cur < target {
        limit.rlim_cur = target;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

// ^ An accept error that belongs to one connection or to a short resource shortage ends
// that connection only. Before 0.40.0 it stopped wsxd and ended every PTY at once.
fn accept_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::EPROTO)
    )
}

// ^ A missing resident-adapter renewal revokes only its verified attachment.
// Event-only adapters never enter this map; a reported shutdown remains immediate.
fn expire_agent_presence(daemon: &Daemon, now: Instant) {
    let mut state = lock(&daemon.state);
    let expired = state
        .agent_presence_leases
        .iter()
        .filter(|(_, renewed_at)| now.saturating_duration_since(**renewed_at) >= AGENT_PRESENCE_TTL)
        .map(|(pane_id, _)| *pane_id)
        .collect::<Vec<_>>();
    let mut changed = Vec::new();
    for pane_id in expired {
        state.agent_presence_leases.remove(&pane_id);
        let revision = state.revision.saturating_add(changed.len() as u64 + 1);
        let Some(pane) = state
            .persisted
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
        else {
            continue;
        };
        let Some(agent) = pane
            .agent
            .as_mut()
            .filter(|agent| agent.attached && agent.presence_id.is_some())
        else {
            continue;
        };
        detach_agent(agent);
        pane.revision = revision;
        observe_agent_exchange(
            &mut state.persisted,
            pane_id,
            AgentState::Unknown,
            false,
            revision,
            unix_time_millis(),
        );
        clear_agent_wake(&mut state, pane_id);
        state.claude_reconciliations.remove(&pane_id);
        changed.push((pane_id, revision));
    }
    if changed.is_empty() {
        return;
    }
    if save_state(&daemon.state_path, &state.persisted).is_err() {
        state.persistence_dirty = true;
    }
    for (pane_id, revision) in changed {
        publish(
            daemon,
            &mut state,
            revision,
            "agent.presence_expired",
            pane_id.0,
        );
    }
}

fn retry_dirty_persistence(daemon: &Daemon) {
    let mut state = lock(&daemon.state);
    if state.persistence_dirty && save_state(&daemon.state_path, &state.persisted).is_ok() {
        state.persistence_dirty = false;
    }
}

fn advance_replacement(daemon: &Daemon) -> bool {
    let mut state = lock(&daemon.state);
    if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Intentional);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
    }
    let replacement_ready = if state.stopping || state.replacement_executable.is_some() {
        false
    } else {
        state
            .replacement_target
            .clone()
            .is_some_and(|target| replacement_blockers(&mut state, &target).is_empty())
    };
    if replacement_ready {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Replacement);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
    }
    state.stopping
}

struct ClientGuard(Arc<AtomicUsize>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn acquire_singleton_lock(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsxd singleton lock",
        ));
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(file)
    } else {
        let error = io::Error::last_os_error();
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another wsxd owns {}: {error}", path.display()),
        ))
    }
}

/// Listen on `socket`, which appears only once it is owner-only: a client refuses a socket
/// another user could reach, so one that connected between a bind and a chmod failed.
/// ^ The name gains one character, so it stays within the socket path limit; the
/// singleton lock keeps a second wsxd from using the same name.
fn bind_owner_only(socket: &Path) -> io::Result<UnixListener> {
    let mut name = socket.file_name().unwrap_or_default().to_os_string();
    name.push("~");
    let staging = socket.with_file_name(name);
    let _ = fs::remove_file(&staging);
    let listener = UnixListener::bind(&staging)?;
    if let Err(error) = fs::set_permissions(&staging, fs::Permissions::from_mode(0o600))
        .and_then(|()| fs::rename(&staging, socket))
    {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    Ok(listener)
}

fn cleanup(daemon: &Daemon, socket: &Path) {
    let (runtimes, reason, preserve_runtimes) = {
        let mut state = lock(&daemon.state);
        state.stopping = true;
        state.leases.clear();
        state.transferred_streams.clear();
        state.agent_wake_leases.clear();
        state.agent_presence_leases.clear();
        state.agent_wake_tokens.clear();
        state.claude_reconciliations.clear();
        state.plugin_events.clear();
        let runtimes = std::mem::take(&mut state.runtimes);
        state.runtime_generations.clear();
        let reason = state.stop_reason.unwrap_or(StopReason::Unexpected);
        let preserve_runtimes = reason == StopReason::Handoff;
        if !preserve_runtimes {
            let _ = save_state(&daemon.state_path, &state.persisted);
        }
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        let marker = if matches!(reason, StopReason::Replacement | StopReason::Handoff) {
            let target = state.replacement_target.as_deref().unwrap_or_default();
            match state
                .replacement_daemon_revision
                .filter(|revision| *revision > 0)
            {
                Some(revision) => format!("replacement:{target}|{revision}"),
                None => format!("replacement:{target}"),
            }
        } else {
            reason.marker().to_string()
        };
        (runtimes, marker, preserve_runtimes)
    };
    if !preserve_runtimes {
        for runtime in runtimes.into_values() {
            runtime.terminate();
        }
    }
    if !preserve_runtimes {
        let _ = write_lifecycle_marker(&daemon.lifecycle_path, &reason);
        let _ = fs::remove_file(socket);
    }
}

fn serve_client(mut stream: UnixStream, daemon: Arc<Daemon>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(line) = read_bounded_line(&mut reader)? else {
        return Ok(());
    };
    let request = match serde_json::from_str::<Request>(&line) {
        Ok(request) => request,
        Err(error) => {
            return write_response(
                &mut stream,
                Response::Error(ApiError::new("invalid_json", error.to_string())),
            )
        }
    };
    let Request::Hello { protocol } = request else {
        return write_response(
            &mut stream,
            Response::Error(ApiError::new(
                "handshake_required",
                "hello must be the first request on a connection",
            )),
        );
    };
    let compatible = protocol == PROTOCOL_VERSION;
    write_response(
        &mut stream,
        Response::Hello {
            protocol: PROTOCOL_VERSION,
            epoch: daemon.epoch,
            capabilities: capabilities(),
        },
    )?;
    if !compatible {
        let Some(line) = read_bounded_line(&mut reader)? else {
            return Ok(());
        };
        // ^ docs/daemon-live-upgrade.md: a request this daemon does not know is answered,
        // never dropped, so a client of another release reads a reason.
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(
                request @ (Request::Shutdown
                | Request::LifecycleStatus
                | Request::PrepareReplacement { .. }
                | Request::PrepareHandoff { .. }),
            ) => handle(&daemon, request),
            Ok(_) => Response::Error(ApiError::new(
                "protocol_mismatch",
                format!("client {protocol}, daemon {PROTOCOL_VERSION}"),
            )),
            Err(error) => Response::Error(ApiError::new("invalid_json", error.to_string())),
        };
        return write_response(&mut stream, response);
    }

    while let Some(line) = read_bounded_line(&mut reader)? {
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stream,
                    Response::Error(ApiError::new("invalid_json", error.to_string())),
                )?;
                continue;
            }
        };
        let (kind, lease, size) = match request {
            Request::TerminalSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            } => (
                TerminalStreamKind::Frames,
                (pane_id, client_id, takeover),
                (rows, cols),
            ),
            Request::TerminalRawSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            } => (
                TerminalStreamKind::Raw,
                (pane_id, client_id, takeover),
                (rows, cols),
            ),
            request => {
                write_response(&mut stream, handle(&daemon, request))?;
                continue;
            }
        };
        return serve_terminal_stream(stream, daemon, kind, lease, size);
    }
    Ok(())
}

fn read_bounded_line(reader: &mut BufReader<UnixStream>) -> io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_REQUEST_BYTES || !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request is incomplete or exceeds limit",
        ));
    }
    Ok(Some(line))
}

fn write_response(stream: &mut UnixStream, response: Response) -> io::Result<()> {
    let bytes = encode_line(&response).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds limit",
        ));
    }
    stream.write_all(&bytes)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalStreamKind {
    /// Rendered cell frames for the TUI.
    Frames,
    /// PTY bytes for a client that runs its own emulator (docs/terminal-raw-stream.md).
    Raw,
}

fn serve_terminal_stream(
    mut stream: UnixStream,
    daemon: Arc<Daemon>,
    kind: TerminalStreamKind,
    (pane_id, client_id, takeover): (PaneId, u64, bool),
    (rows, cols): (u16, u16),
) -> io::Result<()> {
    let (acquire_revision, lease_generation) =
        match acquire_terminal_lease(&daemon, pane_id, client_id, takeover) {
            Ok(acquired) => acquired,
            Err(error) => {
                write_response(&mut stream, Response::Error(error))?;
                return Ok(());
            }
        };
    if let Err(error) = touch_terminal_project(&daemon, pane_id) {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    // A raw subscriber may keep the pane's size: it then scales its own view.
    let keep_size = kind == TerminalStreamKind::Raw && rows == 0 && cols == 0;
    let resized = if keep_size {
        Ok(())
    } else {
        resize_terminal(
            &daemon,
            pane_id,
            LeaseAccess::Stream {
                client_id,
                generation: lease_generation,
            },
            rows,
            cols,
        )
        .map(|_| ())
    };
    if let Err(error) = resized {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    // Clipboard writes are effects for the client that was attached when they occurred.
    // Never replay writes captured before this stream became active.
    if let Err(error) =
        with_stream_runtime(&daemon, pane_id, client_id, lease_generation, |runtime| {
            let _ = runtime.take_clipboard_writes();
            Ok(())
        })
    {
        release_stream_lease(&daemon, pane_id, client_id, lease_generation);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    write_response(
        &mut stream,
        Response::Ack {
            revision: acquire_revision,
        },
    )?;

    stream.set_read_timeout(None)?;
    let active = Arc::new(AtomicBool::new(true));
    let resync = Arc::new(AtomicBool::new(true));
    let input_error = Arc::new(Mutex::new(None::<ApiError>));
    let input_stream = stream.try_clone()?;
    let input_daemon = Arc::clone(&daemon);
    let input_active = Arc::clone(&active);
    let input_resync = Arc::clone(&resync);
    let input_error_slot = Arc::clone(&input_error);
    let input_thread = thread::Builder::new()
        .name("wsxd-terminal-input".into())
        .spawn(move || {
            let mut reader = BufReader::new(input_stream);
            loop {
                let message = match read_bounded_line(&mut reader) {
                    Ok(Some(line)) => match serde_json::from_str::<TerminalClientMessage>(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            *lock(&input_error_slot) =
                                Some(ApiError::new("invalid_stream_message", error.to_string()));
                            break;
                        }
                    },
                    Ok(None) => break,
                    Err(error) => {
                        *lock(&input_error_slot) =
                            Some(ApiError::new("stream_read_failed", error.to_string()));
                        break;
                    }
                };
                match message {
                    TerminalClientMessage::Resync => {
                        input_resync.store(true, Ordering::Release);
                        input_daemon.changed.notify_all();
                        continue;
                    }
                    TerminalClientMessage::Detach => break,
                    message => {
                        if let Err(error) = handle_terminal_stream_input(
                            &input_daemon,
                            pane_id,
                            client_id,
                            lease_generation,
                            message,
                        ) {
                            *lock(&input_error_slot) = Some(error);
                            break;
                        }
                    }
                }
            }
            input_active.store(false, Ordering::Release);
            input_daemon.changed.notify_all();
        })?;

    let lease = StreamLease {
        pane_id,
        client_id,
        generation: lease_generation,
    };
    let result = match kind {
        TerminalStreamKind::Frames => {
            stream_terminal_updates(&mut stream, &daemon, lease, &active, &resync, &input_error)
        }
        TerminalStreamKind::Raw => {
            stream_raw_terminal_output(&mut stream, &daemon, lease, &active, &resync, &input_error)
        }
    };
    active.store(false, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = input_thread.join();
    release_stream_lease(&daemon, pane_id, client_id, lease_generation);
    result
}

fn stream_terminal_updates(
    stream: &mut UnixStream,
    daemon: &Arc<Daemon>,
    lease: StreamLease,
    active: &AtomicBool,
    resync: &AtomicBool,
    input_error: &Mutex<Option<ApiError>>,
) -> io::Result<()> {
    let mut baseline = None;
    let mut last_frame_sent = Instant::now();
    while active.load(Ordering::Acquire) {
        // The handoff holds every pane operation lock, so tell the client before waiting on one.
        let handoff = terminal_stream_lease_error(&lock(&daemon.state), lease, Instant::now())
            .filter(|error| error.code == "daemon_handoff");
        if let Some(error) = handoff {
            write_stream_message(stream, &TerminalServerMessage::Error(error))?;
            break;
        }
        // ^ docs/terminal-raw-stream.md: messages are sampled under the pane operation
        // lock and written after it, so a slow reader cannot delay input, resize or a takeover.
        let mut messages = Vec::new();
        let mut ended = false;
        let operation_lock = terminal_operation_lock(daemon, lease.pane_id);
        let operation = lock(&operation_lock);
        let runtime = match leased_stream_runtime(&lock(&daemon.state), lease) {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(operation);
                write_stream_message(stream, &TerminalServerMessage::Error(error))?;
                break;
            }
        };
        let exited = runtime.exited();
        if resync.swap(false, Ordering::AcqRel) {
            baseline = None;
        }
        let frame_due = baseline.is_none() || last_frame_sent.elapsed() >= PRESENTATION_CADENCE;
        let sample = runtime.presentation_sample(baseline, frame_due);
        let sample_revision = sample.revision;
        let synchronized_output = sample.synchronized_output;
        messages.extend(
            sample
                .clipboard_writes
                .into_iter()
                .map(TerminalServerMessage::ClipboardWrite),
        );
        let mut frame_sent = false;
        if let Some(error) = lock(input_error).take() {
            messages.push(TerminalServerMessage::Error(error));
            ended = true;
        } else if exited {
            messages.push(TerminalServerMessage::Exited);
            ended = true;
        } else {
            match sample.update {
                Ok(Some(update)) => {
                    baseline = Some(update.revision());
                    messages.push(TerminalServerMessage::Update(update));
                    frame_sent = true;
                }
                Ok(None) => {}
                Err(error) => {
                    messages.push(TerminalServerMessage::Error(ApiError::new(
                        "frame_failed",
                        error.to_string(),
                    )));
                    ended = true;
                }
            }
        }
        drop(operation);
        for message in &messages {
            write_stream_message(stream, message)?;
        }
        if frame_sent {
            last_frame_sent = Instant::now();
        }
        if ended {
            break;
        }
        let state = lock(&daemon.state);
        // ^ [[wsx Architecture]] Recheck every wake predicate while holding the
        // notifier mutex. Frames wait only for the bounded presentation cadence;
        // effects bypass it in the atomically sampled FIFO above.
        if !active.load(Ordering::Acquire) || resync.load(Ordering::Acquire) {
            continue;
        }
        let current_revision = runtime.revision();
        if synchronized_output && sample_revision != current_revision {
            continue;
        }
        let wait = if !synchronized_output && baseline != Some(current_revision) {
            PRESENTATION_CADENCE.saturating_sub(last_frame_sent.elapsed())
        } else {
            Duration::from_millis(250)
        };
        if wait.is_zero() {
            continue;
        }
        let _ = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    Ok(())
}

// ^ docs/terminal-raw-stream.md: a replay, then the PTY bytes after it. A subscriber
// that falls behind the tap gets a new replay instead of a gap. Messages are sampled
// under the pane operation lock and written after it, so a slow reader cannot delay
// a takeover of the pane.
fn stream_raw_terminal_output(
    stream: &mut UnixStream,
    daemon: &Arc<Daemon>,
    lease: StreamLease,
    active: &AtomicBool,
    resync: &AtomicBool,
    input_error: &Mutex<Option<ApiError>>,
) -> io::Result<()> {
    let mut position = None::<u64>;
    let mut size = (0, 0);
    while active.load(Ordering::Acquire) {
        // The handoff holds every pane operation lock, so tell the client before waiting on one.
        let handoff = terminal_stream_lease_error(&lock(&daemon.state), lease, Instant::now())
            .filter(|error| error.code == "daemon_handoff");
        if let Some(error) = handoff {
            write_stream_message(stream, &TerminalRawServerMessage::Error(error))?;
            break;
        }
        let mut messages = Vec::new();
        let mut ended = false;
        let operation_lock = terminal_operation_lock(daemon, lease.pane_id);
        let operation = lock(&operation_lock);
        let runtime = match leased_stream_runtime(&lock(&daemon.state), lease) {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(operation);
                write_stream_message(stream, &TerminalRawServerMessage::Error(error))?;
                break;
            }
        };
        // Read the exit flag and the revision first: bytes ingested before the exit
        // are still sent, and a later byte or resize changes the revision.
        let exited = runtime.exited();
        let revision = runtime.revision();
        if resync.swap(false, Ordering::AcqRel) {
            position = None;
        }
        let mut drained = false;
        // ^ Bytes of an open synchronized update wait for its end, so a client emulator without
        // mode 2026 (xterm.js 5.5) never draws half an update (docs/terminal-synchronized-output.md).
        let mut holding = false;
        let sampled = match position {
            None => runtime.raw_replay().map(|replay| {
                position = Some(replay.sequence);
                size = (replay.rows, replay.cols);
                messages.push(TerminalRawServerMessage::Replay(TerminalRawReplay {
                    ansi: replay.ansi,
                    rows: replay.rows,
                    cols: replay.cols,
                    truncated: replay.truncated,
                }));
            }),
            Some(after) => runtime.raw_sample(after).map(|sample| {
                messages.extend(
                    sample
                        .clipboard_writes
                        .into_iter()
                        .map(TerminalRawServerMessage::ClipboardWrite),
                );
                if (sample.rows, sample.cols) != size {
                    // Two emulators reflow a resized screen differently, so the
                    // client restarts from this one's screen: a replay follows.
                    size = (sample.rows, sample.cols);
                    messages.push(TerminalRawServerMessage::Resized {
                        rows: sample.rows,
                        cols: sample.cols,
                    });
                    position = None;
                    return;
                }
                match sample.output {
                    None => position = None,
                    Some(mut bytes) => {
                        if sample.synchronized_output {
                            let open = synchronized_update_start(&bytes).unwrap_or(0);
                            holding = open < bytes.len();
                            bytes.truncate(open);
                        }
                        drained = bytes.is_empty() && !holding;
                        position = Some(after + bytes.len() as u64);
                        if !bytes.is_empty() {
                            messages.push(TerminalRawServerMessage::Output(bytes));
                        }
                    }
                }
            }),
        };
        if let Err(error) = sampled {
            messages.push(TerminalRawServerMessage::Error(ApiError::new(
                "replay_failed",
                error.to_string(),
            )));
            ended = true;
        } else if let Some(error) = lock(input_error).take() {
            messages.push(TerminalRawServerMessage::Error(error));
            ended = true;
        } else if exited && drained {
            messages.push(TerminalRawServerMessage::Exited);
            ended = true;
        }
        drop(operation);
        for message in &messages {
            write_stream_message(stream, message)?;
        }
        if ended {
            break;
        }
        let state = lock(&daemon.state);
        // Recheck every wake predicate while holding the notifier mutex.
        if !active.load(Ordering::Acquire)
            || resync.load(Ordering::Acquire)
            || (!drained && !holding)
            || runtime.exited() != exited
            || runtime.revision() != revision
        {
            continue;
        }
        // ^ A held update is sent when it ends, which new output wakes, or when its bound
        // expires, which nothing wakes: look again soon.
        let wait = if holding {
            SYNCHRONIZED_HOLD_POLL
        } else {
            Duration::from_millis(250)
        };
        let _ = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    Ok(())
}

const SYNCHRONIZED_HOLD_POLL: Duration = Duration::from_millis(50);

/// Where the open synchronized update (`CSI ? 2026 h`) starts in these bytes, if it does:
/// the bytes before it are complete and may go now.
fn synchronized_update_start(bytes: &[u8]) -> Option<usize> {
    const START: &[u8] = b"\x1b[?2026h";
    bytes
        .windows(START.len())
        .rposition(|window| window == START)
}

/// The runtime of a stream that still holds its pane lease.
fn leased_stream_runtime(
    state: &State,
    lease: StreamLease,
) -> Result<Arc<TerminalRuntime>, ApiError> {
    if let Some(error) = terminal_stream_lease_error(state, lease, Instant::now()) {
        return Err(error);
    }
    state.runtimes.get(&lease.pane_id).cloned().ok_or_else(|| {
        ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        )
    })
}

fn record_transferred_stream(state: &mut State, stream: StreamLease) {
    while state.transferred_streams.len() >= MAX_CLIENTS {
        state.transferred_streams.pop_front();
    }
    state.transferred_streams.push_back(stream);
}

/// An exchange types into the pane, so a live terminal controller blocks it. With
/// `takeover`, a person's own action, it takes the pane instead: the controller's stream
/// ends with `lease_transferred`, as after `acquire_terminal_lease` with a takeover. The
/// caller holds the pane operation lock, so no controller can come back before the write.
fn take_terminal_lease(state: &mut State, pane_id: PaneId, takeover: bool) -> Result<(), ApiError> {
    let Some(lease) = state
        .leases
        .get(&pane_id)
        .copied()
        .filter(|lease| lease.expires_at > Instant::now())
    else {
        return Ok(());
    };
    if !takeover {
        return Err(api("terminal_busy", "pane has another writable controller"));
    }
    state.leases.remove(&pane_id);
    record_transferred_stream(
        state,
        StreamLease {
            pane_id,
            client_id: lease.client_id,
            generation: lease.generation,
        },
    );
    Ok(())
}

fn terminal_stream_lease_error(
    state: &State,
    stream: StreamLease,
    now: Instant,
) -> Option<ApiError> {
    if state.handoff_in_progress || state.stop_reason == Some(StopReason::Handoff) {
        return Some(ApiError::new(
            "daemon_handoff",
            "wsxd is handing this terminal to a newer daemon",
        ));
    }
    if state.stopping {
        return Some(ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        ));
    }
    if state.leases.get(&stream.pane_id).is_some_and(|current| {
        current.client_id == stream.client_id
            && current.generation == stream.generation
            && current.expires_at > now
    }) {
        return None;
    }
    if state.transferred_streams.contains(&stream) {
        Some(ApiError::new(
            "lease_transferred",
            "terminal control moved to another wsx instance",
        ))
    } else {
        Some(ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        ))
    }
}

fn write_stream_message(stream: &mut UnixStream, message: &impl Serialize) -> io::Result<()> {
    let bytes = encode_line(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal update exceeds limit",
        ));
    }
    stream.write_all(&bytes)
}

fn release_stream_lease(daemon: &Daemon, pane_id: PaneId, client_id: u64, lease_generation: u64) {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    let stream = StreamLease {
        pane_id,
        client_id,
        generation: lease_generation,
    };
    state
        .transferred_streams
        .retain(|candidate| *candidate != stream);
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.client_id == client_id && lease.generation == lease_generation)
    {
        state.leases.remove(&pane_id);
        let runtime = (!state.stopping && !state.handoff_in_progress)
            .then(|| state.runtimes.get(&pane_id).cloned())
            .flatten();
        drop(state);
        if let Some(runtime) = runtime {
            let _ = runtime.clear_selection();
        }
    }
}

fn release_client_lease(daemon: &Daemon, pane_id: PaneId, client_id: u64) -> Result<u64, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    require_lease(&state, pane_id, LeaseAccess::Client(client_id))?;
    state.leases.remove(&pane_id);
    let revision = state.revision;
    let runtime = state.runtimes.get(&pane_id).cloned();
    drop(state);
    if let Some(runtime) = runtime {
        runtime.clear_selection().map_err(terminal_api)?;
    }
    Ok(revision)
}

fn with_stream_runtime<T>(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    lease_generation: u64,
    operation: impl FnOnce(&TerminalRuntime) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let (runtime, _) = leased_runtime(
        daemon,
        pane_id,
        LeaseAccess::Stream {
            client_id,
            generation: lease_generation,
        },
    )?;
    operation(&runtime)
}

fn key_interrupts_agent(key: &KeyEvent) -> bool {
    key.code == KeyCode::Escape
        || (key.code == KeyCode::Text && key.control && key.text.eq_ignore_ascii_case("c"))
}

fn request_claude_reconcile(daemon: &Daemon, pane_id: PaneId, interrupted: bool) {
    if !interrupted {
        return;
    }
    let now = Instant::now();
    let mut state = lock(&daemon.state);
    let Some(agent) = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| {
            agent.attached && agent.provider == "claude" && agent.capabilities.escape_interrupts
        })
    else {
        return;
    };
    let event_state = agent.state;
    let interrupt_revision = state
        .runtimes
        .get(&pane_id)
        .map(|runtime| runtime.revision());
    state
        .claude_reconciliations
        .entry(pane_id)
        .and_modify(|reconciliation| {
            reconciliation.terminal_revision = None;
            reconciliation.interrupt_requested = true;
            reconciliation.interrupt_revision = interrupt_revision;
        })
        .or_insert(ClaudeReconciliation {
            event_state,
            event_at: now
                .checked_sub(claude_status::WORKING_EVENT_LEAD)
                .unwrap_or(now),
            terminal_revision: None,
            terminal: None,
            interrupt_requested: true,
            interrupt_revision,
        });
}

fn handle_terminal_stream_input(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    lease_generation: u64,
    message: TerminalClientMessage,
) -> Result<(), ApiError> {
    match message {
        TerminalClientMessage::Input(bytes) => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "input exceeds limit"));
            }
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.write(&bytes).map_err(terminal_api)
            })?;
            request_claude_reconcile(daemon, pane_id, bytes == [3]);
            Ok(())
        }
        TerminalClientMessage::Key(key) => {
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.key(&key).map_err(terminal_api)
            })?;
            // Claude has no interrupt hook. Input only requests a fresh terminal
            // observation; current provider UI evidence decides the resulting state.
            // ^ https://code.claude.com/docs/en/hooks#stop
            request_claude_reconcile(daemon, pane_id, key_interrupts_agent(&key));
            Ok(())
        }
        TerminalClientMessage::Paste(text) => {
            if text.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "paste exceeds limit"));
            }
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.paste(&text).map_err(terminal_api)
            })
        }
        TerminalClientMessage::Mouse(mouse) => {
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.mouse(&mouse).map_err(terminal_api)
            })
        }
        TerminalClientMessage::Resize { rows, cols } => {
            resize_terminal(
                daemon,
                pane_id,
                LeaseAccess::Stream {
                    client_id,
                    generation: lease_generation,
                },
                rows,
                cols,
            )?;
            Ok(())
        }
        TerminalClientMessage::Heartbeat => {
            let mut state = lock(&daemon.state);
            refresh_lease(
                &mut state,
                pane_id,
                LeaseAccess::Stream {
                    client_id,
                    generation: lease_generation,
                },
            )
        }
        TerminalClientMessage::Resync | TerminalClientMessage::Detach => Err(api(
            "invalid_stream_message",
            "stream control message reached the input handler",
        )),
    }
}

fn acquire_terminal_lease(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    takeover: bool,
) -> Result<(u64, u64), ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let state = lock(&daemon.state);
    if state.stopping || state.handoff_in_progress {
        return Err(api(
            "daemon_replacing",
            "wsxd is replacing its runtime owner",
        ));
    }
    let runtime = Arc::clone(require_runtime(&state, pane_id)?);
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.expires_at > Instant::now() && lease.client_id != client_id)
        && !takeover
    {
        return Err(api("terminal_busy", "pane has another writable controller"));
    }
    drop(state);

    // ^ Selection cleanup can notify through the runtime callback, which reacquires daemon state.
    runtime.clear_selection().map_err(terminal_api)?;

    let mut state = lock(&daemon.state);
    require_runtime(&state, pane_id)?;
    let now = Instant::now();
    let displaced = state.leases.get(&pane_id).and_then(|lease| {
        (lease.expires_at > now && lease.client_id != client_id).then_some(StreamLease {
            pane_id,
            client_id: lease.client_id,
            generation: lease.generation,
        })
    });
    if let Some(displaced) = displaced {
        record_transferred_stream(&mut state, displaced);
    }
    let generation = NEXT_LEASE_GENERATION.fetch_add(1, Ordering::Relaxed);
    state.leases.insert(
        pane_id,
        Lease {
            client_id,
            generation,
            expires_at: now + LEASE_TTL,
        },
    );
    Ok((state.revision, generation))
}

fn handle(daemon: &Arc<Daemon>, request: Request) -> Response {
    match handle_inner(daemon, request) {
        Ok(response) => response,
        Err(error) => Response::Error(error),
    }
}

fn handle_inner(daemon: &Arc<Daemon>, request: Request) -> Result<Response, ApiError> {
    // ^ An exchange delivery takes the mutation lock itself and releases it while the PTY
    // takes the prompt: see deliver_exchange_round.
    let _mutation = (!matches!(
        &request,
        Request::Hello { .. }
            | Request::Snapshot
            | Request::Poll { .. }
            | Request::View { .. }
            | Request::AgentExchangeWait { .. }
            | Request::PluginList
            | Request::LifecycleStatus
            | Request::AgentExchangeCreate { .. }
            | Request::AgentExchangeCreateStashingDraft { .. }
            | Request::AgentExchangeCreateKeyed { .. }
            | Request::AgentExchangeContinue { .. }
            | Request::AgentExchangeContinueStashingDraft { .. }
            | Request::AgentExchangeContinueKeyed { .. }
            | Request::AgentSteer { .. }
    ))
    .then(|| lock(&daemon.mutation_lock));
    if !matches!(&request, Request::Hello { .. } | Request::LifecycleStatus)
        && lock(&daemon.state).handoff_in_progress
    {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    match request {
        Request::Hello { protocol } => {
            if protocol != PROTOCOL_VERSION {
                return Err(api(
                    "protocol_mismatch",
                    format!("client {protocol}, daemon {PROTOCOL_VERSION}"),
                ));
            }
            Ok(Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: daemon.epoch,
                capabilities: capabilities(),
            })
        }
        Request::Snapshot => Ok(Response::Snapshot(snapshot(daemon, &lock(&daemon.state)))),
        Request::Poll {
            after_revision,
            timeout_ms,
            tui,
        } => poll(daemon, after_revision, timeout_ms, tui),
        Request::SynchronizeProjects { projects } => synchronize_projects(daemon, projects),
        Request::SessionCreate {
            worktree_id,
            label,
            command,
            initial_input,
            rows,
            cols,
        } => create_session(
            daemon,
            worktree_id,
            label,
            command,
            initial_input,
            rows,
            cols,
        ),
        Request::SessionRename {
            session_id,
            label,
            expected_revision,
        } => mutate(daemon, |persisted, _revision| {
            let session = session_mut(persisted, session_id)?;
            expect_revision(session.revision, expected_revision)?;
            session.label = bounded_label(label)?;
            Ok(("session.renamed", session_id.0))
        }),
        Request::SessionReorder {
            session_id,
            target_session_id,
            placement,
            expected_revision,
        } => reorder_session(
            daemon,
            session_id,
            target_session_id,
            placement,
            expected_revision,
        ),
        Request::SessionClose {
            session_id,
            expected_revision,
        } => close_session(daemon, session_id, expected_revision),
        Request::SessionRestart {
            pane_id,
            expected_revision,
        } => restart_session(daemon, pane_id, expected_revision),
        Request::PaneSplit {
            session_id,
            target,
            axis,
            label,
            command,
            initial_input,
            rows,
            cols,
            expected_revision,
        } => split_pane(
            daemon,
            session_id,
            target,
            axis,
            label,
            command,
            initial_input,
            rows,
            cols,
            expected_revision,
        ),
        Request::PaneFocus {
            session_id,
            pane_id,
        } => mutate(daemon, |persisted, revision| {
            let session = session_mut(persisted, session_id)?;
            if !session.panes.contains(&pane_id) {
                return Err(api("not_found", "pane is not in session"));
            }
            session.focused_pane = pane_id;
            session.revision = revision;
            Ok(("pane.focused", pane_id.0))
        }),
        Request::PaneClose {
            pane_id,
            expected_revision,
        } => close_pane(daemon, pane_id, expected_revision),
        Request::TerminalAcquire {
            pane_id,
            client_id,
            takeover,
        } => {
            let (revision, _) = acquire_terminal_lease(daemon, pane_id, client_id, takeover)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalRelease { pane_id, client_id } => {
            let revision = release_client_lease(daemon, pane_id, client_id)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalHeartbeat { pane_id, client_id } => mutate_runtime(daemon, |state| {
            refresh_lease(state, pane_id, LeaseAccess::Client(client_id))
        }),
        Request::TerminalSubscribe { .. } | Request::TerminalRawSubscribe { .. } => Err(api(
            "invalid_request",
            "a terminal subscribe is only valid as a connection upgrade",
        )),
        Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        } => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "input exceeds limit"));
            }
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.write(&bytes).map_err(terminal_api)?;
            request_claude_reconcile(daemon, pane_id, bytes == [3]);
            Ok(Response::Ack { revision })
        }
        Request::TerminalKey {
            pane_id,
            client_id,
            key,
        } => {
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.key(&key).map_err(terminal_api)?;
            request_claude_reconcile(daemon, pane_id, key_interrupts_agent(&key));
            Ok(Response::Ack { revision })
        }
        Request::TerminalPaste {
            pane_id,
            client_id,
            text,
        } => {
            if text.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "paste exceeds limit"));
            }
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.paste(&text).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalMouse {
            pane_id,
            client_id,
            mouse,
        } => {
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.mouse(&mouse).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalResize {
            pane_id,
            client_id,
            rows,
            cols,
        } => resize_terminal(daemon, pane_id, LeaseAccess::Client(client_id), rows, cols),
        Request::View { pane_ids } => view(daemon, pane_ids),
        Request::AgentReport {
            pane_id,
            runtime_generation,
            provider,
            state: agent_state,
            attached,
            presence_id,
            conversation_id,
            session_ref,
            wake_token,
            capabilities,
            received,
            compacting,
        } => agent_report_with_attachment(
            daemon,
            RuntimeAgentAuthority::new(pane_id, runtime_generation),
            AgentReportInput {
                provider,
                state: agent_state,
                attached,
                presence_id,
                conversation_id,
                session_ref,
                wake_token,
                capabilities,
                received,
                compacting,
            },
        ),
        Request::AgentPresenceRenew {
            pane_id,
            runtime_generation,
            presence_id,
        } => renew_agent_presence(daemon, pane_id, runtime_generation, presence_id),
        Request::AgentWakeRenew {
            pane_id,
            runtime_generation,
            wake_token,
        } => renew_agent_wake(daemon, pane_id, runtime_generation, wake_token),
        Request::AgentClear {
            pane_id,
            runtime_generation,
            next_runtime_generation,
        } => agent_clear(daemon, pane_id, runtime_generation, next_runtime_generation),
        Request::AgentExchangeCreate {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery::default(),
        ),
        Request::AgentExchangeCreateStashingDraft {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery {
                stash_draft: true,
                key: None,
            },
        ),
        Request::AgentExchangeCreateKeyed {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            key,
            stash_draft,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery {
                stash_draft,
                key: Some(key),
            },
        ),
        Request::AgentExchangeContinueStashingDraft {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery {
                stash_draft: true,
                key: None,
            },
        ),
        Request::AgentExchangeContinue {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery::default(),
        ),
        Request::AgentExchangeContinueKeyed {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            key,
            stash_draft,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery {
                stash_draft,
                key: Some(key),
            },
        ),
        Request::AgentExchangeGet {
            exchange_id,
            include_frame,
        } => get_agent_exchange(daemon, exchange_id, include_frame),
        Request::AgentExchangeList { pane_id } => list_agent_exchanges(daemon, pane_id),
        Request::AgentExchangeWait {
            exchange_id,
            after_revision,
            timeout_ms,
            include_frame,
        } => wait_agent_exchange(
            daemon,
            exchange_id,
            after_revision,
            timeout_ms,
            include_frame,
        ),
        Request::AgentExchangeReceipt {
            exchange_id,
            round,
            runtime_generation,
            receipt,
        } => {
            report_agent_exchange_receipt(daemon, exchange_id, round, &runtime_generation, receipt)
        }
        Request::AgentExchangeBoundReceipt {
            exchange_id,
            pane_id,
            round,
            runtime_generation,
            delivery_sha256,
            input_id,
            receipt,
        } => report_agent_exchange_receipt_with_binding(
            daemon,
            exchange_id,
            round,
            &runtime_generation,
            receipt,
            Some((pane_id, &delivery_sha256, &input_id)),
        ),
        Request::AgentExchangeCancel {
            exchange_id,
            expected_revision,
            takeover,
        } => cancel_agent_exchange(daemon, exchange_id, expected_revision, takeover),
        Request::AgentSteer {
            pane_id,
            text,
            takeover,
        } => steer_agent(daemon, pane_id, &text, takeover),
        Request::PluginList => Ok(Response::Plugins(lock(&daemon.state).plugins.clone())),
        Request::PluginReload => {
            let mut state = lock(&daemon.state);
            state.plugins = plugins::discover();
            bump(daemon, &mut state, "plugins", 0);
            Ok(Response::Plugins(state.plugins.clone()))
        }
        Request::PluginReviewCancel { request_id } => {
            daemon.review_jobs.cancel(&request_id);
            Ok(Response::Ack {
                revision: lock(&daemon.state).revision,
            })
        }
        Request::PluginReview {
            plugin_id,
            worktree_id,
            request_id,
            comparison,
            limits,
            operation,
        } => {
            let (plugin, path) = {
                let state = lock(&daemon.state);
                let plugin = state
                    .plugins
                    .iter()
                    .find(|plugin| {
                        plugin.enabled
                            && plugin.id == plugin_id
                            && plugin
                                .worktree_review
                                .as_ref()
                                .is_some_and(|spec| spec.comparisons.contains(&comparison))
                    })
                    .cloned()
                    .ok_or_else(|| api("plugin_unavailable", "review provider unavailable"))?;
                let path = state
                    .persisted
                    .worktrees
                    .iter()
                    .find(|w| w.id == worktree_id)
                    .map(|w| w.path.clone())
                    .ok_or_else(|| api("worktree_not_found", "worktree unavailable"))?;
                (plugin, path)
            };
            let request = ReviewRequest {
                api_version: REVIEW_API_VERSION,
                request_id,
                worktree_id,
                worktree_path: path,
                comparison,
                limits,
                operation,
            };
            request
                .validate()
                .map_err(|message| api("invalid_review_request", message))?;
            let cancelled = daemon
                .review_jobs
                .start(&request.request_id)
                .map_err(|error| api("review_unavailable", error.to_string()))?;
            let result = plugins::review::invoke(&plugin, &request, &cancelled);
            daemon.review_jobs.finish(&request.request_id);
            result
                .map(|response| Response::PluginReview {
                    epoch: daemon.epoch,
                    response,
                })
                .map_err(|error| {
                    api(
                        match error.kind() {
                            io::ErrorKind::TimedOut => "review_timeout",
                            io::ErrorKind::Interrupted => "review_cancelled",
                            io::ErrorKind::InvalidData => "review_invalid_output",
                            _ => "review_unavailable",
                        },
                        error.to_string(),
                    )
                })
        }
        Request::PluginRender {
            plugin_id,
            pane_id,
            columns,
            rows,
            generation,
        } => render_plugin_view(daemon, plugin_id, pane_id, columns, rows, generation),
        Request::LifecycleStatus => {
            let mut state = lock(&daemon.state);
            Ok(Response::Lifecycle(lifecycle_status(daemon, &mut state)))
        }
        Request::PrepareReplacement {
            target_binary_id,
            target_daemon_revision,
        } => prepare_replacement_with_revision(daemon, target_binary_id, target_daemon_revision),
        Request::PrepareHandoff {
            target_binary_id,
            target_version,
            target_protocol,
            target_daemon_revision,
            executable,
            restart,
        } => prepare_handoff(
            daemon,
            target_binary_id,
            target_version,
            target_protocol,
            target_daemon_revision,
            executable,
            restart,
        ),
        Request::Shutdown => {
            let mut state = lock(&daemon.state);
            state.stopping = true;
            state.stop_reason = Some(StopReason::Intentional);
            daemon.changed.notify_all();
            daemon.plugin_changed.notify_all();
            Ok(Response::Ack {
                revision: state.revision,
            })
        }
    }
}

fn render_plugin_view(
    daemon: &Daemon,
    plugin_id: String,
    pane_id: PaneId,
    columns: u16,
    rows: u16,
    generation: u64,
) -> Result<Response, ApiError> {
    if !plugins::valid_token(&plugin_id) || columns == 0 || rows == 0 || columns > 500 || rows > 512
    {
        return Err(api(
            "invalid_plugin_view",
            "plugin view dimensions are invalid",
        ));
    }
    let (plugin, worktree_id, worktree_path) = {
        let state = lock(&daemon.state);
        let plugin = state
            .plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.id == plugin_id && plugin.sidecar.is_some())
            .cloned()
            .ok_or_else(|| api("plugin_unavailable", "plugin sidecar is unavailable"))?;
        let pane = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .ok_or_else(|| api("pane_not_found", "pane does not exist"))?;
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
            .ok_or_else(|| api("session_not_found", "session does not exist"))?;
        let worktree = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .ok_or_else(|| api("worktree_not_found", "worktree does not exist"))?;
        (plugin, worktree.id, worktree.path.clone())
    };
    plugins::render(
        &plugin,
        plugins::RenderContext {
            pane_id,
            worktree_id,
            worktree_path: &worktree_path,
            epoch: daemon.epoch,
            columns,
            rows,
            generation,
        },
    )
    .map(Response::PluginView)
    .map_err(|error| api("plugin_view_failed", error.to_string()))
}

fn capabilities() -> Capabilities {
    Capabilities {
        pane_splits: true,
        plugins: true,
        plugin_views: true,
        worktree_review_available: false,
        agent_reports: true,
        agent_session_restore: true,
        resume_shell_fallback: true,
        listening_ports: true,
        foreground_jobs: true,
        process_restore: false,
        lifecycle_coordination: true,
        version_coordination: true,
        daemon_revision_coordination: true,
        live_handoff: true,
        agent_exchanges: true,
        agent_exchange_bound_receipts: true,
        raw_terminal_stream: true,
        daemon_restart: true,
        exchange_command: true,
        exchange_shell: true,
        agent_steer: true,
    }
}

fn live_runtime_count(state: &State) -> usize {
    state
        .runtimes
        .values()
        .filter(|runtime| !runtime.exited())
        .count()
}

fn lifecycle_status(daemon: &Daemon, state: &mut State) -> DaemonLifecycle {
    expire_tui_presence(state, Instant::now());
    let target = state.replacement_target.clone();
    let blockers = target
        .as_deref()
        .map(|target| {
            if state.replacement_executable.is_some() {
                handoff_blockers(state, target)
            } else {
                replacement_blockers(state, target)
            }
        })
        .unwrap_or_default();
    let target_version = target
        .as_deref()
        .and_then(binary_identity_version)
        .unwrap_or_default()
        .to_string();
    DaemonLifecycle {
        protocol: PROTOCOL_VERSION,
        epoch: daemon.epoch,
        binary_id: daemon.binary_id.clone(),
        version: WSX_VERSION.to_string(),
        daemon_revision: DAEMON_REVISION,
        started_unix_ms: daemon.started_unix_ms,
        phase: if state.stopping {
            DaemonPhase::Stopping
        } else if target.is_some() {
            DaemonPhase::ReplacementPending
        } else {
            DaemonPhase::Ready
        },
        live_runtimes: live_runtime_count(state),
        active_clients: daemon.active_clients.load(Ordering::Acquire),
        active_tuis: state.tui_clients.len() + usize::from(state.legacy_tui_until.is_some()),
        recovered_from_backup: daemon.recovered_from_backup,
        replacement_target: target,
        replacement_target_version: target_version,
        replacement_blockers: blockers,
        last_handoff_error: state.last_handoff_error.clone(),
    }
}

fn replacement_blockers(state: &mut State, target_binary_id: &str) -> Vec<ReplacementBlocker> {
    let now = Instant::now();
    expire_tui_presence(state, now);
    let mut blockers = Vec::new();
    let target_daemon_revision = state.replacement_daemon_revision.unwrap_or_default();
    if state.legacy_tui_until.is_some()
        || state.tui_clients.values().any(|client| {
            if target_daemon_revision == 0 {
                client.target_binary_id != target_binary_id
            } else {
                client.target_daemon_revision != target_daemon_revision
            }
        })
    {
        blockers.push(ReplacementBlocker::OtherTui);
    }
    if has_fresh_working_agent(state, now) {
        blockers.push(ReplacementBlocker::WorkingAgent);
    }
    if state.recovery_in_progress {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    if live_runtime_count(state) > 0 && !state.listener_scan_complete {
        blockers.push(ReplacementBlocker::ListenerScanPending);
    }
    if !state.foreground_jobs.is_empty() {
        blockers.push(ReplacementBlocker::ForegroundJob);
    }
    if state
        .listening_ports
        .values()
        .any(|ports| !ports.is_empty())
    {
        blockers.push(ReplacementBlocker::ListeningPort);
    }
    blockers
}

fn replacement_response(
    state: &State,
    disposition: ReplacementDisposition,
    target_version: String,
    blockers: Vec<ReplacementBlocker>,
    use_current_daemon: bool,
) -> Response {
    Response::Replacement {
        disposition,
        live_runtimes: live_runtime_count(state),
        daemon_version: WSX_VERSION.to_string(),
        target_version,
        blockers,
        use_current_daemon,
    }
}

#[cfg(test)]
fn prepare_replacement(daemon: &Daemon, target_binary_id: String) -> Result<Response, ApiError> {
    prepare_replacement_with_revision(daemon, target_binary_id, 0)
}

fn prepare_replacement_with_revision(
    daemon: &Daemon,
    target_binary_id: String,
    target_daemon_revision: u32,
) -> Result<Response, ApiError> {
    if target_binary_id.is_empty() || target_binary_id.len() > 512 {
        return Err(api(
            "invalid_request",
            "replacement binary identity is invalid",
        ));
    }
    let target_version = binary_identity_version(&target_binary_id)
        .ok_or_else(|| {
            api(
                "invalid_request",
                "replacement binary identity has no valid wsx version",
            )
        })?
        .to_string();
    let candidate_order = compare_binary_identities(&target_binary_id, &daemon.binary_id)
        .ok_or_else(|| {
            api(
                "invalid_request",
                "replacement binary identity is malformed",
            )
        })?;
    let mut state = lock(&daemon.state);
    if (target_daemon_revision > 0 && target_daemon_revision <= DAEMON_REVISION)
        || (target_daemon_revision == 0 && candidate_order != std::cmp::Ordering::Greater)
    {
        return Ok(replacement_response(
            &state,
            ReplacementDisposition::Deferred,
            target_version,
            Vec::new(),
            true,
        ));
    }

    if let Some(current) = state.replacement_target.as_deref() {
        let order = match (state.replacement_daemon_revision, target_daemon_revision) {
            (Some(current), target) if current > 0 && target > 0 => target.cmp(&current),
            _ => compare_binary_identities(&target_binary_id, current)
                .ok_or_else(|| api("invalid_request", "pending binary identity is malformed"))?,
        };
        if order != std::cmp::Ordering::Greater {
            return Ok(replacement_response(
                &state,
                ReplacementDisposition::Deferred,
                target_version,
                vec![ReplacementBlocker::PendingTarget],
                false,
            ));
        }
    }
    state.replacement_target = Some(target_binary_id.clone());
    state.replacement_daemon_revision = Some(target_daemon_revision);
    let blockers = replacement_blockers(&mut state, &target_binary_id);
    let disposition = if blockers.is_empty() {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Replacement);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        ReplacementDisposition::Stopping
    } else {
        ReplacementDisposition::Deferred
    };
    Ok(replacement_response(
        &state,
        disposition,
        target_version,
        blockers,
        false,
    ))
}

fn prepare_handoff(
    daemon: &Daemon,
    target_binary_id: String,
    target_version: String,
    target_protocol: u32,
    target_daemon_revision: u32,
    executable: PathBuf,
    restart: bool,
) -> Result<Response, ApiError> {
    if target_protocol == 0 || target_daemon_revision < DAEMON_REVISION {
        return Err(api("invalid_request", "handoff target is not newer"));
    }
    if binary_identity_version(&target_binary_id) != Some(target_version.as_str()) {
        return Err(api(
            "invalid_request",
            "handoff target version does not match its identity",
        ));
    }
    // ^ docs/daemon-live-upgrade.md: only an explicit restart may hand off into this
    // daemon's own version, including the same file. It never selects an older version.
    let newer = compare_binary_identities(&target_binary_id, &daemon.binary_id)
        == Some(std::cmp::Ordering::Greater);
    let same_version_restart = restart && target_version == WSX_VERSION;
    if !newer && !same_version_restart {
        return Err(api("invalid_request", "handoff target is not newer"));
    }
    let executable = validate_handoff_executable(&executable, &target_binary_id)
        .map_err(|error| api("invalid_handoff_target", error.to_string()))?;
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api(
            "handoff_in_progress",
            "wsxd handoff is already in progress",
        ));
    }
    if let Some(current_revision) = state.replacement_daemon_revision {
        if current_revision > target_daemon_revision {
            return Ok(replacement_response(
                &state,
                ReplacementDisposition::Deferred,
                target_version,
                vec![ReplacementBlocker::PendingTarget],
                false,
            ));
        }
    }
    state.replacement_target = Some(target_binary_id.clone());
    state.replacement_daemon_revision = Some(target_daemon_revision);
    state.replacement_protocol = Some(target_protocol);
    state.replacement_executable = Some(executable);
    let blockers = handoff_blockers(&mut state, &target_binary_id);
    daemon.changed.notify_all();
    Ok(replacement_response(
        &state,
        if blockers.is_empty() {
            ReplacementDisposition::Stopping
        } else {
            ReplacementDisposition::Deferred
        },
        target_version,
        blockers,
        false,
    ))
}

fn validate_handoff_executable(path: &Path, expected_identity: &str) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let metadata = canonical.metadata()?;
    let owner = metadata.uid();
    if !metadata.is_file()
        || (owner != unsafe { libc::geteuid() } && owner != 0)
        || metadata.mode() & 0o022 != 0
        || !binary_identity_matches_file(&canonical, expected_identity)?
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsxd handoff executable",
        ));
    }
    Ok(canonical)
}

fn handoff_blockers(state: &mut State, _target_binary_id: &str) -> Vec<ReplacementBlocker> {
    expire_tui_presence(state, Instant::now());
    // ^ docs/daemon-live-upgrade.md: a TUI that speaks the target protocol reconnects
    // to the successor on its own, and a TUI that follows re-executes into the installed
    // wsx. Only a TUI that can do neither must leave first.
    let target_protocol = state.replacement_protocol.unwrap_or(u32::MAX);
    let another_tui = state.legacy_tui_until.is_some()
        || state
            .tui_clients
            .values()
            .any(|client| client.max_protocol < target_protocol && !client.follows);
    let mut blockers = Vec::new();
    if another_tui {
        blockers.push(ReplacementBlocker::OtherTui);
    }
    if state.recovery_in_progress {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    if state.runtimes.len() > handoff::MAX_HANDOFF_PANES {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    blockers
}

fn snapshot(daemon: &Daemon, state: &State) -> Snapshot {
    let mut listening_ports = state
        .listening_ports
        .iter()
        .map(|(pane_id, tcp)| PanePorts {
            pane_id: *pane_id,
            tcp: tcp.clone(),
        })
        .collect::<Vec<_>>();
    listening_ports.sort_by_key(|ports| ports.pane_id);
    let mut pane_activity = state
        .foreground_jobs
        .iter()
        .map(|pane_id| PaneActivity {
            pane_id: *pane_id,
            foreground_job: true,
        })
        .collect::<Vec<_>>();
    pane_activity.sort_by_key(|activity| activity.pane_id);
    let mut capabilities = capabilities();
    capabilities.worktree_review_available = state
        .plugins
        .iter()
        .any(|p| p.enabled && p.worktree_review.is_some());
    Snapshot {
        protocol: PROTOCOL_VERSION,
        epoch: daemon.epoch,
        revision: state.revision,
        projects: state.persisted.projects.clone(),
        worktrees: state.persisted.worktrees.clone(),
        sessions: state.persisted.sessions.clone(),
        panes: state
            .persisted
            .panes
            .iter()
            .map(|pane| pane.pane.clone())
            .collect(),
        listening_ports,
        pane_activity,
        plugin_sidecars: plugins::sidecars(&state.plugins),
        capabilities,
    }
}

fn poll(
    daemon: &Daemon,
    after: u64,
    timeout_ms: u64,
    tui: Option<TuiClientPresence>,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    record_tui_presence(&mut state, tui)?;
    if state.revision <= after && !state.stopping {
        state = daemon
            .changed
            .wait_timeout(state, Duration::from_millis(timeout_ms.min(30_000)))
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
    let earliest = state
        .events
        .front()
        .map(event_revision)
        .unwrap_or(state.revision);
    if after.saturating_add(1) < earliest {
        return Ok(Response::Events {
            revision: state.revision,
            events: vec![Event::ResyncRequired {
                revision: state.revision,
            }],
        });
    }
    Ok(Response::Events {
        revision: state.revision,
        events: state
            .events
            .iter()
            .filter(|event| event_revision(event) > after)
            .cloned()
            .collect(),
    })
}

fn record_tui_presence(state: &mut State, tui: Option<TuiClientPresence>) -> Result<(), ApiError> {
    let now = Instant::now();
    expire_tui_presence(state, now);
    let Some(tui) = tui else {
        state.legacy_tui_until = Some(now + TUI_PRESENCE_TTL);
        return Ok(());
    };
    if tui.instance_id == 0
        || tui.version.len() > 64
        || compare_wsx_versions(&tui.version, &tui.version).is_none()
        || tui.target_binary_id.is_empty()
        || tui.target_binary_id.len() > 512
        || binary_identity_version(&tui.target_binary_id) != Some(tui.version.as_str())
        || compare_binary_identities(&tui.target_binary_id, &tui.target_binary_id).is_none()
    {
        return Err(api("invalid_tui", "TUI presence is invalid"));
    }
    state.tui_clients.insert(
        tui.instance_id,
        TuiPresenceLease {
            target_binary_id: tui.target_binary_id,
            target_daemon_revision: tui.target_daemon_revision,
            max_protocol: tui.max_protocol,
            follows: tui.follows,
            expires_at: now + TUI_PRESENCE_TTL,
        },
    );
    Ok(())
}

fn expire_tui_presence(state: &mut State, now: Instant) {
    state
        .tui_clients
        .retain(|_, client| client.expires_at > now);
    if state.legacy_tui_until.is_some_and(|expires| expires <= now) {
        state.legacy_tui_until = None;
    }
}

fn event_revision(event: &Event) -> u64 {
    match event {
        Event::Changed { revision, .. }
        | Event::Exited { revision, .. }
        | Event::ResyncRequired { revision } => *revision,
    }
}

fn synchronize_projects(
    daemon: &Arc<Daemon>,
    specs: Vec<ProjectSpec>,
) -> Result<Response, ApiError> {
    if specs.len() > 256 {
        return Err(api("invalid_projects", "too many projects"));
    }
    let mut canonical = Vec::with_capacity(specs.len());
    let mut seen_projects = HashSet::new();
    let mut seen_worktrees = HashSet::new();
    for spec in specs {
        let project_path = fs::canonicalize(&spec.path).map_err(io_api)?;
        if !seen_projects.insert(project_path.clone()) {
            return Err(api("duplicate_project", "duplicate project path"));
        }
        let name = bounded_label(spec.name)?;
        let mut worktrees = Vec::new();
        for worktree in spec.worktrees {
            let path = match fs::canonicalize(&worktree.path) {
                Ok(path) => path,
                // ^ Removed after the client listed it, as a task that closes itself removes its
                // copy: it is gone, like a worktree left out of the list.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_api(error)),
            };
            if !seen_worktrees.insert(path.clone()) {
                return Err(api("duplicate_worktree", "duplicate worktree path"));
            }
            worktrees.push((path, bounded_label(worktree.branch)?));
        }
        canonical.push((project_path, name, worktrees));
    }

    let mut state = lock(&daemon.state);
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let mut retained_projects = Vec::new();
    let mut retained_worktrees = Vec::new();
    for (path, name, worktrees) in canonical {
        let project_id = match persisted
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id)
        {
            Some(id) => id,
            None => ProjectId(next_id(&mut persisted)?),
        };
        retained_projects.push(project_id);
        if let Some(project) = persisted
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.name = name;
            project.revision = revision;
        } else {
            persisted.projects.push(Project {
                id: project_id,
                path,
                name,
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                revision,
            });
        }
        for (path, branch) in worktrees {
            let worktree_id = match persisted
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| worktree.id)
            {
                Some(id) => id,
                None => WorktreeId(next_id(&mut persisted)?),
            };
            retained_worktrees.push(worktree_id);
            if let Some(worktree) = persisted
                .worktrees
                .iter_mut()
                .find(|worktree| worktree.id == worktree_id)
            {
                worktree.project_id = project_id;
                worktree.branch = branch;
                worktree.revision = revision;
            } else {
                persisted.worktrees.push(Worktree {
                    id: worktree_id,
                    project_id,
                    path,
                    branch,
                    revision,
                });
            }
        }
    }
    let session_worktrees = persisted
        .sessions
        .iter()
        .map(|session| session.worktree_id)
        .collect::<HashSet<_>>();
    for project_id in persisted
        .worktrees
        .iter()
        .filter(|worktree| session_worktrees.contains(&worktree.id))
        .map(|worktree| worktree.project_id)
    {
        if !retained_projects.contains(&project_id) {
            retained_projects.push(project_id);
        }
    }
    persisted
        .projects
        .retain(|project| retained_projects.contains(&project.id));
    persisted.worktrees.retain(|worktree| {
        retained_worktrees.contains(&worktree.id) || session_worktrees.contains(&worktree.id)
    });
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "projects.synchronized",
        0,
    )?;
    Ok(Response::Ack { revision })
}

fn create_session(
    daemon: &Arc<Daemon>,
    worktree_id: WorktreeId,
    label: String,
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    let label = bounded_label(label)?;
    let recipe = launch_recipe(command, initial_input, rows, cols)?;
    let (cwd, session_id, pane_id, terminal_id, revision) = {
        let mut state = lock(&daemon.state);
        if state.stopping {
            return Err(api("daemon_stopping", "wsxd is stopping"));
        }
        let mut persisted = state.persisted.clone();
        let worktree = persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or_else(|| api("not_found", "worktree not found"))?;
        let cwd = worktree.path.clone();
        let project_id = worktree.project_id;
        let project_index = persisted
            .projects
            .iter()
            .position(|project| project.id == project_id)
            .ok_or_else(|| api("not_found", "project not found"))?;
        let session_id = SessionId(next_id(&mut persisted)?);
        let pane_id = PaneId(next_id(&mut persisted)?);
        let terminal_id = TerminalId(next_id(&mut persisted)?);
        let revision = state.revision.saturating_add(1);
        persisted.projects[project_index].last_terminal_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
        persisted.panes.push(PersistedPane {
            pane: Pane {
                id: pane_id,
                terminal_id,
                session_id,
                label: "terminal".into(),
                agent: None,
                exited: true,
                revision,
            },
            recovery: Some(recipe.clone()),
            recovery_quarantined: false,
        });
        persisted.sessions.push(Session {
            id: session_id,
            worktree_id,
            label,
            primary_pane: pane_id,
            focused_pane: pane_id,
            panes: vec![pane_id],
            layout: PaneLayout::Leaf { pane_id },
            revision,
        });
        commit_publish(
            daemon,
            &mut state,
            persisted,
            revision,
            "session.created",
            session_id.0,
        )?;
        (cwd, session_id, pane_id, terminal_id, revision)
    };
    let runtime = spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe)?;
    let mut state = lock(&daemon.state);
    if state.stopping
        || !state
            .persisted
            .worktrees
            .iter()
            .any(|worktree| worktree.id == worktree_id)
    {
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "worktree changed while terminal was starting",
        ));
    }
    let runtime = Arc::new(runtime);
    if !runtime.exited() {
        let mut persisted = state.persisted.clone();
        if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
            pane.exited = false;
        }
        if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
            drop(state);
            runtime.terminate();
            return Err(error);
        }
    }
    state.runtimes.insert(pane_id, Arc::clone(&runtime));
    if runtime.exited() {
        record_terminal_exit(daemon, &mut state, pane_id);
    }
    Ok(Response::Created {
        revision,
        id: session_id.0,
    })
}

#[allow(clippy::too_many_arguments)]
fn split_pane(
    daemon: &Arc<Daemon>,
    session_id: SessionId,
    target: PaneId,
    axis: SplitAxis,
    label: String,
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
    expected: u64,
) -> Result<Response, ApiError> {
    let label = bounded_label(label)?;
    let recipe = launch_recipe(command, initial_input, rows, cols)?;
    let (cwd, pane_id, terminal_id) = {
        let mut state = lock(&daemon.state);
        if state.stopping {
            return Err(api("daemon_stopping", "wsxd is stopping"));
        }
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| api("not_found", "session not found"))?;
        expect_revision(session.revision, expected)?;
        if !session.panes.contains(&target) {
            return Err(api("not_found", "target pane is not in session"));
        }
        let worktree_id = session.worktree_id;
        let cwd = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or_else(|| api("not_found", "worktree not found"))?
            .path
            .clone();
        let mut persisted = state.persisted.clone();
        let pane_id = PaneId(next_id(&mut persisted)?);
        let terminal_id = TerminalId(next_id(&mut persisted)?);
        commit_persisted(daemon, &mut state, persisted)?;
        (cwd, pane_id, terminal_id)
    };
    let runtime = Arc::new(spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe)?);
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let Some(index) = persisted
        .sessions
        .iter()
        .position(|session| session.id == session_id)
    else {
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "session closed while terminal was starting",
        ));
    };
    if state.stopping
        || persisted.sessions[index].revision != expected
        || !persisted.sessions[index].panes.contains(&target)
    {
        drop(state);
        runtime.terminate();
        return Err(api(
            "revision_conflict",
            "session changed while terminal was starting",
        ));
    }
    if !persisted.sessions[index]
        .layout
        .split(target, pane_id, axis)
    {
        drop(state);
        runtime.terminate();
        return Err(api("invalid_layout", "target pane is absent from layout"));
    }
    let revision = state.revision.saturating_add(1);
    let session = &mut persisted.sessions[index];
    session.panes.push(pane_id);
    session.focused_pane = pane_id;
    session.revision = revision;
    persisted.panes.push(PersistedPane {
        pane: Pane {
            id: pane_id,
            terminal_id,
            session_id,
            label,
            agent: None,
            exited: runtime.exited(),
            revision,
        },
        recovery: Some(recipe),
        recovery_quarantined: false,
    });
    if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
        drop(state);
        runtime.terminate();
        return Err(error);
    }
    state.runtimes.insert(pane_id, Arc::clone(&runtime));
    publish(daemon, &mut state, revision, "pane.created", pane_id.0);
    if runtime.exited() {
        record_terminal_exit(daemon, &mut state, pane_id);
    }
    Ok(Response::Created {
        revision,
        id: pane_id.0,
    })
}

fn reorder_session(
    daemon: &Arc<Daemon>,
    session_id: SessionId,
    target_session_id: SessionId,
    placement: SessionPlacement,
    expected_revision: u64,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    reorder_session_state(
        &mut persisted,
        session_id,
        target_session_id,
        placement,
        expected_revision,
    )?;
    let revision = state.revision.saturating_add(1);
    persisted
        .sessions
        .iter_mut()
        .find(|session| session.id == session_id)
        .expect("reordered session must remain present")
        .revision = revision;
    commit_persisted(daemon, &mut state, persisted)?;
    let bumped = bump(daemon, &mut state, "session.reordered", session_id.0);
    debug_assert_eq!(bumped, revision);
    Ok(Response::Ack { revision })
}

fn reorder_session_state(
    persisted: &mut Persisted,
    session_id: SessionId,
    target_session_id: SessionId,
    placement: SessionPlacement,
    expected_revision: u64,
) -> Result<(), ApiError> {
    if session_id == target_session_id {
        return Err(api(
            "invalid_target",
            "session cannot be reordered relative to itself",
        ));
    }
    let source_index = persisted
        .sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| api("not_found", "session not found"))?;
    let target = persisted
        .sessions
        .iter()
        .find(|session| session.id == target_session_id)
        .ok_or_else(|| api("not_found", "target session not found"))?;
    let source = &persisted.sessions[source_index];
    expect_revision(source.revision, expected_revision)?;
    if source.worktree_id != target.worktree_id {
        return Err(api(
            "invalid_target",
            "sessions can only be reordered within one worktree",
        ));
    }

    let source = persisted.sessions.remove(source_index);
    let target_index = persisted
        .sessions
        .iter()
        .position(|session| session.id == target_session_id)
        .expect("validated target must remain after removing a distinct session");
    let insert_at = match placement {
        SessionPlacement::Before => target_index,
        SessionPlacement::After => target_index + 1,
    };
    persisted.sessions.insert(insert_at, source);
    Ok(())
}

fn close_session(daemon: &Arc<Daemon>, id: SessionId, expected: u64) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let session = state
        .persisted
        .sessions
        .iter()
        .find(|session| session.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "session not found"))?;
    expect_revision(session.revision, expected)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    terminate_agent_exchanges(
        &mut persisted,
        &session.panes,
        AgentExchangeState::TargetExited,
        revision,
    );
    persisted.panes.retain(|pane| pane.session_id != id);
    persisted.sessions.retain(|session| session.id != id);
    commit_persisted(daemon, &mut state, persisted)?;
    let runtimes = session
        .panes
        .iter()
        .filter_map(|pane_id| state.forget_pane(*pane_id))
        .collect::<Vec<_>>();
    publish(daemon, &mut state, revision, "session.closed", id.0);
    drop(state);
    for runtime in runtimes {
        runtime.terminate();
    }
    Ok(Response::Ack { revision })
}

// ^ [[Session Model]] An exited pane keeps its saved recipe, so a restart restores the transcript identity instead of leaving a hollow session.
fn restart_session(daemon: &Arc<Daemon>, id: PaneId, expected: u64) -> Result<Response, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, id);
    let _operation = lock(&operation_lock);
    let (cwd, terminal_id, launch, agent) = {
        let state = lock(&daemon.state);
        if state.stopping || state.handoff_in_progress {
            return Err(api(
                "daemon_replacing",
                "wsxd is replacing its runtime owner",
            ));
        }
        if state.recovery_in_progress {
            return Err(api(
                "recovery_in_progress",
                "wsxd is still restoring saved sessions",
            ));
        }
        let pane = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == id)
            .cloned()
            .ok_or_else(|| api("not_found", "pane not found"))?;
        expect_revision(pane.revision, expected)?;
        if !pane.exited {
            return Err(api(
                "invalid_state",
                "pane is still running; restart accepts only an exited pane",
            ));
        }
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
            .ok_or_else(|| api("not_found", "session not found"))?;
        let cwd = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .map(|worktree| worktree.path.clone())
            .ok_or_else(|| api("not_found", "worktree not found"))?;
        let recipe = pane
            .recovery
            .clone()
            .filter(|recipe| validate_recipe(recipe).is_ok())
            .unwrap_or_else(default_launch_recipe);
        let mut agent = pane.agent.clone();
        let launch = recovery_launch(agent.as_mut(), &recipe, true);
        let launch = match prepare_recovery_launch(launch, &cwd) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("wsxd restart pane {}: {error}", id.0);
                RecoveryLaunch {
                    recipe: shell_launch_recipe(recipe.rows, recipe.cols),
                    resume_key: None,
                    provider: None,
                    await_readiness: false,
                }
            }
        };
        // ^ A saved identity only authorizes a resume plan; the replacement runtime must report its own generation.
        let agent = if launch.resume_key.is_some() {
            agent.map(|mut agent| {
                detach_agent(&mut agent);
                agent
            })
        } else {
            None
        };
        (cwd, pane.terminal_id, launch, agent)
    };
    let runtime = spawn_runtime(daemon, id, terminal_id, &cwd, &launch.recipe)?;
    let runtime = Arc::new(runtime);
    let live = !runtime.exited();
    let mut state = lock(&daemon.state);
    if state.stopping || !state.persisted.panes.iter().any(|pane| pane.id == id) {
        state.runtime_generations.remove(&id);
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "session changed while the terminal was restarting",
        ));
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let pane = persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == id)
        .expect("pane was checked above");
    pane.exited = !live;
    pane.revision = revision;
    pane.agent = agent;
    if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
        // ^ A failed restart must not leave a generation that could authorize agent reports.
        state.runtime_generations.remove(&id);
        drop(state);
        runtime.terminate();
        return Err(error);
    }
    // ^ The dead pane's stream has no authority over the replacement runtime.
    state.forget_pane_leases(id);
    state.runtimes.insert(id, Arc::clone(&runtime));
    publish(daemon, &mut state, revision, "session.restarted", id.0);
    if !live {
        record_terminal_exit(daemon, &mut state, id);
    }
    Ok(Response::Ack { revision })
}

fn close_pane(daemon: &Arc<Daemon>, id: PaneId, expected: u64) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "pane not found"))?;
    expect_revision(pane.revision, expected)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let session = persisted
        .sessions
        .iter_mut()
        .find(|session| session.id == pane.session_id)
        .ok_or_else(|| api("not_found", "session not found"))?;
    if session.panes.len() == 1 {
        return Err(api("last_pane", "close the session instead"));
    }
    session.layout.remove(id);
    session.panes.retain(|pane_id| *pane_id != id);
    if session.focused_pane == id {
        session.focused_pane = session.panes[0];
    }
    if session.primary_pane == id {
        session.primary_pane = session.panes[0];
    }
    session.revision = revision;
    terminate_agent_exchanges(
        &mut persisted,
        &[id],
        AgentExchangeState::TargetExited,
        revision,
    );
    persisted.panes.retain(|candidate| candidate.id != id);
    commit_persisted(daemon, &mut state, persisted)?;
    let runtime = state.forget_pane(id);
    publish(daemon, &mut state, revision, "pane.closed", id.0);
    drop(state);
    if let Some(runtime) = runtime {
        runtime.terminate();
    }
    Ok(Response::Ack { revision })
}

fn resize_terminal(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let (runtime, _) = leased_runtime(daemon, pane_id, access)?;
    runtime.resize(rows, cols).map_err(terminal_api)?;
    let mut state = lock(&daemon.state);
    if let Some(recipe) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.recovery.as_mut())
    {
        recipe.rows = rows;
        recipe.cols = cols;
        if let Err(error) = save_state(&daemon.state_path, &state.persisted) {
            state.persistence_dirty = true;
            return Err(api(
                "durability_degraded",
                format!(
                    "terminal resized but its recovery dimensions are not yet durable: {error}"
                ),
            ));
        }
    }
    Ok(Response::Ack {
        revision: state.revision,
    })
}

fn view(daemon: &Daemon, pane_ids: Vec<PaneId>) -> Result<Response, ApiError> {
    if pane_ids.len() > MAX_VIEW_PANES {
        return Err(api("invalid_request", "too many panes requested"));
    }
    let pane_ids = unique_view_pane_ids(pane_ids);
    let (snapshot, runtimes) = {
        let state = lock(&daemon.state);
        let runtimes = pane_ids
            .into_iter()
            .filter_map(|id| state.runtimes.get(&id).cloned())
            .collect::<Vec<_>>();
        (snapshot(daemon, &state), runtimes)
    };
    let mut frames = Vec::with_capacity(runtimes.len());
    let mut total_cells = 0;
    for runtime in runtimes {
        let frame = runtime.frame().map_err(terminal_api)?;
        total_cells = add_view_cells(total_cells, frame.cells.len())?;
        frames.push(frame);
    }
    Ok(Response::View { snapshot, frames })
}

fn unique_view_pane_ids(pane_ids: Vec<PaneId>) -> Vec<PaneId> {
    let mut seen = HashSet::new();
    pane_ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

fn add_view_cells(total: usize, next: usize) -> Result<usize, ApiError> {
    let total = total
        .checked_add(next)
        .filter(|total| *total <= MAX_VIEW_CELLS)
        .ok_or_else(|| api("response_too_large", "terminal view exceeds cell limit"))?;
    Ok(total)
}

fn project_index_for_pane(state: &Persisted, pane_id: PaneId) -> Result<usize, ApiError> {
    let pane = state
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let session = state
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id && session.panes.contains(&pane_id))
        .ok_or_else(|| api("not_found", "session not found"))?;
    let worktree = state
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .ok_or_else(|| api("not_found", "worktree not found"))?;
    state
        .projects
        .iter()
        .position(|project| project.id == worktree.project_id)
        .ok_or_else(|| api("not_found", "project not found"))
}

fn touch_terminal_project(daemon: &Arc<Daemon>, pane_id: PaneId) -> Result<(), ApiError> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let project_index = project_index_for_pane(&persisted, pane_id)?;
    let revision = state.revision.saturating_add(1);
    persisted.projects[project_index].last_terminal_active_unix_ms = Some(unix_time_millis());
    persisted.projects[project_index].revision = revision;
    commit_persisted(daemon, &mut state, persisted)?;
    bump(daemon, &mut state, "project.terminal_entered", pane_id.0);
    Ok(())
}

fn terminate_agent_exchanges(
    persisted: &mut Persisted,
    pane_ids: &[PaneId],
    exchange_state: AgentExchangeState,
    revision: u64,
) {
    let now = unix_time_millis();
    for exchange in &mut persisted.agent_exchanges {
        if pane_ids.contains(&exchange.pane_id) && !exchange.is_terminal() {
            exchange.state = exchange_state;
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
}

fn observe_agent_exchange(
    persisted: &mut Persisted,
    pane_id: PaneId,
    agent_state: AgentState,
    attached: bool,
    revision: u64,
    now: u64,
) {
    let receipts = persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| agent.attached && agent.capabilities.prompt_receipts);
    let Some(exchange) = persisted
        .agent_exchanges
        .iter_mut()
        .rev()
        .find(|exchange| exchange.pane_id == pane_id && !exchange.is_terminal())
    else {
        return;
    };
    // ^ With prompt receipts a round follows the agent's turns only once the agent took it:
    // until then they belong to another prompt (docs/agent-orchestration.md "Taken, not typed").
    // A command round may start no prompt at all (Claude's /compact): it follows the lifecycle.
    if attached
        && receipts
        && !exchange.command
        && matches!(
            exchange.state,
            AgentExchangeState::Submitted | AgentExchangeState::Delivered
        )
    {
        return;
    }
    let next = if !attached {
        Some(AgentExchangeState::Interrupted)
    } else if exchange.state == AgentExchangeState::CancelRequested
        && agent_state != AgentState::Working
    {
        Some(AgentExchangeState::Cancelled)
    } else {
        match agent_state {
            AgentState::Working => Some(AgentExchangeState::WorkingObserved),
            AgentState::Blocked => Some(AgentExchangeState::BlockedObserved),
            AgentState::Done => Some(AgentExchangeState::DoneObserved),
            AgentState::Error => Some(AgentExchangeState::ErrorObserved),
            // ^ docs/agent-orchestration.md "Interrupted turn": an agent that was seen at
            // work on this exchange and is idle again was stopped by a person (Escape in
            // its terminal, or a denied dialog). No "done" will follow, so the exchange
            // ends here and the next request is not held back until the deadline.
            AgentState::Idle
                if matches!(
                    exchange.state,
                    AgentExchangeState::WorkingObserved | AgentExchangeState::BlockedObserved
                ) =>
            {
                Some(AgentExchangeState::Interrupted)
            }
            AgentState::Unknown | AgentState::Idle => None,
        }
    };
    if let Some(next) = next {
        exchange.state = next;
        exchange.evidence = AgentExchangeEvidence::PaneLifecycle;
        exchange.updated_unix_ms = now;
        exchange.revision = revision;
    }
}

/// How long a delivered round may stay untaken while its agent is not working.
const TAKE_WAIT_MS: u64 = 5_000;

/// The agent's own hook says which prompt a turn took. The round it names is accepted; a
/// round still waiting when another prompt starts was not taken, so that turn never
/// becomes its result. ref: docs/agent-orchestration.md "Taken, not typed"
fn receive_prompt(
    persisted: &mut Persisted,
    pane_id: PaneId,
    received: Option<PromptReceived>,
    revision: u64,
    now: u64,
) {
    let Some(received) = received else {
        return;
    };
    let Some(exchange) = persisted.agent_exchanges.iter_mut().rev().find(|exchange| {
        exchange.pane_id == pane_id
            && matches!(
                exchange.state,
                AgentExchangeState::Submitted | AgentExchangeState::Delivered
            )
    }) else {
        return;
    };
    let named = received
        == PromptReceived::Round {
            exchange: exchange.id,
            round: exchange.round,
        };
    // ^ A command round has no exchange line: the prompt the agent took is taken as it.
    if named || exchange.command {
        exchange.state = AgentExchangeState::Accepted;
        exchange.evidence = AgentExchangeEvidence::PaneLifecycle;
    } else {
        exchange.state = AgentExchangeState::DeliveryFailed;
        exchange.failure = Some("not_taken: the agent took another prompt first".into());
    }
    exchange.updated_unix_ms = now;
    exchange.revision = revision;
}

/// A delivered round that an agent with prompt receipts has not taken within
/// `TAKE_WAIT_MS` while it is not working, such as a paste dropped as a turn ended, is not
/// taken. Returns whether a round changed.
fn fail_untaken_rounds(daemon: &Daemon, state: &mut State) -> Result<bool, ApiError> {
    let now = unix_time_millis();
    let untaken = |exchange: &AgentExchange, panes: &[PersistedPane]| {
        exchange.state == AgentExchangeState::Delivered
            && !exchange.command
            && now.saturating_sub(exchange.updated_unix_ms) >= TAKE_WAIT_MS
            && panes
                .iter()
                .find(|pane| pane.id == exchange.pane_id)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| {
                    agent.attached
                        && agent.capabilities.prompt_receipts
                        && agent.state != AgentState::Working
                        && agent
                            .state_since_unix_ms
                            .is_none_or(|since| now.saturating_sub(since) >= TAKE_WAIT_MS)
                })
    };
    let persisted = &state.persisted;
    if !persisted
        .agent_exchanges
        .iter()
        .any(|exchange| untaken(exchange, &persisted.panes))
    {
        return Ok(false);
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let panes = persisted.panes.clone();
    for exchange in &mut persisted.agent_exchanges {
        if untaken(exchange, &panes) {
            exchange.state = AgentExchangeState::DeliveryFailed;
            exchange.failure = Some("not_taken: the agent did not take it within 5 s".into());
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.not_taken",
        0,
    )?;
    Ok(true)
}

// ^ [[Agent Lifecycle Authority]] wsx-core normalizes provider events and bounded
// companion evidence; this boundary owns reconciliation, generation checks,
// persistence, wake authority, and resume identity.
struct AgentReportInput {
    provider: String,
    state: AgentState,
    attached: bool,
    presence_id: Option<String>,
    conversation_id: Option<String>,
    session_ref: Option<AgentSessionRef>,
    wake_token: Option<String>,
    capabilities: AgentCapabilities,
    received: Option<PromptReceived>,
    compacting: bool,
}

#[cfg(test)]
fn agent_report(
    daemon: &Arc<Daemon>,
    runtime: RuntimeAgentAuthority,
    provider: String,
    state: AgentState,
    conversation_id: Option<String>,
    session_ref: Option<AgentSessionRef>,
    capabilities: AgentCapabilities,
) -> Result<Response, ApiError> {
    agent_report_with_attachment(
        daemon,
        runtime,
        AgentReportInput {
            provider,
            state,
            attached: true,
            presence_id: None,
            conversation_id,
            session_ref,
            wake_token: None,
            capabilities,
            received: None,
            compacting: false,
        },
    )
}

fn agent_report_with_attachment(
    daemon: &Arc<Daemon>,
    runtime: RuntimeAgentAuthority,
    report: AgentReportInput,
) -> Result<Response, ApiError> {
    let AgentReportInput {
        provider,
        state: agent_state,
        attached,
        presence_id,
        conversation_id,
        session_ref,
        wake_token,
        mut capabilities,
        received,
        compacting,
    } = report;
    let RuntimeAgentAuthority {
        pane_id,
        generation: runtime_generation,
    } = runtime;
    let provider = bounded_provider(provider)?;
    let presence_id = presence_id.map(validate_presence_id).transpose()?;
    if !attached && presence_id.is_some() {
        return Err(api(
            "invalid_presence",
            "detached reports cannot claim presence",
        ));
    }
    let wake_token = match wake_token {
        Some(wake_token)
            if attached && agent_state == AgentState::Working && provider == "claude" =>
        {
            Some(bounded_wake_token(wake_token)?)
        }
        Some(_) => {
            return Err(api(
                "invalid_wake_token",
                "wake token requires an attached Claude Working report",
            ))
        }
        None => None,
    };
    let conversation_id = conversation_id.map(|value| truncate_utf8(value, 512));
    let explicit_session_ref = validated_session_ref(session_ref)?;
    if explicit_session_ref
        .as_ref()
        .is_some_and(|session_ref| resume::plan(&provider, session_ref).is_none())
    {
        return Err(api(
            "invalid_agent_session",
            "provider does not support this agent session reference",
        ));
    }
    let mut session_ref = explicit_session_ref.or_else(|| {
        conversation_id
            .as_ref()
            .and_then(|value| AgentSessionRef::id(value.clone()))
            .filter(|session_ref| resume::plan(&provider, session_ref).is_some())
    });
    let mut conversation_id = conversation_id;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, runtime_generation.as_deref())?;
    let mut persisted = state.persisted.clone();

    // ^ [[Session Model]] Agent activity belongs to the project reached through
    // pane/session/worktree ownership. Resolve ancestry before changing persisted state.
    let pane_index = persisted
        .panes
        .iter()
        .position(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let project_index = project_index_for_pane(&persisted, pane_id)?;

    let previous_agent = persisted.panes[pane_index].agent.clone();
    let agent_id = match previous_agent.as_ref().map(|agent| agent.id) {
        Some(id) => id,
        None => AgentInstanceId(next_id(&mut persisted)?),
    };
    // ^ Preserve native metadata only for the same attached provider/session.
    if attached {
        if let (Some(current), Some(previous)) = (
            &mut session_ref,
            previous_agent
                .as_ref()
                .filter(|previous| previous.attached && previous.provider == provider),
        ) {
            if let Some(old) = &previous.session_ref {
                if current.kind == old.kind
                    && current.value == old.value
                    && current.transcript_path.is_none()
                {
                    current.transcript_path = old.transcript_path.clone();
                }
            }
        }
    }
    if !attached {
        if let Some(previous) = previous_agent
            .as_ref()
            .filter(|agent| agent.provider == provider)
        {
            conversation_id = conversation_id.or_else(|| previous.conversation_id.clone());
            session_ref = session_ref.or_else(|| previous.session_ref.clone());
            capabilities = previous.capabilities.clone();
        }
    }
    capabilities.resume |= session_ref.is_some();
    let now = Instant::now();
    let is_claude = provider == "claude";
    let claude_reconciliation = (is_claude && attached).then(|| {
        let mut reconciliation = state
            .claude_reconciliations
            .get(&pane_id)
            .copied()
            .unwrap_or(ClaudeReconciliation {
                event_state: agent_state,
                event_at: now,
                terminal_revision: None,
                terminal: None,
                interrupt_requested: false,
                interrupt_revision: None,
            });
        reconciliation.event_state = agent_state;
        reconciliation.interrupt_requested = false;
        reconciliation.interrupt_revision = None;
        reconciliation.event_at = now;
        reconciliation.terminal_revision = None;
        reconciliation.terminal = None;
        reconciliation
    });
    let reported_state = if attached {
        claude_reconciliation.map_or(agent_state, |reconciliation| reconciliation.effective(now))
    } else {
        AgentState::Unknown
    };
    // A report of the state the agent is already in keeps when that state began.
    let state_since_unix_ms = previous_agent
        .as_ref()
        .filter(|previous| previous.id == agent_id && previous.state == reported_state)
        .and_then(|previous| previous.state_since_unix_ms)
        .unwrap_or_else(unix_time_millis);
    let revision = state.revision.saturating_add(1);
    persisted.panes[pane_index].agent = Some(AgentInfo {
        id: agent_id,
        provider,
        state: reported_state,
        state_since_unix_ms: Some(state_since_unix_ms),
        attached,
        presence_id: presence_id.clone(),
        conversation_id,
        session_ref,
        capabilities,
        source: "adapter".into(),
        compacting: attached && compacting,
    });
    persisted.panes[pane_index].revision = revision;
    if attached && agent_state == AgentState::Working {
        persisted.projects[project_index].last_agent_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
    }
    receive_prompt(
        &mut persisted,
        pane_id,
        received,
        revision,
        unix_time_millis(),
    );
    observe_agent_exchange(
        &mut persisted,
        pane_id,
        reported_state,
        attached,
        revision,
        unix_time_millis(),
    );
    commit_persisted(daemon, &mut state, persisted)?;
    if presence_id.is_some() {
        state.agent_presence_leases.insert(pane_id, now);
    } else {
        state.agent_presence_leases.remove(&pane_id);
    }
    if let Some(reconciliation) = claude_reconciliation {
        state.claude_reconciliations.insert(pane_id, reconciliation);
    } else {
        state.claude_reconciliations.remove(&pane_id);
    }
    if attached && agent_state == AgentState::Working && reported_state == AgentState::Working {
        state.agent_wake_leases.insert(pane_id, now);
        if let Some(wake_token) = wake_token {
            state.agent_wake_tokens.insert(pane_id, wake_token);
        } else {
            state.agent_wake_tokens.remove(&pane_id);
        }
    } else {
        clear_agent_wake(&mut state, pane_id);
    }
    let revision = bump(daemon, &mut state, "agent.reported", pane_id.0);
    Ok(Response::Ack { revision })
}

fn clear_agent_wake(state: &mut State, pane_id: PaneId) {
    state.agent_wake_leases.remove(&pane_id);
    state.agent_wake_tokens.remove(&pane_id);
}

// ^ crates/wsx-core/src/runtime/protocol.rs owns the opt-in identity; the
// resident assets in crates/wsx-core/integrations renew without disk writes.
fn renew_agent_presence(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: String,
    presence_id: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let presence_id = validate_presence_id(presence_id)?;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let matches = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id && !pane.exited)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| agent.attached && agent.presence_id.as_deref() == Some(&presence_id));
    if !matches {
        return Err(api(
            "stale_presence",
            "agent attachment no longer owns the pane",
        ));
    }
    let renewed_at = state
        .agent_presence_leases
        .get_mut(&pane_id)
        .ok_or_else(|| api("stale_presence", "agent presence lease is no longer active"))?;
    if Instant::now().saturating_duration_since(*renewed_at) >= AGENT_PRESENCE_TTL {
        return Err(api("stale_presence", "agent presence lease expired"));
    }
    *renewed_at = Instant::now();
    Ok(Response::Ack {
        revision: state.revision,
    })
}

// ^ [[Wake Mode and Report Lease]] Renewal is ephemeral and cannot mutate agent state.
fn renew_agent_wake(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: String,
    wake_token: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let wake_token = bounded_wake_token(wake_token)?;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let active = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.attached && agent.provider == "claude" && agent.state == AgentState::Working
        });
    if !active {
        return Err(api(
            "wake_not_active",
            "pane does not have an attached working Claude agent",
        ));
    }
    match state.agent_wake_tokens.get(&pane_id) {
        Some(active_token) if active_token != &wake_token => {
            return Err(api(
                "stale_wake_heartbeat",
                "wake heartbeat does not belong to the active Claude prompt",
            ));
        }
        Some(_) => {}
        None => {
            state.agent_wake_tokens.insert(pane_id, wake_token);
        }
    }
    state.agent_wake_leases.insert(pane_id, Instant::now());
    Ok(Response::Ack {
        revision: state.revision,
    })
}

fn agent_clear(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    runtime_generation: String,
    next_runtime_generation: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let next_runtime_generation = bounded_runtime_generation(next_runtime_generation)?;
    if runtime_generation == next_runtime_generation {
        return Err(api(
            "invalid_runtime_generation",
            "replacement runtime generation must be distinct",
        ));
    }
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let mut persisted = state.persisted.clone();
    let pane = persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let revision = state.revision.saturating_add(1);
    pane.agent = None;
    pane.revision = revision;
    terminate_agent_exchanges(
        &mut persisted,
        &[pane_id],
        AgentExchangeState::TargetReplaced,
        revision,
    );
    commit_persisted(daemon, &mut state, persisted)?;
    clear_agent_wake(&mut state, pane_id);
    state.agent_presence_leases.remove(&pane_id);
    state.claude_reconciliations.remove(&pane_id);
    state
        .runtime_generations
        .insert(pane_id, next_runtime_generation);
    let revision = bump(daemon, &mut state, "agent.cleared", pane_id.0);
    Ok(Response::Ack { revision })
}

const MAX_EXCHANGE_COMMAND_BYTES: usize = 256;

fn validate_exchange_prompt(prompt: &str) -> Result<(), ApiError> {
    if prompt.is_empty() || prompt.len() > MAX_AGENT_EXCHANGE_PROMPT_BYTES {
        return Err(api("invalid_prompt", "prompt must be 1..65536 bytes"));
    }
    if prompt
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(api(
            "invalid_prompt",
            "prompt contains terminal control characters",
        ));
    }
    Ok(())
}

fn validate_exchange_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    (timeout_ms > 0 && timeout_ms <= MAX_AGENT_EXCHANGE_TIMEOUT_MS)
        .then_some(timeout_ms)
        .ok_or_else(|| api("invalid_timeout", "timeout must be 1ms..24h"))
}

fn exchange_claims(
    access: AgentExchangeAccess,
    claims: Vec<PathBuf>,
    worktree: &Worktree,
) -> Result<Vec<PathBuf>, ApiError> {
    if access == AgentExchangeAccess::ReadOnly {
        if !claims.is_empty() {
            return Err(api(
                "invalid_claim",
                "read-only exchanges cannot declare write claims",
            ));
        }
        return Ok(Vec::new());
    }
    if claims.len() > MAX_AGENT_EXCHANGE_WRITE_CLAIMS {
        return Err(api("invalid_claim", "too many write claims"));
    }
    let claims = if claims.is_empty() {
        vec![worktree.path.clone()]
    } else {
        claims
    };
    if claims.iter().any(|claim| {
        !claim.is_absolute()
            || !claim.starts_with(&worktree.path)
            || claim
                .components()
                .any(|part| matches!(part, Component::ParentDir))
    }) {
        return Err(api(
            "invalid_claim",
            "write claims must be absolute paths inside the target worktree",
        ));
    }
    let canonical_worktree = fs::canonicalize(&worktree.path)
        .map_err(|error| api("invalid_claim", format!("cannot resolve worktree: {error}")))?;
    let mut unique = claims
        .into_iter()
        .map(|claim| canonical_exchange_claim(&claim, &worktree.path, &canonical_worktree))
        .collect::<Result<Vec<_>, _>>()?;
    unique.sort();
    unique.dedup();
    Ok(unique)
}

fn canonical_exchange_claim(
    claim: &Path,
    worktree: &Path,
    canonical_worktree: &Path,
) -> Result<PathBuf, ApiError> {
    let relative = claim.strip_prefix(worktree).map_err(|_| {
        api(
            "invalid_claim",
            "write claim is outside the target worktree",
        )
    })?;
    let mut resolved = canonical_worktree.to_path_buf();
    let mut components = relative.components();
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(api(
                "invalid_claim",
                "write claim contains an invalid path component",
            ));
        };
        let candidate = resolved.join(component);
        match fs::canonicalize(&candidate) {
            Ok(path) => {
                if !path.starts_with(canonical_worktree) {
                    return Err(api(
                        "invalid_claim",
                        "write claim resolves outside the target worktree",
                    ));
                }
                resolved = path;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                resolved = candidate;
                for remaining in components {
                    let Component::Normal(remaining) = remaining else {
                        return Err(api(
                            "invalid_claim",
                            "write claim contains an invalid path component",
                        ));
                    };
                    resolved.push(remaining);
                }
                break;
            }
            Err(error) => {
                return Err(api(
                    "invalid_claim",
                    format!("cannot resolve write claim: {error}"),
                ))
            }
        }
    }
    Ok(resolved)
}

fn claims_overlap(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter().any(|left| {
        right
            .iter()
            .any(|right| left.starts_with(right) || right.starts_with(left))
    })
}

fn has_writer_claim_conflict(exchanges: &[AgentExchange], claims: &[PathBuf]) -> bool {
    exchanges.iter().any(|exchange| {
        exchange.access == AgentExchangeAccess::Writer
            && !exchange.is_terminal()
            && claims_overlap(claims, &exchange.write_claims)
    })
}

fn slash_command(line: &str) -> bool {
    line.strip_prefix('/')
        .is_some_and(|rest| rest.starts_with(|character: char| character.is_ascii_alphanumeric()))
}

/// `!` and a command, such as `! git status` or `!ls`.
fn shell_line(line: &str) -> bool {
    line.strip_prefix('!')
        .is_some_and(|rest| !rest.trim().is_empty())
}

/// What one exchange round types into the agent's terminal.
struct ExchangeInput {
    prompt: String,
    /// A one-line slash command, typed as written (docs/agent-orchestration.md "Commands").
    command: bool,
    /// Take the pane from a live terminal controller (docs/agent-orchestration.md
    /// "Terminal control").
    takeover: bool,
}

impl ExchangeInput {
    fn validate(&self) -> Result<(), ApiError> {
        validate_exchange_prompt(&self.prompt)?;
        // ^ An agent runs a command only at the start of its input, so a command round
        // carries no exchange line. It must be one short line that names a command
        // (`/name`) or a shell line (`!` and a command, docs/agent-orchestration.md "Commands").
        if self.command
            && (self.prompt.len() > MAX_EXCHANGE_COMMAND_BYTES
                || self.prompt.contains(['\n', '\t'])
                || !(slash_command(&self.prompt) || shell_line(&self.prompt)))
        {
            return Err(api(
                "invalid_prompt",
                "a command must be one line of at most 256 bytes that starts with /name or !",
            ));
        }
        Ok(())
    }

    fn terminal_text(
        &self,
        id: AgentExchangeId,
        round: u32,
        access: AgentExchangeAccess,
    ) -> String {
        if self.command {
            return self.prompt.clone();
        }
        let access = match access {
            AgentExchangeAccess::ReadOnly => "read-only",
            AgentExchangeAccess::Writer => "writer",
        };
        // ^ wsx_core::runtime::agent_exchange_envelope_start: `agent wait --result` finds
        // the round in the agent's history by this line.
        format!(
            "{}{access}]\n{}",
            agent_exchange_envelope_start(id, round),
            self.prompt
        )
    }
}

// ^ Provider-specific editor protocol, never agent identity or completion.
// Documented Ctrl+S stashes idle Claude input; fresh empty chrome is the ACK.
// See docs/agent-orchestration.md and Claude's interactive-mode documentation.
fn claude_editor(observation: &AgentTerminalObservation) -> Option<&str> {
    if claude_status::classify(&observation.title, &observation.text)
        != Some(claude_status::TerminalEvidence::Idle)
    {
        return None;
    }
    claude_input(observation)
}

/// The text of Claude's input line, at work or idle; `Some("")` when it is empty.
fn claude_input(observation: &AgentTerminalObservation) -> Option<&str> {
    let lines: Vec<_> = observation.text.lines().collect();
    let border = |line: &str| {
        let line = line.trim();
        !line.is_empty() && line.chars().all(|character| matches!(character, '─' | '━'))
    };
    // ^ Claude Code 2.1.3xx draws up to seven footer rows under the editor's bottom border
    // (status line, mode, update notice, background agents): look above that border.
    let prompt = |line: &&str| line.trim_start().starts_with('❯');
    let end = match lines.iter().rposition(|line| border(line)) {
        Some(bottom) if !lines[bottom + 1..].iter().any(prompt) => bottom,
        _ => lines.len(),
    };
    let (index, input) =
        lines[..end]
            .iter()
            .enumerate()
            .rev()
            .take(8)
            .find_map(|(index, line)| {
                line.trim_start()
                    .strip_prefix('❯')
                    .map(|input| (index, input.trim()))
            })?;
    // ^ A blank first line is not an empty multiline editor. Stop at Claude's
    // horizontal input border, not its footer. Ambiguous placeholder UI is refused.
    if input.starts_with("Try ") {
        return None;
    }
    if !input.is_empty() {
        return Some(input);
    }
    for line in &lines[index + 1..] {
        if border(line) {
            break;
        }
        if !line.trim().is_empty() {
            return Some(line.trim());
        }
    }
    Some("")
}

// ^ docs/agent-orchestration.md: an empty Claude editor shows a suggestion such as
// `❯ Try "fix typecheck errors"` or, after a turn, a next prompt in plain words
// (Claude 2.1.300). Claude draws it dimmed; text a person typed is not. Only the
// editor line of the frame, dimmed as a whole, counts as empty, whatever its words.
fn claude_suggestion(runtime: &TerminalRuntime, observation: &AgentTerminalObservation) -> bool {
    if claude_status::classify(&observation.title, &observation.text)
        != Some(claude_status::TerminalEvidence::Idle)
    {
        return false;
    }
    let Ok(frame) = runtime.frame() else {
        return false;
    };
    if frame.cols == 0 {
        return false;
    }
    // The editor is the lowest prompt line; a short conversation leaves blank rows below it.
    let input = |row: &[Cell]| {
        let text: String = row.iter().map(|cell| cell.symbol.as_str()).collect();
        text.trim_start()
            .strip_prefix('❯')
            .map(|input| input.trim().to_owned())
    };
    let editor = frame
        .cells
        .chunks(usize::from(frame.cols))
        .rev()
        .find(|row| input(row).is_some());
    editor.is_some_and(|row| {
        input(row).is_some_and(|input| !input.is_empty())
            && row
                .iter()
                .filter(|cell| !cell.symbol.trim().is_empty() && cell.symbol != "❯")
                .all(|cell| cell.modifiers.dim)
    })
}

fn validate_draft_target(agent: &AgentInfo, runtime: &TerminalRuntime) -> Result<(), ApiError> {
    if agent.provider != "claude" {
        return Err(api(
            "draft_policy_unsupported",
            "draft stashing is supported only for Claude",
        ));
    }
    if !agent.attached || !matches!(agent.state, AgentState::Idle | AgentState::Done) {
        return Err(api(
            "agent_busy",
            "draft stashing requires an idle or done Claude agent",
        ));
    }
    let observation = runtime
        .agent_observation()
        .map_err(|_| api("editor_unavailable", "cannot read current Claude editor"))?;
    if claude_editor(&observation).is_none() && !claude_suggestion(runtime, &observation) {
        return Err(api(
            "editor_unavailable",
            "Claude input editor is not visibly idle",
        ));
    }
    Ok(())
}

fn stash_claude_draft(runtime: &TerminalRuntime, deadline: Instant) -> Result<(), ApiError> {
    let before = runtime
        .agent_observation()
        .map_err(|_| api("editor_unavailable", "cannot observe Claude editor"))?;
    // An empty editor needs no key: Ctrl+S would restore the existing stash.
    if claude_editor(&before) == Some("") || claude_suggestion(runtime, &before) {
        return Ok(());
    }
    runtime
        .write_within(b"\x13", deadline)
        .map_err(|_| api("editor_unavailable", "cannot stash Claude editor"))?;
    let observe_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < observe_until {
        let after = runtime
            .agent_observation()
            .map_err(|_| api("editor_unavailable", "cannot observe stashed Claude editor"))?;
        // ^ The emptied editor may show Claude's dimmed suggestion again.
        if after.revision > before.revision
            && (claude_editor(&after) == Some("") || claude_suggestion(runtime, &after))
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(api(
        "editor_stash_unconfirmed",
        "prompt was not sent because fresh empty input was not observed",
    ))
}

fn deliver_agent_exchange(
    runtime: &TerminalRuntime,
    input: &str,
    deadline: Instant,
) -> Result<(), wsx_terminal::TerminalError> {
    runtime.paste_within(input, deadline)?;
    runtime.write_within(b"\r", deadline)
}

/// Upper bound of the PTY write of one round. A healthy agent takes a 64 KiB prompt in
/// milliseconds. An agent that stops reading must not hold its pane and a live handoff.
const AGENT_EXCHANGE_WRITE_LIMIT: Duration = Duration::from_secs(10);

/// The locks a delivery holds, or the exchange that a repeated key already made.
enum DeliveryLocks<'a> {
    Held(MutexGuard<'a, ()>, MutexGuard<'a, ()>),
    Repeat(Box<AgentExchange>),
}

/// Take the mutation lock and the pane-operation lock of a delivery. Other requests take
/// the mutation lock first and then wait for the pane, so a delivery never waits for its
/// pane while it holds the mutation lock: a slow delivery to that pane would hold every
/// change. `repeat` finds the exchange of a repeated key, which is returned at once: the
/// key is saved with the submitted round, also while that round is still in the PTY. A
/// continuation key is compared before the revision, because a repeat carries a newer one.
fn lock_delivery<'a>(
    daemon: &'a Daemon,
    operation: &'a Mutex<()>,
    repeat: impl Fn(&State) -> Option<&AgentExchange>,
) -> DeliveryLocks<'a> {
    loop {
        let mutation = lock(&daemon.mutation_lock);
        if let Some(exchange) = repeat(&lock(&daemon.state)).cloned() {
            return DeliveryLocks::Repeat(Box::new(exchange));
        }
        match operation.try_lock() {
            Ok(guard) => return DeliveryLocks::Held(mutation, guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return DeliveryLocks::Held(mutation, poisoned.into_inner())
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        drop(mutation);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Ends a delivery's hold on live handoff, also when recording its outcome fails.
struct DeliveryInFlight<'a>(&'a Daemon);

impl Drop for DeliveryInFlight<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.deliveries_in_flight = state.deliveries_in_flight.saturating_sub(1);
    }
}

/// Write the persisted `round` of exchange `id` to its PTY and record whether it arrived.
/// ^ docs/agent-orchestration.md "Idempotent delivery": the mutation lock is released while the PTY
/// takes the prompt, so other changes, among them the agent's own reports, go on while an
/// agent reads slowly. `deliveries_in_flight` keeps a live handoff from starting until the
/// outcome is recorded. The pane-operation lock is released before the mutation lock is
/// taken again, because other requests take the mutation lock first.
#[allow(clippy::too_many_arguments)]
fn deliver_exchange_round(
    daemon: &Daemon,
    mutation: MutexGuard<'_, ()>,
    operation: MutexGuard<'_, ()>,
    mut state: MutexGuard<'_, State>,
    id: AgentExchangeId,
    round: u32,
    runtime: &TerminalRuntime,
    stash_draft: bool,
    input: &str,
) -> Result<Response, ApiError> {
    let delivery_sha256 = format!("{:x}", Sha256::digest(input.as_bytes()));
    let deadline_unix_ms = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map_or(0, |exchange| exchange.deadline_unix_ms);
    state.deliveries_in_flight += 1;
    let _in_flight = DeliveryInFlight(daemon);
    drop(state);
    drop(mutation);
    let remaining = Duration::from_millis(deadline_unix_ms.saturating_sub(unix_time_millis()));
    let deadline = Instant::now() + remaining.min(AGENT_EXCHANGE_WRITE_LIMIT);
    let stashed = if stash_draft {
        stash_claude_draft(runtime, deadline)
    } else {
        Ok(())
    };
    let delivered = stashed.and_then(|()| {
        deliver_agent_exchange(runtime, input, deadline)
            .map_err(|error| api("terminal_write_failed", error.to_string()))
    });
    drop(operation);
    let _mutation = lock(&daemon.mutation_lock);
    let mut state = lock(&daemon.state);
    let current = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    // ^ A change made during the write, such as an expiry, keeps its own outcome.
    if current.state != AgentExchangeState::Submitted || current.round != round {
        return Ok(Response::AgentExchange {
            exchange: current,
            frame: None,
        });
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .expect("exchange found above");
    record_delivery(exchange, delivered, delivery_sha256);
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.delivery",
        id.0,
    )?;
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}

fn expire_due_agent_exchanges(daemon: &Daemon, state: &mut State) -> Result<(), ApiError> {
    let now = unix_time_millis();
    if !state
        .persisted
        .agent_exchanges
        .iter()
        .any(|exchange| !exchange.is_terminal() && exchange.deadline_unix_ms <= now)
    {
        return Ok(());
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    for exchange in &mut persisted.agent_exchanges {
        if !exchange.is_terminal() && exchange.deadline_unix_ms <= now {
            exchange.state = AgentExchangeState::Expired;
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.expired",
        0,
    )?;
    Ok(())
}

/// How one round reaches the agent's editor.
#[derive(Default)]
struct ExchangeDelivery {
    stash_draft: bool,
    /// ^ docs/agent-orchestration.md "Idempotent delivery": a repeated key returns the
    /// exchange that the first request made and writes nothing to the PTY.
    key: Option<String>,
}

fn validate_exchange_key(key: Option<&str>) -> Result<(), ApiError> {
    match key {
        Some(key)
            if key.is_empty()
                || key.len() > MAX_AGENT_EXCHANGE_KEY_BYTES
                || !key.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Err(api(
                "invalid_key",
                "key must be 1..128 printable ASCII bytes without spaces",
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
fn create_agent_exchange(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    prompt: String,
    timeout_ms: u64,
    access: AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
) -> Result<Response, ApiError> {
    create_agent_exchange_with(
        daemon,
        pane_id,
        ExchangeInput {
            prompt,
            command: false,
            takeover: false,
        },
        timeout_ms,
        access,
        write_claims,
        ExchangeDelivery::default(),
    )
}

fn create_agent_exchange_with(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    input: ExchangeInput,
    timeout_ms: u64,
    access: AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
    delivery: ExchangeDelivery,
) -> Result<Response, ApiError> {
    let ExchangeDelivery { stash_draft, key } = delivery;
    input.validate()?;
    validate_exchange_key(key.as_deref())?;
    let timeout_ms = validate_exchange_timeout(timeout_ms)?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let (mutation, operation) = match lock_delivery(daemon, &operation_lock, |state| {
        key.as_deref().and_then(|key| {
            state.persisted.agent_exchanges.iter().find(|exchange| {
                exchange.pane_id == pane_id && exchange.key.as_deref() == Some(key)
            })
        })
    }) {
        DeliveryLocks::Held(mutation, operation) => (mutation, operation),
        DeliveryLocks::Repeat(exchange) => {
            return Ok(Response::AgentExchange {
                exchange: *exchange,
                frame: None,
            })
        }
    };
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    expire_due_agent_exchanges(daemon, &mut state)?;
    if state.stopping {
        return Err(api(
            "daemon_unavailable",
            "wsxd cannot start an exchange during shutdown",
        ));
    }
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let agent = pane
        .agent
        .as_ref()
        .filter(|agent| agent.attached && agent.capabilities.prompt)
        .ok_or_else(|| {
            api(
                "agent_unavailable",
                "pane has no attached prompt-capable agent",
            )
        })?;
    if matches!(
        agent.state,
        AgentState::Working | AgentState::Unknown | AgentState::Error
    ) {
        return Err(api(
            "agent_busy",
            "agent is not ready for an explicit exchange",
        ));
    }
    if state
        .persisted
        .agent_exchanges
        .iter()
        .any(|exchange| exchange.pane_id == pane_id && !exchange.is_terminal())
    {
        return Err(api("exchange_busy", "pane already has an active exchange"));
    }
    let session = state
        .persisted
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id)
        .cloned()
        .ok_or_else(|| api("not_found", "session not found"))?;
    let worktree = state
        .persisted
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .cloned()
        .ok_or_else(|| api("not_found", "worktree not found"))?;
    let claims = exchange_claims(access, write_claims, &worktree)?;
    if access == AgentExchangeAccess::Writer
        && has_writer_claim_conflict(&state.persisted.agent_exchanges, &claims)
    {
        return Err(api(
            "write_claim_conflict",
            "an active writer already owns an overlapping path",
        ));
    }
    let generation = state
        .runtime_generations
        .get(&pane_id)
        .cloned()
        .ok_or_else(|| {
            api(
                "terminal_unavailable",
                "pane has no live runtime generation",
            )
        })?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    if stash_draft {
        validate_draft_target(agent, &runtime)?;
    }
    take_terminal_lease(&mut state, pane_id, input.takeover)?;
    let revision = state.revision.saturating_add(1);
    let now = unix_time_millis();
    let mut persisted = state.persisted.clone();
    while persisted.agent_exchanges.len() >= MAX_AGENT_EXCHANGES {
        let Some(index) = persisted
            .agent_exchanges
            .iter()
            .position(|exchange| exchange.is_terminal())
        else {
            return Err(api("exchange_limit", "too many active agent exchanges"));
        };
        persisted.agent_exchanges.remove(index);
    }
    let id = AgentExchangeId(next_id(&mut persisted)?);
    let deadline = now.saturating_add(timeout_ms);
    persisted.agent_exchanges.push(AgentExchange {
        id,
        pane_id,
        session_id: session.id,
        worktree_id: worktree.id,
        project_id: worktree.project_id,
        agent_id: agent.id,
        provider: agent.provider.clone(),
        runtime_generation: generation,
        round: 1,
        access,
        write_claims: claims,
        state: AgentExchangeState::Submitted,
        evidence: AgentExchangeEvidence::IntentPersisted,
        created_unix_ms: now,
        updated_unix_ms: now,
        deadline_unix_ms: deadline,
        delivery_revision: revision,
        delivery_sha256: None,
        native_input_id: None,
        failure: None,
        key,
        continue_key: None,
        command: input.command,
        revision,
    });
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.submitted",
        id.0,
    )?;

    let text = input.terminal_text(id, 1, access);
    deliver_exchange_round(
        daemon,
        mutation,
        operation,
        state,
        id,
        1,
        &runtime,
        stash_draft,
        &text,
    )
}

#[cfg(test)]
fn continue_agent_exchange(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    prompt: String,
    timeout_ms: u64,
) -> Result<Response, ApiError> {
    let input = ExchangeInput {
        prompt,
        command: false,
        takeover: false,
    };
    continue_agent_exchange_with(
        daemon,
        id,
        expected_revision,
        input,
        timeout_ms,
        ExchangeDelivery::default(),
    )
}

fn continue_agent_exchange_with(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    input: ExchangeInput,
    timeout_ms: u64,
    delivery: ExchangeDelivery,
) -> Result<Response, ApiError> {
    let ExchangeDelivery { stash_draft, key } = delivery;
    input.validate()?;
    validate_exchange_key(key.as_deref())?;
    let timeout_ms = validate_exchange_timeout(timeout_ms)?;
    let pane_id = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map(|exchange| exchange.pane_id)
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let (mutation, operation) = match lock_delivery(daemon, &operation_lock, |state| {
        state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id && key.is_some() && exchange.continue_key == key)
    }) {
        DeliveryLocks::Held(mutation, operation) => (mutation, operation),
        DeliveryLocks::Repeat(exchange) => {
            return Ok(Response::AgentExchange {
                exchange: *exchange,
                frame: None,
            })
        }
    };
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    let previous = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    expect_revision(previous.revision, expected_revision)?;
    if !previous.can_continue()
        || matches!(
            previous.state,
            AgentExchangeState::Cancelled
                | AgentExchangeState::TargetExited
                | AgentExchangeState::TargetReplaced
        )
    {
        return Err(api(
            "invalid_exchange_state",
            "exchange cannot continue from its current state",
        ));
    }
    expect_runtime_generation(&state, pane_id, Some(&previous.runtime_generation))?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    if stash_draft {
        if state.stopping || state.handoff_in_progress {
            return Err(api(
                "daemon_unavailable",
                "daemon cannot stash a draft during shutdown or handoff",
            ));
        }
        let agent = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.agent.as_ref())
            .filter(|agent| agent.id == previous.agent_id && agent.attached)
            .ok_or_else(|| api("agent_unavailable", "exchange agent is no longer attached"))?;
        validate_draft_target(agent, &runtime)?;
    }
    let next_round = previous
        .round
        .checked_add(1)
        .ok_or_else(|| api("round_exhausted", "exchange round space exhausted"))?;
    take_terminal_lease(&mut state, pane_id, input.takeover)?;
    let revision = state.revision.saturating_add(1);
    let now = unix_time_millis();
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.round = next_round;
    exchange.state = AgentExchangeState::Submitted;
    exchange.evidence = AgentExchangeEvidence::IntentPersisted;
    // ^ A continuation cannot reuse the prior round's native input binding.
    exchange.delivery_sha256 = None;
    exchange.native_input_id = None;
    exchange.continue_key = key;
    exchange.command = input.command;
    exchange.updated_unix_ms = now;
    exchange.deadline_unix_ms = now.saturating_add(timeout_ms);
    exchange.delivery_revision = revision;
    exchange.revision = revision;
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.continued",
        id.0,
    )?;
    let text = input.terminal_text(id, next_round, previous.access);
    deliver_exchange_round(
        daemon,
        mutation,
        operation,
        state,
        id,
        next_round,
        &runtime,
        stash_draft,
        &text,
    )
}

/// The outcome of one round's PTY delivery. Only a successful delivery publishes the
/// round's input digest (docs/agent-orchestration.md); a failed one keeps its reason.
fn record_delivery(
    exchange: &mut AgentExchange,
    delivered: Result<(), ApiError>,
    delivery_sha256: String,
) {
    match delivered {
        Ok(()) => {
            exchange.delivery_sha256 = Some(delivery_sha256);
            exchange.failure = None;
            exchange.evidence = AgentExchangeEvidence::PtyDelivery;
            exchange.state = AgentExchangeState::Delivered;
        }
        Err(error) => {
            exchange.delivery_sha256 = None;
            exchange.failure = Some(format!("{}: {}", error.code, error.message));
            exchange.state = AgentExchangeState::DeliveryFailed;
        }
    }
}

fn expire_agent_exchange(
    daemon: &Daemon,
    state: &mut State,
    id: AgentExchangeId,
) -> Result<(), ApiError> {
    let now = unix_time_millis();
    let should_expire = state.persisted.agent_exchanges.iter().any(|exchange| {
        exchange.id == id && !exchange.is_terminal() && exchange.deadline_unix_ms <= now
    });
    if !should_expire {
        return Ok(());
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.state = AgentExchangeState::Expired;
    exchange.updated_unix_ms = now;
    exchange.revision = revision;
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.expired",
        id.0,
    )?;
    Ok(())
}

fn agent_exchange_response(
    daemon: &Daemon,
    exchange: AgentExchange,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let frame = if include_frame {
        let runtime = lock(&daemon.state).runtimes.get(&exchange.pane_id).cloned();
        runtime
            .filter(|runtime| !runtime.exited())
            .map(|runtime| runtime.frame().map_err(terminal_api))
            .transpose()?
    } else {
        None
    };
    Ok(Response::AgentExchange { exchange, frame })
}

fn get_agent_exchange(
    daemon: &Daemon,
    id: AgentExchangeId,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let exchange = {
        let mut state = lock(&daemon.state);
        expire_agent_exchange(daemon, &mut state, id)?;
        state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id)
            .cloned()
            .ok_or_else(|| api("not_found", "agent exchange not found"))?
    };
    agent_exchange_response(daemon, exchange, include_frame)
}

fn list_agent_exchanges(daemon: &Daemon, pane_id: Option<PaneId>) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    expire_due_agent_exchanges(daemon, &mut state)?;
    let exchanges = state
        .persisted
        .agent_exchanges
        .iter()
        .filter(|exchange| pane_id.is_none_or(|pane_id| exchange.pane_id == pane_id))
        .cloned()
        .collect();
    Ok(Response::AgentExchanges(exchanges))
}

fn wait_agent_exchange(
    daemon: &Daemon,
    id: AgentExchangeId,
    after_revision: u64,
    timeout_ms: u64,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let timeout = Duration::from_millis(timeout_ms.min(30_000));
    let deadline = Instant::now() + timeout;
    {
        let _mutation = lock(&daemon.mutation_lock);
        expire_agent_exchange(daemon, &mut lock(&daemon.state), id)?;
    }
    let mut state = lock(&daemon.state);
    loop {
        let current = state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id)
            .ok_or_else(|| api("not_found", "agent exchange not found"))?;
        if current.revision > after_revision || current.is_wait_boundary() {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let (next_state, timeout_result) = daemon
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next_state;
        if timeout_result.timed_out() {
            break;
        }
    }
    drop(state);
    {
        let _mutation = lock(&daemon.mutation_lock);
        expire_agent_exchange(daemon, &mut lock(&daemon.state), id)?;
    }
    let exchange = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    agent_exchange_response(daemon, exchange, include_frame)
}

fn report_agent_exchange_receipt(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    round: u32,
    runtime_generation: &str,
    receipt: AgentExchangeReceipt,
) -> Result<Response, ApiError> {
    report_agent_exchange_receipt_with_binding(daemon, id, round, runtime_generation, receipt, None)
}

fn report_agent_exchange_receipt_with_binding(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    round: u32,
    runtime_generation: &str,
    receipt: AgentExchangeReceipt,
    binding: Option<(PaneId, &str, &str)>,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    // ^ docs/agent-orchestration.md: receipts cannot outlive an active request deadline.
    expire_agent_exchange(daemon, &mut state, id)?;
    let previous = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    if previous.runtime_generation != runtime_generation {
        return Err(api(
            "stale_runtime",
            "agent exchange receipt has a stale runtime generation",
        ));
    }
    if previous.round != round {
        return Err(api(
            "stale_exchange_round",
            "agent exchange receipt has a stale round",
        ));
    }
    let supports_receipts = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == previous.pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.id == previous.agent_id && agent.attached && agent.capabilities.exchange_receipts
        });
    if !supports_receipts {
        return Err(api(
            "unsupported_exchange_receipt",
            "bound agent did not advertise request-bound exchange receipts",
        ));
    }
    // ^ docs/agent-orchestration.md: only an exact delivered round may bind native input.
    if let Some((pane_id, digest, input_id)) = binding {
        expect_runtime_generation(&state, previous.pane_id, Some(runtime_generation))?;
        if pane_id != previous.pane_id
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || previous.delivery_sha256.as_deref() != Some(digest)
            || input_id.is_empty()
            || input_id.len() > 128
            || !input_id.bytes().all(|byte| byte.is_ascii_graphic())
            || previous
                .native_input_id
                .as_deref()
                .is_some_and(|id| id != input_id)
        {
            return Err(api(
                "invalid_delivery_binding",
                "receipt does not match the exact delivered native input",
            ));
        }
        if previous.state != AgentExchangeState::Completed
            && previous.deadline_unix_ms <= unix_time_millis()
        {
            return Err(api(
                "receipt_deadline_exceeded",
                "native receipt arrived after its request deadline",
            ));
        }
        if receipt == AgentExchangeReceipt::Completed && previous.native_input_id.is_none() {
            return Err(api(
                "native_input_unaccepted",
                "native completion requires a prior bound acceptance",
            ));
        }
    } else if previous.native_input_id.is_some() {
        return Err(api(
            "bound_receipt_required",
            "native-bound input requires a bound receipt",
        ));
    }
    let next_state = match receipt {
        AgentExchangeReceipt::Accepted
            if matches!(
                previous.state,
                AgentExchangeState::Submitted
                    | AgentExchangeState::Delivered
                    | AgentExchangeState::Accepted
                    | AgentExchangeState::WorkingObserved
                    | AgentExchangeState::BlockedObserved
            ) =>
        {
            if matches!(
                previous.state,
                AgentExchangeState::WorkingObserved | AgentExchangeState::BlockedObserved
            ) {
                previous.state
            } else {
                AgentExchangeState::Accepted
            }
        }
        AgentExchangeReceipt::Completed
            if matches!(
                previous.state,
                AgentExchangeState::Submitted
                    | AgentExchangeState::Delivered
                    | AgentExchangeState::Accepted
                    | AgentExchangeState::WorkingObserved
                    | AgentExchangeState::BlockedObserved
                    | AgentExchangeState::DoneObserved
                    | AgentExchangeState::CancelRequested
                    | AgentExchangeState::Completed
            ) =>
        {
            AgentExchangeState::Completed
        }
        _ => {
            return Err(api(
                "invalid_exchange_state",
                "exchange cannot accept a request-bound receipt in its current state",
            ))
        }
    };
    if previous.state == next_state
        && previous.evidence == AgentExchangeEvidence::RequestBound
        && binding
            .is_none_or(|(_, _, input_id)| previous.native_input_id.as_deref() == Some(input_id))
    {
        return Ok(Response::AgentExchange {
            exchange: previous,
            frame: None,
        });
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .expect("validated exchange remains present");
    exchange.state = next_state;
    exchange.evidence = AgentExchangeEvidence::RequestBound;
    if let Some((_, _, input_id)) = binding {
        exchange.native_input_id = Some(input_id.to_owned());
    }
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.receipt",
        id.0,
    )?;
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}

/// docs/agent-orchestration.md "Steer": a working Claude takes a message typed into its input
/// line at its next step, fires `UserPromptSubmit` for it at once and saves it as a
/// `queued_command` (Claude Code 2.1.309). No exchange: the turn that runs keeps its round.
fn steer_agent(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    text: &str,
    takeover: bool,
) -> Result<Response, ApiError> {
    if text.trim().is_empty() || text.len() > MAX_AGENT_EXCHANGE_PROMPT_BYTES {
        return Err(api(
            "invalid_request",
            "a steer message must have text and fit an exchange prompt",
        ));
    }
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let runtime = {
        let mut state = lock(&daemon.state);
        let agent = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .ok_or_else(|| api("not_found", "pane not found"))?
            .agent
            .clone()
            .filter(|agent| agent.attached)
            .ok_or_else(|| api("agent_unavailable", "pane has no attached agent"))?;
        if agent.provider != "claude" {
            return Err(api(
                "steer_unsupported",
                "only Claude takes a message during its turn",
            ));
        }
        if agent.state != AgentState::Working {
            return Err(api("agent_not_working", "the agent is not at work"));
        }
        // ^ A round still waiting to be taken would read this prompt's receipt as its own
        // refusal (receive_prompt).
        if state.persisted.agent_exchanges.iter().any(|exchange| {
            exchange.pane_id == pane_id
                && matches!(
                    exchange.state,
                    AgentExchangeState::Submitted | AgentExchangeState::Delivered
                )
        }) {
            return Err(api("exchange_busy", "a delivered message is not taken yet"));
        }
        let runtime = state
            .runtimes
            .get(&pane_id)
            .filter(|runtime| !runtime.exited())
            .cloned()
            .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
        let observation = runtime
            .agent_observation()
            .map_err(|_| api("editor_unavailable", "cannot observe Claude's input line"))?;
        // ^ Typed text in the input line would be sent with this message.
        if claude_input(&observation) != Some("") {
            return Err(api(
                "editor_unavailable",
                "Claude's input line is not empty",
            ));
        }
        take_terminal_lease(&mut state, pane_id, takeover)?;
        runtime
    };
    deliver_agent_exchange(&runtime, text, Instant::now() + AGENT_EXCHANGE_WRITE_LIMIT)
        .map_err(terminal_api)?;
    let revision = lock(&daemon.state).revision;
    Ok(Response::Ack { revision })
}

fn cancel_agent_exchange(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    takeover: bool,
) -> Result<Response, ApiError> {
    let pane_id = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map(|exchange| exchange.pane_id)
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    let exchange = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    expect_revision(exchange.revision, expected_revision)?;
    if exchange.is_terminal() {
        return Err(api(
            "invalid_exchange_state",
            "exchange is already terminal",
        ));
    }
    expect_runtime_generation(&state, pane_id, Some(&exchange.runtime_generation))?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    // ^ docs/agent-orchestration.md "Cancel": the provider's own interrupt. Escape ends a
    // Claude, Pi or omp turn and leaves the agent ready. Ctrl+C does not stop a working
    // Pi, and in Claude its second press quits. Both were checked against the real
    // programs on 2026-10-12. Other providers get Ctrl+C.
    let escape = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.attached
                && (agent.capabilities.escape_interrupts
                    || matches!(agent.provider.as_str(), "pi" | "omp"))
        });
    take_terminal_lease(&mut state, pane_id, takeover)?;
    runtime
        .write(if escape { b"\x1b" } else { b"\x03" })
        .map_err(terminal_api)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.state = AgentExchangeState::CancelRequested;
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.cancel_requested",
        id.0,
    )?;
    drop(state);
    // Claude has no interrupt hook: only its screen shows that the turn ended. Without
    // this the agent stays working here and the exchange stays in cancel_requested.
    request_claude_reconcile(daemon, pane_id, true);
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}

fn expect_runtime_generation(
    state: &State,
    pane_id: PaneId,
    provided: Option<&str>,
) -> Result<(), ApiError> {
    let expected = state.runtime_generations.get(&pane_id).ok_or_else(|| {
        api(
            "terminal_unavailable",
            "pane has no live runtime generation",
        )
    })?;
    if provided == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(api(
            "stale_runtime",
            "agent mutation does not belong to the live pane runtime",
        ))
    }
}

/// Ends an agent's attachment: its runtime is gone or has not yet confirmed itself.
fn detach_agent(agent: &mut AgentInfo) {
    agent.attached = false;
    set_agent_state(agent, AgentState::Unknown);
    agent.presence_id = None;
}

/// Sets an agent's state; a changed state records when it began.
fn set_agent_state(agent: &mut AgentInfo, state: AgentState) {
    if agent.state != state || agent.state_since_unix_ms.is_none() {
        agent.state = state;
        agent.state_since_unix_ms = Some(unix_time_millis());
    }
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The clone-mutate-save-assign tail: the durable file first, then the in-memory copy, so a
/// failed save leaves `state.persisted` as it was. The fsync stays under the caller's lock.
fn commit_persisted(
    daemon: &Daemon,
    state: &mut State,
    persisted: Persisted,
) -> Result<(), ApiError> {
    state.persist(&daemon.state_path, persisted).map_err(io_api)
}

/// `commit_persisted`, then publish the change under `revision`.
fn commit_publish(
    daemon: &Daemon,
    state: &mut State,
    persisted: Persisted,
    revision: u64,
    entity: &str,
    id: u64,
) -> Result<(), ApiError> {
    commit_persisted(daemon, state, persisted)?;
    publish(daemon, state, revision, entity, id);
    Ok(())
}

fn mutate<F>(daemon: &Arc<Daemon>, mutation: F) -> Result<Response, ApiError>
where
    F: FnOnce(&mut Persisted, u64) -> Result<(&'static str, u64), ApiError>,
{
    let mut state = lock(&daemon.state);
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let (entity, id) = mutation(&mut persisted, revision)?;
    if let Some(session) = persisted
        .sessions
        .iter_mut()
        .find(|session| session.id.0 == id)
    {
        session.revision = revision;
    }
    commit_persisted(daemon, &mut state, persisted)?;
    publish(daemon, &mut state, revision, entity, id);
    Ok(Response::Ack { revision })
}
fn mutate_runtime<F>(daemon: &Daemon, mutation: F) -> Result<Response, ApiError>
where
    F: FnOnce(&mut State) -> Result<(), ApiError>,
{
    let mut state = lock(&daemon.state);
    mutation(&mut state)?;
    Ok(Response::Ack {
        revision: state.revision,
    })
}
fn publish(daemon: &Daemon, state: &mut State, revision: u64, entity: &str, id: u64) {
    state.revision = revision;
    state.push_event(Event::Changed {
        revision,
        entity: entity.into(),
        id,
    });
    state.push_plugin_event(entity, revision, id);
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
}

fn bump(daemon: &Daemon, state: &mut State, entity: &str, id: u64) -> u64 {
    let revision = state.revision.saturating_add(1);
    publish(daemon, state, revision, entity, id);
    revision
}

// ^ [[Session Model]] Agent adapters need stable pane identity from this spawn
// boundary; crates/wsx-core/src/integration owns installation and bundled assets.
fn terminal_agent_environment(pane_id: PaneId, runtime_generation: &str) -> Vec<(String, String)> {
    let mut environment = vec![
        (WSX_PANE_ID_ENV.into(), pane_id.to_string()),
        (
            WSX_RUNTIME_GENERATION_ENV.into(),
            runtime_generation.to_string(),
        ),
    ];
    // ^ docs/daemon-discovery.md: a shell profile may change XDG_STATE_HOME or HOME per
    // directory; the pane's clients keep the socket of the daemon that owns the pane.
    let socket = default_socket_path();
    environment.push(("WSX_SOCKET".into(), socket.to_string_lossy().into_owned()));
    environment.push((
        "WSX_AGENT_REPORT_BIN".into(),
        socket
            .with_extension("reporter")
            .to_string_lossy()
            .into_owned(),
    ));
    // ^ docs/terminal-context.md "Claude in a pane": Claude's inline renderer leaves a full
    // copy of a tall reply in the scrollback on every resize (anthropics/claude-code#103181),
    // and a phone and a desktop resize a pane often. Its full-screen renderer draws on the
    // alternate screen instead. An owner's own value, here or set later by the shell, wins.
    if std::env::var_os(CLAUDE_FULLSCREEN_ENV).is_none() {
        environment.push((CLAUDE_FULLSCREEN_ENV.into(), "1".into()));
    }
    environment
}

/// Claude Code's switch to its full-screen renderer for one process
/// (https://code.claude.com/docs/en/fullscreen).
const CLAUDE_FULLSCREEN_ENV: &str = "CLAUDE_CODE_NO_FLICKER";

fn runtime_notify(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    runtime_generation: Option<String>,
) -> Arc<dyn Fn() + Send + Sync> {
    let weak: Weak<Daemon> = Arc::downgrade(daemon);
    Arc::new(move || {
        let Some(daemon) = weak.upgrade() else { return };
        let mut state = lock(&daemon.state);
        // ^ Bind the callback itself to the registered runtime generation. Looking up only the
        // current runtime would let an abandoned callback publish a change for its successor.
        if state.runtime_generations.get(&pane_id) != runtime_generation.as_ref() {
            return;
        }
        let Some(runtime) = state.runtimes.get(&pane_id).cloned() else {
            return;
        };
        if runtime.exited() {
            record_terminal_exit(&daemon, &mut state, pane_id);
        } else {
            let revision = state.revision.saturating_add(1);
            state.revision = revision;
            state.push_terminal_changed(revision, pane_id);
            daemon.changed.notify_all();
        }
    })
}

fn spawn_runtime(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: &Path,
    recipe: &LaunchRecipe,
) -> Result<TerminalRuntime, ApiError> {
    let runtime_generation = next_runtime_generation(daemon);
    let notify = runtime_notify(daemon, pane_id, Some(runtime_generation.clone()));
    {
        let mut state = lock(&daemon.state);
        clear_agent_wake(&mut state, pane_id);
        state.agent_presence_leases.remove(&pane_id);
        state.claude_reconciliations.remove(&pane_id);
        state
            .runtime_generations
            .insert(pane_id, runtime_generation.clone());
    }
    let startup = startup_input(recipe);
    let runtime = TerminalRuntime::spawn(
        pane_id,
        terminal_id,
        cwd,
        &recipe.command,
        &terminal_agent_environment(pane_id, &runtime_generation),
        startup.as_deref(),
        recipe.rows,
        recipe.cols,
        notify,
    );
    if runtime.is_err() {
        let mut state = lock(&daemon.state);
        if state.runtime_generations.get(&pane_id) == Some(&runtime_generation) {
            state.runtime_generations.remove(&pane_id);
        }
    }
    runtime.map_err(terminal_api)
}

fn next_runtime_generation(daemon: &Daemon) -> String {
    let sequence = daemon
        .next_runtime_generation
        .fetch_add(1, Ordering::Relaxed);
    format!("{:016x}:{sequence:016x}", daemon.epoch)
}

fn record_terminal_exit(daemon: &Daemon, state: &mut State, pane_id: PaneId) {
    state.forget_pane_observations(pane_id);
    let Some(pane) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id && !pane.exited)
    else {
        return;
    };
    state.revision = state.revision.saturating_add(1);
    let revision = state.revision;
    pane.exited = true;
    if let Some(agent) = pane.agent.as_mut() {
        detach_agent(agent);
    }
    pane.revision = revision;
    terminate_agent_exchanges(
        &mut state.persisted,
        &[pane_id],
        AgentExchangeState::TargetExited,
        revision,
    );
    state.push_event(Event::Exited { revision, pane_id });
    state.push_plugin_event("terminal.exited", revision, pane_id.0);
    state.persistence_dirty = true;
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
}

fn apply_claude_terminal_observation(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: &str,
    observation: AgentTerminalObservation,
    now: Instant,
) -> Result<(), ApiError> {
    let terminal = claude_status::classify(&observation.title, &observation.text);
    let mut state = lock(&daemon.state);
    if state.runtime_generations.get(&pane_id).map(String::as_str) != Some(runtime_generation) {
        return Ok(());
    }
    let Some(pane_index) = state
        .persisted
        .panes
        .iter()
        .position(|pane| pane.id == pane_id)
    else {
        return Ok(());
    };
    let Some(agent) = state.persisted.panes[pane_index]
        .agent
        .as_ref()
        .filter(|agent| agent.attached && agent.provider == "claude")
    else {
        state.claude_reconciliations.remove(&pane_id);
        return Ok(());
    };
    let event_at = now
        .checked_sub(claude_status::WORKING_EVENT_LEAD)
        .unwrap_or(now);
    let mut reconciliation = state
        .claude_reconciliations
        .get(&pane_id)
        .copied()
        .unwrap_or(ClaudeReconciliation {
            event_state: agent.state,
            event_at,
            terminal_revision: None,
            terminal: None,
            interrupt_requested: false,
            interrupt_revision: None,
        });
    // An old prompt cannot confirm that Ctrl+C settled the current turn.
    let terminal = if terminal == Some(claude_status::TerminalEvidence::Idle)
        && reconciliation
            .interrupt_revision
            .is_some_and(|revision| observation.revision <= revision)
    {
        None
    } else {
        terminal
    };
    reconciliation.terminal_revision = Some(observation.revision);
    reconciliation.terminal = terminal;
    if terminal.is_none() {
        state.claude_reconciliations.insert(pane_id, reconciliation);
        return Ok(());
    }
    let effective = reconciliation.effective(now);
    if effective == agent.state {
        state.claude_reconciliations.insert(pane_id, reconciliation);
        return Ok(());
    }

    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    set_agent_state(
        persisted.panes[pane_index]
            .agent
            .as_mut()
            .expect("validated Claude agent must remain present"),
        effective,
    );
    persisted.panes[pane_index].revision = revision;
    if effective == AgentState::Working {
        let project_index = project_index_for_pane(&persisted, pane_id)?;
        persisted.projects[project_index].last_agent_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
    }
    // A state read from the screen ends a cancelled exchange like a reported one does.
    observe_agent_exchange(
        &mut persisted,
        pane_id,
        effective,
        true,
        revision,
        unix_time_millis(),
    );
    commit_persisted(daemon, &mut state, persisted)?;
    state.claude_reconciliations.insert(pane_id, reconciliation);
    if effective != AgentState::Working {
        clear_agent_wake(&mut state, pane_id);
    }
    bump(daemon, &mut state, "agent.reconciled", pane_id.0);
    Ok(())
}

fn spawn_claude_reconciler(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let now = Instant::now();
        let targets = {
            let state = lock(&daemon.state);
            if state.stopping {
                return;
            }
            state
                .persisted
                .panes
                .iter()
                .filter_map(|pane| {
                    let agent = pane
                        .agent
                        .as_ref()
                        .filter(|agent| agent.attached && agent.provider == "claude")?;
                    let runtime = state.runtimes.get(&pane.id)?.clone();
                    let generation = state.runtime_generations.get(&pane.id)?.clone();
                    let runtime_revision = runtime.revision();
                    let reconciliation = state.claude_reconciliations.get(&pane.id).copied();
                    let grace_due = reconciliation.is_some_and(|reconciliation| {
                        reconciliation.event_state == AgentState::Working
                            && reconciliation.terminal
                                == Some(claude_status::TerminalEvidence::Idle)
                            && now.saturating_duration_since(reconciliation.event_at)
                                >= claude_status::WORKING_EVENT_LEAD
                            && reconciliation.effective(now) != agent.state
                    });
                    (reconciliation.is_none()
                        || reconciliation.and_then(|value| value.terminal_revision)
                            != Some(runtime_revision)
                        || grace_due)
                        .then_some((pane.id, generation, runtime))
                })
                .collect::<Vec<_>>()
        };
        for (pane_id, generation, runtime) in targets {
            let Ok(observation) = runtime.agent_observation() else {
                continue;
            };
            let _ = apply_claude_terminal_observation(
                &daemon,
                pane_id,
                &generation,
                observation,
                Instant::now(),
            );
        }
        thread::sleep(CLAUDE_RECONCILE_INTERVAL);
    })
}

fn has_fresh_working_agent(state: &mut State, now: Instant) -> bool {
    let live_working = state
        .persisted
        .panes
        .iter()
        .filter(|pane| {
            pane.agent
                .as_ref()
                .is_some_and(|agent| agent.state == AgentState::Working)
                && state.runtime_generations.contains_key(&pane.id)
                && state
                    .runtimes
                    .get(&pane.id)
                    .is_some_and(|runtime| !runtime.exited())
        })
        .map(|pane| pane.id)
        .collect::<HashSet<_>>();
    state.agent_wake_leases.retain(|pane_id, renewed_at| {
        live_working.contains(pane_id)
            && now.saturating_duration_since(*renewed_at) < AGENT_WAKE_LEASE_TTL
    });
    state
        .agent_wake_tokens
        .retain(|pane_id, _| state.agent_wake_leases.contains_key(pane_id));
    !state.agent_wake_leases.is_empty()
}

#[cfg(target_os = "macos")]
fn spawn_wake_controller(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || {
        let mut controller = wake::Controller::new();
        let mut reported_config_error = None;
        loop {
            let now = Instant::now();
            let (stopping, has_working_agent) = {
                let mut state = lock(&daemon.state);
                (state.stopping, has_fresh_working_agent(&mut state, now))
            };
            if stopping {
                return;
            }
            if !has_working_agent {
                controller.reconcile(false, now);
                thread::sleep(WAKE_POLL_INTERVAL);
                continue;
            }
            let enabled = match GlobalConfig::load() {
                Ok((config, None)) => {
                    reported_config_error = None;
                    config.wake_mode
                }
                Ok((_, Some(warning))) => {
                    if reported_config_error.as_deref() != Some(warning.as_str()) {
                        eprintln!("wsxd wake mode disabled: {warning}");
                        reported_config_error = Some(warning);
                    }
                    false
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    if reported_config_error.as_deref() != Some(&error) {
                        eprintln!("wsxd wake mode disabled: {error}");
                        reported_config_error = Some(error);
                    }
                    false
                }
            };
            controller.reconcile(enabled && has_working_agent, now);
            thread::sleep(WAKE_POLL_INTERVAL);
        }
    })
}

#[cfg(not(target_os = "macos"))]
fn spawn_wake_controller(_daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    thread::spawn(|| {})
}

fn live_runtimes(daemon: &Daemon) -> Option<Vec<(PaneId, Arc<TerminalRuntime>)>> {
    let state = lock(&daemon.state);
    (!state.stopping).then(|| {
        state
            .runtimes
            .iter()
            .filter(|(_, runtime)| !runtime.exited())
            .map(|(pane_id, runtime)| (*pane_id, Arc::clone(runtime)))
            .collect()
    })
}

fn scan_runtimes_current(state: &State, sampled: &[(PaneId, Arc<TerminalRuntime>)]) -> bool {
    !state.stopping
        && state
            .runtimes
            .iter()
            .filter(|(_, runtime)| !runtime.exited())
            .count()
            == sampled.len()
        && sampled.iter().all(|(pane_id, runtime)| {
            state
                .runtimes
                .get(pane_id)
                .is_some_and(|current| Arc::ptr_eq(current, runtime))
                && !runtime.exited()
        })
}

// ^ A foreground group is cheap kernel PTY metadata. Keep its live indicator
// independent of the two-second lsof/ps listener scan and never infer provider identity.
fn spawn_foreground_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let Some(runtimes) = live_runtimes(&daemon) else {
            return;
        };
        let foreground_jobs = runtimes
            .iter()
            .filter_map(|(pane_id, runtime)| runtime.has_foreground_job().then_some(*pane_id))
            .collect();
        let mut state = lock(&daemon.state);
        if scan_runtimes_current(&state, &runtimes)
            && apply_foreground_scan(&mut state, foreground_jobs)
        {
            bump(&daemon, &mut state, "pane_activity.changed", 0);
        }
        // ^ This loop's short interval also times rounds that were never taken.
        if !state.handoff_in_progress {
            if let Err(error) = fail_untaken_rounds(&daemon, &mut state) {
                eprintln!("wsxd: cannot end an untaken round: {}", error.message);
            }
        }
        drop(state);
        thread::sleep(FOREGROUND_SCAN_INTERVAL);
    })
}

fn spawn_port_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let Some(runtimes) = live_runtimes(&daemon) else {
            return;
        };
        let process_groups = runtimes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                runtime.process_group_id().map(|group| (*pane_id, group))
            })
            .collect::<HashMap<_, _>>();
        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports().map(|listeners| {
                let process_terminals = scan_process_terminals();
                attribute_listening_ports(&process_groups, &listeners, process_terminals.as_ref())
            })
        };
        let mut state = lock(&daemon.state);
        if scan_runtimes_current(&state, &runtimes) && apply_port_scan(&mut state, detected) {
            bump(&daemon, &mut state, "pane_activity.changed", 0);
        }
        drop(state);

        let started = Instant::now();
        while started.elapsed() < PORT_SCAN_INTERVAL {
            if lock(&daemon.state).stopping {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ListenerProcess {
    pid: libc::pid_t,
    group: libc::pid_t,
    ports: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessTerminal {
    group: libc::pid_t,
    tty: String,
}

fn apply_foreground_scan(state: &mut State, foreground_jobs: HashSet<PaneId>) -> bool {
    if state.foreground_jobs == foreground_jobs {
        return false;
    }
    state.foreground_jobs = foreground_jobs;
    true
}

fn apply_port_scan(state: &mut State, detected: Option<HashMap<PaneId, Vec<u16>>>) -> bool {
    let Some(next) = detected else { return false };
    state.listener_scan_complete = true;
    if state.listening_ports == next {
        return false;
    }
    state.listening_ports = next;
    true
}

fn scan_listening_ports() -> Option<Vec<ListenerProcess>> {
    let mut command = Command::new("lsof");
    command.args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpgn"]);
    let bytes = bounded_command_output(command, true)?;
    parse_lsof_ports(&String::from_utf8_lossy(&bytes))
}

fn scan_process_terminals() -> Option<HashMap<libc::pid_t, ProcessTerminal>> {
    let mut command = Command::new("ps");
    command.args(["-axo", "pid=,pgid=,tty="]);
    let bytes = bounded_command_output(command, false)?;
    parse_process_terminals(&String::from_utf8_lossy(&bytes))
}

fn bounded_command_output(mut command: Command, accept_code_one: bool) -> Option<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut bounded = stdout.take(MAX_PORT_SCAN_BYTES + 1);
        bounded.read_to_end(&mut bytes).ok()?;
        (bytes.len() as u64 <= MAX_PORT_SCAN_BYTES).then_some(bytes)
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PORT_SCAN_TIMEOUT => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()??;
    (status.success() || (accept_code_one && status.code() == Some(1))).then_some(bytes)
}

fn parse_lsof_ports(output: &str) -> Option<Vec<ListenerProcess>> {
    let mut current_pid = None;
    let mut processes = HashMap::<libc::pid_t, (Option<libc::pid_t>, Vec<u16>)>::new();
    for line in output.lines() {
        match line.as_bytes().first().copied() {
            Some(b'p') => {
                current_pid = line[1..].parse::<libc::pid_t>().ok();
                if let Some(pid) = current_pid {
                    processes.entry(pid).or_default();
                }
            }
            Some(b'g') => {
                if let Some(pid) = current_pid {
                    processes.entry(pid).or_default().0 = line[1..].parse().ok();
                }
            }
            Some(b'n') => {
                let Some(pid) = current_pid else {
                    continue;
                };
                let endpoint = line[1..].split_whitespace().next().unwrap_or_default();
                let Some(port) = endpoint
                    .rsplit(':')
                    .next()
                    .and_then(|value| value.parse().ok())
                else {
                    continue;
                };
                processes.entry(pid).or_default().1.push(port);
            }
            _ => {}
        }
    }
    let mut listeners = processes
        .into_iter()
        .filter_map(|(pid, (group, mut ports))| {
            ports.sort_unstable();
            ports.dedup();
            Some(ListenerProcess {
                pid,
                group: group?,
                ports,
            })
        })
        .collect::<Vec<_>>();
    listeners.sort_by_key(|listener| listener.pid);
    Some(listeners)
}

fn parse_process_terminals(output: &str) -> Option<HashMap<libc::pid_t, ProcessTerminal>> {
    let mut processes = HashMap::new();
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(group), Some(tty), None) = (
            fields.next().and_then(|value| value.parse().ok()),
            fields.next().and_then(|value| value.parse().ok()),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        if !matches!(tty, "?" | "??" | "-") {
            processes.insert(
                pid,
                ProcessTerminal {
                    group,
                    tty: tty.to_string(),
                },
            );
        }
    }
    Some(processes)
}

fn attribute_listening_ports(
    process_groups: &HashMap<PaneId, libc::pid_t>,
    listeners: &[ListenerProcess],
    process_terminals: Option<&HashMap<libc::pid_t, ProcessTerminal>>,
) -> HashMap<PaneId, Vec<u16>> {
    let panes_by_group = process_groups
        .iter()
        .map(|(pane_id, group)| (*group, *pane_id))
        .collect::<HashMap<_, _>>();
    let mut panes_by_tty = HashMap::<&str, Option<PaneId>>::new();
    if let Some(processes) = process_terminals {
        for process in processes.values() {
            let Some(pane_id) = panes_by_group.get(&process.group).copied() else {
                continue;
            };
            panes_by_tty
                .entry(&process.tty)
                .and_modify(|owner| {
                    if *owner != Some(pane_id) {
                        *owner = None;
                    }
                })
                .or_insert(Some(pane_id));
        }
    }

    let mut ports_by_pane = HashMap::<PaneId, Vec<u16>>::new();
    for listener in listeners {
        let pane_id = panes_by_group.get(&listener.group).copied().or_else(|| {
            let processes = process_terminals?;
            let tty = &processes.get(&listener.pid)?.tty;
            panes_by_tty.get(tty.as_str()).copied().flatten()
        });
        if let Some(pane_id) = pane_id {
            ports_by_pane
                .entry(pane_id)
                .or_default()
                .extend(&listener.ports);
        }
    }
    for ports in ports_by_pane.values_mut() {
        ports.sort_unstable();
        ports.dedup();
    }
    ports_by_pane
}

fn spawn_plugin_dispatcher(daemon: &Arc<Daemon>) -> io::Result<thread::JoinHandle<()>> {
    let daemon = Arc::clone(daemon);
    thread::Builder::new()
        .name("wsx-plugin-dispatch".into())
        .spawn(move || loop {
            let next = {
                let mut state = lock(&daemon.state);
                while state.plugin_events.is_empty() && !state.stopping {
                    state = daemon
                        .plugin_changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if state.stopping {
                    return;
                }
                state
                    .plugin_events
                    .pop_front()
                    .map(|(name, payload)| (state.plugins.clone(), name, payload))
            };
            if let Some((plugins, name, payload)) = next {
                plugins::emit(&plugins, &name, &payload);
            }
        })
}

/// The pane's operation lock. A pane id with no runtime and no persisted record gets a lock that nothing keeps:
/// the caller's own check then answers with its usual error, and a client that sends made-up
/// pane ids cannot grow `terminal_operation_locks`.
fn terminal_operation_lock(daemon: &Daemon, pane_id: PaneId) -> Arc<Mutex<()>> {
    let mut state = lock(&daemon.state);
    let known = state.runtimes.contains_key(&pane_id)
        || state.persisted.panes.iter().any(|pane| pane.id == pane_id);
    if !known {
        return Arc::new(Mutex::new(()));
    }
    Arc::clone(
        state
            .terminal_operation_locks
            .entry(pane_id)
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn leased_runtime(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    let mut state = lock(&daemon.state);
    if state.stopping || state.handoff_in_progress {
        return Err(api(
            "daemon_replacing",
            "wsxd is replacing its runtime owner",
        ));
    }
    refresh_lease(&mut state, pane_id, access)?;
    Ok((
        Arc::clone(require_runtime(&state, pane_id)?),
        state.revision,
    ))
}
fn require_runtime(state: &State, pane_id: PaneId) -> Result<&Arc<TerminalRuntime>, ApiError> {
    state
        .runtimes
        .get(&pane_id)
        .ok_or_else(|| api("terminal_unavailable", "terminal process is not running"))
}
fn require_lease(state: &State, pane_id: PaneId, access: LeaseAccess) -> Result<(), ApiError> {
    let owned = state.leases.get(&pane_id).is_some_and(|lease| {
        let identity_matches = match access {
            LeaseAccess::Client(client_id) => lease.client_id == client_id,
            LeaseAccess::Stream {
                client_id,
                generation,
            } => lease.client_id == client_id && lease.generation == generation,
        };
        identity_matches && lease.expires_at > Instant::now()
    });
    if owned {
        Ok(())
    } else {
        Err(api(
            "lease_required",
            "client does not own an active terminal lease",
        ))
    }
}

fn refresh_lease(state: &mut State, pane_id: PaneId, access: LeaseAccess) -> Result<(), ApiError> {
    require_lease(state, pane_id, access)?;
    if let Some(lease) = state.leases.get_mut(&pane_id) {
        lease.expires_at = Instant::now() + LEASE_TTL;
    }
    Ok(())
}
fn session_mut(state: &mut Persisted, id: SessionId) -> Result<&mut Session, ApiError> {
    state
        .sessions
        .iter_mut()
        .find(|session| session.id == id)
        .ok_or_else(|| api("not_found", "session not found"))
}
fn next_id(state: &mut Persisted) -> Result<u64, ApiError> {
    let id = state.next_id.max(1);
    state.next_id = id
        .checked_add(1)
        .ok_or_else(|| api("id_exhausted", "stable identifier space exhausted"))?;
    Ok(id)
}
fn expect_revision(actual: u64, expected: u64) -> Result<(), ApiError> {
    if actual == expected {
        Ok(())
    } else {
        Err(api(
            "revision_conflict",
            format!("expected {expected}, current {actual}"),
        ))
    }
}
fn bounded_label(value: String) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        Err(api("invalid_label", "label must be 1..128 bytes"))
    } else {
        Ok(value.into())
    }
}
/// A non-empty value of at most `max` bytes, every byte accepted by `allowed`.
fn bounded_ascii(
    value: String,
    max: usize,
    allowed: impl Fn(u8) -> bool,
    code: &str,
    message: &str,
) -> Result<String, ApiError> {
    if value.is_empty() || value.len() > max || !value.bytes().all(allowed) {
        Err(api(code, message))
    } else {
        Ok(value)
    }
}
fn bounded_provider(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value.trim().to_ascii_lowercase(),
        64,
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'),
        "invalid_provider",
        "invalid provider name",
    )
}
fn bounded_wake_token(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value,
        128,
        |byte| byte.is_ascii_alphanumeric() || byte == b'-',
        "invalid_wake_token",
        "wake token is malformed",
    )
}
fn validate_presence_id(value: String) -> Result<String, ApiError> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes.iter().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        Err(api("invalid_presence", "presence ID must be a UUID"))
    } else {
        Ok(value)
    }
}
fn bounded_runtime_generation(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value,
        128,
        |byte| byte.is_ascii_hexdigit() || byte == b':',
        "invalid_runtime_generation",
        "runtime generation is malformed",
    )
}

fn validated_session_ref(
    session_ref: Option<AgentSessionRef>,
) -> Result<Option<AgentSessionRef>, ApiError> {
    let Some(session_ref) = session_ref else {
        return Ok(None);
    };
    let transcript_path = session_ref.transcript_path;
    let validated = match session_ref.kind {
        AgentSessionRefKind::Id => AgentSessionRef::id(session_ref.value),
        AgentSessionRefKind::Path => AgentSessionRef::path(session_ref.value),
    }
    .ok_or_else(|| api("invalid_agent_session", "invalid agent session reference"))?;
    let validated = if let Some(path) = transcript_path {
        validated
            .with_transcript_path(
                path.to_str()
                    .ok_or_else(|| api("invalid_agent_session", "invalid transcript path"))?
                    .to_owned(),
            )
            .ok_or_else(|| api("invalid_agent_session", "invalid transcript path"))?
    } else {
        validated
    };
    Ok(Some(validated))
}

fn truncate_utf8(mut value: String, max: usize) -> String {
    if value.len() > max {
        let boundary = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= max)
            .last()
            .unwrap_or(0);
        value.truncate(boundary);
    }
    value
}
fn normalize_command(command: Vec<String>) -> Vec<String> {
    if command.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
    } else {
        command
    }
}
fn startup_input(recipe: &LaunchRecipe) -> Option<Vec<u8>> {
    recipe.initial_input.as_ref().map(|input| {
        let mut bytes = input.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    })
}
fn validate_recipe(recipe: &LaunchRecipe) -> Result<(), String> {
    let startup = startup_input(recipe);
    validate_launch(
        recipe.rows,
        recipe.cols,
        &recipe.command,
        startup.as_deref(),
    )
    .map_err(|error| error.to_string())
}
fn launch_recipe(
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<LaunchRecipe, ApiError> {
    let recipe = LaunchRecipe {
        command: normalize_command(command),
        initial_input,
        rows,
        cols,
    };
    validate_recipe(&recipe).map_err(|error| api("invalid_launch", error))?;
    Ok(recipe)
}
fn default_launch_recipe() -> LaunchRecipe {
    launch_recipe(Vec::new(), None, 24, 80).unwrap_or_else(|_| LaunchRecipe {
        command: vec!["/bin/sh".into()],
        initial_input: None,
        rows: 24,
        cols: 80,
    })
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

fn validate_persisted(state: &Persisted) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "inconsistent wsx state");
    let mut ids = HashSet::new();
    let mut max_id = 0;
    let mut insert = |id: u64| {
        max_id = max_id.max(id);
        id > 0 && ids.insert(id)
    };
    for project in &state.projects {
        if !insert(project.id.0) {
            return Err(invalid());
        }
    }
    for worktree in &state.worktrees {
        if !insert(worktree.id.0)
            || !state
                .projects
                .iter()
                .any(|project| project.id == worktree.project_id)
        {
            return Err(invalid());
        }
    }
    for session in &state.sessions {
        if !insert(session.id.0)
            || session.panes.is_empty()
            || !session.panes.contains(&session.primary_pane)
            || !session.panes.contains(&session.focused_pane)
            || !state
                .worktrees
                .iter()
                .any(|worktree| worktree.id == session.worktree_id)
        {
            return Err(invalid());
        }
        let mut layout = Vec::new();
        session.layout.panes(&mut layout);
        let declared = session.panes.iter().copied().collect::<HashSet<_>>();
        let arranged = layout.iter().copied().collect::<HashSet<_>>();
        if declared.len() != session.panes.len()
            || arranged.len() != layout.len()
            || declared != arranged
        {
            return Err(invalid());
        }
    }
    for pane in &state.panes {
        if !insert(pane.id.0)
            || !insert(pane.terminal_id.0)
            || pane.agent.as_ref().is_some_and(|agent| !insert(agent.id.0))
            || !state
                .sessions
                .iter()
                .any(|session| session.id == pane.session_id && session.panes.contains(&pane.id))
        {
            return Err(invalid());
        }
    }
    if state.agent_exchanges.len() > MAX_AGENT_EXCHANGES {
        return Err(invalid());
    }
    for exchange in &state.agent_exchanges {
        if !insert(exchange.id.0)
            || exchange.round == 0
            || exchange.runtime_generation.is_empty()
            || exchange.deadline_unix_ms < exchange.created_unix_ms
            || (exchange.access == AgentExchangeAccess::ReadOnly
                && !exchange.write_claims.is_empty())
            || exchange
                .write_claims
                .iter()
                .any(|claim| !claim.is_absolute())
        {
            return Err(invalid());
        }
        if !exchange.is_terminal() {
            let Some(pane) = state.panes.iter().find(|pane| pane.id == exchange.pane_id) else {
                return Err(invalid());
            };
            let Some(session) = state
                .sessions
                .iter()
                .find(|session| session.id == exchange.session_id)
            else {
                return Err(invalid());
            };
            let Some(worktree) = state
                .worktrees
                .iter()
                .find(|worktree| worktree.id == exchange.worktree_id)
            else {
                return Err(invalid());
            };
            if pane.session_id != session.id
                || session.worktree_id != worktree.id
                || worktree.project_id != exchange.project_id
                || pane.agent.as_ref().map(|agent| agent.id) != Some(exchange.agent_id)
                || exchange
                    .write_claims
                    .iter()
                    .any(|claim| !claim.starts_with(&worktree.path))
            {
                return Err(invalid());
            }
        }
    }
    if state.sessions.iter().any(|session| {
        session.panes.iter().any(|id| {
            !state
                .panes
                .iter()
                .any(|pane| pane.id == *id && pane.session_id == session.id)
        })
    }) || state.next_id == 0
        || state.next_id <= max_id
    {
        return Err(invalid());
    }
    Ok(())
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
