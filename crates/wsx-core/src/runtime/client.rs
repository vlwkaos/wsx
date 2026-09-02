use super::protocol::{
    encode_line, Request, Response, TerminalClientMessage, TerminalServerMessage,
    MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Client {
    socket: PathBuf,
}
impl Client {
    pub fn local() -> Self {
        Self {
            socket: super::protocol::default_socket_path(),
        }
    }
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Gracefully stop the current daemon and wait for its socket cleanup.
    pub fn shutdown(&self) -> io::Result<()> {
        match probe_existing_daemon(self)? {
            ExistingDaemon::Missing => Ok(()),
            ExistingDaemon::Ready => match self.call(&Request::Shutdown) {
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

pub fn ensure_available() -> io::Result<()> {
    let client = Client::local();
    if !daemon_needs_start(probe_existing_daemon(&client)?)? {
        return Ok(());
    }

    let binary = daemon_binary();
    let mut child = Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
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
            Ok(Response::Snapshot(_)) => return Ok(()),
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

#[derive(Debug)]
enum ExistingDaemon {
    Ready,
    Missing,
    Incompatible {
        stream: Option<UnixStream>,
        advertised_protocol: Option<u32>,
    },
}

fn daemon_needs_start(existing: ExistingDaemon) -> io::Result<bool> {
    match existing {
        ExistingDaemon::Ready => Ok(false),
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
            match round_trip(&mut stream, &Request::Snapshot)? {
                Response::Snapshot(_) => Ok(ExistingDaemon::Ready),
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
        Response::Hello { protocol, .. } => Ok(ExistingDaemon::Incompatible {
            stream: Some(stream),
            advertised_protocol: Some(protocol),
        }),
        Response::Error(error) if error.code == "protocol_mismatch" => {
            Ok(ExistingDaemon::Incompatible {
                stream: None,
                advertised_protocol: None,
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
            Ok(ExistingDaemon::Ready | ExistingDaemon::Incompatible { .. })
                if Instant::now() < deadline => {}
            Err(error) if daemon_is_stopped_error(&error) => return Ok(()),
            Err(_) if Instant::now() < deadline => {}
            Ok(ExistingDaemon::Ready | ExistingDaemon::Incompatible { .. }) => {
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
        let thread = thread::Builder::new()
            .name("wsx-runtime-events".into())
            .spawn(move || {
                let mut revision = 0;
                let mut connected = false;
                while !stop.load(Ordering::Acquire) {
                    match client.call(&Request::Poll {
                        after_revision: revision,
                        timeout_ms: 1_000,
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
                            thread::sleep(Duration::from_millis(250));
                        }
                    }
                }
            })?;
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
