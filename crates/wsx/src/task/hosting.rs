//! What the task host and the manager host share: their owner-only socket and the
//! report of their pane's state through the existing agent report path.
//! ref: docs/tasks.md "Host"

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use wsx_core::runtime;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// `sun_path` holds 104 bytes on macOS, including the final NUL.
const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Bind an owner-only socket, replacing a stale one. A socket that answers is another host.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        bail!(
            "socket path {} is longer than {MAX_SOCKET_PATH_BYTES} bytes",
            socket.display()
        );
    }
    wsx_core::task::store::prepare_socket_dir(socket)?;
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("another host already serves {}", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove stale {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve one JSON request per connection: hand it to the host's loop as an event and
/// write the answer back. A request that is too large or not valid JSON is dropped.
pub fn serve<Request, Response, Event>(
    listener: UnixListener,
    events: Sender<Event>,
    wrap: impl Fn(Request, Sender<Response>) -> Event,
    max_request_bytes: u64,
    reply_wait: Duration,
) where
    Request: DeserializeOwned,
    Response: Serialize,
{
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
        let mut bytes = Vec::new();
        if (&mut stream)
            .take(max_request_bytes + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > max_request_bytes
        {
            continue;
        }
        let Ok(request) = serde_json::from_slice::<Request>(&bytes) else {
            continue;
        };
        let (reply, answer) = channel();
        if events.send(wrap(request, reply)).is_err() {
            return;
        }
        if let Ok(response) = answer.recv_timeout(reply_wait) {
            if let Ok(mut line) = serde_json::to_vec(&response) {
                line.push(b'\n');
                let _ = stream.write_all(&line);
            }
        }
    }
}

/// The pane this host runs in, from wsxd's environment.
pub fn own_pane() -> Option<u64> {
    std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
}

/// Show the host's state in wsx lists through the existing agent report path.
/// Returns why wsxd did not take it, if it did not.
pub fn report_pane(
    pane_id: u64,
    provider: &str,
    state: runtime::AgentState,
    attached: bool,
) -> Option<String> {
    let request = runtime::Request::AgentReport {
        pane_id: runtime::PaneId(pane_id),
        runtime_generation: std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV).ok(),
        provider: provider.into(),
        state,
        attached,
        presence_id: None,
        conversation_id: None,
        session_ref: None,
        wake_token: None,
        capabilities: runtime::AgentCapabilities::default(),
        received: None,
        compacting: false,
        compacted: false,
    };
    match runtime::Client::new(runtime::default_socket_path()).call(&request) {
        Ok(runtime::Response::Error(error)) => {
            Some(format!("agent report refused: {}", error.message))
        }
        Ok(_) => None,
        Err(error) => Some(format!("agent report failed: {error:#}")),
    }
}

/// One line of a host's progress view: time since the host started, then the text.
pub fn progress(started: std::time::Instant, text: &str) {
    let elapsed = started.elapsed().as_secs();
    println!(
        "{:02}:{:02}:{:02} {text}",
        elapsed / 3600,
        elapsed / 60 % 60,
        elapsed % 60
    );
}

/// Free space for unprivileged writes on the volume of `path`, in MiB.
pub fn free_mb(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid C string and `stats` is written before it is read.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    // ^ `into`: the field types differ between platforms.
    #[allow(clippy::useless_conversion)]
    let (blocks, size): (u64, u64) = (stats.f_bavail.into(), stats.f_frsize.into());
    Some(blocks.saturating_mul(size) / (1024 * 1024))
}

/// Start `wsx <args>` in a session of its own, its output appended to `log`, so it outlives
/// this host and its pane (a landed task closes itself). The file the host was started as
/// runs, so a replaced binary is the one that starts. `keep` stays open in it under the same
/// descriptor number, so a lock held on it passes to the new process.
pub fn spawn_wsx(args: &[&str], log: std::fs::File, keep: Option<&std::fs::File>) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    let path = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(std::env::current_exe, Ok)
        .context("cannot find the wsx binary")?;
    let mut command = std::process::Command::new(path);
    command
        .args(args)
        .env_remove(wsx_core::task::owner::ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let keep = keep.map(|file| file.as_raw_fd());
    // SAFETY: setsid and fcntl are async-signal-safe and touch no memory of this process.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(fd) = keep {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("cannot start wsx {}", args.join(" ")))?;
    Ok(())
}

/// The wsx file a host was started as. A deploy that replaces it makes a running host
/// exec the new file before its next agent start, so the agent gets the new launch.
/// ref: docs/tasks.md "Host"
pub struct Binary {
    path: std::path::PathBuf,
    identity: String,
}

impl Binary {
    /// The path the host was started with (its argv[0], which `task new` and `say` make
    /// absolute), else the running file.
    pub fn capture() -> Option<Self> {
        let path = std::env::args_os()
            .next()
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::current_exe().ok())?;
        let identity = runtime::binary_identity(&path).ok()?;
        Some(Self { path, identity })
    }

    /// True when another file is now at the path. A missing file is not followed, and
    /// neither is one put there while a trial runs: it may be the trial's build.
    pub fn replaced(&self) -> bool {
        runtime::binary_identity(&self.path).is_ok_and(|now| now != self.identity)
            && !wsx_core::task::store::any_trial()
    }

    /// Replace this process with the file now at the path and the same arguments; the
    /// pane and its pid stay. Returns only when that file could not start.
    pub fn exec(&self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(&self.path)
            .args(std::env::args_os().skip(1))
            .exec()
    }
}
