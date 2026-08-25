//! Bounded protocol-20 client for Herdr's local newline-delimited JSON socket.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn local() -> Result<Self> {
        Self::new(super::api_socket_path()?)
    }

    pub fn new(socket_path: PathBuf) -> Result<Self> {
        validate_socket_path(&socket_path)?;
        Ok(Self { socket_path })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!(
            "wsx:{}:{}:{}",
            std::process::id(),
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            method.replace('.', "_")
        );
        request_at(&self.socket_path, &id, method, params, REQUEST_TIMEOUT)
    }
}

pub(super) fn connect(path: &Path, timeout: Duration) -> Result<UnixStream> {
    validate_socket(path)?;
    let stream = UnixStream::connect(path)
        .with_context(|| format!("could not connect to Herdr socket {}", path.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

pub(super) fn write_request(
    stream: &mut UnixStream,
    id: &str,
    method: &str,
    params: Value,
) -> Result<()> {
    super::validate_id(id, "Herdr request id")?;
    super::validate_id(method, "Herdr method")?;
    let mut bytes = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("Herdr request is too large");
    }
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub(super) fn read_line(reader: &mut BufReader<UnixStream>) -> Result<String> {
    let mut line = String::new();
    let bytes = reader
        .by_ref()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_line(&mut line)
        .context("could not read Herdr response")?;
    if bytes == 0 {
        bail!("Herdr socket closed");
    }
    if bytes > MAX_RESPONSE_BYTES || !line.ends_with('\n') {
        bail!("Herdr response exceeds the size limit or is incomplete");
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    Ok(line)
}

fn request_at(
    path: &Path,
    id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value> {
    let mut stream = connect(path, timeout)?;
    write_request(&mut stream, id, method, params)?;
    let mut reader = BufReader::new(stream);
    let line = read_line(&mut reader)?;
    let envelope: Value =
        serde_json::from_str(&line).map_err(|_| anyhow!("Herdr {method} returned invalid JSON"))?;
    result_from_envelope(&envelope, id, method)
}

fn result_from_envelope(envelope: &Value, expected_id: &str, method: &str) -> Result<Value> {
    let object = envelope
        .as_object()
        .ok_or_else(|| anyhow!("Herdr {method} response is malformed"))?;
    let response_id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Herdr {method} response has no id"))?;
    if response_id != expected_id {
        bail!("Herdr {method} response id does not match the request");
    }
    match (object.get("result"), object.get("error")) {
        (Some(_), Some(_)) => bail!("Herdr {method} response has both result and error"),
        (None, Some(error)) => {
            let error = error
                .as_object()
                .ok_or_else(|| anyhow!("Herdr {method} error is malformed"))?;
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Herdr {method} error has no code"))?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Herdr {method} error has no message"))?;
            super::validate_label(code, "Herdr error code")?;
            super::validate_label(message, "Herdr error message")?;
            bail!("Herdr {method} failed ({code}): {message}");
        }
        (Some(result), None) => Ok(result.clone()),
        (None, None) => bail!("Herdr {method} response has neither result nor error"),
    }
}

fn validate_socket_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.as_os_str().is_empty() {
        bail!("Herdr socket path must be absolute");
    }
    Ok(())
}

fn validate_socket(path: &Path) -> Result<()> {
    validate_socket_path(path)?;
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("could not inspect Herdr socket {}", path.display()))?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("Herdr socket is not owned by the current user");
    }
    if metadata.mode() & 0o077 != 0 {
        bail!("Herdr socket permissions are not owner-only");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn socket_path(name: &str) -> PathBuf {
        let dir = std::env::current_dir()
            .unwrap()
            .join(".work/tests")
            .join(format!(
                "wsx-herdr-client-{}-{}-{name}",
                std::process::id(),
                REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("herdr.sock")
    }

    #[test]
    fn request_correlates_id_and_returns_result() {
        let path = socket_path("result");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let request: Value = serde_json::from_str(&read_line(&mut reader).unwrap()).unwrap();
            let mut stream = stream;
            writeln!(
                stream,
                "{}",
                json!({"id": request["id"], "result": {"type": "pong"}})
            )
            .unwrap();
        });
        let result = request_at(&path, "test:1", "ping", json!({}), REQUEST_TIMEOUT).unwrap();
        assert_eq!(result["type"], "pong");
        server.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn request_rejects_mismatched_response_id() {
        let path = socket_path("mismatch");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            writeln!(stream, "{}", json!({"id": "other", "result": {}})).unwrap();
        });
        assert!(request_at(&path, "test:1", "ping", json!({}), REQUEST_TIMEOUT).is_err());
        server.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn response_requires_exactly_one_well_formed_result_or_error() {
        for envelope in [
            json!({"id": "test:1", "result": {}, "error": {"code": "bad", "message": "bad"}}),
            json!({"id": "test:1", "error": "bad", "result": {}}),
            json!({"id": "test:1", "error": "bad"}),
            json!({"id": "test:1"}),
        ] {
            assert!(result_from_envelope(&envelope, "test:1", "test").is_err());
        }
        assert_eq!(
            result_from_envelope(
                &json!({"id": "test:1", "result": {"type": "ok"}}),
                "test:1",
                "test"
            )
            .unwrap()["type"],
            "ok"
        );
    }

    #[test]
    fn client_rejects_relative_socket_paths() {
        assert!(Client::new(PathBuf::from("relative.sock")).is_err());
    }
}
