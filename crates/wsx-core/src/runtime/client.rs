use super::protocol::{
    binary_identity, binary_identity_with_version, encode_line, Request, Response,
    TerminalClientMessage, TerminalRawServerMessage, TerminalServerMessage, MAX_RESPONSE_BYTES,
    PROTOCOL_VERSION, ROUTINE_DAEMON_ARG,
};
use std::{
    borrow::Cow,
    collections::HashMap,
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
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const HANDOFF_WAIT: Duration = Duration::from_secs(35);
// A single request waits less than a monitor, so a caller's thread is not held for long.
const CALL_HANDOFF_WAIT: Duration = Duration::from_secs(5);
const START_WINDOW: Duration = Duration::from_secs(60);
// How long a wsxd may take between its process start and the start it reports.
const DAEMON_START_SLACK: Duration = Duration::from_secs(60);
const MAX_START_ATTEMPTS: usize = 3;
static ACTIVE_TUI_MONITORS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_PROTOCOLS: OnceLock<Mutex<HashMap<PathBuf, u32>>> = OnceLock::new();
const LEGACY_BASELINE_PROTOCOL: u32 = 11;

fn active_protocol(socket: &Path) -> u32 {
    ACTIVE_PROTOCOLS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(socket)
        .copied()
        .unwrap_or(PROTOCOL_VERSION)
}

fn set_active_protocol(socket: &Path, protocol: u32) {
    ACTIVE_PROTOCOLS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(socket.to_path_buf(), protocol);
}

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
    DaemonRestarted,
    ReplacementDeferred {
        daemon_version: String,
        target_version: String,
        live_runtimes: usize,
        blockers: Vec<super::domain::ReplacementBlocker>,
    },
    /// The automatic upgrade rolled back and the running daemon speaks this protocol: this
    /// client works with it. ref: docs/daemon-live-upgrade.md "A failed automatic upgrade"
    UpgradeFailed {
        reason: String,
    },
}

/// Result of `restart_daemon`. No outcome ends a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRestart {
    NotRunning,
    Restarted {
        live_runtimes: usize,
    },
    Deferred {
        live_runtimes: usize,
        blockers: Vec<super::domain::ReplacementBlocker>,
    },
    /// The running daemon cannot hand off into this build.
    Unsupported {
        daemon_version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    Missing,
    Compatible,
    Incompatible {
        daemon_protocol: Option<u32>,
        lifecycle: Option<Box<super::domain::DaemonLifecycle>>,
        lifecycle_error: Option<String>,
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

    /// Observe whether a daemon exists and can serve this client without starting or replacing it.
    pub fn status(&self) -> io::Result<DaemonStatus> {
        Ok(match probe_existing_daemon(self)? {
            ExistingDaemon::Missing => DaemonStatus::Missing,
            ExistingDaemon::Ready { .. } => DaemonStatus::Compatible,
            ExistingDaemon::Incompatible {
                mut stream,
                advertised_protocol,
                lifecycle_coordination,
                ..
            } => {
                let observation = if lifecycle_coordination
                    && advertised_protocol.is_some_and(|protocol| {
                        (LEGACY_BASELINE_PROTOCOL..PROTOCOL_VERSION).contains(&protocol)
                    }) {
                    Some(
                        match stream
                            .as_mut()
                            .ok_or_else(|| io::Error::other("legacy wsxd stream is unavailable"))
                            .and_then(|stream| round_trip(stream, &Request::LifecycleStatus))
                        {
                            Ok(Response::Lifecycle(status)) => Ok(status),
                            Ok(response) => Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("unexpected wsxd lifecycle response: {response:?}"),
                            )),
                            Err(error) => Err(error),
                        },
                    )
                } else {
                    None
                };
                let (lifecycle, lifecycle_error) = match observation {
                    Some(Ok(status)) => (Some(Box::new(status)), None),
                    Some(Err(error)) => (None, Some(error.to_string())),
                    None => (None, None),
                };
                DaemonStatus::Incompatible {
                    daemon_protocol: advertised_protocol,
                    lifecycle,
                    lifecycle_error,
                }
            }
        })
    }

    /// Read the lifecycle of a running daemon of any protocol that reports one.
    pub fn lifecycle(&self) -> io::Result<super::domain::DaemonLifecycle> {
        lifecycle_status(self)
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

    /// `wsx daemon stop --force`: a graceful stop, and for a daemon whose live handoff never
    /// finished, the end of its process.
    pub fn force_shutdown(&self) -> io::Result<()> {
        let Err(error) = self.shutdown() else {
            return Ok(());
        };
        match stuck_daemon_process(self) {
            Ok(Some(daemon)) => end_stuck_daemon(self, daemon),
            Ok(None) => Err(error),
            Err(reason) => Err(io::Error::new(
                error.kind(),
                format!("{error}; wsxd was not ended: {reason}"),
            )),
        }
    }

    pub fn call(&self, request: &Request) -> io::Result<Response> {
        let started = Instant::now();
        let result = self.call_once(request);
        // ^ docs/daemon-live-upgrade.md: a request that a handing-off daemon refused or
        // never received is sent once more to the successor.
        // A connection lost mid-request may have applied a mutation; only a read repeats.
        let repeats_lost = matches!(
            request,
            Request::Snapshot
                | Request::View { .. }
                | Request::LifecycleStatus
                | Request::AgentExchangeWait { .. }
        );
        let mut result = if not_applied(&result, repeats_lost) {
            // ^ The marker may already read `ready` when this looks: a request that certainly
            // was not applied goes once more whether or not this client saw the handoff.
            wait_for_handoff(&self.socket, CALL_HANDOFF_WAIT);
            self.call_once(&after_elapsed(request, started.elapsed()))
        } else {
            result
        };
        if let Ok(Response::Error(error)) = &mut result {
            explain_stuck_handoff(self, error);
        }
        result
    }

    fn call_once(&self, request: &Request) -> io::Result<Response> {
        self.call_at_protocol(request, active_protocol(&self.socket))
    }

    pub fn wait_agent_exchange(&self, request: &Request) -> io::Result<Response> {
        if !matches!(request, Request::AgentExchangeWait { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "exchange wait requires an agent_exchange_wait request",
            ));
        }
        self.call(request)
    }

    fn call_at_protocol(&self, request: &Request, protocol: u32) -> io::Result<Response> {
        let mut stream = self.connect()?;
        if !matches!(request, Request::Hello { .. }) {
            validate_hello(
                round_trip(&mut stream, &Request::Hello { protocol })?,
                protocol,
            )?;
        }
        stream.set_read_timeout(Some(reply_timeout(request)))?;
        round_trip(&mut stream, request)
    }

    fn connect(&self) -> io::Result<UnixStream> {
        // ^ docs/daemon-discovery.md: the error names its kind and the socket it tried, so a
        // client that looks in the wrong place says where it looked.
        self.connect_once().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{:?} at {}: {error}", error.kind(), self.socket.display()),
            )
        })
    }

    fn connect_once(&self) -> io::Result<UnixStream> {
        // ^ macOS answers EINVAL to a socket option or the peer token of a connection that a
        // closing listener dropped, as the old wsxd's does during a handoff: connect again.
        for attempt in 1.. {
            validate_socket(&self.socket)?;
            let stream = UnixStream::connect(&self.socket)?;
            let ready = stream
                .set_read_timeout(Some(IO_TIMEOUT))
                .and_then(|()| stream.set_write_timeout(Some(IO_TIMEOUT)))
                .and_then(|()| validate_peer_owner(&stream));
            match ready {
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) && attempt < 5 => {
                    thread::sleep(Duration::from_millis(20));
                }
                result => return result.map(|()| stream),
            }
        }
        unreachable!("the loop returns on its last attempt")
    }
}

