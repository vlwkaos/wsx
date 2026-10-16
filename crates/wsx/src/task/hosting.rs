//! What the task host and the manager host share: their owner-only socket and the
//! report of their pane's state through the existing agent report path.
//! ref: docs/tasks.md "Host"

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
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
