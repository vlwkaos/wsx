//! Persistent wsx authority for project/session structure, PTYs, frames, leases, and plugins.
// ^ [[wsx Architecture]] Snapshots are authoritative; events only invalidate revisions.

mod plugins;

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    ops::{Deref, DerefMut},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wsx_core::runtime::*;
use wsx_terminal::{validate_launch, TerminalRuntime};

const EVENT_LIMIT: usize = 1024;
const PLUGIN_EVENT_LIMIT: usize = 256;
const MAX_CLIENTS: usize = 64;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_VIEW_PANES: usize = 32;
const MAX_VIEW_CELLS: usize = 1_000_000;
const LEASE_TTL: Duration = Duration::from_secs(3);
const PORT_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const PORT_SCAN_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_PORT_SCAN_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchRecipe {
    command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
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
    expires_at: Instant,
}
struct State {
    persisted: Persisted,
    revision: u64,
    runtimes: HashMap<PaneId, Arc<TerminalRuntime>>,
    resize_locks: HashMap<PaneId, Arc<Mutex<()>>>,
    listening_ports: HashMap<PaneId, Vec<u16>>,
    leases: HashMap<PaneId, Lease>,
    events: VecDeque<Event>,
    plugins: Vec<PluginManifest>,
    plugin_events: VecDeque<(String, String)>,
    stopping: bool,
}
struct Daemon {
    state: Mutex<State>,
    changed: Condvar,
    plugin_changed: Condvar,
    active_clients: Arc<AtomicUsize>,
    epoch: u64,
    state_path: PathBuf,
}

fn recover_runtimes(daemon: &Arc<Daemon>) -> io::Result<()> {
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
            if let Some(agent) = &mut pane.agent {
                agent.state = AgentState::Unknown;
                agent.capabilities = AgentCapabilities::default();
                agent.source = "persisted".into();
            }
            if pane.recovery_quarantined {
                continue;
            }
            if pane.recovery.is_none() {
                pane.recovery = Some(default_launch_recipe());
            }
            let Some(recipe) = pane.recovery.as_ref() else {
                continue;
            };
            if let Err(error) = validate_recipe(recipe) {
                eprintln!("wsxd recovery pane {}: {error}", pane.id.0);
                pane.recovery = None;
                pane.recovery_quarantined = true;
                continue;
            }
            let Some(cwd) = session_worktrees
                .get(&pane.session_id)
                .and_then(|worktree_id| worktree_paths.get(worktree_id))
                .cloned()
            else {
                eprintln!("wsxd recovery pane {}: worktree is absent", pane.id.0);
                continue;
            };
            attempts.push((pane.id, pane.terminal_id, cwd, recipe.clone()));
        }
        attempts
    };

    for (pane_id, terminal_id, cwd, recipe) in attempts {
        let runtime = match spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe) {
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
        } else if let Some(pane) = state
            .persisted
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
        {
            pane.exited = false;
        }
    }
    let state = lock(&daemon.state);
    save_state(&daemon.state_path, &state.persisted)
}

