use super::protocol::{
    binary_identity, encode_line, Request, Response, TerminalClientMessage, TerminalServerMessage,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        net::UnixStream,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const START_WINDOW: Duration = Duration::from_secs(60);
const MAX_START_ATTEMPTS: usize = 3;
static ACTIVE_TUI_MONITORS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Current,
    RecoveredFromBackup,
    LegacyCompatible,
    NewerDaemon {
        daemon_version: String,
    },
    DaemonReplaced {
        previous_version: String,
    },
    ReplacementDeferred {
        daemon_version: String,
        target_version: String,
        live_runtimes: usize,
        blockers: Vec<super::domain::ReplacementBlocker>,
    },
}

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
    automatic_start: bool,
}
impl Client {
    pub fn local() -> Self {
        Self {
            socket: super::protocol::default_socket_path(),
            automatic_start: true,
        }
    }
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            automatic_start: false,
        }
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Gracefully stop the current daemon and wait for its socket cleanup.
    pub fn shutdown(&self) -> io::Result<()> {
        match probe_existing_daemon(self)? {
            ExistingDaemon::Missing => Ok(()),
            ExistingDaemon::Ready { .. } => match self.call(&Request::Shutdown) {
                Ok(Response::Ack { .. }) => wait_until_stopped(self),
                Ok(Response::Error(error)) => Err(io::Error::other(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected wsxd shutdown response",
                )),
                Err(error) if daemon_is_stopped_error(&error) => Ok(()),
                Err(error) => Err(error),
            },
            ExistingDaemon::Incompatible {
                stream,
                advertised_protocol,
                ..
            } => {
                shutdown_incompatible_daemon(self, stream, advertised_protocol)?;
                wait_until_stopped(self)
            }
        }
    }

    pub fn call(&self, request: &Request) -> io::Result<Response> {
        let mut stream = self.connect()?;
        if !matches!(request, Request::Hello { .. }) {
            validate_hello(round_trip(
                &mut stream,
                &Request::Hello {
                    protocol: PROTOCOL_VERSION,
                },
            )?)?;
        }
        round_trip(&mut stream, request)
    }

    fn connect(&self) -> io::Result<UnixStream> {
        validate_socket(&self.socket)?;
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        validate_peer_owner(&stream)?;
        Ok(stream)
    }
}

struct Handshake {
    epoch: u64,
}

fn validate_hello(response: Response) -> io::Result<Handshake> {
    match response {
        Response::Hello {
            protocol, epoch, ..
        } if protocol == PROTOCOL_VERSION => Ok(Handshake { epoch }),
        Response::Hello { protocol, .. } => Err(io::Error::other(format!(
            "protocol_mismatch: client {PROTOCOL_VERSION}, daemon {protocol}"
        ))),
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd protocol handshake failed",
        )),
    }
}

fn round_trip(stream: &mut UnixStream, request: &Request) -> io::Result<Response> {
    stream.write_all(&encode_line(request).map_err(io::Error::other)?)?;
    stream.flush()?;
    read_json_line(stream, MAX_RESPONSE_BYTES)
}

fn round_trip_buffered(
    writer: &mut UnixStream,
    reader: &mut impl BufRead,
    request: &Request,
) -> io::Result<Response> {
    writer.write_all(&encode_line(request).map_err(io::Error::other)?)?;
    writer.flush()?;
    read_buffered_json_line(reader, MAX_RESPONSE_BYTES)
}

fn read_buffered_json_line<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
    limit: usize,
) -> io::Result<T> {
    let mut response = Vec::with_capacity(4096);
    let read = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut response)?;
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "daemon closed response",
        ));
    }
    if response.len() > limit || response.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon response is incomplete or exceeds limit",
        ));
    }
    response.pop();
    serde_json::from_slice(&response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_json_line<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
    limit: usize,
) -> io::Result<T> {
    read_buffered_json_line(&mut BufReader::new(reader), limit)
}

const TERMINAL_INPUT_QUEUE: usize = 1024;
const TERMINAL_UPDATE_QUEUE: usize = 64;

pub struct TerminalStream {
    epoch: u64,
    input: mpsc::SyncSender<TerminalClientMessage>,
    updates: mpsc::Receiver<TerminalServerMessage>,
    resync: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    socket: UnixStream,
    writer: Option<thread::JoinHandle<()>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl TerminalStream {
    pub fn connect(
        client: &Client,
        pane_id: super::domain::PaneId,
        client_id: u64,
        takeover: bool,
        rows: u16,
        cols: u16,
    ) -> io::Result<Self> {
        let mut stream = client.connect()?;
        let mut reader = BufReader::with_capacity(64 * 1024, stream.try_clone()?);
        let handshake = validate_hello(round_trip_buffered(
            &mut stream,
            &mut reader,
            &Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
        )?)?;
        match round_trip_buffered(
            &mut stream,
            &mut reader,
            &Request::TerminalSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            },
        )? {
            Response::Ack { .. } => {}
            Response::Error(error) => {
                return Err(io::Error::other(format!(
                    "{}: {}",
                    error.code, error.message
                )))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected terminal stream response",
                ))
            }
        }
        stream.set_read_timeout(None)?;
        let socket = stream.try_clone()?;
        let write_stream = stream.try_clone()?;
        let (input_tx, input_rx) = mpsc::sync_channel(TERMINAL_INPUT_QUEUE);
        let (update_tx, update_rx) = mpsc::sync_channel(TERMINAL_UPDATE_QUEUE);
        let resync = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));
        let writer_resync = Arc::clone(&resync);
        let writer_stop = Arc::clone(&stopping);
        let writer = thread::Builder::new()
            .name("wsx-terminal-writer".into())
            .spawn(move || terminal_writer(write_stream, input_rx, &writer_resync, &writer_stop))?;
        let reader_stop = Arc::clone(&stopping);
        let reader = thread::Builder::new()
            .name("wsx-terminal-reader".into())
            .spawn(move || terminal_reader(reader, update_tx, &reader_stop))?;
        Ok(Self {
            epoch: handshake.epoch,
            input: input_tx,
            updates: update_rx,
            resync,
            stopping,
            socket,
            writer: Some(writer),
            reader: Some(reader),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn request_resync(&self) {
        self.resync.store(true, Ordering::Release);
    }

    pub fn try_send(
        &self,
        message: TerminalClientMessage,
    ) -> Result<(), mpsc::TrySendError<TerminalClientMessage>> {
        self.input.try_send(message)
    }

    pub fn try_recv(&self) -> Result<TerminalServerMessage, mpsc::TryRecvError> {
        self.updates.try_recv()
    }
}

impl Drop for TerminalStream {
    fn drop(&mut self) {
        let _ = self.input.try_send(TerminalClientMessage::Detach);
        self.stopping.store(true, Ordering::Release);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.writer.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.reader.take() {
            let _ = thread.join();
        }
    }
}

fn terminal_writer(
    mut stream: UnixStream,
    input: mpsc::Receiver<TerminalClientMessage>,
    resync: &AtomicBool,
    stopping: &AtomicBool,
) {
    let mut last_heartbeat = Instant::now();
    while !stopping.load(Ordering::Acquire) {
        let message = if resync.swap(false, Ordering::AcqRel) {
            Some(TerminalClientMessage::Resync)
        } else {
            match input.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => Some(message),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if last_heartbeat.elapsed() >= Duration::from_secs(1) =>
                {
                    Some(TerminalClientMessage::Heartbeat)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let Some(message) = message else { continue };
        if matches!(&message, TerminalClientMessage::Heartbeat) {
            last_heartbeat = Instant::now();
        }
        let bytes = match encode_line(&message) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
            break;
        }
        if matches!(&message, TerminalClientMessage::Detach) {
            break;
        }
    }
}

fn terminal_reader(
    mut reader: BufReader<UnixStream>,
    updates: mpsc::SyncSender<TerminalServerMessage>,
    stopping: &AtomicBool,
) {
    while !stopping.load(Ordering::Acquire) {
        let message = match read_buffered_json_line(&mut reader, MAX_RESPONSE_BYTES) {
            Ok(message) => message,
            Err(error) => {
                let _ = updates.try_send(TerminalServerMessage::Error(
                    super::protocol::ApiError::new("stream_disconnected", error.to_string()),
                ));
                break;
            }
        };
        let terminal = matches!(
            &message,
            TerminalServerMessage::Error(_) | TerminalServerMessage::Exited
        );
        if !queue_terminal_update(&updates, message, stopping) || terminal {
            break;
        }
    }
    stopping.store(true, Ordering::Release);
}

fn queue_terminal_update(
    updates: &mpsc::SyncSender<TerminalServerMessage>,
    mut message: TerminalServerMessage,
    stopping: &AtomicBool,
) -> bool {
    // ^ [[wsx Architecture]] Backpressure may pause this reader, but
    // stream shutdown must always be able to interrupt a full update queue.
    loop {
        match updates.try_send(message) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(pending)) => {
                if stopping.load(Ordering::Acquire) {
                    return false;
                }
                message = pending;
                thread::sleep(Duration::from_millis(10));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

pub fn new_client_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    time ^ (u64::from(std::process::id()) << 32) ^ NEXT.fetch_add(1, Ordering::Relaxed)
}

fn validate_socket(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "wsxd socket path must be absolute",
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wsxd socket is not an owner-only same-user socket",
        ));
    }
    Ok(())
}

