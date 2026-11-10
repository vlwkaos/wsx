//! The egress proxy of a strict agent: an HTTP CONNECT tunnel on 127.0.0.1 that names the
//! agent's own provider and nothing else. Seatbelt cannot name a remote host, so the strict
//! profile refuses every non-loopback address and the agent reaches its provider through
//! this proxy (`HTTPS_PROXY`). It tunnels bytes only (no TLS interception), answers 403 to any
//! other CONNECT target and to any plain HTTP request, and resolves the name itself: a
//! name that resolves to a loopback, private or link-local address is refused unless the
//! provider itself is on such an address (decided without a lookup on the host thread). One proxy per agent process; dropping it closes the
//! listener, every tunnel and every thread. ref: docs/tasks.md "Permissions"

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Tunnels open at once; the next connection gets 503.
const MAX_CONNECTIONS: usize = 32;
/// The request head: a CONNECT line and its headers.
const HEAD_LIMIT: usize = 8192;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A tunnel with no byte in either direction this long ends. Longer than Pi's own 5 minute
/// body timeout, so a model that thinks quietly keeps its stream.
const IDLE: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `Drop` waits for the connection threads after it shut their sockets.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// One host and port the proxy tunnels to.
pub struct Target {
    host: String,
    port: u16,
    /// The provider itself is on a loopback, private or link-local address, so its name may
    /// resolve there. Known at once for an IP literal or `localhost`; for any other name the
    /// first CONNECT decides it from its answer and it stays for the launch. The host thread
    /// never looks a name up.
    private_ok: OnceLock<bool>,
}

impl Target {
    /// `host` is a name or an IP literal without brackets.
    pub fn new(host: &str, port: u16) -> Self {
        let host = host.to_ascii_lowercase();
        let private_ok = OnceLock::new();
        match host.parse::<IpAddr>() {
            Ok(ip) => drop(private_ok.set(!public(ip))),
            Err(_) if host == "localhost" => drop(private_ok.set(true)),
            Err(_) => {}
        }
        Self {
            host,
            port,
            private_ok,
        }
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        let same_ip = || matches!((self.host.parse::<IpAddr>(), host.parse::<IpAddr>()), (Ok(a), Ok(b)) if a == b);
        self.port == port && (self.host == host || same_ip())
    }
}

/// An address on the public internet: not this machine's, a private network's, a link-local
/// one (cloud metadata is 169.254.169.254), shared address space (Tailscale), documentation,
/// multicast or reserved.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_documentation()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && octets[1] & 0xc0 == 64))
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                !(v6.is_unspecified()
                    || v6.is_loopback()
                    || v6.is_multicast()
                    || first & 0xfe00 == 0xfc00
                    || first & 0xffc0 == 0xfe80)
            }
        },
    }
}

struct Shared {
    targets: Vec<Target>,
    stop: AtomicBool,
    /// Connection threads that run.
    active: AtomicUsize,
    next: AtomicU64,
    /// The sockets of each open tunnel, so a stop can shut them.
    live: Mutex<HashMap<u64, Vec<TcpStream>>>,
}