pub fn run() -> io::Result<()> {
    let socket = default_socket_path();
    let state_path = state_path();
    secure_parent(&socket)?;
    secure_parent(&state_path)?;
    let _singleton_lock = acquire_singleton_lock(&state_path.with_extension("lock"))?;
    prepare_socket(&socket)?;
    let persisted = load_state(&state_path)?;
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State {
            persisted,
            revision: 1,
            runtimes: HashMap::new(),
            resize_locks: HashMap::new(),
            listening_ports: HashMap::new(),
            leases: HashMap::new(),
            events: VecDeque::new(),
            plugins: plugins::discover(),
            plugin_events: VecDeque::new(),
            stopping: false,
        }),
        changed: Condvar::new(),
        plugin_changed: Condvar::new(),
        active_clients: Arc::new(AtomicUsize::new(0)),
        epoch: epoch(),
        state_path,
    });
    // ^ [[Session Model]] Session identity remains durable even when wsxd must
    // recreate the process behind a pane rather than restore that process.
    recover_runtimes(&daemon)?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let plugin_dispatcher = match spawn_plugin_dispatcher(&daemon) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };
    let port_scanner = spawn_port_scanner(&daemon);

    while !lock(&daemon.state).stopping {
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
                return Err(error);
            }
        }
    }
    cleanup(&daemon, &socket);
    let _ = plugin_dispatcher.join();
    let _ = port_scanner.join();
    Ok(())
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
        .open(path)?;
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
    let runtimes = {
        let mut state = lock(&daemon.state);
        state.stopping = true;
        state.leases.clear();
        state.plugin_events.clear();
        let runtimes = std::mem::take(&mut state.runtimes);
        let _ = save_state(&daemon.state_path, &state.persisted);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        runtimes
    };
    for runtime in runtimes.into_values() {
        runtime.terminate();
    }
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
        let response = if matches!(request, Request::Shutdown) {
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
    let acquired = handle(
        &daemon,
        Request::TerminalAcquire {
            pane_id,
            client_id,
            takeover,
        },
    );
    if !matches!(acquired, Response::Ack { .. }) {
        write_response(&mut stream, acquired)?;
        return Ok(());
    }
    if let Err(error) = touch_terminal_project(&daemon, pane_id) {
        release_stream_lease(&daemon, pane_id, client_id);
        write_response(&mut stream, Response::Error(error))?;
        return Ok(());
    }
    let resized = handle(
        &daemon,
        Request::TerminalResize {
            pane_id,
            client_id,
            rows,
            cols,
        },
    );
    if !matches!(resized, Response::Ack { .. }) {
        release_stream_lease(&daemon, pane_id, client_id);
        write_response(&mut stream, resized)?;
        return Ok(());
    }
    write_response(&mut stream, resized)?;

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
                let request = match message {
                    TerminalClientMessage::Key(key) => Request::TerminalKey {
                        pane_id,
                        client_id,
                        key,
                    },
                    TerminalClientMessage::Paste(text) => Request::TerminalPaste {
                        pane_id,
                        client_id,
                        text,
                    },
                    TerminalClientMessage::Mouse(mouse) => Request::TerminalMouse {
                        pane_id,
                        client_id,
                        mouse,
                    },
                    TerminalClientMessage::Input(bytes) => Request::TerminalInput {
                        pane_id,
                        client_id,
                        bytes,
                    },
                    TerminalClientMessage::Resize { rows, cols } => Request::TerminalResize {
                        pane_id,
                        client_id,
                        rows,
                        cols,
                    },
                    TerminalClientMessage::Heartbeat => {
                        Request::TerminalHeartbeat { pane_id, client_id }
                    }
                    TerminalClientMessage::Resync => {
                        input_resync.store(true, Ordering::Release);
                        input_daemon.changed.notify_all();
                        continue;
                    }
                    TerminalClientMessage::Detach => break,
                };
                if let Response::Error(error) = handle(&input_daemon, request) {
                    *lock(&input_error_slot) = Some(error);
                    break;
                }
            }
            input_active.store(false, Ordering::Release);
            input_daemon.changed.notify_all();
        })?;

    let result = stream_terminal_updates(
        &mut stream,
        &daemon,
        pane_id,
        client_id,
        &active,
        &resync,
        &input_error,
    );
    active.store(false, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = input_thread.join();
    release_stream_lease(&daemon, pane_id, client_id);
    result
}