// ^ [[MacOS Daemon Authentication and Recovery]]
// macOS LOCAL_PEERTOKEN returns audit_token_t, defined as eight natural_t values.
// See the platform SDK's mach/message.h and bsm/libbsm.h contracts.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct AuditToken {
    value: [u32; 8],
}

#[cfg(target_os = "macos")]
#[link(name = "bsm")]
extern "C" {
    fn audit_token_to_euid(token: AuditToken) -> libc::uid_t;
}

#[cfg(target_os = "macos")]
fn peer_audit_token(stream: &UnixStream) -> io::Result<AuditToken> {
    let mut token = AuditToken { value: [0; 8] };
    let mut length = std::mem::size_of::<AuditToken>() as libc::socklen_t;
    // ^ The kernel writes at most `length` bytes into this correctly sized C buffer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            &mut token as *mut AuditToken as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<AuditToken>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd peer returned an invalid audit token",
        ));
    }
    Ok(token)
}

#[cfg(target_os = "macos")]
fn validate_peer_owner(stream: &UnixStream) -> io::Result<()> {
    let token = peer_audit_token(stream)?;
    // ^ The kernel-issued peer UID keeps the daemon boundary host-local to this account.
    if unsafe { audit_token_to_euid(token) } != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wsxd socket peer belongs to another user",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn validate_peer_owner(_stream: &UnixStream) -> io::Result<()> {
    Ok(())
}

pub fn ensure_available() -> io::Result<Availability> {
    ensure_available_with(&Client::local(), false)
}

pub fn recover_daemon() -> io::Result<Availability> {
    ensure_available_with(&Client::local(), true)
}

pub fn ensure_background_available() -> io::Result<Availability> {
    ensure_background_available_with(&Client::local())
}

fn ensure_background_available_with(client: &Client) -> io::Result<Availability> {
    if !client.automatic_start {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "automatic recovery is disabled for a custom wsxd client",
        ));
    }
    if !client.socket().exists() && !background_recovery_allowed(client.socket()) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "wsxd was stopped intentionally",
        ));
    }
    ensure_available_with(client, false)
}

fn ensure_available_with(client: &Client, reset_crash_budget: bool) -> io::Result<Availability> {
    ensure_available_with_binary(client, reset_crash_budget, &daemon_binary())
}

fn ensure_available_with_binary(
    client: &Client,
    reset_crash_budget: bool,
    binary: &Path,
) -> io::Result<Availability> {
    let target_binary_id = binary_identity(binary).ok();
    let first = probe_existing_daemon(client)?;
    if let Some(availability) =
        ready_without_transition(client, &first, target_binary_id.as_deref())?
    {
        return Ok(availability);
    }

    let mut bootstrap = acquire_bootstrap_lock(client.socket())?;
    let existing = probe_existing_daemon(client)?;
    if let Some(availability) =
        ready_without_transition(client, &existing, target_binary_id.as_deref())?
    {
        return Ok(availability);
    }

    match existing {
        ExistingDaemon::Missing => {
            let planned = consume_planned_marker(client.socket(), target_binary_id.as_deref())?;
            start_daemon(
                client,
                binary,
                &mut bootstrap,
                reset_crash_budget || planned,
            )
        }
        ExistingDaemon::Ready {
            lifecycle_coordination: true,
            version_coordination,
        }
        | ExistingDaemon::Incompatible {
            lifecycle_coordination: true,
            version_coordination,
            ..
        } => {
            let target_binary_id = target_binary_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("could not identify replacement daemon {}", binary.display()),
                )
            })?;
            let status = lifecycle_status(client)?;
            let daemon_version = lifecycle_version(&status);
            let target_version = super::protocol::WSX_VERSION.to_string();
            let compatible = matches!(existing, ExistingDaemon::Ready { .. });
            if compatible && !version_coordination && legacy_daemon_has_other_clients(&status) {
                return Ok(Availability::ReplacementDeferred {
                    daemon_version,
                    target_version,
                    live_runtimes: status.live_runtimes,
                    blockers: vec![super::domain::ReplacementBlocker::LegacyDaemon],
                });
            }
            match lifecycle_round_trip(
                client,
                &Request::PrepareReplacement {
                    target_binary_id: target_binary_id.clone(),
                },
            )? {
                Response::Replacement {
                    disposition: super::domain::ReplacementDisposition::Stopping,
                    ..
                } => {
                    wait_until_stopped(client)?;
                    consume_planned_marker(client.socket(), Some(&target_binary_id))?;
                    start_daemon(client, binary, &mut bootstrap, true)?;
                    Ok(Availability::DaemonReplaced {
                        previous_version: daemon_version,
                    })
                }
                Response::Replacement {
                    disposition: super::domain::ReplacementDisposition::Deferred,
                    live_runtimes,
                    daemon_version: response_daemon_version,
                    target_version: response_target_version,
                    blockers,
                    use_current_daemon,
                } if compatible => {
                    let daemon_version = nonempty_or(response_daemon_version, daemon_version);
                    if use_current_daemon {
                        Ok(Availability::NewerDaemon { daemon_version })
                    } else {
                        Ok(Availability::ReplacementDeferred {
                            daemon_version,
                            target_version: nonempty_or(response_target_version, target_version),
                            live_runtimes,
                            blockers: if blockers.is_empty() && !version_coordination {
                                vec![super::domain::ReplacementBlocker::LegacyDaemon]
                            } else {
                                blockers
                            },
                        })
                    }
                }
                Response::Replacement { live_runtimes, .. } => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "replacement_deferred: incompatible wsxd is protecting {live_runtimes} live runtime(s)"
                    ),
                )),
                Response::Error(error) if compatible && error.code == "replacement_conflict" => {
                    Ok(Availability::ReplacementDeferred {
                        daemon_version,
                        target_version,
                        live_runtimes: status.live_runtimes,
                        blockers: vec![super::domain::ReplacementBlocker::PendingTarget],
                    })
                }
                response => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected wsxd replacement response: {response:?}"),
                )),
            }
        }
        incompatible @ ExistingDaemon::Incompatible { .. } => {
            daemon_needs_start(incompatible)?;
            unreachable!("incompatible daemon must return an error");
        }
        ExistingDaemon::Ready { .. } => Ok(Availability::LegacyCompatible),
    }
}