/// How long the reply to `request` may take.
/// ^ docs/agent-orchestration.md "Idempotent delivery": wsxd answers a delivery only after
/// the PTY took the whole prompt, which an agent that reads slowly holds back. The exchange
/// timeout is the budget its caller already accepted. An exchange wait is answered at its
/// own timeout, which wsxd limits to 30 seconds.
fn reply_timeout(request: &Request) -> Duration {
    let wait_ms = match request {
        Request::AgentExchangeCreate { timeout_ms, .. }
        | Request::AgentExchangeCreateStashingDraft { timeout_ms, .. }
        | Request::AgentExchangeCreateKeyed { timeout_ms, .. }
        | Request::AgentExchangeContinue { timeout_ms, .. }
        | Request::AgentExchangeContinueStashingDraft { timeout_ms, .. }
        | Request::AgentExchangeContinueKeyed { timeout_ms, .. } => {
            (*timeout_ms).min(super::protocol::MAX_AGENT_EXCHANGE_TIMEOUT_MS)
        }
        Request::AgentExchangeWait { timeout_ms, .. } => (*timeout_ms).min(30_000),
        _ => 0,
    };
    IO_TIMEOUT.saturating_add(Duration::from_millis(wait_ms))
}

struct Handshake {
    epoch: u64,
}

fn validate_hello(response: Response, expected_protocol: u32) -> io::Result<Handshake> {
    match response {
        Response::Hello {
            protocol, epoch, ..
        } if protocol == expected_protocol => Ok(Handshake { epoch }),
        Response::Hello { protocol, .. } => Err(io::Error::other(format!(
            "protocol_mismatch: client {expected_protocol}, daemon {protocol}"
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

/// What the stream reader must know about a server message type.
pub trait TerminalStreamMessage: serde::de::DeserializeOwned + Send + 'static {
    fn disconnected(error: super::protocol::ApiError) -> Self;
    fn ends_stream(&self) -> bool;
}

impl TerminalStreamMessage for TerminalServerMessage {
    fn disconnected(error: super::protocol::ApiError) -> Self {
        Self::Error(error)
    }
    fn ends_stream(&self) -> bool {
        matches!(self, Self::Error(_) | Self::Exited)
    }
}

impl TerminalStreamMessage for TerminalRawServerMessage {
    fn disconnected(error: super::protocol::ApiError) -> Self {
        Self::Error(error)
    }
    fn ends_stream(&self) -> bool {
        matches!(self, Self::Error(_) | Self::Exited)
    }
}

/// The raw PTY byte stream of one pane (docs/terminal-raw-stream.md).
pub type RawTerminalStream = TerminalStream<TerminalRawServerMessage>;

pub struct TerminalStream<M = TerminalServerMessage> {
    epoch: u64,
    input: mpsc::SyncSender<TerminalClientMessage>,
    updates: mpsc::Receiver<M>,
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
        Self::subscribe(
            client,
            &Request::TerminalSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            },
        )
    }
}

impl RawTerminalStream {
    /// `rows` and `cols` both 0 keep the pane's current size.
    pub fn connect_raw(
        client: &Client,
        pane_id: super::domain::PaneId,
        client_id: u64,
        takeover: bool,
        rows: u16,
        cols: u16,
    ) -> io::Result<Self> {
        Self::subscribe(
            client,
            &Request::TerminalRawSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            },
        )
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<TerminalRawServerMessage, mpsc::RecvTimeoutError> {
        self.updates.recv_timeout(timeout)
    }
}

impl<M: TerminalStreamMessage> TerminalStream<M> {
    fn subscribe(client: &Client, request: &Request) -> io::Result<Self> {
        // ^ docs/daemon-live-upgrade.md "Terminal streams": opening a stream while wsxd hands
        // off waits for the successor once. Nothing but a lease for the same `client_id`
        // exists before the Ack, so every failure before it is safe to repeat.
        let mut outcome = Self::subscribe_once(client, request);
        let again = match &outcome {
            Ok(Err(error)) => refused_in_handoff(&error.code),
            Ok(Ok(_)) => false,
            Err(error) => not_applied_error(error, true),
        };
        if again {
            wait_for_handoff(client.socket(), CALL_HANDOFF_WAIT);
            outcome = Self::subscribe_once(client, request);
        }
        outcome?.map_err(|error| io::Error::other(format!("{}: {}", error.code, error.message)))
    }

    /// One connect, handshake and subscribe. The inner error is wsxd's refusal.
    fn subscribe_once(
        client: &Client,
        request: &Request,
    ) -> io::Result<Result<Self, super::protocol::ApiError>> {
        let mut stream = client.connect()?;
        let mut reader = BufReader::with_capacity(64 * 1024, stream.try_clone()?);
        let protocol = active_protocol(client.socket());
        let handshake = validate_hello(
            round_trip_buffered(&mut stream, &mut reader, &Request::Hello { protocol })?,
            protocol,
        )?;
        match round_trip_buffered(&mut stream, &mut reader, request)? {
            Response::Ack { .. } => {}
            Response::Error(error) => return Ok(Err(error)),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected terminal stream response",
                ))
            }
        }
        // ^ macOS answers EINVAL to setsockopt on a socket whose peer already closed: an exited
        // pane sends its last screen and ends the stream. The reader still reads what was sent.
        match stream.set_read_timeout(None) {
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
            result => result?,
        }
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
        Ok(Ok(Self {
            epoch: handshake.epoch,
            input: input_tx,
            updates: update_rx,
            resync,
            stopping,
            socket,
            writer: Some(writer),
            reader: Some(reader),
        }))
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

    /// A sender for another thread, while this stream's owner blocks on updates.
    pub fn input(&self) -> mpsc::SyncSender<TerminalClientMessage> {
        self.input.clone()
    }

    pub fn try_recv(&self) -> Result<M, mpsc::TryRecvError> {
        self.updates.try_recv()
    }
}