fn stream_terminal_updates(
    stream: &mut UnixStream,
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    client_id: u64,
    active: &AtomicBool,
    resync: &AtomicBool,
    input_error: &Mutex<Option<ApiError>>,
) -> io::Result<()> {
    let mut baseline = None;
    while active.load(Ordering::Acquire) {
        if let Some(error) = lock(input_error).take() {
            write_stream_message(stream, &TerminalServerMessage::Error(error))?;
            break;
        }
        let runtime = {
            let state = lock(&daemon.state);
            if state.stopping
                || !state.leases.get(&pane_id).is_some_and(|lease| {
                    lease.client_id == client_id && lease.expires_at > Instant::now()
                })
            {
                None
            } else {
                state.runtimes.get(&pane_id).cloned()
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
        if runtime.exited() {
            write_stream_message(stream, &TerminalServerMessage::Exited)?;
            break;
        }
        if resync.swap(false, Ordering::AcqRel) {
            baseline = None;
        }
        let revision = runtime.revision();
        let synchronized_output = runtime.synchronized_output_active();
        // A new subscriber always receives one complete surface. Synchronized
        // output suppresses only later intermediate revisions.
        if baseline.is_none() || (!synchronized_output && baseline != Some(revision)) {
            match runtime.frame_update(baseline) {
                Ok(update) => {
                    baseline = Some(update.revision());
                    write_stream_message(stream, &TerminalServerMessage::Update(update))?;
                }
                Err(error) => {
                    write_stream_message(
                        stream,
                        &TerminalServerMessage::Error(ApiError::new(
                            "frame_failed",
                            error.to_string(),
                        )),
                    )?;
                    break;
                }
            }
        }
        let state = lock(&daemon.state);
        // ^ [[wsx Architecture]] Recheck every wake predicate while
        // holding the notifier mutex so output cannot be stranded for 250 ms.
        if !active.load(Ordering::Acquire) || resync.load(Ordering::Acquire) {
            continue;
        }
        if synchronized_output {
            if revision != runtime.revision() {
                continue;
            }
        } else if baseline != Some(runtime.revision()) {
            continue;
        }
        let _ = daemon
            .changed
            .wait_timeout(state, Duration::from_millis(250))
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

fn release_stream_lease(daemon: &Daemon, pane_id: PaneId, client_id: u64) {
    let mut state = lock(&daemon.state);
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.client_id == client_id)
    {
        state.leases.remove(&pane_id);
    }
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
        Request::ProjectRecentClear {
            project_id,
            expected_revision,
        } => clear_project_recent(daemon, project_id, expected_revision),
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
        } => mutate(daemon, |state| {
            let session = session_mut(state, session_id)?;
            expect_revision(session.revision, expected_revision)?;
            session.label = bounded_label(label)?;
            Ok(("session.renamed", session_id.0))
        }),
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
        } => mutate(daemon, |state| {
            let revision = state.revision.saturating_add(1);
            let session = session_mut(state, session_id)?;
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
        } => mutate_runtime(daemon, |state| {
            require_runtime(state, pane_id)?;
            if state.leases.get(&pane_id).is_some_and(|lease| {
                lease.expires_at > Instant::now() && lease.client_id != client_id
            }) && !takeover
            {
                return Err(api("terminal_busy", "pane has another writable controller"));
            }
            state.leases.insert(
                pane_id,
                Lease {
                    client_id,
                    expires_at: Instant::now() + LEASE_TTL,
                },
            );
            Ok(())
        }),
        Request::TerminalRelease { pane_id, client_id } => mutate_runtime(daemon, |state| {
            require_lease(state, pane_id, client_id)?;
            state.leases.remove(&pane_id);
            Ok(())
        }),
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
            provider,
            state: agent_state,
            conversation_id,
            capabilities,
        } => agent_report(
            daemon,
            pane_id,
            provider,
            agent_state,
            conversation_id,
            capabilities,
        ),
        Request::PluginList => Ok(Response::Plugins(lock(&daemon.state).plugins.clone())),
        Request::PluginReload => {
            let mut state = lock(&daemon.state);
            state.plugins = plugins::discover();
            Ok(Response::Plugins(state.plugins.clone()))
        }
        Request::Shutdown => {
            let mut state = lock(&daemon.state);
            state.stopping = true;
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
        listening_ports: true,
        process_restore: false,
    }
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
    let revision = bump(daemon, &mut state, "projects.synchronized", 0);
    let mut retained_projects = Vec::new();
    let mut retained_worktrees = Vec::new();
    for (path, name, worktrees) in canonical {
        let project_id = match state
            .persisted
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id)
        {
            Some(id) => id,
            None => ProjectId(next_id(&mut state.persisted)?),
        };
        retained_projects.push(project_id);
        if let Some(project) = state
            .persisted
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.name = name;
            project.revision = revision;
        } else {
            state.persisted.projects.push(Project {
                id: project_id,
                path,
                name,
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                revision,
            });
        }
        for (path, branch) in worktrees {
            let worktree_id = match state
                .persisted
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| worktree.id)
            {
                Some(id) => id,
                None => WorktreeId(next_id(&mut state.persisted)?),
            };
            retained_worktrees.push(worktree_id);
            if let Some(worktree) = state
                .persisted
                .worktrees
                .iter_mut()
                .find(|worktree| worktree.id == worktree_id)
            {
                worktree.project_id = project_id;
                worktree.branch = branch;
                worktree.revision = revision;
            } else {
                state.persisted.worktrees.push(Worktree {
                    id: worktree_id,
                    project_id,
                    path,
                    branch,
                    revision,
                });
            }
        }
    }
    let session_worktrees = state
        .persisted
        .sessions
        .iter()
        .map(|session| session.worktree_id)
        .collect::<HashSet<_>>();
    for project_id in state
        .persisted
        .worktrees
        .iter()
        .filter(|worktree| session_worktrees.contains(&worktree.id))
        .map(|worktree| worktree.project_id)
    {
        if !retained_projects.contains(&project_id) {
            retained_projects.push(project_id);
        }
    }
    state
        .persisted
        .projects
        .retain(|project| retained_projects.contains(&project.id));
    state.persisted.worktrees.retain(|worktree| {
        retained_worktrees.contains(&worktree.id) || session_worktrees.contains(&worktree.id)
    });
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
        let worktree = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or_else(|| api("not_found", "worktree not found"))?;
        let cwd = worktree.path.clone();
        let project_id = worktree.project_id;
        let project_index = state
            .persisted
            .projects
            .iter()
            .position(|project| project.id == project_id)
            .ok_or_else(|| api("not_found", "project not found"))?;
        let session_id = SessionId(next_id(&mut state.persisted)?);
        let pane_id = PaneId(next_id(&mut state.persisted)?);
        let terminal_id = TerminalId(next_id(&mut state.persisted)?);
        let revision = bump(daemon, &mut state, "session.created", session_id.0);
        state.persisted.projects[project_index].last_terminal_active_unix_ms =
            Some(unix_time_millis());
        state.persisted.projects[project_index].revision = revision;
        state.persisted.panes.push(PersistedPane {
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
        state.persisted.sessions.push(Session {
            id: session_id,
            worktree_id,
            label,
            primary_pane: pane_id,
            focused_pane: pane_id,
            panes: vec![pane_id],
            layout: PaneLayout::Leaf { pane_id },
            revision,
        });
        save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
        let pane_id = PaneId(next_id(&mut state.persisted)?);
        let terminal_id = TerminalId(next_id(&mut state.persisted)?);
        save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
        (cwd, pane_id, terminal_id)
    };
    let runtime = spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe)?;
    let mut state = lock(&daemon.state);
    let Some(index) = state
        .persisted
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
    if state.persisted.sessions[index].revision != expected
        || !state.persisted.sessions[index].panes.contains(&target)
    {
        drop(state);
        runtime.terminate();
        return Err(api(
            "revision_conflict",
            "session changed while terminal was starting",
        ));
    }
    if !state.persisted.sessions[index]
        .layout
        .split(target, pane_id, axis)
    {
        drop(state);
        runtime.terminate();
        return Err(api("invalid_layout", "target pane is absent from layout"));
    }
    let revision = bump(daemon, &mut state, "pane.created", pane_id.0);
    let session = &mut state.persisted.sessions[index];
    session.panes.push(pane_id);
    session.focused_pane = pane_id;
    session.revision = revision;
    state.persisted.panes.push(PersistedPane {
        pane: Pane {
            id: pane_id,
            terminal_id,
            session_id,
            label,
            agent: None,
            exited: false,
            revision,
        },
        recovery: Some(recipe),
        recovery_quarantined: false,
    });
    let runtime = Arc::new(runtime);
    state.runtimes.insert(pane_id, Arc::clone(&runtime));
    if runtime.exited() {
        record_terminal_exit(daemon, &mut state, pane_id);
    }
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
    Ok(Response::Created {
        revision,
        id: pane_id.0,
    })
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
    let mut runtimes = Vec::new();
    for pane_id in &session.panes {
        if let Some(runtime) = state.runtimes.remove(pane_id) {
            runtimes.push(runtime);
        }
        state.leases.remove(pane_id);
        state.resize_locks.remove(pane_id);
        state.listening_ports.remove(pane_id);
    }
    state.persisted.panes.retain(|pane| pane.session_id != id);
    state.persisted.sessions.retain(|session| session.id != id);
    let revision = bump(daemon, &mut state, "session.closed", id.0);
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
    let session = state
        .persisted
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
    let runtime = state.runtimes.remove(&id);
    state.leases.remove(&id);
    state.resize_locks.remove(&id);
    state.listening_ports.remove(&id);
    state.persisted.panes.retain(|pane| pane.id != id);
    let revision = bump(daemon, &mut state, "pane.closed", id.0);
    if let Some(session) = state
        .persisted
        .sessions
        .iter_mut()
        .find(|session| session.id == pane.session_id)
    {
        session.revision = revision;
    }
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
    let resize_lock = {
        let mut state = lock(&daemon.state);
        refresh_lease(&mut state, pane_id, client_id)?;
        Arc::clone(
            state
                .resize_locks
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    };
    let _resize = lock(&resize_lock);
    let (runtime, _) = leased_runtime(daemon, pane_id, client_id)?;
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
        save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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

fn clear_project_recent(
    daemon: &Arc<Daemon>,
    project_id: ProjectId,
    expected_revision: u64,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let project = persisted
        .projects
        .iter_mut()
        .find(|project| project.id == project_id)
        .ok_or_else(|| api("not_found", "project not found"))?;
    expect_revision(project.revision, expected_revision)?;
    let revision = state.revision.saturating_add(1);
    project.last_agent_active_unix_ms = None;
    project.last_terminal_active_unix_ms = None;
    project.revision = revision;
    save_state(&daemon.state_path, &persisted).map_err(io_api)?;
    state.persisted = persisted;
    let revision = bump(daemon, &mut state, "project.recent_cleared", project_id.0);
    Ok(Response::Ack { revision })
}

fn agent_report(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    provider: String,
    agent_state: AgentState,
    conversation_id: Option<String>,
    capabilities: AgentCapabilities,
) -> Result<Response, ApiError> {
    let provider = bounded_provider(provider)?;
    let conversation_id = conversation_id.map(|value| truncate_utf8(value, 512));
    let mut state = lock(&daemon.state);
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
    let revision = bump(daemon, &mut state, "agent.reported", pane_id.0);
    Ok(Response::Ack { revision })
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn mutate<F>(daemon: &Arc<Daemon>, mutation: F) -> Result<Response, ApiError>
where
    F: FnOnce(&mut State) -> Result<(&'static str, u64), ApiError>,
{
    let mut state = lock(&daemon.state);
    let (entity, id) = mutation(&mut state)?;
    let revision = bump(daemon, &mut state, entity, id);
    if let Some(session) = state
        .persisted
        .sessions
        .iter_mut()
        .find(|session| session.id.0 == id)
    {
        session.revision = revision;
    }
    save_state(&daemon.state_path, &state.persisted).map_err(io_api)?;
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
fn bump(daemon: &Daemon, state: &mut State, entity: &str, id: u64) -> u64 {
    state.revision = state.revision.saturating_add(1);
    let revision = state.revision;
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
    revision
}

// ^ [[Session Model]] Agent adapters need stable pane identity from this spawn
// boundary; crates/wsx-core/src/integration owns installation and bundled assets.
fn terminal_agent_environment(pane_id: PaneId) -> Vec<(String, String)> {
    let mut environment = vec![("WSX_PANE_ID".into(), pane_id.to_string())];
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
    let startup = startup_input(recipe);
    TerminalRuntime::spawn(
        pane_id,
        terminal_id,
        cwd,
        &recipe.command,
        &terminal_agent_environment(pane_id),
        startup.as_deref(),
        recipe.rows,
        recipe.cols,
        notify,
    )
    .map_err(terminal_api)
}

fn record_terminal_exit(daemon: &Daemon, state: &mut State, pane_id: PaneId) {
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
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
}

fn spawn_port_scanner(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let process_groups = {
            let state = lock(&daemon.state);
            if state.stopping {
                return;
            }
            state
                .runtimes
                .iter()
                .filter(|(_, runtime)| !runtime.exited())
                .filter_map(|(pane_id, runtime)| {
                    runtime.process_group_id().map(|group| (*pane_id, group))
                })
                .collect::<HashMap<_, _>>()
        };

        let detected = if process_groups.is_empty() {
            Some(HashMap::new())
        } else {
            scan_listening_ports().map(|by_group| {
                process_groups
                    .into_iter()
                    .filter_map(|(pane_id, group)| {
                        by_group
                            .get(&group)
                            .filter(|ports| !ports.is_empty())
                            .cloned()
                            .map(|ports| (pane_id, ports))
                    })
                    .collect::<HashMap<_, _>>()
            })
        };
        if let Some(next) = detected {
            let mut state = lock(&daemon.state);
            if state.listening_ports != next {
                state.listening_ports = next;
                bump(&daemon, &mut state, "ports.changed", 0);
            }
        }

        let started = Instant::now();
        while started.elapsed() < PORT_SCAN_INTERVAL {
            if lock(&daemon.state).stopping {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}

fn scan_listening_ports() -> Option<HashMap<libc::pid_t, Vec<u16>>> {
    let mut child = Command::new("lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpgn"])
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
    if !status.success() && status.code() != Some(1) {
        return None;
    }
    parse_lsof_ports(&String::from_utf8_lossy(&bytes))
}

fn parse_lsof_ports(output: &str) -> Option<HashMap<libc::pid_t, Vec<u16>>> {
    let mut current_group = None;
    let mut ports = HashMap::<libc::pid_t, Vec<u16>>::new();
    for line in output.lines() {
        match line.as_bytes().first().copied() {
            Some(b'p') => current_group = None,
            Some(b'g') => current_group = line[1..].parse::<libc::pid_t>().ok(),
            Some(b'n') => {
                let Some(group) = current_group else {
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
                ports.entry(group).or_default().push(port);
            }
            _ => {}
        }
    }
    for values in ports.values_mut() {
        values.sort_unstable();
        values.dedup();
    }
    Some(ports)
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

fn leased_runtime(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    let mut state = lock(&daemon.state);
    refresh_lease(&mut state, pane_id, client_id)?;
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
    match state.leases.get(&pane_id) {
        Some(lease) if lease.client_id == client_id && lease.expires_at > Instant::now() => Ok(()),
        _ => Err(api(
            "lease_required",
            "client does not own an active terminal lease",
        )),
    }
}

fn refresh_lease(state: &mut State, pane_id: PaneId, client_id: u64) -> Result<(), ApiError> {
    require_lease(state, pane_id, client_id)?;
    if let Some(lease) = state.leases.get_mut(&pane_id) {
        lease.expires_at = Instant::now() + LEASE_TTL;
    }
    Ok(())
}
fn session_mut(state: &mut State, id: SessionId) -> Result<&mut Session, ApiError> {
    state
        .persisted
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
fn load_state(path: &Path) -> io::Result<Persisted> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0 =>
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe wsx state file",
            ));
        }
        Ok(metadata) if metadata.len() > 8 * 1024 * 1024 => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "state file too large",
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Persisted::default()),
        Err(error) => return Err(error),
    }
    let mut state: Persisted = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_persisted(&state)?;
    for pane in &mut state.panes {
        pane.exited = true;
    }
    Ok(state)
}
fn save_state(path: &Path, state: &Persisted) -> io::Result<()> {
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    validate_persisted(state)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state exceeds limit",
        ));
    }
    let temporary = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
                resize_locks: HashMap::new(),
                listening_ports: HashMap::new(),
                leases: HashMap::new(),
                events: VecDeque::new(),
                plugins: Vec::new(),
                plugin_events: VecDeque::new(),
                stopping: false,
            }),
            changed: Condvar::new(),
            plugin_changed: Condvar::new(),
            active_clients: Arc::new(AtomicUsize::new(0)),
            epoch: 1,
            state_path: path.clone(),
        });
        (daemon, path)
    }

    #[test]
    fn terminal_agent_environment_exposes_stable_pane_identity() {
        let environment = terminal_agent_environment(PaneId(42));
        assert!(environment
            .iter()
            .any(|(name, value)| name == "WSX_PANE_ID" && value == "42"));
    }

    fn report_agent(daemon: &Arc<Daemon>, state: AgentState) -> Result<Response, ApiError> {
        agent_report(
            daemon,
            PaneId(4),
            "codex".into(),
            state,
            Some("conversation".into()),
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
    fn recent_clear_removes_both_sources_and_later_terminal_entry_readds_project() {
        let mut persisted = agent_test_persisted();
        persisted.projects[0].last_agent_active_unix_ms = Some(10);
        persisted.projects[0].last_terminal_active_unix_ms = Some(20);
        let (daemon, path) = agent_test_daemon(persisted);

        clear_project_recent(&daemon, ProjectId(1), 7).unwrap();
        {
            let state = lock(&daemon.state);
            assert_eq!(state.persisted.projects[0].last_agent_active_unix_ms, None);
            assert_eq!(
                state.persisted.projects[0].last_terminal_active_unix_ms,
                None
            );
            assert_eq!(state.persisted.projects[0].revision, 8);
        }

        touch_terminal_project(&daemon, PaneId(4)).unwrap();
        let state = lock(&daemon.state);
        assert!(state.persisted.projects[0]
            .last_terminal_active_unix_ms
            .is_some());
        assert_eq!(state.persisted.projects[0].last_agent_active_unix_ms, None);
        drop(state);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn recent_clear_save_failure_keeps_live_state_unchanged() {
        let mut persisted = agent_test_persisted();
        persisted.projects[0].last_agent_active_unix_ms = Some(10);
        persisted.projects[0].last_terminal_active_unix_ms = Some(20);
        let (daemon, path) = agent_test_daemon(persisted);
        fs::create_dir(&path).unwrap();
        let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

        assert!(clear_project_recent(&daemon, ProjectId(1), 7).is_err());
        let state = lock(&daemon.state);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
        drop(state);
        fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn recent_clear_rejects_stale_revision_without_mutation() {
        let mut persisted = agent_test_persisted();
        persisted.projects[0].last_agent_active_unix_ms = Some(10);
        let (daemon, path) = agent_test_daemon(persisted);

        let error = clear_project_recent(&daemon, ProjectId(1), 6).unwrap_err();
        assert_eq!(error.code, "revision_conflict");
        assert_eq!(
            lock(&daemon.state).persisted.projects[0].last_agent_active_unix_ms,
            Some(10)
        );
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
        let group = unsafe { libc::getpgrp() };

        let ports = scan_listening_ports().unwrap();

        assert!(ports.get(&group).is_some_and(|ports| ports.contains(&port)));
    }

    #[test]
    fn lsof_parser_groups_and_deduplicates_tcp_listeners() {
        let ports = parse_lsof_ports(
            "p100\ng42\nf8\nn127.0.0.1:3000\nf9\nn*:3000\np101\ng43\nf7\nn[::1]:5173\n",
        )
        .unwrap();

        assert_eq!(ports.get(&42), Some(&vec![3000]));
        assert_eq!(ports.get(&43), Some(&vec![5173]));
    }
}
