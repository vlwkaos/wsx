#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::{
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
pub(super) const MAX_HANDOFF_PANES: usize = 64;
#[cfg(unix)]
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

#[cfg(unix)]
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct PaneHandoff {
    pub state: wsx_terminal::TerminalHandoffState,
    #[serde(default)]
    pub runtime_generation: Option<String>,
}

#[cfg(unix)]
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    source_version: String,
    source_protocol: u32,
    expected_version: String,
    expected_protocol: u32,
    expected_daemon_revision: u32,
    pub persisted: super::Persisted,
    pub panes: Vec<PaneHandoff>,
}

#[cfg(unix)]
pub(super) struct Received {
    pub manifest: Manifest,
    pub singleton_lock: OwnedFd,
    pub pane_fds: Vec<OwnedFd>,
    pub stream: UnixStream,
}

#[cfg(unix)]
pub(super) fn manifest(
    persisted: super::Persisted,
    panes: Vec<PaneHandoff>,
    expected_version: String,
    expected_protocol: u32,
    expected_daemon_revision: u32,
) -> Manifest {
    Manifest {
        version: HANDOFF_VERSION,
        source_version: super::WSX_VERSION.into(),
        source_protocol: super::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        expected_daemon_revision,
        persisted,
        panes,
    }
}

#[cfg(unix)]
pub(super) fn socket_path(primary_socket: &Path, nonce: u64) -> io::Result<PathBuf> {
    let parent = primary_socket
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "wsxd socket has no parent"))?;
    Ok(parent.join(format!("h-{nonce:x}.sock")))
}

#[cfg(unix)]
pub(super) fn bind(path: &Path) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(unix)]
pub(super) fn spawn_import(executable: &Path, socket: &Path, token: &str) -> io::Result<Child> {
    let mut command = Command::new(executable);
    command
        .arg(super::HANDOFF_IMPORT_ARG)
        .arg(socket)
        .arg(token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("spawn handoff wsxd {}: {error}", executable.display()),
        )
    })
}

#[cfg(unix)]
pub(super) fn accept_and_send(
    listener: UnixListener,
    path: &Path,
    token: &str,
    manifest: &Manifest,
    singleton_lock: RawFd,
    pane_fds: &[RawFd],
) -> io::Result<UnixStream> {
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(value) => break value,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff successor",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    };
    stream.set_nonblocking(false)?;
    validate_peer_owner(&stream)?;
    stream.set_read_timeout(Some(HANDOFF_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDOFF_TIMEOUT))?;
    if read_line(&mut stream)?.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff token mismatch",
        ));
    }
    let encoded = serde_json::to_vec(manifest).map_err(io::Error::other)?;
    if encoded.len() > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "handoff manifest exceeds limit",
        ));
    }
    stream.write_all(&(encoded.len() as u64).to_be_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    if read_line(&mut stream)?.trim_end() != "validated" {
        return Err(io::Error::other("handoff successor rejected manifest"));
    }
    let mut fds = Vec::with_capacity(pane_fds.len() + 1);
    fds.push(singleton_lock);
    fds.extend_from_slice(pane_fds);
    send_fds(&stream, &fds)?;
    if read_line(&mut stream)?.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff successor did not restore runtimes",
        ));
    }
    let _ = std::fs::remove_file(path);
    Ok(stream)
}

#[cfg(unix)]
pub(super) fn receive(path: &Path, token: &str) -> io::Result<Received> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_nonblocking(false)?;
    validate_peer_owner(&stream)?;
    stream.set_read_timeout(Some(HANDOFF_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDOFF_TIMEOUT))?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut size = [0u8; 8];
    stream.read_exact(&mut size)?;
    let size = u64::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid handoff manifest size",
        ));
    }
    let mut encoded = vec![0u8; size];
    stream.read_exact(&mut encoded)?;
    let manifest: Manifest = serde_json::from_slice(&encoded).map_err(io::Error::other)?;
    validate_manifest(&manifest)?;
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let mut fds = receive_fds(&stream, manifest.panes.len() + 1)?;
    let singleton_lock = fds.remove(0);
    Ok(Received {
        manifest,
        singleton_lock,
        pane_fds: fds,
        stream,
    })
}