impl<M> Drop for TerminalStream<M> {
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

fn terminal_reader<M: TerminalStreamMessage>(
    mut reader: BufReader<UnixStream>,
    updates: mpsc::SyncSender<M>,
    stopping: &AtomicBool,
) {
    while !stopping.load(Ordering::Acquire) {
        let message: M = match read_buffered_json_line(&mut reader, MAX_RESPONSE_BYTES) {
            Ok(message) => message,
            Err(error) => {
                let _ = updates.try_send(M::disconnected(super::protocol::ApiError::new(
                    "stream_disconnected",
                    error.to_string(),
                )));
                break;
            }
        };
        let terminal = message.ends_stream();
        if !queue_terminal_update(&updates, message, stopping) || terminal {
            break;
        }
    }
    stopping.store(true, Ordering::Release);
}

fn queue_terminal_update<M>(
    updates: &mpsc::SyncSender<M>,
    mut message: M,
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

// ^ docs/daemon-live-upgrade.md "A stuck handoff": the kernel names the process that serves a
// connection. Its UID and start time come from the kernel too.
#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // ^ The kernel writes at most `length` bytes into this pid_t.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            &mut pid as *mut libc::pid_t as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// The effective UID of `pid` and its start time in Unix milliseconds.
#[cfg(target_os = "macos")]
fn process_identity(pid: libc::pid_t) -> io::Result<(libc::uid_t, u128)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // ^ proc_pidinfo writes at most `size` bytes into this proc_bsdinfo.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} is gone"),
        ));
    }
    Ok((
        info.pbi_uid,
        u128::from(info.pbi_start_tvsec) * 1000 + u128::from(info.pbi_start_tvusec) / 1000,
    ))
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // ^ The kernel writes at most `length` bytes into this ucred.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut libc::ucred as *mut libc::c_void,
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credentials.pid)
}

/// The effective UID of `pid` and its start time in Unix milliseconds: proc(5) `stat` field 22
/// counts clock ticks since `btime`.
#[cfg(target_os = "linux")]
fn process_identity(pid: libc::pid_t) -> io::Result<(libc::uid_t, u128)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "unreadable /proc entry");
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let uid = fs::metadata(format!("/proc/{pid}"))?.uid();
    // ^ The command name may hold spaces and parentheses; the fields follow its last `)`.
    let ticks = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u128>().ok())
        .ok_or_else(invalid)?;
    let boot = fs::read_to_string("/proc/stat")?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse::<u128>().ok())
        .ok_or_else(invalid)?;
    let per_second = u128::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(invalid)?;
    Ok((uid, boot * 1000 + ticks * 1000 / per_second))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_pid(_stream: &UnixStream) -> io::Result<libc::pid_t> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system does not name a socket's peer process",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_identity(_pid: libc::pid_t) -> io::Result<(libc::uid_t, u128)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system does not report a process start time",
    ))
}

/// Confirm, start or upgrade `client`'s wsxd. Only a `Client::local` may start or replace
/// one; a custom client, such as a test's, never does (docs/ui-state-ownership.md "Test isolation").
pub fn ensure_available(client: &Client) -> io::Result<Availability> {
    require_automatic_start(client)?;
    ensure_available_with(client, false)
}

pub fn recover_daemon() -> io::Result<Availability> {
    ensure_available_with(&Client::local(), true)
}

pub fn ensure_background_available(client: &Client) -> io::Result<Availability> {
    require_automatic_start(client)?;
    if !client.socket().exists() && !background_recovery_allowed(client.socket()) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "wsxd was stopped intentionally",
        ));
    }
    ensure_available_with(client, false)
}

fn require_automatic_start(client: &Client) -> io::Result<()> {
    if client.automatic_start {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic recovery is disabled for a custom wsxd client",
    ))
}

