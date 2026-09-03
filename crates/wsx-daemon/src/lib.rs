//! Persistent wsx authority for project/session structure, PTYs, frames, leases, and plugins.
// ^ [[wsx Architecture]] Snapshots are authoritative; events only invalidate revisions.

mod plugins;
mod state_store;
mod wake;

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    ops::{Deref, DerefMut},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wsx_core::{config::global::GlobalConfig, integration::resume, runtime::*};
use wsx_terminal::{validate_launch, TerminalRuntime};

const EVENT_LIMIT: usize = 1024;
const PLUGIN_EVENT_LIMIT: usize = 256;
const MAX_CLIENTS: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_VIEW_PANES: usize = 32;
const MAX_VIEW_CELLS: usize = 1_000_000;
const LEASE_TTL: Duration = Duration::from_secs(3);
const AGENT_WAKE_LEASE_TTL: Duration = Duration::from_secs(30 * 60);
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const PRESENTATION_CADENCE: Duration = Duration::from_millis(8);
static NEXT_LEASE_GENERATION: AtomicU64 = AtomicU64::new(1);
static STOP_SIGNAL: AtomicI32 = AtomicI32::new(0);
const PORT_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const PORT_SCAN_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_PORT_SCAN_BYTES: u64 = 256 * 1024;
pub const RESUME_SUPERVISOR_ARG: &str = "__wsx_resume_supervisor";

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
}
impl Default for Persisted {
    fn default() -> Self {
        Self {
            next_id: 1,
            projects: Vec::new(),
            worktrees: Vec::new(),
            sessions: Vec::new(),
            panes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Lease {
    client_id: u64,
    generation: u64,
    expires_at: Instant,
}

#[derive(Clone, Copy)]
enum LeaseAccess {
    Client(u64),
    Stream { client_id: u64, generation: u64 },
}

#[derive(Clone, Copy)]
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
    Intentional,
    LoginEnded,
}

impl StopReason {
    fn marker(self) -> &'static str {
        match self {
            Self::Unexpected => "unexpected",
            Self::Replacement => "replacement",
            Self::Intentional => "intentional",
            Self::LoginEnded => "login_ended",
        }
    }
}

struct State {
    persisted: Persisted,
    revision: u64,
    runtimes: HashMap<PaneId, Arc<TerminalRuntime>>,
    runtime_generations: HashMap<PaneId, String>,
    agent_wake_leases: HashMap<PaneId, Instant>,
    terminal_operation_locks: HashMap<PaneId, Arc<Mutex<()>>>,
    listening_ports: HashMap<PaneId, Vec<u16>>,
    foreground_jobs: HashSet<PaneId>,
    leases: HashMap<PaneId, Lease>,
    events: VecDeque<Event>,
    plugins: Vec<PluginManifest>,
    plugin_events: VecDeque<(String, String)>,
    replacement_target: Option<String>,
    stop_reason: Option<StopReason>,
    persistence_dirty: bool,
    stopping: bool,
}
struct Daemon {
    state: Mutex<State>,
    changed: Condvar,
    plugin_changed: Condvar,
    active_clients: Arc<AtomicUsize>,
    epoch: u64,
    binary_id: String,
    started_unix_ms: u64,
    recovered_from_backup: bool,
    next_runtime_generation: AtomicU64,
    state_path: PathBuf,
    lifecycle_path: PathBuf,
}

fn recover_runtimes(daemon: &Arc<Daemon>, resume_agents: bool) -> io::Result<()> {
    let attempts = {
        let mut state = lock(&daemon.state);
        let session_worktrees = state
            .persisted
            .sessions
            .iter()
            .map(|session| (session.id, session.worktree_id))
            .collect::<HashMap<_, _>>();
        let worktree_paths = state
            .persisted
            .worktrees
            .iter()
            .map(|worktree| (worktree.id, worktree.path.clone()))
            .collect::<HashMap<_, _>>();
        let mut attempts = Vec::new();
        for pane in &mut state.persisted.panes {
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
                continue;
            }
            let launch = recovery_launch(pane.agent.as_mut(), &saved_recipe, resume_agents);
            // ^ [[Session Model]] Persisted identity authorizes only the resume plan.
            // The replacement runtime must report its own generation before projection.
            pane.agent = None;
            let Some(cwd) = session_worktrees
                .get(&pane.session_id)
                .and_then(|worktree_id| worktree_paths.get(worktree_id))
                .cloned()
            else {
                eprintln!("wsxd recovery pane {}: worktree is absent", pane.id.0);
                continue;
            };
            attempts.push((pane.id, pane.terminal_id, cwd, launch));
        }
        attempts
    };

    let mut resumed_sessions = HashSet::new();
    for (pane_id, terminal_id, cwd, launch) in attempts {
        let launch = deduplicate_recovery_launch(launch, &resumed_sessions);
        let launch = match prepare_recovery_launch(launch, &cwd) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {error}", pane_id.0);
                continue;
            }
        };
        let runtime = match spawn_runtime(daemon, pane_id, terminal_id, &cwd, &launch.recipe) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {}", pane_id.0, error.message);
                continue;
            }
        };
        let mut state = lock(&daemon.state);
        state.runtimes.insert(pane_id, Arc::clone(&runtime));
        if runtime.exited() {
            record_terminal_exit(daemon, &mut state, pane_id);
        } else {
            resumed_sessions.extend(launch.resume_key);
            if let Some(pane) = state
                .persisted
                .panes
                .iter_mut()
                .find(|pane| pane.id == pane_id)
            {
                pane.exited = false;
            }
        }
    }
    let state = lock(&daemon.state);
    save_state(&daemon.state_path, &state.persisted)
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
        };
    }
    let Some(agent) = agent else {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
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
        };
    };
    agent.session_ref = Some(session_ref.clone());
    let Some(plan) = resume::plan(&agent.provider, &session_ref) else {
        return RecoveryLaunch {
            recipe: shell_launch_recipe(saved_recipe.rows, saved_recipe.cols),
            resume_key: None,
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
    drop(signals);
    if !status.success() {
        eprintln!("wsxd resumed agent exited with {status}; opening a shell");
    }
    transition_resumed_agent_to_shell()?;
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    let error = Command::new(shell).exec();
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
    match Client::local().call(&Request::AgentClear {
        pane_id,
        runtime_generation,
        next_runtime_generation: next_runtime_generation.clone(),
    })? {
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
    install_shutdown_signals()?;
    let socket = default_socket_path();
    let state_path = state_path();
    secure_parent(&socket)?;
    secure_parent(&state_path)?;
    let _singleton_lock = acquire_singleton_lock(&state_path.with_extension("lock"))?;
    prepare_socket(&socket)?;
    let (persisted, recovered_from_backup) = load_state_with_status(&state_path)?;
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State {
            persisted,
            revision: 1,
            runtimes: HashMap::new(),
            runtime_generations: HashMap::new(),
            agent_wake_leases: HashMap::new(),
            terminal_operation_locks: HashMap::new(),
            listening_ports: HashMap::new(),
            foreground_jobs: HashSet::new(),
            leases: HashMap::new(),
            events: VecDeque::new(),
            plugins: plugins::discover(),
            plugin_events: VecDeque::new(),
            replacement_target: None,
            stop_reason: None,
            persistence_dirty: false,
            stopping: false,
        }),
        changed: Condvar::new(),
        plugin_changed: Condvar::new(),
        active_clients: Arc::new(AtomicUsize::new(0)),
        epoch: epoch(),
        binary_id: binary_identity(&std::env::current_exe()?)?,
        started_unix_ms: unix_time_millis(),
        recovered_from_backup,
        next_runtime_generation: AtomicU64::new(1),
        state_path,
        lifecycle_path: lifecycle_marker_path(&socket),
    });
    // ^ [[Session Model]] Session identity remains durable while native agent
    // resume or the saved recipe recreates a process; neither restores the old PTY.
    recover_runtimes(&daemon, resume_agents_on_restore())?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    write_lifecycle_marker(&daemon.lifecycle_path, "ready")?;
    let plugin_dispatcher = match spawn_plugin_dispatcher(&daemon) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };
    let port_scanner = spawn_port_scanner(&daemon);
    let wake_controller = spawn_wake_controller(&daemon);

    while !advance_replacement(&daemon) {
        retry_dirty_persistence(&daemon);
        match listener.accept() {
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                cleanup(&daemon, &socket);
                let _ = plugin_dispatcher.join();
                let _ = port_scanner.join();
                let _ = wake_controller.join();
                return Err(error);
            }
        }
    }
    cleanup(&daemon, &socket);
    let _ = plugin_dispatcher.join();
    let _ = port_scanner.join();
    let _ = wake_controller.join();
    Ok(())
}