#[cfg(unix)]
fn validate_manifest(manifest: &Manifest) -> io::Result<()> {
    let current_version = manifest.expected_version == super::WSX_VERSION;
    // ^ Protocol-15 daemons through revision 7 could only authorize a successor
    // under the source version. Accept that one legacy bridge while the requested
    // daemon revision and this successor's own version remain authoritative.
    let legacy_identity_bridge = manifest.source_protocol == super::PROTOCOL_VERSION
        && manifest.expected_version == manifest.source_version
        && wsx_core::runtime::compare_wsx_versions(&manifest.source_version, super::WSX_VERSION)
            == Some(std::cmp::Ordering::Less);
    if manifest.version != HANDOFF_VERSION
        || manifest.expected_protocol != super::PROTOCOL_VERSION
        || manifest.expected_daemon_revision != super::DAEMON_REVISION
        || !(current_version || legacy_identity_bridge)
        || manifest.panes.len() > MAX_HANDOFF_PANES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff manifest is incompatible with this wsxd",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(super) fn request_publish(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"publish\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(super) fn wait_publish(stream: &mut UnixStream) -> io::Result<()> {
    expect_line(stream, "publish", HANDOFF_TIMEOUT)
}

#[cfg(unix)]
pub(super) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(super) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    expect_line(stream, "ready", HANDOFF_TIMEOUT)
}

#[cfg(unix)]
pub(super) fn commit(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"commit\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(super) fn wait_commit(stream: &mut UnixStream) -> io::Result<()> {
    expect_line(stream, "commit", HANDOFF_TIMEOUT)
}

#[cfg(unix)]
pub(super) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(super) fn wait_owned(stream: &mut UnixStream) {
    let _ = expect_line(stream, "owned", OWNED_TIMEOUT);
}

#[cfg(unix)]
fn expect_line(stream: &mut UnixStream, expected: &str, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    let value = read_line(stream)?;
    if value.trim_end() == expected {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "expected handoff {expected}, received {}",
            value.trim_end()
        )))
    }
}

#[cfg(unix)]
pub(super) fn cleanup_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(unix)]
fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    while bytes.len() <= 4096 {
        if stream.read(&mut byte)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "handoff stream closed",
            ));
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "handoff control line exceeds limit",
    ))
}

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
fn validate_peer_owner(stream: &UnixStream) -> io::Result<()> {
    let mut token = AuditToken { value: [0; 8] };
    let mut length = std::mem::size_of::<AuditToken>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            (&mut token as *mut AuditToken).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<AuditToken>()
        || unsafe { audit_token_to_euid(token) } != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff peer belongs to another user",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn validate_peer_owner(_stream: &UnixStream) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() || fds.len() > MAX_HANDOFF_PANES + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid handoff descriptor count",
        ));
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    }];
    let bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(bytes as u32) as usize }];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = iov.as_ptr() as *mut libc::iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other(
                "handoff control message allocation failed",
            ));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), libc::CMSG_DATA(header), bytes);
        if libc::sendmsg(stream.as_raw_fd(), &message, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn receive_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<OwnedFd>> {
    if expected == 0 || expected > MAX_HANDOFF_PANES + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid expected handoff descriptor count",
        ));
    }
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    }];
    let bytes = expected * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(bytes as u32) as usize }];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = iov.as_mut_ptr();
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;
    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handoff descriptor message was truncated",
        ));
    }
    let mut result = Vec::new();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff descriptor message is missing",
            ));
        }
        let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
        let count = data_len / std::mem::size_of::<RawFd>();
        let data = libc::CMSG_DATA(header).cast::<RawFd>();
        for index in 0..count {
            result.push(OwnedFd::from_raw_fd(*data.add(index)));
        }
    }
    if result.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expected {expected} handoff descriptors, got {}",
                result.len()
            ),
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_for(source_version: &str, expected_version: &str) -> Manifest {
        Manifest {
            version: HANDOFF_VERSION,
            source_version: source_version.into(),
            source_protocol: super::super::PROTOCOL_VERSION,
            expected_version: expected_version.into(),
            expected_protocol: super::super::PROTOCOL_VERSION,
            expected_daemon_revision: super::super::DAEMON_REVISION,
            persisted: super::super::Persisted::default(),
            panes: Vec::new(),
        }
    }

    #[test]
    fn successor_accepts_only_the_protocol_15_legacy_version_bridge() {
        assert!(validate_manifest(&manifest_for("0.25.0", "0.25.0")).is_ok());
        assert!(validate_manifest(&manifest_for(
            super::super::WSX_VERSION,
            super::super::WSX_VERSION,
        ))
        .is_ok());

        let mut future = manifest_for("99.0.0", "99.0.0");
        assert!(validate_manifest(&future).is_err());
        future.source_version = "0.25.0".into();
        future.source_protocol -= 1;
        assert!(validate_manifest(&future).is_err());
    }
}