fn ensure_available_with(client: &Client, reset_crash_budget: bool) -> io::Result<Availability> {
    wait_for_handoff(client.socket(), HANDOFF_WAIT);
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
        set_active_protocol(client.socket(), PROTOCOL_VERSION);
        return Ok(availability);
    }

    let mut bootstrap = acquire_bootstrap_lock(client.socket())?;
    let existing = probe_existing_daemon(client)?;
    if let Some(availability) =
        ready_without_transition(client, &existing, target_binary_id.as_deref())?
    {
        set_active_protocol(client.socket(), PROTOCOL_VERSION);
        return Ok(availability);
    }

    match existing {
        ExistingDaemon::Missing => {
            set_active_protocol(client.socket(), PROTOCOL_VERSION);
            let planned = consume_planned_marker(
                client.socket(),
                target_binary_id.as_deref(),
                super::protocol::DAEMON_REVISION,
            )?;
            let availability = start_daemon(
                client,
                binary,
                &mut bootstrap,
                reset_crash_budget || planned != PlannedStart::None,
            )?;
            Ok(if planned == PlannedStart::Replacement {
                Availability::DaemonRestarted
            } else {
                availability
            })
        }
        ExistingDaemon::Ready {
            lifecycle_coordination: true,
            version_coordination,
            ..
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
            let compatibility_protocol = match &existing {
                ExistingDaemon::Incompatible {
                    advertised_protocol: Some(protocol),
                    ..
                } if (LEGACY_BASELINE_PROTOCOL..PROTOCOL_VERSION).contains(protocol) => {
                    Some(*protocol)
                }
                _ => None,
            };
            let live_handoff = matches!(
                &existing,
                ExistingDaemon::Ready {
                    live_handoff: true,
                    ..
                }
            ) || matches!(
                &existing,
                ExistingDaemon::Incompatible {
                    live_handoff: true,
                    advertised_protocol: Some(protocol),
                    ..
                } if (LEGACY_BASELINE_PROTOCOL..PROTOCOL_VERSION).contains(protocol)
            );
            if compatible && !version_coordination && legacy_daemon_has_other_clients(&status) {
                return Ok(Availability::ReplacementDeferred {
                    daemon_version,
                    target_version,
                    live_runtimes: status.live_runtimes,
                    blockers: vec![super::domain::ReplacementBlocker::LegacyDaemon],
                });
            }
            // ^ An upgrade from this daemon into this build already rolled back: work with the
            // running daemon, and try again only for another daemon or another build.
            if live_handoff && compatible {
                if let Some(reason) =
                    failed_upgrade(client.socket(), status.epoch, &target_binary_id)
                {
                    set_active_protocol(client.socket(), PROTOCOL_VERSION);
                    return Ok(Availability::UpgradeFailed { reason });
                }
            }
            let replacement_request = if live_handoff {
                let Some((handoff_binary_id, handoff_version)) = pending_handoff_target(
                    &status,
                    binary,
                    &daemon_version,
                    &target_binary_id,
                    &target_version,
                )?
                else {
                    return Ok(Availability::ReplacementDeferred {
                        daemon_version,
                        target_version: lifecycle_target_version(&status, &target_version),
                        live_runtimes: status.live_runtimes,
                        blockers: status.replacement_blockers,
                    });
                };
                Request::PrepareHandoff {
                    target_binary_id: handoff_binary_id,
                    target_version: handoff_version,
                    target_protocol: PROTOCOL_VERSION,
                    target_daemon_revision: super::protocol::DAEMON_REVISION,
                    executable: binary.to_path_buf(),
                    restart: false,
                }
            } else {
                Request::PrepareReplacement {
                    target_binary_id: target_binary_id.clone(),
                    target_daemon_revision: super::protocol::DAEMON_REVISION,
                }
            };
            let mut response = lifecycle_round_trip(client, &replacement_request)?;
            // ^ Protocol-15 daemons through revision 7 recompute a candidate identity with
            // their own version. Retry once with that legacy prefix; the successor still
            // validates the requested daemon revision and the source/target version bridge.
            if let Some(request) = legacy_handoff_retry_request(
                live_handoff,
                &status,
                &daemon_version,
                &response,
                binary,
            )? {
                response = lifecycle_round_trip(client, &request)?;
            }
            match response {
                Response::Replacement {
                    disposition: super::domain::ReplacementDisposition::Stopping,
                    ..
                } => {
                    set_active_protocol(client.socket(), PROTOCOL_VERSION);
                    if live_handoff {
                        match wait_until_handoff_ready(client, super::protocol::DAEMON_REVISION)? {
                            HandoffWait::Ready => {}
                            // ^ The same protocol: the running daemon serves this client, so a
                            // failed upgrade is a notice, not an error.
                            HandoffWait::RolledBack(after) if compatible => {
                                let reason = rollback_reason(&after);
                                note_failed_upgrade(
                                    client.socket(),
                                    after.epoch,
                                    &target_binary_id,
                                    &reason,
                                );
                                return Ok(Availability::UpgradeFailed { reason });
                            }
                            HandoffWait::RolledBack(after) => {
                                return Err(io::Error::other(format!(
                                    "wsxd live handoff failed and was rolled back ({}); the running daemon kept its sessions",
                                    rollback_reason(&after)
                                )));
                            }
                        }
                    } else {
                        wait_until_stopped(client)?;
                        consume_planned_marker(
                            client.socket(),
                            Some(&target_binary_id),
                            super::protocol::DAEMON_REVISION,
                        )?;
                        start_daemon(client, binary, &mut bootstrap, true)?;
                    }
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
                Response::Replacement {
                    live_runtimes,
                    daemon_version: response_daemon_version,
                    target_version: response_target_version,
                    blockers,
                    ..
                } if compatibility_protocol.is_some() => {
                    set_active_protocol(client.socket(), compatibility_protocol.unwrap());
                    Ok(Availability::ReplacementDeferred {
                        daemon_version: nonempty_or(response_daemon_version, daemon_version),
                        target_version: nonempty_or(response_target_version, target_version),
                        live_runtimes,
                        blockers,
                    })
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
                        target_version: lifecycle_target_version(&status, &target_version),
                        live_runtimes: status.live_runtimes,
                        blockers: status.replacement_blockers,
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
        ExistingDaemon::Ready { .. } => {
            set_active_protocol(client.socket(), PROTOCOL_VERSION);
            Ok(Availability::LegacyCompatible)
        }
    }
}

/// A running daemon that the next `ensure_available` replaces through a live handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpgrade {
    pub daemon_version: String,
    pub daemon_revision: u32,
    pub live_runtimes: usize,
}

/// Report a pending live upgrade without starting it. The running daemon stays usable
/// to this client, so a caller may also continue on it. See docs/daemon-live-upgrade.md.
pub fn pending_upgrade() -> io::Result<Option<PendingUpgrade>> {
    let client = Client::local();
    let target_binary_id = binary_identity(&daemon_binary()).ok();
    let existing = probe_existing_daemon(&client)?;
    let live_handoff = match &existing {
        ExistingDaemon::Ready {
            lifecycle_coordination: true,
            live_handoff,
            ..
        } => *live_handoff,
        ExistingDaemon::Incompatible {
            lifecycle_coordination: true,
            live_handoff,
            advertised_protocol: Some(protocol),
            ..
        } => *live_handoff && (LEGACY_BASELINE_PROTOCOL..PROTOCOL_VERSION).contains(protocol),
        _ => false,
    };
    if !live_handoff
        || ready_without_transition(&client, &existing, target_binary_id.as_deref())?.is_some()
    {
        return Ok(None);
    }
    let status = lifecycle_status(&client)?;
    Ok(Some(PendingUpgrade {
        daemon_version: lifecycle_version(&status),
        daemon_revision: status.daemon_revision,
        live_runtimes: status.live_runtimes,
    }))
}

/// Hand the running daemon off into the installed wsxd, also when it is the same build.
/// Sessions keep their PTYs. This never stops a daemon. See docs/daemon-live-upgrade.md.
pub fn restart_daemon() -> io::Result<DaemonRestart> {
    restart_with_binary(&Client::local(), &daemon_binary())
}

fn restart_with_binary(client: &Client, binary: &Path) -> io::Result<DaemonRestart> {
    wait_for_handoff(client.socket(), HANDOFF_WAIT);
    let _bootstrap = acquire_bootstrap_lock(client.socket())?;
    let status = match lifecycle_status(client) {
        Ok(status) => status,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(DaemonRestart::NotRunning)
        }
        Err(error) => return Err(error),
    };
    let request = Request::PrepareHandoff {
        target_binary_id: binary_identity(binary)?,
        target_version: super::protocol::WSX_VERSION.to_string(),
        target_protocol: PROTOCOL_VERSION,
        target_daemon_revision: super::protocol::DAEMON_REVISION,
        executable: binary.to_path_buf(),
        restart: true,
    };
    let unsupported = !daemon_capabilities(client)?.daemon_restart;
    match lifecycle_round_trip(client, &request) {
        Ok(Response::Replacement {
            disposition: super::domain::ReplacementDisposition::Stopping,
            live_runtimes,
            ..
        }) => {
            wait_until_restarted(client, status.epoch)?;
            set_active_protocol(client.socket(), PROTOCOL_VERSION);
            Ok(DaemonRestart::Restarted { live_runtimes })
        }
        Ok(Response::Replacement {
            live_runtimes,
            blockers,
            ..
        }) => Ok(DaemonRestart::Deferred {
            live_runtimes,
            blockers,
        }),
        // An older daemon refuses an equal build or does not know the request.
        Ok(_) | Err(_) if unsupported => Ok(DaemonRestart::Unsupported {
            daemon_version: lifecycle_version(&status),
        }),
        Ok(Response::Error(error)) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        Ok(response) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected wsxd restart response: {response:?}"),
        )),
        Err(error) => Err(error),
    }
}

