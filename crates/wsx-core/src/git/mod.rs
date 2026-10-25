pub mod info;
pub mod worktree;

use std::path::Path;
use std::process::Command;

/// Base git command scoped to `repo` via `-C`.
/// Stdin null + env vars prevent any interactive prompt from opening /dev/tty:
///   GIT_TERMINAL_PROMPT=0  — disables git's own credential prompts
///   GIT_SSH_COMMAND        — BatchMode=yes + ConnectTimeout=5 so SSH fails fast
pub fn git_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .stdin(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=5",
        );
    cmd
}

/// Spawn `cmd` in its own process group with piped stdout/stderr.
/// Kills the entire group on timeout so ssh + credential helpers are also reaped.
/// After the process ends, waits at most `STREAM_GRACE` for its output streams to close.
pub fn output_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    output_with_timeout_inner(cmd, timeout, Bound::None)
}

/// Timeout-bounded subprocess output with a per-stream byte limit.
pub fn output_with_timeout_limit(
    cmd: &mut Command,
    timeout: std::time::Duration,
    max_stream_bytes: usize,
) -> std::io::Result<std::process::Output> {
    output_with_timeout_inner(cmd, timeout, Bound::Limit(max_stream_bytes))
}

/// Timeout-bounded subprocess output that keeps the last `tail_bytes` of each stream. The
/// process writes as much as it likes and runs to its end: a check's verdict is its exit.
pub fn output_with_timeout_tail(
    cmd: &mut Command,
    timeout: std::time::Duration,
    tail_bytes: usize,
) -> std::io::Result<std::process::Output> {
    output_with_timeout_inner(cmd, timeout, Bound::Tail(tail_bytes))
}

#[derive(Clone, Copy)]
enum Bound {
    None,
    /// More than this many bytes is an error, and the pipe closes.
    Limit(usize),
    /// Read everything; keep the last this many bytes.
    Tail(usize),
}

/// ^ Once the process has ended, its output streams get this long to close. A process that
/// left the group (a daemon a test run started) can hold them open for hours; then the output
/// read so far is the result, and the reader thread ends with that process.
const STREAM_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn output_with_timeout_inner(
    cmd: &mut Command,
    timeout: std::time::Duration,
    bound: Bound,
) -> std::io::Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;

    // ! own process group so killpg doesn't hit the parent
    cmd.process_group(0);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let child_pgid = child.id() as libc::pid_t;

    let stdout = Stream::read(child.stdout.take(), bound);
    let stderr = Stream::read(child.stderr.take(), bound);

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            // Kill the entire process group — git + ssh + credential helpers
            unsafe { libc::killpg(child_pgid, libc::SIGKILL) };
            let _ = child.wait();
            let deadline = Instant::now() + STREAM_GRACE;
            let _ = stdout.finish(deadline);
            let _ = stderr.finish(deadline);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "git command timed out",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let deadline = Instant::now() + STREAM_GRACE;
    let stdout = stdout.finish(deadline);
    let stderr = stderr.finish(deadline);
    Ok(std::process::Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// One output stream, read in its own thread into a buffer the caller can take at any time.
struct Stream {
    bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<std::io::Result<()>>,
    bound: Bound,
}

impl Stream {
    fn read<R: std::io::Read + Send + 'static>(reader: Option<R>, bound: Bound) -> Self {
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (sent, done) = std::sync::mpsc::channel();
        match reader {
            Some(reader) => {
                let shared = bytes.clone();
                std::thread::spawn(move || {
                    let _ = sent.send(read_bounded(reader, bound, &shared));
                });
            }
            None => {
                let _ = sent.send(Ok(()));
            }
        }
        Self { bytes, done, bound }
    }

    /// The bytes read by `deadline`, or the read's error.
    fn finish(self, deadline: std::time::Instant) -> std::io::Result<Vec<u8>> {
        use std::sync::mpsc::RecvTimeoutError;
        match self
            .done
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(result) => result?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("subprocess output reader panicked"))
            }
        }
        let mut bytes = std::mem::take(
            &mut *self
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if let Bound::Tail(keep) = self.bound {
            bytes.drain(..bytes.len().saturating_sub(keep));
        }
        Ok(bytes)
    }
}

fn read_bounded<R: std::io::Read>(
    mut reader: R,
    bound: Bound,
    bytes: &std::sync::Mutex<Vec<u8>>,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut bytes = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        bytes.extend_from_slice(&chunk[..read]);
        match bound {
            Bound::Limit(limit) if bytes.len() > limit => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "subprocess output exceeded byte limit",
                ));
            }
            Bound::Tail(keep) if bytes.len() > 2 * keep => {
                let excess = bytes.len() - keep;
                bytes.drain(..excess);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::read_bounded;
    use std::io::Cursor;

    #[test]
    fn bounded_reader_rejects_oversized_output() {
        let bytes = std::sync::Mutex::new(Vec::new());
        let error = read_bounded(Cursor::new(b"four"), super::Bound::Limit(3), &bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