pub struct Egress {
    port: u16,
    shared: Arc<Shared>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Egress {
    pub fn start(targets: Vec<Target>) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            targets,
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            next: AtomicU64::new(0),
            live: Mutex::new(HashMap::new()),
        });
        let worker = shared.clone();
        let accept = std::thread::Builder::new()
            .name("wsx-egress".into())
            .spawn(move || accept_loop(&listener, &worker))?;
        Ok(Self {
            port,
            shared,
            accept: Some(accept),
        })
    }

    /// The agent's `HTTPS_PROXY`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Egress {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // ^ A blocking `accept` ends on a connection of its own.
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_secs(1),
        );
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        for streams in lock(&self.shared.live).values() {
            for stream in streams {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        let deadline = Instant::now() + STOP_WAIT;
        while self.shared.active.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        if shared.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            shared.active.fetch_sub(1, Ordering::SeqCst);
            let _ = refuse(stream, "503 Service Unavailable");
            continue;
        }
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("wsx-egress-conn".into())
            .spawn(move || {
                let _ = serve(&worker, stream);
                worker.active.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            shared.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Keeps a tunnel's sockets in `Shared::live` while it runs.
struct Tracked<'a> {
    shared: &'a Shared,
    id: u64,
}

impl<'a> Tracked<'a> {
    fn new(shared: &'a Shared) -> Self {
        Self {
            shared,
            id: shared.next.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Fails once the proxy is stopping: the tunnel must not start.
    fn add(&self, stream: &TcpStream) -> io::Result<()> {
        let mut live = lock(&self.shared.live);
        if self.shared.stop.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        live.entry(self.id).or_default().push(stream.try_clone()?);
        Ok(())
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        lock(&self.shared.live).remove(&self.id);
    }
}

fn serve(shared: &Shared, mut client: TcpStream) -> io::Result<()> {
    let tracked = Tracked::new(shared);
    tracked.add(&client)?;
    client.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let (head, rest) = read_head(&mut client)?;
    let Some(target) = connect_target(&head)
        .and_then(|(host, port)| shared.targets.iter().find(|t| t.matches(&host, port)))
    else {
        return refuse(client, "403 Forbidden");
    };
    let upstream = match dial(target) {
        Ok(upstream) => upstream,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return refuse(client, "403 Forbidden")
        }
        Err(_) => return refuse(client, "502 Bad Gateway"),
    };
    tracked.add(&upstream)?;
    upstream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    splice(client, upstream, &rest)
}

fn refuse(mut client: TcpStream, status: &str) -> io::Result<()> {
    client.write_all(
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    client.shutdown(Shutdown::Both)
}

/// The request head and the bytes read past it.
fn read_head(client: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let deadline = Instant::now() + HEAD_TIMEOUT;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() >= HEAD_LIMIT {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        client.set_read_timeout(Some(left))?;
        match client.read(&mut chunk)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// `CONNECT host:port HTTP/1.x`: the host (lowercase, no brackets) and the port. A plain
/// request, another method or a malformed line is `None`.
fn connect_target(head: &str) -> Option<(String, u16)> {
    let mut parts = head.lines().next()?.split(' ');
    let (method, authority, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "CONNECT"
        || !version.starts_with("HTTP/1.")
        || parts.next().is_some()
        || authority.contains('@')
    {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => {
            let (host, rest) = inner.split_once(']')?;
            (host, rest.strip_prefix(':')?)
        }
        None => authority.rsplit_once(':')?,
    };
    Some((host.to_ascii_lowercase(), port.parse().ok()?))
}

/// Resolve the target's name and connect to the first address it may reach. A name with no
/// public address is a private provider (decided once, here); otherwise its private addresses
/// are skipped.
fn dial(target: &Target) -> io::Result<TcpStream> {
    let mut last = io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the name resolves to a loopback or private address",
    );
    let found: Vec<SocketAddr> = (target.host.as_str(), target.port)
        .to_socket_addrs()?
        .collect();
    let private_ok = *target
        .private_ok
        .get_or_init(|| !found.is_empty() && found.iter().all(|addr| !public(addr.ip())));
    for addr in found
        .into_iter()
        .filter(|addr| private_ok || public(addr.ip()))
        .take(4)
    {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Copy both ways until either side ends or the tunnel is idle, then close both sockets.
fn splice(client: TcpStream, upstream: TcpStream, rest: &[u8]) -> io::Result<()> {
    let (mut client_in, mut client_out) = (client.try_clone()?, client);
    let (mut upstream_in, mut upstream_out) = (upstream.try_clone()?, upstream);
    upstream_out.write_all(rest)?;
    client_in.set_read_timeout(Some(POLL))?;
    upstream_in.set_read_timeout(Some(POLL))?;
    let start = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let up = {
        let last = last.clone();
        std::thread::Builder::new()
            .name("wsx-egress-up".into())
            .spawn(move || pump(&mut client_in, &mut upstream_out, &last, start))?
    };
    pump(&mut upstream_in, &mut client_out, &last, start);
    let _ = up.join();
    Ok(())
}

fn pump(from: &mut TcpStream, to: &mut TcpStream, last: &AtomicU64, start: Instant) {
    let millis = |at: Instant| at.duration_since(start).as_millis() as u64;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                last.store(millis(Instant::now()), Ordering::Relaxed);
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                let idle = millis(Instant::now()).saturating_sub(last.load(Ordering::Relaxed));
                if idle > IDLE.as_millis() as u64 {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // ^ Ends the other direction's read too.
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}