extern "C" fn request_signal_shutdown(signal: libc::c_int) {
    STOP_SIGNAL.store(signal, Ordering::Release);
}

fn install_shutdown_signals() -> io::Result<()> {
    for signal in [libc::SIGHUP, libc::SIGTERM] {
        let previous = unsafe {
            libc::signal(
                signal,
                request_signal_shutdown as *const () as libc::sighandler_t,
            )
        };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn retry_dirty_persistence(daemon: &Daemon) {
    let mut state = lock(&daemon.state);
    if state.persistence_dirty && save_state(&daemon.state_path, &state.persisted).is_ok() {
        state.persistence_dirty = false;
    }
}

fn advance_replacement(daemon: &Daemon) -> bool {
    let mut state = lock(&daemon.state);
    let signal = STOP_SIGNAL.swap(0, Ordering::AcqRel);
    if signal != 0 {
        state.stopping = true;
        state.stop_reason = Some(if signal == libc::SIGHUP {
            StopReason::LoginEnded
        } else {
            StopReason::Intentional
        });
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
    }
    if !state.stopping && state.replacement_target.is_some() && live_runtime_count(&state) == 0 {
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

fn cleanup(daemon: &Daemon, socket: &Path) {
    let (runtimes, reason) = {
        let mut state = lock(&daemon.state);
        state.stopping = true;
        state.leases.clear();
        state.agent_wake_leases.clear();
        state.plugin_events.clear();
        let runtimes = std::mem::take(&mut state.runtimes);
        state.runtime_generations.clear();
        let _ = save_state(&daemon.state_path, &state.persisted);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        let reason = state.stop_reason.unwrap_or(StopReason::Unexpected);
        let marker = if reason == StopReason::Replacement {
            format!(
                "replacement:{}",
                state.replacement_target.as_deref().unwrap_or_default()
            )
        } else {
            reason.marker().to_string()
        };
        (runtimes, marker)
    };
    for runtime in runtimes.into_values() {
        runtime.terminate();
    }
    let _ = write_lifecycle_marker(&daemon.lifecycle_path, &reason);
    let _ = fs::remove_file(socket);
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
        let request = serde_json::from_str::<Request>(&line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid request: {error}"),
            )
        })?;
        let response = if matches!(
            request,
            Request::Shutdown | Request::LifecycleStatus | Request::PrepareReplacement { .. }
        ) {
            handle(&daemon, request)
        } else {
            Response::Error(ApiError::new(
                "protocol_mismatch",
                format!("client {protocol}, daemon {PROTOCOL_VERSION}"),
            ))
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
        if let Request::TerminalSubscribe {
            pane_id,
            client_id,
            takeover,
            rows,
            cols,
        } = request
        {
            return serve_terminal_stream(stream, daemon, pane_id, client_id, takeover, rows, cols);
        }
        write_response(&mut stream, handle(&daemon, request))?;
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
fn serve_terminal_stream(
    mut stream: UnixStream,
    daemon: Arc<Daemon>,
    pane_id: PaneId,
    client_id: u64,
    takeover: bool,
    rows: u16,
    cols: u16,
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
    let resized =
        resize_terminal_for_stream(&daemon, pane_id, client_id, lease_generation, rows, cols);
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

    let result = stream_terminal_updates(
        &mut stream,
        &daemon,
        StreamLease {
            pane_id,
            client_id,
            generation: lease_generation,
        },
        &active,
        &resync,
        &input_error,
    );
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
        let operation_lock = terminal_operation_lock(daemon, lease.pane_id);
        let operation = lock(&operation_lock);
        let runtime = {
            let state = lock(&daemon.state);
            if state.stopping
                || !state.leases.get(&lease.pane_id).is_some_and(|current| {
                    current.client_id == lease.client_id
                        && current.generation == lease.generation
                        && current.expires_at > Instant::now()
                })
            {
                None
            } else {
                state.runtimes.get(&lease.pane_id).cloned()
            }
        };
        let Some(runtime) = runtime else {
            write_stream_message(
                stream,
                &TerminalServerMessage::Error(ApiError::new(
                    "lease_lost",
                    "terminal stream no longer owns the pane lease",
                )),
            )?;
            break;
        };
        let exited = runtime.exited();
        if resync.swap(false, Ordering::AcqRel) {
            baseline = None;
        }
        let frame_due = baseline.is_none() || last_frame_sent.elapsed() >= PRESENTATION_CADENCE;
        let sample = runtime.presentation_sample(baseline, frame_due);
        for write in sample.clipboard_writes {
            write_stream_message(stream, &TerminalServerMessage::ClipboardWrite(write))?;
        }
        if let Some(error) = lock(input_error).take() {
            write_stream_message(stream, &TerminalServerMessage::Error(error))?;
            break;
        }
        if exited {
            write_stream_message(stream, &TerminalServerMessage::Exited)?;
            break;
        }
        match sample.update {
            Ok(Some(update)) => {
                baseline = Some(update.revision());
                write_stream_message(stream, &TerminalServerMessage::Update(update))?;
                last_frame_sent = Instant::now();
            }
            Ok(None) => {}
            Err(error) => {
                write_stream_message(
                    stream,
                    &TerminalServerMessage::Error(ApiError::new("frame_failed", error.to_string())),
                )?;
                break;
            }
        }
        drop(operation);
        let state = lock(&daemon.state);
        // ^ [[wsx Architecture]] Recheck every wake predicate while holding the
        // notifier mutex. Frames wait only for the bounded presentation cadence;
        // effects bypass it in the atomically sampled FIFO above.
        if !active.load(Ordering::Acquire) || resync.load(Ordering::Acquire) {
            continue;
        }
        let current_revision = runtime.revision();
        if sample.synchronized_output && sample.revision != current_revision {
            continue;
        }
        let wait = if !sample.synchronized_output && baseline != Some(current_revision) {
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

fn write_stream_message(
    stream: &mut UnixStream,
    message: &TerminalServerMessage,
) -> io::Result<()> {
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
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.client_id == client_id && lease.generation == lease_generation)
    {
        state.leases.remove(&pane_id);
        let runtime = state.runtimes.get(&pane_id).cloned();
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
    require_lease(&state, pane_id, client_id)?;
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
    let (runtime, _) = leased_runtime_for_stream(daemon, pane_id, client_id, lease_generation)?;
    operation(&runtime)
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
            })
        }
        TerminalClientMessage::Key(key) => {
            with_stream_runtime(daemon, pane_id, client_id, lease_generation, |runtime| {
                runtime.key(&key).map_err(terminal_api)
            })
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
            resize_terminal_for_stream(daemon, pane_id, client_id, lease_generation, rows, cols)?;
            Ok(())
        }
        TerminalClientMessage::Heartbeat => {
            let mut state = lock(&daemon.state);
            refresh_lease_with_access(
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
    let generation = NEXT_LEASE_GENERATION.fetch_add(1, Ordering::Relaxed);
    state.leases.insert(
        pane_id,
        Lease {
            client_id,
            generation,
            expires_at: Instant::now() + LEASE_TTL,
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
        } => poll(daemon, after_revision, timeout_ms),
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
        Request::TerminalHeartbeat { pane_id, client_id } => {
            mutate_runtime(daemon, |state| refresh_lease(state, pane_id, client_id))
        }
        Request::TerminalSubscribe { .. } => Err(api(
            "invalid_request",
            "terminal_subscribe is only valid as a connection upgrade",
        )),
        Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        } => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "input exceeds limit"));
            }
            let (runtime, revision) = leased_runtime(daemon, pane_id, client_id)?;
            runtime.write(&bytes).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalKey {
            pane_id,
            client_id,
            key,
        } => {
            let (runtime, revision) = leased_runtime(daemon, pane_id, client_id)?;
            runtime.key(&key).map_err(terminal_api)?;
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
            let (runtime, revision) = leased_runtime(daemon, pane_id, client_id)?;
            runtime.paste(&text).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalMouse {
            pane_id,
            client_id,
            mouse,
        } => {
            let (runtime, revision) = leased_runtime(daemon, pane_id, client_id)?;
            runtime.mouse(&mouse).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalResize {
            pane_id,
            client_id,
            rows,
            cols,
        } => resize_terminal(daemon, pane_id, client_id, rows, cols),
        Request::View { pane_ids } => view(daemon, pane_ids),
        Request::AgentReport {
            pane_id,
            runtime_generation,
            provider,
            state: agent_state,
            conversation_id,
            session_ref,
            capabilities,
        } => agent_report(
            daemon,
            RuntimeAgentAuthority::new(pane_id, runtime_generation),
            provider,
            agent_state,
            conversation_id,
            session_ref,
            capabilities,
        ),
        Request::AgentClear {
            pane_id,
            runtime_generation,
            next_runtime_generation,
        } => agent_clear(daemon, pane_id, runtime_generation, next_runtime_generation),
        Request::PluginList => Ok(Response::Plugins(lock(&daemon.state).plugins.clone())),
        Request::PluginReload => {
            let mut state = lock(&daemon.state);
            state.plugins = plugins::discover();
            Ok(Response::Plugins(state.plugins.clone()))
        }
        Request::LifecycleStatus => Ok(Response::Lifecycle(lifecycle_status(
            daemon,
            &lock(&daemon.state),
        ))),
        Request::PrepareReplacement { target_binary_id } => {
            prepare_replacement(daemon, target_binary_id)
        }
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

fn capabilities() -> Capabilities {
    Capabilities {
        pane_splits: true,
        plugins: true,
        agent_reports: true,
        agent_session_restore: true,
        resume_shell_fallback: true,
        listening_ports: true,
        foreground_jobs: true,
        process_restore: false,
        lifecycle_coordination: true,
    }
}

fn live_runtime_count(state: &State) -> usize {
    state
        .runtimes
        .values()
        .filter(|runtime| !runtime.exited())
        .count()
}

fn lifecycle_status(daemon: &Daemon, state: &State) -> DaemonLifecycle {
    DaemonLifecycle {
        protocol: PROTOCOL_VERSION,
        epoch: daemon.epoch,
        binary_id: daemon.binary_id.clone(),
        started_unix_ms: daemon.started_unix_ms,
        phase: if state.stopping {
            DaemonPhase::Stopping
        } else if state.replacement_target.is_some() {
            DaemonPhase::ReplacementPending
        } else {
            DaemonPhase::Ready
        },
        live_runtimes: live_runtime_count(state),
        active_clients: daemon.active_clients.load(Ordering::Acquire),
        recovered_from_backup: daemon.recovered_from_backup,
        replacement_target: state.replacement_target.clone(),
    }
}

fn prepare_replacement(daemon: &Daemon, target_binary_id: String) -> Result<Response, ApiError> {
    if target_binary_id.is_empty() || target_binary_id.len() > 512 {
        return Err(api(
            "invalid_request",
            "replacement binary identity is invalid",
        ));
    }
    let mut state = lock(&daemon.state);
    match state.replacement_target.as_deref() {
        Some(current) if current != target_binary_id.as_str() => {
            return Err(api(
                "replacement_conflict",
                "another wsxd binary is already pending replacement",
            ))
        }
        Some(_) => {}
        None => state.replacement_target = Some(target_binary_id),
    }
    let live_runtimes = live_runtime_count(&state);
    let disposition = if live_runtimes == 0 {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Replacement);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        ReplacementDisposition::Stopping
    } else {
        ReplacementDisposition::Deferred
    };
    Ok(Response::Replacement {
        disposition,
        live_runtimes,
    })
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
        capabilities: capabilities(),
    }
}

fn poll(daemon: &Daemon, after: u64, timeout_ms: u64) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
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
            let path = fs::canonicalize(&worktree.path).map_err(io_api)?;
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
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    publish(daemon, &mut state, revision, "projects.synchronized", 0);
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
        save_state(&daemon.state_path, &persisted).map_err(io_api)?;
        state.persisted = persisted;
        publish(
            daemon,
            &mut state,
            revision,
            "session.created",
            session_id.0,
        );
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
        if let Err(error) = save_state(&daemon.state_path, &persisted).map_err(io_api) {
            drop(state);
            runtime.terminate();
            return Err(error);
        }
        state.persisted = persisted;
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
        save_state(&daemon.state_path, &persisted).map_err(io_api)?;
        state.persisted = persisted;
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
    if let Err(error) = save_state(&daemon.state_path, &persisted).map_err(io_api) {
        drop(state);
        runtime.terminate();
        return Err(error);
    }
    state.persisted = persisted;
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
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
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
    persisted.panes.retain(|pane| pane.session_id != id);
    persisted.sessions.retain(|session| session.id != id);
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    let mut runtimes = Vec::new();
    for pane_id in &session.panes {
        if let Some(runtime) = state.runtimes.remove(pane_id) {
            runtimes.push(runtime);
        }
        state.runtime_generations.remove(pane_id);
        state.agent_wake_leases.remove(pane_id);
        state.leases.remove(pane_id);
        state.terminal_operation_locks.remove(pane_id);
        state.listening_ports.remove(pane_id);
        state.foreground_jobs.remove(pane_id);
    }
    publish(daemon, &mut state, revision, "session.closed", id.0);
    drop(state);
    for runtime in runtimes {
        runtime.terminate();
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
    persisted.panes.retain(|candidate| candidate.id != id);
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    let runtime = state.runtimes.remove(&id);
    state.runtime_generations.remove(&id);
    state.agent_wake_leases.remove(&id);
    state.leases.remove(&id);
    state.terminal_operation_locks.remove(&id);
    state.listening_ports.remove(&id);
    state.foreground_jobs.remove(&id);
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
    client_id: u64,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    resize_terminal_with_access(daemon, pane_id, LeaseAccess::Client(client_id), rows, cols)
}

fn resize_terminal_for_stream(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    generation: u64,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    resize_terminal_with_access(
        daemon,
        pane_id,
        LeaseAccess::Stream {
            client_id,
            generation,
        },
        rows,
        cols,
    )
}

fn resize_terminal_with_access(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let (runtime, _) = leased_runtime_with_access(daemon, pane_id, access)?;
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
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    bump(daemon, &mut state, "project.terminal_entered", pane_id.0);
    Ok(())
}

fn agent_report(
    daemon: &Arc<Daemon>,
    runtime: RuntimeAgentAuthority,
    provider: String,
    agent_state: AgentState,
    conversation_id: Option<String>,
    session_ref: Option<AgentSessionRef>,
    mut capabilities: AgentCapabilities,
) -> Result<Response, ApiError> {
    let RuntimeAgentAuthority {
        pane_id,
        generation: runtime_generation,
    } = runtime;
    let provider = bounded_provider(provider)?;
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
    let session_ref = explicit_session_ref.or_else(|| {
        conversation_id
            .as_ref()
            .and_then(|value| AgentSessionRef::id(value.clone()))
            .filter(|session_ref| resume::plan(&provider, session_ref).is_some())
    });
    capabilities.resume |= session_ref.is_some();
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

    let agent_id = match persisted.panes[pane_index]
        .agent
        .as_ref()
        .map(|agent| agent.id)
    {
        Some(id) => id,
        None => AgentInstanceId(next_id(&mut persisted)?),
    };
    let revision = state.revision.saturating_add(1);
    persisted.panes[pane_index].agent = Some(AgentInfo {
        id: agent_id,
        provider,
        state: agent_state,
        conversation_id,
        session_ref,
        capabilities,
        source: "adapter".into(),
    });
    persisted.panes[pane_index].revision = revision;
    if agent_state == AgentState::Working {
        persisted.projects[project_index].last_agent_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
    }
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    if agent_state == AgentState::Working {
        state.agent_wake_leases.insert(pane_id, Instant::now());
    } else {
        state.agent_wake_leases.remove(&pane_id);
    }
    let revision = bump(daemon, &mut state, "agent.reported", pane_id.0);
    Ok(Response::Ack { revision })
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
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    state.agent_wake_leases.remove(&pane_id);
    state
        .runtime_generations
        .insert(pane_id, next_runtime_generation);
    let revision = bump(daemon, &mut state, "agent.cleared", pane_id.0);
    Ok(Response::Ack { revision })
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

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
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
    state.events.push_back(Event::Changed {
        revision,
        entity: entity.into(),
        id,
    });
    while state.events.len() > EVENT_LIMIT {
        state.events.pop_front();
    }
    if state.plugin_events.len() == PLUGIN_EVENT_LIMIT {
        state.plugin_events.pop_front();
    }
    state.plugin_events.push_back((
        entity.into(),
        serde_json::json!({"event":entity,"revision":revision,"id":id}).to_string(),
    ));
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
        ("WSX_PANE_ID".into(), pane_id.to_string()),
        (
            WSX_RUNTIME_GENERATION_ENV.into(),
            runtime_generation.to_string(),
        ),
    ];
    if let Some(binary) = std::env::current_exe()
        .ok()
        .and_then(|daemon| daemon.parent().map(|parent| parent.join("wsx")))
        .filter(|binary| binary.is_file())
    {
        environment.push((
            "WSX_AGENT_REPORT_BIN".into(),
            binary.to_string_lossy().into_owned(),
        ));
    }
    environment
}

fn spawn_runtime(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: &Path,
    recipe: &LaunchRecipe,
) -> Result<TerminalRuntime, ApiError> {
    let weak: Weak<Daemon> = Arc::downgrade(daemon);
    let notify = Arc::new(move || {
        let Some(daemon) = weak.upgrade() else { return };
        let mut state = lock(&daemon.state);
        let exited = state
            .runtimes
            .get(&pane_id)
            .is_some_and(|runtime| runtime.exited());
        if exited {
            record_terminal_exit(&daemon, &mut state, pane_id);
        } else {
            let revision = state.revision.saturating_add(1);
            state.revision = revision;
            state.events.push_back(Event::Changed {
                revision,
                entity: "terminal.changed".into(),
                id: pane_id.0,
            });
            while state.events.len() > EVENT_LIMIT {
                state.events.pop_front();
            }
            daemon.changed.notify_all();
        }
    });
    let runtime_generation = next_runtime_generation(daemon);
    {
        let mut state = lock(&daemon.state);
        state.agent_wake_leases.remove(&pane_id);
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
    state.runtime_generations.remove(&pane_id);
    state.agent_wake_leases.remove(&pane_id);
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
    pane.revision = revision;
    state.events.push_back(Event::Exited { revision, pane_id });
    while state.events.len() > EVENT_LIMIT {
        state.events.pop_front();
    }
    if state.plugin_events.len() == PLUGIN_EVENT_LIMIT {
        state.plugin_events.pop_front();
    }
    state.plugin_events.push_back((
        "terminal.exited".into(),
        serde_json::json!({"event":"terminal.exited","revision":revision,"id":pane_id.0})
            .to_string(),
    ));
    state.persistence_dirty = true;
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
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

fn spawn_port_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let runtimes = {
            let state = lock(&daemon.state);
            if state.stopping {
                return;
            }
            state
                .runtimes
                .iter()
                .filter(|(_, runtime)| !runtime.exited())
                .map(|(pane_id, runtime)| (*pane_id, Arc::clone(runtime)))
                .collect::<Vec<_>>()
        };
        let mut process_groups = HashMap::new();
        let mut foreground_jobs = HashSet::new();
        for (pane_id, runtime) in runtimes {
            if let Some(group) = runtime.process_group_id() {
                process_groups.insert(pane_id, group);
            }
            if runtime.has_foreground_job() {
                foreground_jobs.insert(pane_id);
            }
        }

        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports().map(|listeners| {
                let process_terminals = scan_process_terminals();
                attribute_listening_ports(&process_groups, &listeners, process_terminals.as_ref())
            })
        };
        let mut state = lock(&daemon.state);
        let ports_changed = detected
            .as_ref()
            .is_some_and(|next| state.listening_ports != *next);
        let activity_changed = state.foreground_jobs != foreground_jobs;
        if let Some(next) = detected.filter(|_| ports_changed) {
            state.listening_ports = next;
        }
        if activity_changed {
            state.foreground_jobs = foreground_jobs;
        }
        if ports_changed || activity_changed {
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

fn terminal_operation_lock(daemon: &Daemon, pane_id: PaneId) -> Arc<Mutex<()>> {
    let mut state = lock(&daemon.state);
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
    client_id: u64,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    leased_runtime_with_access(daemon, pane_id, LeaseAccess::Client(client_id))
}

fn leased_runtime_for_stream(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    generation: u64,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    leased_runtime_with_access(
        daemon,
        pane_id,
        LeaseAccess::Stream {
            client_id,
            generation,
        },
    )
}

fn leased_runtime_with_access(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    let mut state = lock(&daemon.state);
    refresh_lease_with_access(&mut state, pane_id, access)?;
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
fn require_lease(state: &State, pane_id: PaneId, client_id: u64) -> Result<(), ApiError> {
    require_lease_with_access(state, pane_id, LeaseAccess::Client(client_id))
}

fn require_lease_with_access(
    state: &State,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(), ApiError> {
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

fn refresh_lease(state: &mut State, pane_id: PaneId, client_id: u64) -> Result<(), ApiError> {
    refresh_lease_with_access(state, pane_id, LeaseAccess::Client(client_id))
}

fn refresh_lease_with_access(
    state: &mut State,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(), ApiError> {
    require_lease_with_access(state, pane_id, access)?;
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
fn bounded_provider(value: String) -> Result<String, ApiError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Err(api("invalid_provider", "invalid provider name"))
    } else {
        Ok(value)
    }
}
fn bounded_runtime_generation(value: String) -> Result<String, ApiError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b':')
    {
        Err(api(
            "invalid_runtime_generation",
            "runtime generation is malformed",
        ))
    } else {
        Ok(value)
    }
}

fn validated_session_ref(
    session_ref: Option<AgentSessionRef>,
) -> Result<Option<AgentSessionRef>, ApiError> {
    let Some(session_ref) = session_ref else {
        return Ok(None);
    };
    let validated = match session_ref.kind {
        AgentSessionRefKind::Id => AgentSessionRef::id(session_ref.value),
        AgentSessionRefKind::Path => AgentSessionRef::path(session_ref.value),
    }
    .ok_or_else(|| api("invalid_agent_session", "invalid agent session reference"))?;
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

fn state_path() -> PathBuf {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir);
    root.join("wsx/state.json")
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
mod tests {
    use super::*;

    fn agent_test_persisted() -> Persisted {
        serde_json::from_str(
            r#"{
                "next_id":6,
                "projects":[{"id":1,"path":"/project","name":"project","revision":7}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":7}],
                "sessions":[{"id":3,"worktree_id":2,"label":"session","primary_pane":4,"focused_pane":4,"panes":[4],"layout":{"kind":"leaf","pane_id":4},"revision":7}],
                "panes":[{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":true,"revision":7}]
            }"#,
        )
        .unwrap()
    }

    fn agent_test_daemon(persisted: Persisted) -> (Arc<Daemon>, PathBuf) {
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "wsx-daemon-agent-test-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);
        let daemon = Arc::new(Daemon {
            state: Mutex::new(State {
                persisted,
                revision: 7,
                runtimes: HashMap::new(),
                runtime_generations: HashMap::from([(
                    PaneId(4),
                    "0000000000000001:0000000000000001".into(),
                )]),
                agent_wake_leases: HashMap::new(),
                terminal_operation_locks: HashMap::new(),
                listening_ports: HashMap::new(),
                foreground_jobs: HashSet::new(),
                leases: HashMap::new(),
                events: VecDeque::new(),
                plugins: Vec::new(),
                plugin_events: VecDeque::new(),
                replacement_target: None,
                stop_reason: None,
                persistence_dirty: false,
                stopping: false,
            }),
            changed: Condvar::new(),
            plugin_changed: Condvar::new(),
            active_clients: Arc::new(AtomicUsize::new(0)),
            epoch: 1,
            binary_id: "test-binary".into(),
            started_unix_ms: 1,
            recovered_from_backup: false,
            next_runtime_generation: AtomicU64::new(1),
            state_path: path.clone(),
            lifecycle_path: path.with_extension("lifecycle"),
        });
        (daemon, path)
    }

    #[test]
    fn lifecycle_status_is_available_across_protocol_mismatch() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let (mut client, server) = UnixStream::pair().unwrap();
        let serving = thread::spawn(move || serve_client(server, daemon).unwrap());
        client
            .write_all(&encode_line(&Request::Hello { protocol: 999 }).unwrap())
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Hello {
                protocol: PROTOCOL_VERSION,
                ..
            }
        ));
        client
            .write_all(&encode_line(&Request::LifecycleStatus).unwrap())
            .unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            serde_json::from_str::<Response>(&line).unwrap(),
            Response::Lifecycle(DaemonLifecycle {
                protocol: PROTOCOL_VERSION,
                ..
            })
        ));
        serving.join().unwrap();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn replacement_stops_atomically_when_no_runtime_is_live() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let response = prepare_replacement(&daemon, "next-binary".into()).unwrap();
        assert_eq!(
            response,
            Response::Replacement {
                disposition: ReplacementDisposition::Stopping,
                live_runtimes: 0,
            }
        );
        let state = lock(&daemon.state);
        assert!(state.stopping);
        assert_eq!(state.stop_reason, Some(StopReason::Replacement));
        drop(state);
        let socket = path.with_extension("sock");
        cleanup(&daemon, &socket);
        assert_eq!(
            fs::read_to_string(path.with_extension("lifecycle"))
                .unwrap()
                .trim(),
            "replacement:next-binary"
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.backup"));
        let _ = fs::remove_file(path.with_extension("lifecycle"));
    }

    #[test]
    fn replacement_waits_without_terminating_a_live_runtime() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let pane_id = PaneId(4);
        let runtime = Arc::new(
            spawn_runtime(
                &daemon,
                pane_id,
                TerminalId(5),
                Path::new("/"),
                &LaunchRecipe {
                    command: vec!["/bin/cat".into()],
                    initial_input: None,
                    rows: 3,
                    cols: 4,
                },
            )
            .unwrap(),
        );
        lock(&daemon.state)
            .runtimes
            .insert(pane_id, Arc::clone(&runtime));
        assert_eq!(
            prepare_replacement(&daemon, "next-binary".into()).unwrap(),
            Response::Replacement {
                disposition: ReplacementDisposition::Deferred,
                live_runtimes: 1,
            }
        );
        assert!(!lock(&daemon.state).stopping);
        assert!(!runtime.exited());
        let conflict = prepare_replacement(&daemon, "different-binary".into()).unwrap_err();
        assert_eq!(conflict.code, "replacement_conflict");
        assert_eq!(
            lock(&daemon.state).replacement_target.as_deref(),
            Some("next-binary")
        );
        runtime.terminate();
        assert!(advance_replacement(&daemon));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn wake_lease_requires_fresh_working_report_and_live_runtime() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let pane_id = PaneId(4);
        let runtime = Arc::new(
            spawn_runtime(
                &daemon,
                pane_id,
                TerminalId(5),
                Path::new("/"),
                &LaunchRecipe {
                    command: vec!["/bin/cat".into()],
                    initial_input: None,
                    rows: 3,
                    cols: 4,
                },
            )
            .unwrap(),
        );
        let generation = {
            let mut state = lock(&daemon.state);
            state.runtimes.insert(pane_id, Arc::clone(&runtime));
            state.runtime_generations[&pane_id].clone()
        };

        agent_report(
            &daemon,
            RuntimeAgentAuthority::new(pane_id, Some(generation.clone())),
            "pi".into(),
            AgentState::Working,
            None,
            None,
            AgentCapabilities::default(),
        )
        .unwrap();
        assert!(has_fresh_working_agent(
            &mut lock(&daemon.state),
            Instant::now()
        ));

        lock(&daemon.state)
            .agent_wake_leases
            .insert(pane_id, Instant::now() - AGENT_WAKE_LEASE_TTL);
        assert!(!has_fresh_working_agent(
            &mut lock(&daemon.state),
            Instant::now()
        ));

        agent_report(
            &daemon,
            RuntimeAgentAuthority::new(pane_id, Some(generation)),
            "pi".into(),
            AgentState::Blocked,
            None,
            None,
            AgentCapabilities::default(),
        )
        .unwrap();
        assert!(!has_fresh_working_agent(
            &mut lock(&daemon.state),
            Instant::now()
        ));
        runtime.terminate();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.backup"));
    }

    #[test]
    fn stale_stream_generation_cannot_use_or_release_reacquired_same_client_lease() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let pane_id = PaneId(4);
        {
            let mut state = lock(&daemon.state);
            state.leases.insert(
                pane_id,
                Lease {
                    client_id: 9,
                    generation: 2,
                    expires_at: Instant::now() + LEASE_TTL,
                },
            );
            let stale = LeaseAccess::Stream {
                client_id: 9,
                generation: 1,
            };
            assert_eq!(
                require_lease_with_access(&state, pane_id, stale)
                    .unwrap_err()
                    .code,
                "lease_required"
            );
        }

        release_stream_lease(&daemon, pane_id, 9, 1);
        assert!(lock(&daemon.state).leases.contains_key(&pane_id));
        release_stream_lease(&daemon, pane_id, 9, 2);
        assert!(!lock(&daemon.state).leases.contains_key(&pane_id));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn only_the_current_lease_generation_clears_terminal_selection() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let pane_id = PaneId(4);
        let runtime = Arc::new(
            spawn_runtime(
                &daemon,
                pane_id,
                TerminalId(5),
                Path::new("/"),
                &LaunchRecipe {
                    command: vec!["/bin/cat".into()],
                    initial_input: None,
                    rows: 3,
                    cols: 4,
                },
            )
            .unwrap(),
        );
        lock(&daemon.state)
            .runtimes
            .insert(pane_id, Arc::clone(&runtime));
        let (_, generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
        for mouse in [
            MouseEvent {
                action: MouseAction::Press,
                button: MouseButton::Left,
                x: 0,
                y: 0,
                in_bounds: true,
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
            MouseEvent {
                action: MouseAction::Motion,
                button: MouseButton::Left,
                x: 2,
                y: 0,
                in_bounds: true,
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
        ] {
            with_stream_runtime(&daemon, pane_id, 9, generation, |runtime| {
                runtime.mouse(&mouse).map_err(terminal_api)
            })
            .unwrap();
        }
        let selection = || match runtime
            .presentation_sample(None, true)
            .update
            .unwrap()
            .unwrap()
        {
            TerminalUpdate::Full(frame) => frame.selection,
            TerminalUpdate::Patch { .. } => panic!("expected full frame"),
        };
        assert!(!selection().is_empty());

        let (_, replacement_generation) =
            acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
        assert!(selection().is_empty());
        for mouse in [
            MouseEvent {
                action: MouseAction::Press,
                button: MouseButton::Left,
                x: 0,
                y: 0,
                in_bounds: true,
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
            MouseEvent {
                action: MouseAction::Motion,
                button: MouseButton::Left,
                x: 2,
                y: 0,
                in_bounds: true,
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
        ] {
            with_stream_runtime(&daemon, pane_id, 9, replacement_generation, |runtime| {
                runtime.mouse(&mouse).map_err(terminal_api)
            })
            .unwrap();
        }
        assert!(!selection().is_empty());

        release_stream_lease(&daemon, pane_id, 9, generation);
        assert!(!selection().is_empty());
        assert!(lock(&daemon.state).leases.contains_key(&pane_id));

        release_stream_lease(&daemon, pane_id, 9, replacement_generation);
        assert!(selection().is_empty());
        assert!(!lock(&daemon.state).leases.contains_key(&pane_id));

        runtime.terminate();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn terminal_takeover_waits_for_the_inflight_generation_operation() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let pane_id = PaneId(4);
        let runtime = Arc::new(
            spawn_runtime(
                &daemon,
                pane_id,
                TerminalId(5),
                Path::new("/"),
                &LaunchRecipe {
                    command: vec!["/bin/cat".into()],
                    initial_input: None,
                    rows: 3,
                    cols: 4,
                },
            )
            .unwrap(),
        );
        lock(&daemon.state)
            .runtimes
            .insert(pane_id, Arc::clone(&runtime));
        let (_, generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
        let operation_lock = terminal_operation_lock(&daemon, pane_id);
        let operation = lock(&operation_lock);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let takeover_daemon = Arc::clone(&daemon);
        let takeover = thread::spawn(move || {
            started_tx.send(()).unwrap();
            completed_tx
                .send(acquire_terminal_lease(&takeover_daemon, pane_id, 9, true))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(completed_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
        drop(operation);
        let (_, takeover_generation) = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        takeover.join().unwrap();

        assert_ne!(generation, takeover_generation);
        assert_eq!(
            with_stream_runtime(&daemon, pane_id, 9, generation, |_| Ok(()))
                .unwrap_err()
                .code,
            "lease_required"
        );
        runtime.terminate();
        let _ = fs::remove_file(path);
    }

    #[test]
    fn legacy_agent_report_promotes_conversation_id_to_session_ref() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());

        let response = agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
            "codex".into(),
            AgentState::Idle,
            Some("legacy-id".into()),
            None,
            AgentCapabilities::default(),
        )
        .unwrap();
        assert!(matches!(response, Response::Ack { revision: 8 }));

        let state = lock(&daemon.state);
        let agent = state.persisted.panes[0].agent.as_ref().unwrap();
        assert_eq!(agent.conversation_id.as_deref(), Some("legacy-id"));
        assert_eq!(
            agent.session_ref.as_ref().unwrap().kind,
            AgentSessionRefKind::Id
        );
        assert_eq!(agent.session_ref.as_ref().unwrap().value, "legacy-id");
        assert!(agent.capabilities.resume);
        drop(state);
        let persisted = load_state(&path).unwrap();
        let agent = persisted.panes[0].agent.as_ref().unwrap();
        assert_eq!(agent.conversation_id.as_deref(), Some("legacy-id"));
        assert_eq!(
            agent.session_ref.as_ref().unwrap().kind,
            AgentSessionRefKind::Id
        );
        assert_eq!(agent.session_ref.as_ref().unwrap().value, "legacy-id");
        assert!(agent.capabilities.resume);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn agent_report_rejects_provider_mismatch_without_mutation() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        let error = agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
            "claude".into(),
            AgentState::Working,
            None,
            Some(AgentSessionRef::path("/absolute/session").unwrap()),
            AgentCapabilities::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_agent_session");

        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        assert!(!path.exists());
    }

    #[test]
    fn agent_report_rejects_invalid_structured_id_without_mutation() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        let error = agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
            "codex".into(),
            AgentState::Working,
            None,
            Some(AgentSessionRef {
                kind: AgentSessionRefKind::Id,
                value: "invalid\nid".into(),
            }),
            AgentCapabilities::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_agent_session");

        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        assert!(!path.exists());
    }

    #[test]
    fn terminal_agent_environment_exposes_stable_pane_identity() {
        let environment =
            terminal_agent_environment(PaneId(42), "0000000000000001:0000000000000001");
        assert!(environment
            .iter()
            .any(|(name, value)| name == "WSX_PANE_ID" && value == "42"));
        assert!(environment.iter().any(|(name, value)| {
            name == WSX_RUNTIME_GENERATION_ENV && value == "0000000000000001:0000000000000001"
        }));
    }

    #[test]
    fn agent_report_rejects_missing_or_stale_runtime_generation() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        for generation in [None, Some("0000000000000001:0000000000000002".into())] {
            let error = agent_report(
                &daemon,
                RuntimeAgentAuthority::new(PaneId(4), generation),
                "codex".into(),
                AgentState::Working,
                Some("conversation".into()),
                None,
                AgentCapabilities::default(),
            )
            .unwrap_err();
            assert_eq!(error.code, "stale_runtime");
        }

        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        assert!(!path.exists());
    }

    #[test]
    fn agent_clear_rotates_runtime_authority_before_shell_fallback() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let old = "0000000000000001:0000000000000001";
        let next = "0000000000000001:0000000000000002";

        let response = agent_clear(&daemon, PaneId(4), old.into(), next.into()).unwrap();
        assert!(matches!(response, Response::Ack { revision: 8 }));
        {
            let state = lock(&daemon.state);
            assert!(state.persisted.panes[0].agent.is_none());
            assert_eq!(
                state
                    .runtime_generations
                    .get(&PaneId(4))
                    .map(String::as_str),
                Some(next)
            );
        }

        let stale = agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some(old.into())),
            "pi".into(),
            AgentState::Done,
            None,
            None,
            AgentCapabilities::default(),
        )
        .unwrap_err();
        assert_eq!(stale.code, "stale_runtime");
        assert!(agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some(next.into())),
            "pi".into(),
            AgentState::Idle,
            None,
            None,
            AgentCapabilities::default(),
        )
        .is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn synchronization_save_failure_publishes_no_state_or_event() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
        let result = synchronize_projects(
            &daemon,
            vec![ProjectSpec {
                path: PathBuf::from("/"),
                name: "root".into(),
                worktrees: vec![WorktreeSpec {
                    path: PathBuf::from("/"),
                    branch: "main".into(),
                }],
            }],
        );
        assert!(result.is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn create_save_failure_publishes_no_session_or_event() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
        let result = create_session(
            &daemon,
            WorktreeId(2),
            "new".into(),
            vec!["/bin/cat".into()],
            None,
            10,
            20,
        );
        assert!(result.is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn durable_mutation_save_failure_publishes_no_state_or_event() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
        let result = mutate(&daemon, |persisted, _revision| {
            session_mut(persisted, SessionId(3))?.label = "changed".into();
            Ok(("session.renamed", 3))
        });
        assert!(result.is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn close_save_failure_keeps_session_and_runtime_maps_untouched() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
        assert!(close_session(&daemon, SessionId(3), 7).is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn runtime_exit_stays_truthful_and_retries_durability() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        {
            let mut state = lock(&daemon.state);
            state.persisted.panes[0].exited = false;
            record_terminal_exit(&daemon, &mut state, PaneId(4));
            assert!(state.persisted.panes[0].exited);
            assert!(state.persistence_dirty);
        }
        fs::remove_dir(&path).unwrap();
        retry_dirty_persistence(&daemon);
        assert!(!lock(&daemon.state).persistence_dirty);
        assert!(path.is_file());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.backup"));
    }

    #[test]
    fn agent_clear_save_failure_preserves_agent_and_runtime_generation() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = {
            let state = lock(&daemon.state);
            (
                serde_json::to_value(&state.persisted).unwrap(),
                state.runtime_generations.clone(),
            )
        };

        assert!(agent_clear(
            &daemon,
            PaneId(4),
            "0000000000000001:0000000000000001".into(),
            "0000000000000001:0000000000000002".into(),
        )
        .is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before.0);
        assert_eq!(state.runtime_generations, before.1);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    fn report_agent(daemon: &Arc<Daemon>, state: AgentState) -> Result<Response, ApiError> {
        agent_report(
            daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
            "codex".into(),
            state,
            Some("conversation".into()),
            None,
            AgentCapabilities::default(),
        )
    }

    #[test]
    fn agent_report_save_failure_keeps_live_state_unchanged() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = {
            let state = lock(&daemon.state);
            serde_json::to_value(&state.persisted).unwrap()
        };

        assert!(report_agent(&daemon, AgentState::Working).is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn working_agent_reports_set_and_refresh_project_activity() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        {
            let mut state = lock(&daemon.state);
            state.persisted.projects[0].last_agent_active_unix_ms = Some(1);
        }

        report_agent(&daemon, AgentState::Working).unwrap();
        let first_agent_id = {
            let state = lock(&daemon.state);
            assert_ne!(
                state.persisted.projects[0].last_agent_active_unix_ms,
                Some(1)
            );
            assert_eq!(state.persisted.panes[0].revision, 8);
            assert_eq!(state.persisted.projects[0].revision, 8);
            assert_eq!(state.events.len(), 1);
            state.persisted.panes[0].agent.as_ref().unwrap().id
        };

        {
            let mut state = lock(&daemon.state);
            state.persisted.projects[0].last_agent_active_unix_ms = Some(1);
        }
        report_agent(&daemon, AgentState::Working).unwrap();
        let state = lock(&daemon.state);
        assert_ne!(
            state.persisted.projects[0].last_agent_active_unix_ms,
            Some(1)
        );
        assert_eq!(
            state.persisted.panes[0].agent.as_ref().unwrap().id,
            first_agent_id
        );
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_working_agent_reports_preserve_project_activity() {
        let mut persisted = agent_test_persisted();
        persisted.projects[0].last_agent_active_unix_ms = Some(123);
        let (daemon, path) = agent_test_daemon(persisted);

        report_agent(&daemon, AgentState::Idle).unwrap();

        let state = lock(&daemon.state);
        assert_eq!(
            state.persisted.projects[0].last_agent_active_unix_ms,
            Some(123)
        );
        assert_eq!(state.persisted.projects[0].revision, 7);
        assert_eq!(state.persisted.panes[0].revision, 8);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn agent_report_with_missing_ancestry_has_no_mutation() {
        let mut persisted = agent_test_persisted();
        persisted.projects.clear();
        let (daemon, path) = agent_test_daemon(persisted);

        assert_eq!(
            report_agent(&daemon, AgentState::Working).unwrap_err().code,
            "not_found"
        );

        let state = lock(&daemon.state);
        assert!(state.persisted.panes[0].agent.is_none());
        assert_eq!(state.persisted.next_id, 6);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        assert!(!path.exists());
    }

    #[test]
    fn working_activity_survives_state_reload() {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        report_agent(&daemon, AgentState::Working).unwrap();

        let reloaded = load_state(&path).unwrap();
        assert!(reloaded.projects[0].last_agent_active_unix_ms.is_some());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn ids_are_monotonic_and_exhaustion_is_typed() {
        let mut state = Persisted::default();
        assert_eq!(next_id(&mut state).unwrap(), 1);
        assert_eq!(next_id(&mut state).unwrap(), 2);
        state.next_id = u64::MAX;
        assert_eq!(next_id(&mut state).unwrap_err().code, "id_exhausted");
    }
    fn reorder_test_persisted() -> Persisted {
        let mut persisted = agent_test_persisted();
        for (session_id, pane_id, terminal_id, label) in [(6, 8, 9, "second"), (7, 10, 11, "third")]
        {
            persisted.sessions.push(Session {
                id: SessionId(session_id),
                worktree_id: WorktreeId(2),
                label: label.into(),
                primary_pane: PaneId(pane_id),
                focused_pane: PaneId(pane_id),
                panes: vec![PaneId(pane_id)],
                layout: PaneLayout::Leaf {
                    pane_id: PaneId(pane_id),
                },
                revision: 7,
            });
            persisted.panes.push(PersistedPane {
                pane: Pane {
                    id: PaneId(pane_id),
                    terminal_id: TerminalId(terminal_id),
                    session_id: SessionId(session_id),
                    label: "terminal".into(),
                    agent: None,
                    exited: true,
                    revision: 7,
                },
                recovery_quarantined: false,
                recovery: None,
            });
        }
        persisted.next_id = 12;
        persisted
    }

    #[test]
    fn session_reorder_is_daemon_owned_and_persists_by_stable_identity() {
        let (daemon, path) = agent_test_daemon(reorder_test_persisted());

        let response = handle(
            &daemon,
            Request::SessionReorder {
                session_id: SessionId(3),
                target_session_id: SessionId(6),
                placement: SessionPlacement::After,
                expected_revision: 7,
            },
        );

        assert!(
            matches!(response, Response::Ack { revision: 8 }),
            "unexpected reorder response: {response:?}"
        );
        assert_eq!(
            lock(&daemon.state)
                .persisted
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            [SessionId(6), SessionId(3), SessionId(7)]
        );
        let reloaded = load_state(&path).unwrap();
        assert_eq!(
            reloaded
                .sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            [SessionId(6), SessionId(3), SessionId(7)]
        );
        assert_eq!(reloaded.sessions[1].revision, 8);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_reorder_rejects_stale_and_cross_worktree_targets_without_mutation() {
        let (daemon, path) = agent_test_daemon(reorder_test_persisted());
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        let stale = handle(
            &daemon,
            Request::SessionReorder {
                session_id: SessionId(3),
                target_session_id: SessionId(6),
                placement: SessionPlacement::After,
                expected_revision: 6,
            },
        );
        assert!(
            matches!(stale, Response::Error(ApiError { code, .. }) if code == "revision_conflict")
        );

        {
            let mut state = lock(&daemon.state);
            state.persisted.sessions[1].worktree_id = WorktreeId(99);
        }
        let cross_worktree = handle(
            &daemon,
            Request::SessionReorder {
                session_id: SessionId(3),
                target_session_id: SessionId(6),
                placement: SessionPlacement::After,
                expected_revision: 7,
            },
        );
        assert!(
            matches!(cross_worktree, Response::Error(ApiError { code, .. }) if code == "invalid_target")
        );
        {
            let mut state = lock(&daemon.state);
            state.persisted.sessions[1].worktree_id = WorktreeId(2);
            assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
            assert_eq!(state.revision, 7);
            assert!(state.events.is_empty());
        }
        assert!(!path.exists());
    }

    #[test]
    fn session_reorder_save_failure_preserves_live_order_and_revision() {
        let (daemon, path) = agent_test_daemon(reorder_test_persisted());
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        let response = handle(
            &daemon,
            Request::SessionReorder {
                session_id: SessionId(3),
                target_session_id: SessionId(6),
                placement: SessionPlacement::After,
                expected_revision: 7,
            },
        );

        assert!(matches!(response, Response::Error(ApiError { code, .. }) if code == "io"));
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn revisions_conflict_explicitly() {
        assert_eq!(expect_revision(2, 1).unwrap_err().code, "revision_conflict");
    }
    #[test]
    fn utf8_truncation_never_splits_codepoints() {
        let value = truncate_utf8("가".repeat(300), 512);
        assert!(value.len() <= 512);
        assert!(value.chars().all(|character| character == '가'));
    }
    #[test]
    fn default_state_is_valid() {
        validate_persisted(&Persisted::default()).unwrap();
    }

    #[test]
    fn legacy_project_without_activity_timestamp_loads() {
        let (_, path) = agent_test_daemon(Persisted::default());
        fs::write(
            &path,
            r#"{
                "next_id":6,
                "projects":[{"id":1,"path":"/project","name":"project","revision":1}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":1}],
                "sessions":[{"id":3,"worktree_id":2,"label":"legacy","primary_pane":4,"focused_pane":4,"panes":[4],"layout":{"kind":"leaf","pane_id":4},"revision":1}],
                "panes":[{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":false,"revision":1}]
            }"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let state = load_state(&path).unwrap();

        assert_eq!(state.projects[0].last_agent_active_unix_ms, None);
        assert_eq!(state.panes.len(), 1);
        assert!(state.panes[0].recovery.is_none());
        assert!(!state.panes[0].recovery_quarantined);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_recovery_recipe_is_quarantined() {
        let pane = serde_json::from_str::<PersistedPane>(
            r#"{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":true,"revision":1,"recovery":{"command":"not-an-argv","rows":24,"cols":80}}"#,
        )
        .unwrap();

        assert!(pane.recovery.is_none());
        assert!(pane.recovery_quarantined);
        let encoded = serde_json::to_string(&pane).unwrap();
        assert!(encoded.contains("recovery_quarantined"));
        assert!(
            serde_json::from_str::<PersistedPane>(&encoded)
                .unwrap()
                .recovery_quarantined
        );
    }

    #[test]
    fn recovered_state_with_agent_metadata_is_valid() {
        let state = serde_json::from_str::<Persisted>(
            r#"{
                "next_id":19,
                "projects":[{"id":1,"path":"/project","name":"project","revision":2}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":2}],
                "sessions":[{"id":15,"worktree_id":2,"label":"recoverable","primary_pane":16,"focused_pane":16,"panes":[16],"layout":{"kind":"leaf","pane_id":16},"revision":26}],
                "panes":[{"id":16,"terminal_id":17,"session_id":15,"label":"terminal","agent":{"id":18,"provider":"codex","state":"unknown","conversation_id":"conversation","capabilities":{"prompt":false,"resume":false,"lifecycle":false},"source":"persisted"},"exited":false,"revision":35,"recovery":{"command":["/bin/sh"],"initial_input":"printf ok","rows":12,"cols":40}}]
            }"#,
        )
        .unwrap();

        validate_persisted(&state).unwrap();
        assert_eq!(state.panes[0].recovery.as_ref().unwrap().rows, 12);
        assert!(!state.panes[0].recovery_quarantined);
    }

    #[test]
    fn launch_recipe_rejects_unbounded_or_nul_startup_input() {
        assert!(launch_recipe(Vec::new(), Some("x".repeat(MAX_INPUT_BYTES)), 24, 80).is_err());
        assert!(launch_recipe(Vec::new(), Some("bad\0input".into()), 24, 80).is_err());
    }

    #[test]
    fn terminal_view_deduplicates_panes_and_bounds_aggregate_cells() {
        assert_eq!(
            unique_view_pane_ids(vec![PaneId(1), PaneId(1), PaneId(2)]),
            vec![PaneId(1), PaneId(2)]
        );
        assert_eq!(add_view_cells(0, MAX_VIEW_CELLS).unwrap(), MAX_VIEW_CELLS);
        assert_eq!(
            add_view_cells(MAX_VIEW_CELLS, 1).unwrap_err().code,
            "response_too_large"
        );
    }

    #[test]
    fn state_save_does_not_follow_the_legacy_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let directory = std::env::current_dir().unwrap().join(format!(
            ".work/daemon-tests/state-save-{}-{}",
            std::process::id(),
            epoch()
        ));
        fs::create_dir_all(&directory).unwrap();
        let state_path = directory.join("state.json");
        let victim = directory.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, state_path.with_extension("json.tmp")).unwrap();

        save_state(&state_path, &Persisted::default()).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        assert_eq!(
            fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!fs::read_dir(&directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("state.json.tmp.")
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn listener_scan_finds_current_process_group_port_when_lsof_is_available() {
        if Command::new("lsof").arg("-v").output().is_err() {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pid = std::process::id() as libc::pid_t;
        let group = unsafe { libc::getpgrp() };

        let listeners = scan_listening_ports().unwrap();

        assert!(listeners.iter().any(|process| {
            process.pid == pid && process.group == group && process.ports.contains(&port)
        }));
    }

    #[test]
    fn lsof_parser_groups_and_deduplicates_tcp_listeners() {
        let listeners = parse_lsof_ports(
            "p100\ng42\nf8\nn127.0.0.1:3000\nf9\nn*:3000\np101\ng43\nf7\nn[::1]:5173\n",
        )
        .unwrap();

        assert_eq!(
            listeners,
            vec![
                ListenerProcess {
                    pid: 100,
                    group: 42,
                    ports: vec![3000],
                },
                ListenerProcess {
                    pid: 101,
                    group: 43,
                    ports: vec![5173],
                },
            ]
        );
    }

    #[test]
    fn listener_attribution_accepts_descendant_groups_on_a_unique_pane_tty() {
        let pane = PaneId(7);
        let process_groups = HashMap::from([(pane, 42)]);
        let listeners = vec![ListenerProcess {
            pid: 101,
            group: 99,
            ports: vec![5173, 8081, 5173],
        }];
        let terminals = parse_process_terminals("100 42 ttys007\n101 99 ttys007\n").unwrap();

        let attributed = attribute_listening_ports(&process_groups, &listeners, Some(&terminals));

        assert_eq!(attributed.get(&pane), Some(&vec![5173, 8081]));
    }

    #[test]
    fn listener_attribution_rejects_ambiguous_or_missing_ttys_but_keeps_exact_groups() {
        let first = PaneId(7);
        let second = PaneId(8);
        let process_groups = HashMap::from([(first, 42), (second, 43)]);
        let listeners = vec![
            ListenerProcess {
                pid: 101,
                group: 99,
                ports: vec![5173],
            },
            ListenerProcess {
                pid: 102,
                group: 42,
                ports: vec![9000],
            },
        ];
        let terminals =
            parse_process_terminals("100 42 ttys007\n103 43 ttys007\n101 99 ttys007\n104 100 ??\n")
                .unwrap();

        let attributed = attribute_listening_ports(&process_groups, &listeners, Some(&terminals));

        assert_eq!(attributed.get(&first), Some(&vec![9000]));
        assert!(!attributed.contains_key(&second));
    }

    const CODEX_SESSION_ID: &str = "0195d8f4-8c88-7b32-8aee-7d3a6c32c5f8";

    fn recovery_test_agent(provider: &str, conversation_id: Option<&str>) -> AgentInfo {
        serde_json::from_value(serde_json::json!({
            "id": 18,
            "provider": provider,
            "state": "unknown",
            "conversation_id": conversation_id,
            "capabilities": {"prompt": false, "resume": false, "lifecycle": false},
            "source": "persisted"
        }))
        .unwrap()
    }

    fn recovery_test_recipe() -> LaunchRecipe {
        LaunchRecipe {
            command: vec!["saved-agent".into(), "--restore".into()],
            initial_input: Some("saved agent-start input".into()),
            rows: 37,
            cols: 113,
        }
    }

    fn assert_recipe_matches(actual: &LaunchRecipe, expected: &LaunchRecipe) {
        assert_eq!(actual.command, expected.command);
        assert_eq!(actual.initial_input, expected.initial_input);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.cols, expected.cols);
    }

    fn assert_clean_shell_recipe(recipe: &LaunchRecipe, saved_recipe: &LaunchRecipe) {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        assert_eq!(recipe.command, vec![shell]);
        assert_eq!(recipe.initial_input, None);
        assert_eq!(recipe.rows, saved_recipe.rows);
        assert_eq!(recipe.cols, saved_recipe.cols);
    }

    #[test]
    fn recovery_launch_valid_codex_id_provides_a_resume_key() {
        let saved_recipe = recovery_test_recipe();
        let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

        let launch = recovery_launch(Some(&mut agent), &saved_recipe, true);

        assert_eq!(
            launch.recipe.command,
            vec![
                "codex".to_string(),
                "resume".to_string(),
                CODEX_SESSION_ID.to_string(),
            ]
        );
        assert_eq!(launch.recipe.initial_input, None);
        assert_eq!(launch.recipe.rows, saved_recipe.rows);
        assert_eq!(launch.recipe.cols, saved_recipe.cols);
        assert!(launch.resume_key.is_some());
    }

    #[test]
    fn recovery_launch_disabled_leaves_saved_recipe_without_a_resume_key() {
        let saved_recipe = recovery_test_recipe();
        let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

        let launch = recovery_launch(Some(&mut agent), &saved_recipe, false);

        assert_recipe_matches(&launch.recipe, &saved_recipe);
        assert!(launch.resume_key.is_none());
    }

    #[test]
    fn recovery_launch_without_agent_or_session_ref_leaves_saved_recipe_unchanged() {
        let saved_recipe = recovery_test_recipe();

        let launch = recovery_launch(None, &saved_recipe, true);
        assert_recipe_matches(&launch.recipe, &saved_recipe);
        assert!(launch.resume_key.is_none());

        let mut agent_without_ref = recovery_test_agent("codex", None);
        let launch = recovery_launch(Some(&mut agent_without_ref), &saved_recipe, true);
        assert_recipe_matches(&launch.recipe, &saved_recipe);
        assert!(launch.resume_key.is_none());
    }

    #[test]
    fn recovery_launch_legacy_codex_conversation_migrates_to_session_ref() {
        let saved_recipe = recovery_test_recipe();
        let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

        let launch = recovery_launch(Some(&mut agent), &saved_recipe, true);

        assert_eq!(
            launch.recipe.command,
            vec![
                "codex".to_string(),
                "resume".to_string(),
                CODEX_SESSION_ID.to_string(),
            ]
        );
        assert!(agent.session_ref.is_some());
    }

    #[test]
    fn recovery_launch_unsupported_or_malformed_ref_uses_clean_shell() {
        let saved_recipe = recovery_test_recipe();
        let mut seeded = recovery_test_agent("codex", Some(CODEX_SESSION_ID));
        recovery_launch(Some(&mut seeded), &saved_recipe, true);

        let mut unsupported = seeded.clone();
        unsupported.provider = "unsupported".into();
        let unsupported_launch = recovery_launch(Some(&mut unsupported), &saved_recipe, true);
        assert_clean_shell_recipe(&unsupported_launch.recipe, &saved_recipe);
        assert!(unsupported_launch.resume_key.is_none());

        let malformed_id = format!("{CODEX_SESSION_ID}\\n");
        let encoded = serde_json::to_string(&seeded)
            .unwrap()
            .replace(CODEX_SESSION_ID, &malformed_id);
        let mut malformed: AgentInfo = serde_json::from_str(&encoded).unwrap();
        let malformed_launch = recovery_launch(Some(&mut malformed), &saved_recipe, true);
        assert_clean_shell_recipe(&malformed_launch.recipe, &saved_recipe);
        assert!(malformed_launch.resume_key.is_none());
    }
}