fn lifecycle_status(client: &Client) -> io::Result<super::domain::DaemonLifecycle> {
    match lifecycle_round_trip(client, &Request::LifecycleStatus)? {
        Response::Lifecycle(status) => Ok(status),
        response => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected wsxd lifecycle response: {response:?}"),
        )),
    }
}

fn legacy_daemon_has_other_clients(status: &super::domain::DaemonLifecycle) -> bool {
    active_client_count_has_other_tui(
        status.active_clients,
        ACTIVE_TUI_MONITORS.load(Ordering::Acquire),
    )
}

fn active_client_count_has_other_tui(active_clients: usize, own_tui_monitors: usize) -> bool {
    active_clients > 1_usize.saturating_add(own_tui_monitors)
}

fn lifecycle_version(status: &super::domain::DaemonLifecycle) -> String {
    if status.version.is_empty() {
        super::protocol::binary_identity_version(&status.binary_id)
            .unwrap_or("unknown")
            .to_string()
    } else {
        status.version.clone()
    }
}

fn nonempty_or(value: String, fallback: String) -> String {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

fn ready_without_transition(
    client: &Client,
    existing: &ExistingDaemon,
    target_binary_id: Option<&str>,
) -> io::Result<Option<Availability>> {
    let ExistingDaemon::Ready {
        lifecycle_coordination,
        ..
    } = existing
    else {
        return Ok(None);
    };
    if !lifecycle_coordination {
        return Ok(Some(Availability::LegacyCompatible));
    }
    let status = lifecycle_status(client)?;
    let daemon_version = lifecycle_version(&status);
    if super::protocol::compare_wsx_versions(&daemon_version, super::protocol::WSX_VERSION)
        == Some(std::cmp::Ordering::Greater)
    {
        return Ok(Some(Availability::NewerDaemon { daemon_version }));
    }
    if target_binary_id.is_none_or(|target| target == status.binary_id) {
        Ok(Some(
            if status.phase == super::domain::DaemonPhase::ReplacementPending {
                Availability::ReplacementDeferred {
                    daemon_version,
                    target_version: nonempty_or(
                        status.replacement_target_version,
                        super::protocol::WSX_VERSION.to_string(),
                    ),
                    live_runtimes: status.live_runtimes,
                    blockers: status.replacement_blockers,
                }
            } else if status.recovered_from_backup {
                Availability::RecoveredFromBackup
            } else {
                Availability::Current
            },
        ))
    } else {
        Ok(None)
    }
}

struct SignalMaskGuard {
    previous: libc::sigset_t,
    restored: bool,
}

impl SignalMaskGuard {
    fn block(signal: libc::c_int) -> io::Result<Self> {
        let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        if unsafe { libc::sigemptyset(&mut blocked) } == -1
            || unsafe { libc::sigaddset(&mut blocked, signal) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self {
            previous,
            restored: false,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    let mut signal_mask = SignalMaskGuard::block(libc::SIGHUP)?;
    let child_signal_mask = signal_mask.previous;
    // ^ crates/wsx-daemon/src/lib.rs owns steady-state signal policy. Block
    // SIGHUP across spawn, then detach and ignore it before the child unblocks.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            let result =
                libc::pthread_sigmask(libc::SIG_SETMASK, &child_signal_mask, std::ptr::null_mut());
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(())
        });
    }

    let child = command.spawn();
    if let Err(error) = signal_mask.restore() {
        if let Ok(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(io::Error::new(
            error.kind(),
            format!("could not restore the daemon launcher signal mask: {error}"),
        ));
    }
    child
}

fn start_daemon(
    client: &Client,
    binary: &Path,
    bootstrap: &mut BootstrapLock,
    reset_crash_budget: bool,
) -> io::Result<Availability> {
    wait_for_singleton_release(client.socket())?;
    record_start_attempt(&mut bootstrap.file, reset_crash_budget)?;
    let expected_binary_id = binary_identity(binary).ok();
    let mut command = Command::new(binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn_detached(&mut command).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not start {}: {error}", binary.display()),
        )
    })?;
    thread::Builder::new()
        .name("wsxd-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;

    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        match client.call(&Request::Snapshot) {
            Ok(Response::Snapshot(snapshot)) => {
                let status = if snapshot.capabilities.lifecycle_coordination {
                    let Response::Lifecycle(status) = client.call(&Request::LifecycleStatus)?
                    else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "started wsxd returned an invalid lifecycle response",
                        ));
                    };
                    Some(status)
                } else {
                    None
                };
                if let Some(expected) = expected_binary_id.as_deref() {
                    let status = status.as_ref().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "started wsxd does not expose lifecycle identity",
                        )
                    })?;
                    if status.binary_id != expected {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "started wsxd binary identity does not match the elected replacement",
                        ));
                    }
                }
                return Ok(
                    if status.is_some_and(|status| status.recovered_from_backup) {
                        Availability::RecoveredFromBackup
                    } else {
                        Availability::Current
                    },
                );
            }
            Ok(Response::Error(error)) => {
                return Err(io::Error::other(format!(
                    "{}: {}",
                    error.code, error.message
                )))
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected wsxd response",
                ))
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("wsxd did not become ready: {error}"),
                ))
            }
        }
    }
}