// A daemon too old to answer a newer Hello with its capabilities has none of the new ones.
fn daemon_capabilities(client: &Client) -> io::Result<super::domain::Capabilities> {
    let mut stream = client.connect()?;
    Ok(
        match round_trip(
            &mut stream,
            &Request::Hello {
                protocol: PROTOCOL_VERSION,
            },
        )? {
            Response::Hello { capabilities, .. } => capabilities,
            _ => Default::default(),
        },
    )
}

// A successor has a new epoch. The old epoch back in the ready phase is a rollback.
fn wait_until_restarted(client: &Client, old_epoch: u64) -> io::Result<()> {
    let deadline = Instant::now() + HANDOFF_WAIT;
    loop {
        if let Ok(status) = lifecycle_status(client) {
            if status.phase == super::domain::DaemonPhase::Ready {
                if status.epoch != old_epoch {
                    return Ok(());
                }
                if status.replacement_target.is_none() {
                    return Err(io::Error::other(format!(
                        "wsxd could not hand off and kept running ({}); no session was stopped",
                        rollback_reason(&status)
                    )));
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "wsxd live handoff did not become ready",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn pending_handoff_target(
    status: &super::domain::DaemonLifecycle,
    binary: &Path,
    daemon_version: &str,
    target_binary_id: &str,
    target_version: &str,
) -> io::Result<Option<(String, String)>> {
    if status.phase != super::domain::DaemonPhase::ReplacementPending {
        return Ok(Some((
            target_binary_id.to_string(),
            target_version.to_string(),
        )));
    }
    if status.replacement_target.as_deref() == Some(target_binary_id) {
        return Ok(Some((
            target_binary_id.to_string(),
            target_version.to_string(),
        )));
    }
    let legacy_id = binary_identity_with_version(binary, daemon_version)?;
    if status.replacement_target.as_deref() == Some(legacy_id.as_str()) {
        return Ok(Some((legacy_id, daemon_version.to_string())));
    }
    // ^ A newer installed binary may convert an older pending cold replacement
    // into live handoff. The daemon still validates the executable and fences a
    // pending higher daemon revision; equal-version different builds stay protected.
    let pending_version = status
        .replacement_target
        .as_deref()
        .and_then(super::protocol::binary_identity_version);
    if pending_version.is_some_and(|version| {
        (status.replacement_target_version.is_empty()
            || status.replacement_target_version == version)
            && super::protocol::compare_wsx_versions(version, target_version)
                == Some(std::cmp::Ordering::Less)
    }) {
        return Ok(Some((
            target_binary_id.to_string(),
            target_version.to_string(),
        )));
    }
    Ok(None)
}

fn legacy_handoff_retry_request(
    live_handoff: bool,
    status: &super::domain::DaemonLifecycle,
    daemon_version: &str,
    response: &Response,
    binary: &Path,
) -> io::Result<Option<Request>> {
    if !live_handoff
        || status.daemon_revision >= super::protocol::DAEMON_REVISION
        || super::protocol::compare_wsx_versions(daemon_version, super::protocol::WSX_VERSION)
            != Some(std::cmp::Ordering::Less)
        || !matches!(
            response,
            Response::Error(error) if error.code == "invalid_handoff_target"
        )
    {
        return Ok(None);
    }
    Ok(Some(Request::PrepareHandoff {
        target_binary_id: binary_identity_with_version(binary, daemon_version)?,
        target_version: daemon_version.to_string(),
        target_protocol: PROTOCOL_VERSION,
        target_daemon_revision: super::protocol::DAEMON_REVISION,
        executable: binary.to_path_buf(),
        restart: false,
    }))
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

fn lifecycle_target_version(status: &super::domain::DaemonLifecycle, fallback: &str) -> String {
    if !status.replacement_target_version.is_empty() {
        return status.replacement_target_version.clone();
    }
    status
        .replacement_target
        .as_deref()
        .and_then(super::protocol::binary_identity_version)
        .unwrap_or(fallback)
        .to_string()
}

fn ready_without_transition(
    client: &Client,
    existing: &ExistingDaemon,
    target_binary_id: Option<&str>,
) -> io::Result<Option<Availability>> {
    let ExistingDaemon::Ready {
        lifecycle_coordination,
        daemon_revision_coordination,
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
    if *daemon_revision_coordination && status.daemon_revision >= super::protocol::DAEMON_REVISION {
        return Ok(Some(if status.recovered_from_backup {
            Availability::RecoveredFromBackup
        } else {
            Availability::Current
        }));
    }
    if target_binary_id.is_none_or(|target| target == status.binary_id) {
        Ok(Some(
            if status.phase == super::domain::DaemonPhase::ReplacementPending {
                Availability::ReplacementDeferred {
                    daemon_version,
                    target_version: lifecycle_target_version(&status, super::protocol::WSX_VERSION),
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
    // ^ crates/wsx-daemon/src/server.rs owns steady-state signal policy. Block
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

/// The lifecycle marker that sits beside `socket`.
pub fn lifecycle_marker_path(socket: &Path) -> PathBuf {
    socket.with_extension("lifecycle")
}

/// Replace the lifecycle marker at `path` (from [`lifecycle_marker_path`]) with `reason`.
pub fn write_lifecycle_marker(path: &Path, reason: &str) -> io::Result<()> {
    crate::fs_util::write_atomic(
        path,
        format!("{reason}\n").as_bytes(),
        crate::fs_util::AtomicWrite::replace().mode(0o600),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlannedStart {
    None,
    Intentional,
    Replacement,
}

fn consume_planned_marker(
    socket: &Path,
    target_binary_id: Option<&str>,
    target_daemon_revision: u32,
) -> io::Result<PlannedStart> {
    let Some(reason) = lifecycle_marker_reason(socket) else {
        return Ok(PlannedStart::None);
    };
    let planned = match reason.as_str() {
        "intentional" | "login_ended" => PlannedStart::Intentional,
        reason if reason.starts_with("replacement:") => {
            let expected = reason.trim_start_matches("replacement:");
            let compatible = expected
                .rsplit_once('|')
                .and_then(|(_, revision)| revision.parse::<u32>().ok())
                .is_some_and(|revision| revision > 0 && revision == target_daemon_revision)
                || target_binary_id == Some(expected);
            if !compatible {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "replacement_protected: the pending wsxd replacement belongs to another daemon revision",
                ));
            }
            PlannedStart::Replacement
        }
        _ => PlannedStart::None,
    };
    if planned != PlannedStart::None {
        write_lifecycle_marker(&lifecycle_marker_path(socket), "starting")?;
    }
    Ok(planned)
}

fn lifecycle_marker_reason(socket: &Path) -> Option<String> {
    let reason = crate::fs_util::read_private_small(&lifecycle_marker_path(socket), 1024)?;
    Some(reason.trim().to_string())
}

// ^ docs/daemon-live-upgrade.md "A failed automatic upgrade": an upgrade that rolled back
// is tried again only for another daemon epoch or another target build. The record is
// `<epoch>|<target binary id>|<reason>`, one line beside the socket.
fn failed_upgrade_path(socket: &Path) -> PathBuf {
    socket.with_extension("upgrade-failed")
}

/// The reason of the failed upgrade from this daemon epoch into this target, if one failed.
fn failed_upgrade(socket: &Path, epoch: u64, target_binary_id: &str) -> Option<String> {
    let line = crate::fs_util::read_private_small(&failed_upgrade_path(socket), 4096)?;
    let mut parts = line.trim_end().splitn(3, '|');
    let (Some(recorded_epoch), Some(recorded_target), Some(reason)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (recorded_epoch.parse::<u64>().ok()? == epoch && recorded_target == target_binary_id)
        .then(|| reason.to_string())
}

/// Record a failed upgrade. A failure to write only means the next command tries again.
fn note_failed_upgrade(socket: &Path, epoch: u64, target_binary_id: &str, reason: &str) {
    let reason = reason.replace(['\n', '\r'], " ");
    let reason: String = reason.chars().take(1000).collect();
    let path = failed_upgrade_path(socket);
    let _ = fs::remove_file(&path);
    if let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        let _ = writeln!(file, "{epoch}|{target_binary_id}|{reason}");
    }
}

/// Why the running daemon rolled back its last handoff, as wsx says it: its own record when
/// it keeps one (revision 32 and later), else a plain sentence.
fn rollback_reason(status: &super::domain::DaemonLifecycle) -> String {
    match &status.last_handoff_error {
        Some(failure) => format!("{} failed: {}", failure.step, failure.error),
        None => "the running wsxd rolled back the handoff and did not say why".into(),
    }
}

/// The failed upgrade into this build while the running daemon is still the one it failed
/// from: its reason, for a notice (docs/daemon-live-upgrade.md "A failed automatic upgrade").
/// It only reads: it never starts or upgrades wsxd.
pub fn upgrade_failure(client: &Client) -> Option<String> {
    let status = lifecycle_status(client).ok()?;
    let target = binary_identity(&daemon_binary()).ok()?;
    failed_upgrade(client.socket(), status.epoch, &target)
}

fn background_recovery_allowed(socket: &Path) -> bool {
    match lifecycle_marker_reason(socket).as_deref() {
        None => !lifecycle_marker_path(socket).exists(),
        Some("ready" | "unexpected" | "starting") => true,
        Some(reason) if reason.starts_with("replacement:") => true,
        // A source daemon that died inside a handoff left a stale marker.
        Some(reason) if reason.starts_with(super::protocol::HANDOFF_MARKER_PREFIX) => {
            !handoff_marker_fresh(reason)
        }
        Some(_) => false,
    }
}

// ^ docs/daemon-live-upgrade.md "A stuck handoff": a daemon that still refuses with
// `daemon_replacing` when no fresh handoff marker is left did not finish its handoff. A
// release before daemon revision 35 never ends that state by itself, so say what happened
// and the next step instead of "in progress". The code stays, so a retrying client still
// reconnects when the daemon serves again.
fn explain_stuck_handoff(client: &Client, error: &mut super::protocol::ApiError) {
    if !handoff_stuck(client.socket(), &error.code) {
        return;
    }
    let since = lifecycle_marker_reason(client.socket())
        .as_deref()
        .and_then(|reason| reason.strip_prefix(super::protocol::HANDOFF_MARKER_PREFIX))
        .and_then(|value| value.parse::<u128>().ok())
        .map(|started| format!(" {} s ago", unix_millis().saturating_sub(started) / 1000))
        .unwrap_or_default();
    let daemon = lifecycle_status(client).map_or_else(
        |_| "wsxd".to_string(),
        |status| format!("wsxd {}", lifecycle_version(&status)),
    );
    error.message = format!(
        "{daemon} started a live handoff{since} that did not finish. Its sessions still run \
         in it, out of reach. `wsx daemon stop --force` ends them and starts over"
    );
}

fn handoff_stuck(socket: &Path, code: &str) -> bool {
    code == "daemon_replacing"
        && !lifecycle_marker_reason(socket)
            .as_deref()
            .is_some_and(handoff_marker_fresh)
}

/// The process of a daemon that refuses with `daemon_replacing` and has no fresh handoff marker.
struct StuckDaemon {
    pid: libc::pid_t,
    started_unix_ms: u128,
}

// ^ docs/daemon-live-upgrade.md "A stuck handoff": only the kernel's peer of this socket is
// ended, and only when it runs as this user and started when the daemon says it started, so
// a reused pid or another process is never signalled.
fn stuck_daemon_process(client: &Client) -> io::Result<Option<StuckDaemon>> {
    let (pid, refused) = ask_daemon(client, &Request::Snapshot)?;
    match refused {
        Response::Error(error) if handoff_stuck(client.socket(), &error.code) => {}
        _ => return Ok(None),
    }
    let (lifecycle_pid, Response::Lifecycle(status)) =
        ask_daemon(client, &Request::LifecycleStatus)?
    else {
        return Err(io::Error::other("it did not report when it started"));
    };
    if lifecycle_pid != pid {
        return Err(io::Error::other("two processes answered on its socket"));
    }
    let daemon = StuckDaemon {
        pid,
        started_unix_ms: u128::from(status.started_unix_ms),
    };
    verify_daemon_process(&daemon)?;
    Ok(Some(daemon))
}

/// One request on a connection that speaks the daemon's protocol, and the pid that answered.
fn ask_daemon(client: &Client, request: &Request) -> io::Result<(libc::pid_t, Response)> {
    let mut stream = client.connect()?;
    let mut stream = match round_trip(
        &mut stream,
        &Request::Hello {
            protocol: PROTOCOL_VERSION,
        },
    )? {
        Response::Hello { .. } => stream,
        Response::Error(error) if error.code == "protocol_mismatch" => {
            connect_unadvertised_legacy_daemon(client)?
        }
        _ => return Err(io::Error::other("wsxd protocol handshake failed")),
    };
    let pid = peer_pid(&stream)?;
    Ok((pid, round_trip(&mut stream, request)?))
}

fn verify_daemon_process(daemon: &StuckDaemon) -> io::Result<()> {
    let (uid, started) = process_identity(daemon.pid)?;
    if uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("process {} belongs to another user", daemon.pid),
        ));
    }
    // ^ A daemon reports its start after it took its state, so the process started first.
    if started + DAEMON_START_SLACK.as_millis() < daemon.started_unix_ms
        || started > daemon.started_unix_ms + 2000
    {
        return Err(io::Error::other(format!(
            "process {} did not start with the daemon",
            daemon.pid
        )));
    }
    Ok(())
}

fn end_stuck_daemon(client: &Client, daemon: StuckDaemon) -> io::Result<()> {
    for (signal, wait) in [
        (libc::SIGTERM, Duration::from_secs(3)),
        (libc::SIGKILL, IO_TIMEOUT),
    ] {
        verify_daemon_process(&daemon)?;
        if unsafe { libc::kill(daemon.pid, signal) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        if socket_released(client.socket(), wait) {
            // ^ What a normal stop leaves: no socket, and a marker of an intentional stop.
            match fs::remove_file(client.socket()) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            return write_lifecycle_marker(&lifecycle_marker_path(client.socket()), "intentional");
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("wsxd process {} did not end", daemon.pid),
    ))
}

/// Whether no process accepts connections on `socket` within `limit`.
fn socket_released(socket: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match UnixStream::connect(socket) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return true
            }
            _ if Instant::now() >= deadline => return false,
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

fn handoff_marker_fresh(reason: &str) -> bool {
    let Some(started) = reason
        .strip_prefix(super::protocol::HANDOFF_MARKER_PREFIX)
        .and_then(|value| value.parse::<u128>().ok())
    else {
        return false;
    };
    unix_millis().saturating_sub(started) < HANDOFF_WAIT.as_millis()
}

// ^ docs/daemon-live-upgrade.md: the source daemon marks the socket gap, so an
// attached client waits for the successor instead of reporting a failure or
// starting a competing daemon. Returns whether a handoff was observed.
/// Whether `result` shows that the daemon did not apply the request: it refused it, or the
/// socket was absent. A lost connection counts only when `repeats_lost` says a repeat is safe.
fn not_applied(result: &io::Result<Response>, repeats_lost: bool) -> bool {
    match result {
        Ok(Response::Error(error)) => refused_in_handoff(&error.code),
        Ok(_) => false,
        Err(error) => not_applied_error(error, repeats_lost),
    }
}

fn not_applied_error(error: &io::Error, repeats_lost: bool) -> bool {
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => true,
        io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof => repeats_lost,
        _ => false,
    }
}

fn refused_in_handoff(code: &str) -> bool {
    matches!(code, "daemon_replacing" | "daemon_unavailable")
}

/// `request` for a repeat: an exchange wait keeps only the time it has left, so the caller's
/// deadline holds across a handoff.
fn after_elapsed(request: &Request, elapsed: Duration) -> Cow<'_, Request> {
    match request {
        Request::AgentExchangeWait {
            exchange_id,
            after_revision,
            timeout_ms,
            include_frame,
        } => Cow::Owned(Request::AgentExchangeWait {
            exchange_id: *exchange_id,
            after_revision: *after_revision,
            timeout_ms: timeout_ms
                .saturating_sub(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                .max(1),
            include_frame: *include_frame,
        }),
        _ => Cow::Borrowed(request),
    }
}

fn wait_for_handoff(socket: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    let mut observed = false;
    while Instant::now() < deadline
        && lifecycle_marker_reason(socket)
            .as_deref()
            .is_some_and(handoff_marker_fresh)
    {
        observed = true;
        thread::sleep(Duration::from_millis(20));
    }
    observed
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
        daemon_revision_coordination: bool,
        live_handoff: bool,
    },
    Missing,
    Incompatible {
        stream: Option<UnixStream>,
        advertised_protocol: Option<u32>,
        lifecycle_coordination: bool,
        version_coordination: bool,
        live_handoff: bool,
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
            && capabilities.foreground_jobs
            && capabilities.agent_exchanges =>
        {
            let lifecycle_coordination = capabilities.lifecycle_coordination;
            let version_coordination = capabilities.version_coordination;
            let daemon_revision_coordination = capabilities.daemon_revision_coordination;
            let live_handoff = capabilities.live_handoff;
            match round_trip(&mut stream, &Request::Snapshot)? {
                Response::Snapshot(_) => Ok(ExistingDaemon::Ready {
                    lifecycle_coordination,
                    version_coordination,
                    daemon_revision_coordination,
                    live_handoff,
                }),
                Response::Error(mut error) => {
                    explain_stuck_handoff(client, &mut error);
                    Err(io::Error::other(format!(
                        "{}: {}",
                        error.code, error.message
                    )))
                }
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
            live_handoff: capabilities.live_handoff,
        }),
        Response::Error(error) if error.code == "protocol_mismatch" => {
            Ok(ExistingDaemon::Incompatible {
                stream: None,
                advertised_protocol: None,
                lifecycle_coordination: false,
                version_coordination: false,
                live_handoff: false,
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

enum HandoffWait {
    Ready,
    /// The source daemon kept running; its lifecycle after the rollback.
    RolledBack(Box<super::domain::DaemonLifecycle>),
}

fn wait_until_handoff_ready(client: &Client, target_revision: u32) -> io::Result<HandoffWait> {
    let deadline = Instant::now() + HANDOFF_WAIT;
    loop {
        match probe_existing_daemon(client) {
            Ok(ExistingDaemon::Ready { .. }) => {
                if let Ok(status) = lifecycle_status(client) {
                    if status.daemon_revision >= target_revision
                        && status.phase == super::domain::DaemonPhase::Ready
                    {
                        return Ok(HandoffWait::Ready);
                    }
                    // The source daemon cleared the target and answers again: it rolled back.
                    if status.phase == super::domain::DaemonPhase::Ready
                        && status.replacement_target.is_none()
                    {
                        return Ok(HandoffWait::RolledBack(Box::new(status)));
                    }
                }
            }
            Ok(ExistingDaemon::Missing | ExistingDaemon::Incompatible { .. }) => {}
            Err(error) if daemon_is_stopped_error(&error) => {}
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "wsxd live handoff did not become ready",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

// ^ Routine startup boundary. Keep wsxd's process submode and asched-core's startup handshake aligned.
pub fn routine_daemon_command() -> Command {
    let mut command = Command::new(daemon_binary());
    command
        .arg(ROUTINE_DAEMON_ARG)
        // ^ Its routines run as the owner, also when the manager's command started it.
        .env_remove(crate::task::owner::ACTOR_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
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
    Connected(Option<Availability>),
    Disconnected(String),
}

pub struct EventMonitor {
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl EventMonitor {
    pub fn start(client: Client) -> io::Result<(Self, mpsc::Receiver<EventSignal>)> {
        Self::spawn(client, false)
    }

    /// Start a monitor for a TUI that re-executes into the installed wsx after a handoff.
    /// See docs/daemon-live-upgrade.md.
    pub fn start_following(client: Client) -> io::Result<(Self, mpsc::Receiver<EventSignal>)> {
        Self::spawn(client, true)
    }

    fn spawn(client: Client, follows: bool) -> io::Result<(Self, mpsc::Receiver<EventSignal>)> {
        let (sender, receiver) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let tui = super::domain::TuiClientPresence {
            instance_id: new_client_id(),
            version: super::protocol::WSX_VERSION.to_string(),
            target_binary_id: binary_identity(&daemon_binary()).unwrap_or_default(),
            target_daemon_revision: super::protocol::DAEMON_REVISION,
            max_protocol: PROTOCOL_VERSION,
            follows,
        };
        let thread = thread::Builder::new()
            .name("wsx-runtime-events".into())
            .spawn(move || {
                let mut revision = 0;
                let mut connected = false;
                let mut recovery_availability = None;
                let mut retried = false;
                while !stop.load(Ordering::Acquire) {
                    // The monitor must see a replaced daemon itself, so it does not use the
                    // retrying call: a reconnect is what tells the TUI to take a new snapshot.
                    match client.call_once(&Request::Poll {
                        after_revision: revision,
                        timeout_ms: 1_000,
                        tui: Some(tui.clone()),
                    }) {
                        Ok(Response::Events {
                            revision: next,
                            events,
                        }) => {
                            retried = false;
                            if !connected {
                                connected = true;
                                let _ = sender
                                    .send(EventSignal::Connected(recovery_availability.take()));
                            }
                            revision = next;
                            // ^ A PTY read publishes `terminal.changed`, yet no snapshot field
                            // derives from output (docs/ui-state-ownership.md "Runtime refresh").
                            // Refreshing on it made every TUI fetch and apply a snapshot per read.
                            if events.iter().any(|event| !is_terminal_output(event)) {
                                let _ = sender.send(EventSignal::Dirty);
                            }
                        }
                        Ok(Response::Error(mut error)) => {
                            connected = false;
                            revision = 0;
                            if wait_for_handoff(client.socket(), HANDOFF_WAIT)
                                || !std::mem::replace(&mut retried, true)
                            {
                                continue;
                            }
                            // ^ docs/daemon-live-upgrade.md "A stuck handoff": say that the
                            // handoff did not finish, and look again each second for a daemon
                            // that serves.
                            let stuck = error.code == "daemon_replacing";
                            if stuck {
                                explain_stuck_handoff(&client, &mut error);
                            }
                            let _ = sender.send(EventSignal::Disconnected(if stuck {
                                error.message
                            } else {
                                format!("{}: {}", error.code, error.message)
                            }));
                            thread::sleep(Duration::from_millis(if stuck { 1_000 } else { 250 }));
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
                            // ^ docs/daemon-live-upgrade.md "Transition marker": a live handoff
                            // is not a failure. This thread can wake after the successor wrote
                            // `ready`, so the daemon gets one more poll per outage before the
                            // owner hears of it; a daemon that died fails that one too.
                            if wait_for_handoff(client.socket(), HANDOFF_WAIT)
                                || !std::mem::replace(&mut retried, true)
                            {
                                continue;
                            }
                            let _ = sender.send(EventSignal::Disconnected(error.to_string()));
                            if daemon_is_stopped_error(&error)
                                && background_recovery_allowed(client.socket())
                            {
                                match ensure_background_available(&client) {
                                    Ok(availability) => {
                                        recovery_availability = Some(availability);
                                        continue;
                                    }
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
/// Output-only event: a PTY read in one pane. Stream and frame readers consume it; a snapshot
/// reader has nothing to refresh for it.
fn is_terminal_output(event: &super::protocol::Event) -> bool {
    matches!(event, super::protocol::Event::Changed { entity, .. } if entity == "terminal.changed")
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
#[path = "client_tests.rs"]
mod tests;