struct BootstrapLock {
    file: File,
}

fn wait_for_singleton_release(socket: &Path) -> io::Result<()> {
    let path = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "wsxd socket has no parent"))?
        .join("state.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
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
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) || Instant::now() >= deadline {
            return Err(io::Error::new(
                error.kind(),
                format!("wsxd singleton lock did not become available: {error}"),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn acquire_bootstrap_lock(socket: &Path) -> io::Result<BootstrapLock> {
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "wsxd socket has no parent"))?;
    match fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error),
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
    let path = socket.with_extension("bootstrap.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 4096
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsxd bootstrap lock",
        ));
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(BootstrapLock { file });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) || Instant::now() >= deadline {
            return Err(io::Error::new(
                error.kind(),
                format!("could not coordinate wsxd startup: {error}"),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn record_start_attempt(file: &mut File, reset: bool) -> io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    let mut attempts = if reset {
        Vec::new()
    } else {
        text.lines()
            .filter_map(|line| line.parse::<u64>().ok())
            .filter(|attempt| *attempt <= now && now - *attempt < START_WINDOW.as_secs())
            .collect::<Vec<_>>()
    };
    if attempts.len() >= MAX_START_ATTEMPTS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "crash_loop: wsxd exceeded 3 automatic starts in 60 seconds; run `wsx daemon recover` to try explicitly",
        ));
    }
    if !attempts.is_empty() {
        thread::sleep(Duration::from_millis(100_u64 << attempts.len().min(3)));
    }
    attempts.push(now);
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for attempt in attempts {
        writeln!(file, "{attempt}")?;
    }
    file.sync_all()
}

fn write_lifecycle_marker(socket: &Path, reason: &str) -> io::Result<()> {
    let path = socket.with_extension("lifecycle");
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

fn consume_planned_marker(socket: &Path, target_binary_id: Option<&str>) -> io::Result<bool> {
    let Some(reason) = lifecycle_marker_reason(socket) else {
        return Ok(false);
    };
    let planned = match reason.as_str() {
        "intentional" | "login_ended" => true,
        reason if reason.starts_with("replacement:") => {
            let expected = reason.trim_start_matches("replacement:");
            if target_binary_id != Some(expected) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "replacement_protected: the pending wsxd replacement belongs to another binary",
                ));
            }
            true
        }
        _ => false,
    };
    if planned {
        write_lifecycle_marker(socket, "starting")?;
    }
    Ok(planned)
}

fn lifecycle_marker_reason(socket: &Path) -> Option<String> {
    let path = socket.with_extension("lifecycle");
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 1024
    {
        return None;
    }
    let mut reason = String::new();
    file.take(1025).read_to_string(&mut reason).ok()?;
    Some(reason.trim().to_string())
}

fn background_recovery_allowed(socket: &Path) -> bool {
    match lifecycle_marker_reason(socket).as_deref() {
        None => !socket.with_extension("lifecycle").exists(),
        Some("ready" | "unexpected" | "starting") => true,
        Some(reason) if reason.starts_with("replacement:") => true,
        Some(_) => false,
    }
}

fn lifecycle_round_trip(client: &Client, request: &Request) -> io::Result<Response> {
    let mut stream = client.connect()?;
    match round_trip(
        &mut stream,
        &Request::Hello {
            protocol: PROTOCOL_VERSION,
        },
    )? {
        Response::Hello { .. } => round_trip(&mut stream, request),
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd lifecycle handshake failed",
        )),
    }
}

#[derive(Debug)]
enum ExistingDaemon {
    Ready {
        lifecycle_coordination: bool,
        version_coordination: bool,
    },
    Missing,
    Incompatible {
        stream: Option<UnixStream>,
        advertised_protocol: Option<u32>,
        lifecycle_coordination: bool,
        version_coordination: bool,
    },
}

fn daemon_needs_start(existing: ExistingDaemon) -> io::Result<bool> {
    match existing {
        ExistingDaemon::Ready { .. } => Ok(false),
        ExistingDaemon::Missing => Ok(true),
        // ^ [[wsx Architecture]] Binary skew must not terminate daemon-owned live PTYs.
        ExistingDaemon::Incompatible {
            advertised_protocol,
            ..
        } => Err(incompatible_daemon_error(advertised_protocol)),
    }
}

fn incompatible_daemon_error(advertised_protocol: Option<u32>) -> io::Error {
    let daemon_protocol = advertised_protocol
        .map(|protocol| protocol.to_string())
        .unwrap_or_else(|| "unknown".into());
    let reason = if advertised_protocol == Some(PROTOCOL_VERSION) {
        "missing required capabilities"
    } else {
        "protocol mismatch"
    };
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "incompatible wsxd is already running ({reason}; client protocol {PROTOCOL_VERSION}, daemon protocol {daemon_protocol}); refusing automatic shutdown to protect live sessions; use a matching wsx binary or run `wsx daemon stop` explicitly"
        ),
    )
}

fn probe_existing_daemon(client: &Client) -> io::Result<ExistingDaemon> {
    let mut stream = match client.connect() {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(ExistingDaemon::Missing)
        }
        Err(error) => return Err(error),
    };
    match round_trip(
        &mut stream,
        &Request::Hello {
            protocol: PROTOCOL_VERSION,
        },
    )? {
        Response::Hello {
            protocol,
            capabilities,
            ..
        } if protocol == PROTOCOL_VERSION
            && capabilities.resume_shell_fallback
            && capabilities.foreground_jobs =>
        {
            let lifecycle_coordination = capabilities.lifecycle_coordination;
            let version_coordination = capabilities.version_coordination;
            match round_trip(&mut stream, &Request::Snapshot)? {
                Response::Snapshot(_) => Ok(ExistingDaemon::Ready {
                    lifecycle_coordination,
                    version_coordination,
                }),
                Response::Error(error) => Err(io::Error::other(format!(
                    "{}: {}",
                    error.code, error.message
                ))),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected wsxd response",
                )),
            }
        }
        Response::Hello {
            protocol,
            capabilities,
            ..
        } => Ok(ExistingDaemon::Incompatible {
            stream: Some(stream),
            advertised_protocol: Some(protocol),
            lifecycle_coordination: capabilities.lifecycle_coordination,
            version_coordination: capabilities.version_coordination,
        }),
        Response::Error(error) if error.code == "protocol_mismatch" => {
            Ok(ExistingDaemon::Incompatible {
                stream: None,
                advertised_protocol: None,
                lifecycle_coordination: false,
                version_coordination: false,
            })
        }
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd protocol handshake failed",
        )),
    }
}

fn shutdown_incompatible_daemon(
    client: &Client,
    stream: Option<UnixStream>,
    advertised_protocol: Option<u32>,
) -> io::Result<()> {
    let mut stream = match (stream, advertised_protocol) {
        (Some(stream), Some(_)) => stream,
        (None, None) => connect_unadvertised_legacy_daemon(client)?,
        _ => {
            return Err(io::Error::other(
                "incompatible wsxd did not advertise a restartable protocol",
            ))
        }
    };
    match round_trip(&mut stream, &Request::Shutdown)? {
        Response::Ack { .. } => Ok(()),
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected wsxd shutdown response",
        )),
    }
}

fn connect_unadvertised_legacy_daemon(client: &Client) -> io::Result<UnixStream> {
    for protocol in (1..PROTOCOL_VERSION).rev() {
        let mut stream = client.connect()?;
        match round_trip(&mut stream, &Request::Hello { protocol })? {
            Response::Hello {
                protocol: accepted, ..
            } if accepted == protocol => {
                if protocol == 1 {
                    // ^ Protocol 1 closes after Hello, so Shutdown requires a fresh connection.
                    drop(stream);
                    return client.connect();
                }
                return Ok(stream);
            }
            Response::Error(error) if error.code == "protocol_mismatch" => continue,
            Response::Error(error) => {
                return Err(io::Error::other(format!(
                    "{}: {}",
                    error.code, error.message
                )))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "legacy wsxd protocol handshake failed",
                ))
            }
        }
    }
    Err(io::Error::other(
        "incompatible wsxd protocol could not be negotiated for shutdown",
    ))
}

fn daemon_is_stopped_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::UnexpectedEof
    )
}

fn wait_until_stopped(client: &Client) -> io::Result<()> {
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        match probe_existing_daemon(client) {
            Ok(ExistingDaemon::Missing) => return Ok(()),
            Ok(ExistingDaemon::Ready { .. } | ExistingDaemon::Incompatible { .. })
                if Instant::now() < deadline => {}
            Err(error) if daemon_is_stopped_error(&error) => return Ok(()),
            Err(_) if Instant::now() < deadline => {}
            Ok(ExistingDaemon::Ready { .. } | ExistingDaemon::Incompatible { .. }) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "wsxd did not stop"))
            }
            Err(error) => return Err(error),
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn daemon_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("WSX_DAEMON_BIN").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let adjacent = parent.join(format!("wsxd{}", std::env::consts::EXE_SUFFIX));
            if adjacent.is_file() {
                return adjacent;
            }
        }
    }
    PathBuf::from(format!("wsxd{}", std::env::consts::EXE_SUFFIX))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSignal {
    Dirty,
    Connected,
    Disconnected(String),
}

pub struct EventMonitor {
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl EventMonitor {
    pub fn start(client: Client) -> io::Result<(Self, mpsc::Receiver<EventSignal>)> {
        let (sender, receiver) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let tui = super::domain::TuiClientPresence {
            instance_id: new_client_id(),
            version: super::protocol::WSX_VERSION.to_string(),
            target_binary_id: binary_identity(&daemon_binary()).unwrap_or_default(),
        };
        let thread = thread::Builder::new()
            .name("wsx-runtime-events".into())
            .spawn(move || {
                let mut revision = 0;
                let mut connected = false;
                while !stop.load(Ordering::Acquire) {
                    match client.call(&Request::Poll {
                        after_revision: revision,
                        timeout_ms: 1_000,
                        tui: Some(tui.clone()),
                    }) {
                        Ok(Response::Events {
                            revision: next,
                            events,
                        }) => {
                            if !connected {
                                connected = true;
                                let _ = sender.send(EventSignal::Connected);
                            }
                            revision = next;
                            if !events.is_empty() {
                                let _ = sender.send(EventSignal::Dirty);
                            }
                        }
                        Ok(Response::Error(error)) => {
                            connected = false;
                            revision = 0;
                            let _ = sender.send(EventSignal::Disconnected(format!(
                                "{}: {}",
                                error.code, error.message
                            )));
                            thread::sleep(Duration::from_millis(250));
                        }
                        Ok(_) => {
                            connected = false;
                            revision = 0;
                            let _ = sender.send(EventSignal::Disconnected(
                                "unexpected daemon poll response".into(),
                            ));
                            thread::sleep(Duration::from_millis(250));
                        }
                        Err(error) => {
                            connected = false;
                            revision = 0;
                            let _ = sender.send(EventSignal::Disconnected(error.to_string()));
                            if daemon_is_stopped_error(&error)
                                && background_recovery_allowed(client.socket())
                            {
                                match ensure_background_available_with(&client) {
                                    Ok(_) => continue,
                                    Err(recovery_error) => {
                                        let _ = sender.send(EventSignal::Disconnected(
                                            recovery_error.to_string(),
                                        ));
                                    }
                                }
                            }
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                }
            })?;
        ACTIVE_TUI_MONITORS.fetch_add(1, Ordering::AcqRel);
        Ok((
            Self {
                stopping,
                thread: Some(thread),
            },
            receiver,
        ))
    }
}
impl Drop for EventMonitor {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        ACTIVE_TUI_MONITORS.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        Cell, Cursor, PaneId, TerminalFrame, TerminalId, TerminalSelectionRange, TerminalUpdate,
    };
    use std::{
        os::unix::{fs::PermissionsExt, net::UnixListener},
        sync::atomic::AtomicUsize,
    };

    fn test_listener(name: &str) -> (PathBuf, UnixListener) {
        let dir = std::env::current_dir().unwrap().join(".work/s");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join(format!(
            "{name}-{}-{}.sock",
            std::process::id(),
            new_client_id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (path, listener)
    }

    fn send_response(stream: &mut UnixStream, response: &Response) {
        stream.write_all(&encode_line(response).unwrap()).unwrap();
    }

    fn current_capabilities() -> super::super::domain::Capabilities {
        super::super::domain::Capabilities {
            resume_shell_fallback: true,
            foreground_jobs: true,
            lifecycle_coordination: true,
            ..Default::default()
        }
    }

    #[test]
    fn compatible_daemon_is_reused_without_shutdown() {
        let (path, listener) = test_listener("compatible-reuse");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            ));
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 1,
                    capabilities: current_capabilities(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Snapshot
            );
            send_response(
                &mut stream,
                &Response::Snapshot(super::super::domain::Snapshot {
                    protocol: PROTOCOL_VERSION,
                    epoch: 1,
                    revision: 1,
                    projects: Vec::new(),
                    worktrees: Vec::new(),
                    sessions: Vec::new(),
                    panes: Vec::new(),
                    listening_ports: Vec::new(),
                    pane_activity: Vec::new(),
                    capabilities: current_capabilities(),
                }),
            );
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        assert!(!daemon_needs_start(probe_existing_daemon(&Client::new(path)).unwrap()).unwrap());
        assert!(daemon_needs_start(ExistingDaemon::Missing).unwrap());
        server.join().unwrap();
    }

    #[test]
    fn lifecycle_ready_and_deferred_are_typed_healthy_outcomes() {
        let (path, listener) = test_listener("lifecycle-outcomes");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            for (phase, live_runtimes) in [
                (super::super::domain::DaemonPhase::Ready, 0),
                (super::super::domain::DaemonPhase::ReplacementPending, 2),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                assert!(matches!(
                    read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                    Request::Hello { .. }
                ));
                send_response(
                    &mut stream,
                    &Response::Hello {
                        protocol: PROTOCOL_VERSION,
                        epoch: 7,
                        capabilities: current_capabilities(),
                    },
                );
                assert_eq!(
                    read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                    Request::LifecycleStatus
                );
                send_response(
                    &mut stream,
                    &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                        protocol: PROTOCOL_VERSION,
                        epoch: 7,
                        binary_id: "target".into(),
                        version: "0.21.0".into(),
                        started_unix_ms: 1,
                        phase,
                        live_runtimes,
                        active_clients: 1,
                        active_tuis: 1,
                        recovered_from_backup: false,
                        replacement_target: None,
                        replacement_target_version: "0.21.0".into(),
                        replacement_blockers: vec![],
                    }),
                );
            }
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });
        let client = Client::new(path);
        let ready = ExistingDaemon::Ready {
            lifecycle_coordination: true,
            version_coordination: true,
        };
        assert_eq!(
            ready_without_transition(&client, &ready, Some("target")).unwrap(),
            Some(Availability::Current)
        );
        assert_eq!(
            ready_without_transition(&client, &ready, Some("target")).unwrap(),
            Some(Availability::ReplacementDeferred {
                daemon_version: "0.21.0".into(),
                target_version: "0.21.0".into(),
                live_runtimes: 2,
                blockers: vec![],
            })
        );
        server.join().unwrap();
    }

    #[test]
    fn legacy_client_count_excludes_the_requester_and_own_tui_monitor() {
        assert!(!active_client_count_has_other_tui(1, 0));
        assert!(!active_client_count_has_other_tui(2, 1));
        assert!(active_client_count_has_other_tui(2, 0));
        assert!(active_client_count_has_other_tui(3, 1));
    }

    #[test]
    fn newer_daemon_is_reused_without_a_downgrade_request() {
        let (path, listener) = test_listener("newer-daemon");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { .. }
            ));
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 7,
                    capabilities: current_capabilities(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::LifecycleStatus
            );
            send_response(
                &mut stream,
                &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                    protocol: PROTOCOL_VERSION,
                    epoch: 7,
                    binary_id: "0.22.0:1:2:3:20".into(),
                    version: "0.22.0".into(),
                    started_unix_ms: 1,
                    phase: super::super::domain::DaemonPhase::Ready,
                    live_runtimes: 2,
                    active_clients: 2,
                    active_tuis: 1,
                    recovered_from_backup: false,
                    replacement_target: None,
                    replacement_target_version: String::new(),
                    replacement_blockers: vec![],
                }),
            );
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });
        let ready = ExistingDaemon::Ready {
            lifecycle_coordination: true,
            version_coordination: true,
        };
        assert_eq!(
            ready_without_transition(&Client::new(path), &ready, Some("0.21.0:1:2:3:10")).unwrap(),
            Some(Availability::NewerDaemon {
                daemon_version: "0.22.0".into()
            })
        );
        server.join().unwrap();
    }

    #[test]
    fn legacy_pending_target_keeps_the_new_client_healthy() {
        let directory = std::env::current_dir().unwrap().join(".work").join(format!(
            "client-version-{}-{}",
            std::process::id(),
            new_client_id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("wsx.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server_path = path.clone();
        let server = thread::spawn(move || {
            for step in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                assert!(matches!(
                    read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                    Request::Hello { .. }
                ));
                send_response(
                    &mut stream,
                    &Response::Hello {
                        protocol: PROTOCOL_VERSION,
                        epoch: 7,
                        capabilities: current_capabilities(),
                    },
                );
                let request = read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap();
                match step {
                    0 | 2 => {
                        assert_eq!(request, Request::Snapshot);
                        send_response(
                            &mut stream,
                            &Response::Snapshot(super::super::domain::Snapshot {
                                protocol: PROTOCOL_VERSION,
                                epoch: 7,
                                revision: 1,
                                projects: vec![],
                                worktrees: vec![],
                                sessions: vec![],
                                panes: vec![],
                                listening_ports: vec![],
                                pane_activity: vec![],
                                capabilities: current_capabilities(),
                            }),
                        );
                    }
                    1 | 3 | 4 => {
                        assert_eq!(request, Request::LifecycleStatus);
                        send_response(
                            &mut stream,
                            &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                                protocol: PROTOCOL_VERSION,
                                epoch: 7,
                                binary_id: "0.20.0:1:2:3:10".into(),
                                version: String::new(),
                                started_unix_ms: 1,
                                phase: super::super::domain::DaemonPhase::ReplacementPending,
                                live_runtimes: 4,
                                active_clients: 1,
                                active_tuis: 0,
                                recovered_from_backup: false,
                                replacement_target: Some("0.20.0:1:2:3:20".into()),
                                replacement_target_version: String::new(),
                                replacement_blockers: vec![],
                            }),
                        );
                    }
                    5 => {
                        assert!(matches!(request, Request::PrepareReplacement { .. }));
                        send_response(
                            &mut stream,
                            &Response::Error(super::super::protocol::ApiError::new(
                                "replacement_conflict",
                                "another wsxd binary is already pending replacement",
                            )),
                        );
                    }
                    _ => unreachable!(),
                }
            }
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        let availability = ensure_available_with_binary(
            &Client::new(path.clone()),
            false,
            &std::env::current_exe().unwrap(),
        )
        .unwrap();
        assert_eq!(
            availability,
            Availability::ReplacementDeferred {
                daemon_version: "0.20.0".into(),
                target_version: super::super::protocol::WSX_VERSION.into(),
                live_runtimes: 4,
                blockers: vec![super::super::domain::ReplacementBlocker::PendingTarget],
            }
        );
        server.join().unwrap();
        let _ = std::fs::remove_file(path.with_extension("bootstrap.lock"));
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn bootstrap_lock_serializes_startup_owners() {
        let (path, listener) = test_listener("bootstrap-lock");
        drop(listener);
        let _ = std::fs::remove_file(&path);
        let first = acquire_bootstrap_lock(&path).unwrap();
        let contender_path = path.clone();
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let contender = thread::spawn(move || {
            let lock = acquire_bootstrap_lock(&contender_path).unwrap();
            acquired_tx.send(()).unwrap();
            drop(lock);
        });
        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        contender.join().unwrap();
        let _ = std::fs::remove_file(path.with_extension("bootstrap.lock"));
    }

    #[test]
    fn successor_waits_for_the_daemon_singleton_lock_not_only_socket_removal() {
        let (seed, listener) = test_listener("singleton-release");
        drop(listener);
        let _ = std::fs::remove_file(&seed);
        let directory = seed.with_extension("state");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("wsx.sock");
        let lock_path = directory.join("state.lock");
        let owner = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let contender_path = path.clone();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let contender = thread::spawn(move || {
            wait_for_singleton_release(&contender_path).unwrap();
            released_tx.send(()).unwrap();
        });
        assert!(released_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(owner);
        released_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        contender.join().unwrap();
        let _ = std::fs::remove_file(lock_path);
        let _ = std::fs::remove_dir(directory);
    }

    #[test]
    fn shared_start_budget_blocks_a_loop_and_explicit_recovery_resets_it() {
        let (path, listener) = test_listener("start-budget");
        drop(listener);
        let _ = std::fs::remove_file(&path);
        let mut lock = acquire_bootstrap_lock(&path).unwrap();
        for _ in 0..MAX_START_ATTEMPTS {
            record_start_attempt(&mut lock.file, false).unwrap();
        }
        assert!(record_start_attempt(&mut lock.file, false)
            .unwrap_err()
            .to_string()
            .contains("crash_loop"));
        record_start_attempt(&mut lock.file, true).unwrap();
        drop(lock);
        let _ = std::fs::remove_file(path.with_extension("bootstrap.lock"));
    }

    #[test]
    fn custom_client_never_spawns_a_default_daemon() {
        let (path, listener) = test_listener("custom-no-recovery");
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            ensure_background_available_with(&Client::new(path.clone()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!path.exists());
    }

    #[test]
    fn intentional_stop_marker_disables_background_recovery() {
        let (path, listener) = test_listener("intentional-marker");
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        let marker = path.with_extension("lifecycle");
        std::fs::write(&marker, "intentional\n").unwrap();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!background_recovery_allowed(&path));
        assert_eq!(
            ensure_background_available_with(&Client {
                socket: path.clone(),
                automatic_start: true,
            })
            .unwrap_err()
            .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(consume_planned_marker(&path, Some("target")).unwrap());
        assert_eq!(lifecycle_marker_reason(&path).as_deref(), Some("starting"));
        assert!(!consume_planned_marker(&path, Some("target")).unwrap());
        std::fs::write(&marker, "login_ended\n").unwrap();
        assert!(!background_recovery_allowed(&path));
        std::fs::write(&marker, "replacement:other\n").unwrap();
        assert!(consume_planned_marker(&path, Some("target")).is_err());
        std::fs::write(&marker, "unexpected\n").unwrap();
        assert!(background_recovery_allowed(&path));
        let _ = std::fs::remove_file(marker);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn detached_spawn_is_session_leader_and_survives_hangup() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("kill -HUP $$; exec sleep 5");

        let mut child = spawn_detached(&mut command).unwrap();
        thread::sleep(Duration::from_millis(20));
        let pid = child.id() as libc::pid_t;
        let session_id = unsafe { libc::getsid(pid) };
        let status = child.try_wait().unwrap();
        if status.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }

        assert_eq!(session_id, pid, "detached daemon must own its session");
        assert!(status.is_none(), "detached daemon exited after SIGHUP");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn local_socket_peer_matches_the_current_user() {
        let (client, server) = UnixStream::pair().unwrap();
        validate_peer_owner(&server).unwrap();
        validate_peer_owner(&client).unwrap();
    }

    #[test]
    fn same_protocol_daemon_without_required_capabilities_is_not_stopped() {
        let (path, listener) = test_listener("missing-capabilities");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            );
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 1,
                    capabilities: super::super::domain::Capabilities::default(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        let error =
            daemon_needs_start(probe_existing_daemon(&Client::new(path)).unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("missing required capabilities"));
        assert!(error.to_string().contains("refusing automatic shutdown"));
        assert!(error.to_string().contains("wsx daemon stop"));
        server.join().unwrap();
    }

    #[test]
    fn protocol_mismatch_does_not_send_shutdown() {
        let (path, listener) = test_listener("protocol-skew");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            );
            send_response(
                &mut stream,
                &Response::Error(super::super::protocol::ApiError::new(
                    "protocol_mismatch",
                    format!("client {PROTOCOL_VERSION}, daemon 8"),
                )),
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        let error =
            daemon_needs_start(probe_existing_daemon(&Client::new(path)).unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("protocol mismatch"));
        assert!(error.to_string().contains("daemon protocol unknown"));
        assert!(error.to_string().contains("refusing automatic shutdown"));
        assert!(error.to_string().contains("wsx daemon stop"));
        server.join().unwrap();
    }

    #[test]
    fn graceful_shutdown_waits_for_socket_cleanup() {
        let (path, listener) = test_listener("graceful-shutdown");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            );
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 1,
                    capabilities: super::super::domain::Capabilities::default(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Shutdown
            );
            send_response(&mut stream, &Response::Ack { revision: 1 });
            drop(stream);
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        Client::new(path).shutdown().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn graceful_shutdown_surfaces_daemon_rejection() {
        let (path, listener) = test_listener("shutdown-rejection");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            ));
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 1,
                    capabilities: super::super::domain::Capabilities::default(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Shutdown
            );
            send_response(
                &mut stream,
                &Response::Error(super::super::protocol::ApiError::new(
                    "shutdown_blocked",
                    "still busy",
                )),
            );
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        let error = Client::new(path).shutdown().unwrap_err();
        assert!(error.to_string().contains("shutdown_blocked: still busy"));
        server.join().unwrap();
    }

    #[test]
    fn graceful_shutdown_accepts_an_already_stopped_daemon() {
        let path = std::env::current_dir()
            .unwrap()
            .join(".work/s/already-stopped.sock");
        let _ = std::fs::remove_file(&path);
        Client::new(path).shutdown().unwrap();
    }

    #[test]
    fn advertised_incompatible_daemon_shuts_down_on_the_handshake_connection() {
        let (path, listener) = test_listener("advertised-upgrade");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            ));
            let legacy_hello = serde_json::json!({
                "type": "hello",
                "data": {
                    "protocol": PROTOCOL_VERSION - 1,
                    "epoch": 1,
                    "capabilities": {
                        "pane_splits": true,
                        "plugins": true,
                        "agent_reports": true,
                        "process_restore": false
                    }
                }
            });
            stream
                .write_all(&encode_line(&legacy_hello).unwrap())
                .unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Shutdown
            );
            send_response(&mut stream, &Response::Ack { revision: 1 });
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        Client::new(path).shutdown().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn unadvertised_protocol_two_daemon_shuts_down_on_the_handshake_connection() {
        let (path, listener) = test_listener("protocol-two-upgrade");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut mismatch, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut mismatch, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            );
            send_response(
                &mut mismatch,
                &Response::Error(super::super::protocol::ApiError::new(
                    "protocol_mismatch",
                    format!("client {PROTOCOL_VERSION}, daemon 2"),
                )),
            );

            for protocol in (3..PROTOCOL_VERSION).rev() {
                let (mut probe, _) = listener.accept().unwrap();
                assert_eq!(
                    read_json_line::<Request>(&mut probe, MAX_RESPONSE_BYTES).unwrap(),
                    Request::Hello { protocol }
                );
                send_response(
                    &mut probe,
                    &Response::Error(super::super::protocol::ApiError::new(
                        "protocol_mismatch",
                        format!("client {protocol}, daemon 2"),
                    )),
                );
            }

            let (mut protocol_two, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut protocol_two, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { protocol: 2 }
            );
            send_response(
                &mut protocol_two,
                &Response::Hello {
                    protocol: 2,
                    epoch: 1,
                    capabilities: super::super::domain::Capabilities::default(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut protocol_two, MAX_RESPONSE_BYTES).unwrap(),
                Request::Shutdown
            );
            send_response(&mut protocol_two, &Response::Ack { revision: 1 });
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        Client::new(path).shutdown().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn legacy_daemon_upgrade_uses_separate_request_connection() {
        let (path, listener) = test_listener("legacy-upgrade");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut mismatch, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut mismatch, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            );
            send_response(
                &mut mismatch,
                &Response::Error(super::super::protocol::ApiError::new(
                    "protocol_mismatch",
                    format!("client {PROTOCOL_VERSION}, daemon 1"),
                )),
            );

            for protocol in (2..PROTOCOL_VERSION).rev() {
                let (mut probe, _) = listener.accept().unwrap();
                assert_eq!(
                    read_json_line::<Request>(&mut probe, MAX_RESPONSE_BYTES).unwrap(),
                    Request::Hello { protocol }
                );
                send_response(
                    &mut probe,
                    &Response::Error(super::super::protocol::ApiError::new(
                        "protocol_mismatch",
                        format!("client {protocol}, daemon 1"),
                    )),
                );
            }

            let (mut hello, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut hello, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { protocol: 1 }
            );
            send_response(
                &mut hello,
                &Response::Hello {
                    protocol: 1,
                    epoch: 1,
                    capabilities: super::super::domain::Capabilities::default(),
                },
            );
            drop(hello);

            let (mut shutdown, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut shutdown, MAX_RESPONSE_BYTES).unwrap(),
                Request::Shutdown
            );
            send_response(&mut shutdown, &Response::Ack { revision: 1 });
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        Client::new(path).shutdown().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn large_json_line_uses_bounded_buffered_reads() {
        struct CountingReader {
            inner: io::Cursor<Vec<u8>>,
            reads: Arc<AtomicUsize>,
        }
        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.reads.fetch_add(1, Ordering::Relaxed);
                self.inner.read(buffer)
            }
        }

        let expected = "x".repeat(1024 * 1024);
        let mut encoded = serde_json::to_vec(&expected).unwrap();
        encoded.push(b'\n');
        let reads = Arc::new(AtomicUsize::new(0));
        let source = CountingReader {
            inner: io::Cursor::new(encoded),
            reads: Arc::clone(&reads),
        };
        let mut reader = BufReader::with_capacity(64 * 1024, source);
        let actual: String = read_buffered_json_line(&mut reader, 2 * 1024 * 1024).unwrap();

        assert_eq!(actual, expected);
        assert!(reads.load(Ordering::Relaxed) < 32);
    }

    #[test]
    fn terminal_stream_preserves_selection_updates_and_clipboard_order_after_subscribe_ack() {
        let (path, listener) = test_listener("buffered-subscribe");
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello {
                    protocol: PROTOCOL_VERSION
                }
            ));
            let mut bytes = encode_line(&Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            })
            .unwrap();
            stream.write_all(&bytes).unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::TerminalSubscribe {
                    rows: 24,
                    cols: 80,
                    ..
                }
            ));
            bytes = encode_line(&Response::Ack { revision: 1 }).unwrap();
            bytes.extend(
                encode_line(&TerminalServerMessage::Update(TerminalUpdate::Full(
                    TerminalFrame {
                        pane_id: PaneId(1),
                        terminal_id: TerminalId(2),
                        revision: 1,
                        cols: 1,
                        rows: 1,
                        cells: vec![Cell::default()],
                        cursor: Cursor {
                            x: 0,
                            y: 0,
                            visible: false,
                            blinking: false,
                            shape: 0,
                        },
                        selection: vec![TerminalSelectionRange {
                            row: 0,
                            start_col: 0,
                            end_col: 0,
                        }],
                    },
                )))
                .unwrap(),
            );
            bytes.extend(
                encode_line(&TerminalServerMessage::Update(TerminalUpdate::Patch {
                    pane_id: PaneId(1),
                    terminal_id: TerminalId(2),
                    base_revision: 1,
                    revision: 2,
                    cols: 1,
                    rows: 1,
                    changed_rows: Vec::new(),
                    cursor: Cursor {
                        x: 0,
                        y: 0,
                        visible: false,
                        blinking: false,
                        shape: 0,
                    },
                    selection: Vec::new(),
                }))
                .unwrap(),
            );
            bytes.extend(
                encode_line(&TerminalServerMessage::ClipboardWrite(b"copied".to_vec())).unwrap(),
            );
            bytes.extend(encode_line(&TerminalServerMessage::Exited).unwrap());
            stream.write_all(&bytes).unwrap();
            thread::sleep(Duration::from_millis(50));
            drop(listener);
            std::fs::remove_file(server_path).unwrap();
        });

        let stream = TerminalStream::connect(
            &Client::new(path),
            super::super::domain::PaneId(1),
            7,
            false,
            24,
            80,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut selections = Vec::new();
        let mut clipboard = None;
        loop {
            match stream.try_recv() {
                Ok(TerminalServerMessage::Update(TerminalUpdate::Full(frame))) => {
                    selections.push(frame.selection)
                }
                Ok(TerminalServerMessage::Update(TerminalUpdate::Patch { selection, .. })) => {
                    selections.push(selection)
                }
                Ok(TerminalServerMessage::ClipboardWrite(text)) => clipboard = Some(text),
                Ok(TerminalServerMessage::Exited) => break,
                Ok(message) => panic!("unexpected terminal message: {message:?}"),
                Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("terminal update missing after ACK: {error}"),
            }
        }
        assert_eq!(
            selections,
            vec![
                vec![TerminalSelectionRange {
                    row: 0,
                    start_col: 0,
                    end_col: 0,
                }],
                Vec::new(),
            ]
        );
        assert_eq!(clipboard.as_deref(), Some(b"copied".as_slice()));
        drop(stream);
        server.join().unwrap();
    }

    #[test]
    fn terminal_reader_shutdown_interrupts_full_update_queue() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let (updates, receiver) = mpsc::sync_channel(1);
        updates.send(TerminalServerMessage::Exited).unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let reader_stopping = Arc::clone(&stopping);
        let (done, finished) = mpsc::channel();
        let reader = thread::spawn(move || {
            terminal_reader(BufReader::new(client), updates, &reader_stopping);
            done.send(()).unwrap();
        });

        server
            .write_all(&encode_line(&TerminalServerMessage::Exited).unwrap())
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        stopping.store(true, Ordering::Release);

        assert!(finished.recv_timeout(Duration::from_secs(1)).is_ok());
        drop(receiver);
        reader.join().unwrap();
    }
}
