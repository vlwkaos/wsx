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
/// Joins reader threads after kill to prevent thread leaks.
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

fn output_with_timeout_inner(
    cmd: &mut Command,
    timeout: std::time::Duration,
    bound: Bound,
) -> std::io::Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    // ! own process group so killpg doesn't hit the parent
    cmd.process_group(0);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let child_pgid = child.id() as libc::pid_t;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_thread = std::thread::spawn(move || read_bounded(stdout, bound));
    let stderr_thread = std::thread::spawn(move || read_bounded(stderr, bound));

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_reader(stdout_thread);
                let stderr = join_reader(stderr_thread);
                return Ok(std::process::Output {
                    status,
                    stdout: stdout?,
                    stderr: stderr?,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    if let Ok(Some(status)) = child.try_wait() {
                        let stdout = join_reader(stdout_thread);
                        let stderr = join_reader(stderr_thread);
                        return Ok(std::process::Output {
                            status,
                            stdout: stdout?,
                            stderr: stderr?,
                        });
                    }
                    // Kill the entire process group — git + ssh + credential helpers
                    unsafe { libc::killpg(child_pgid, libc::SIGKILL) };
                    let _ = child.wait();
                    // Pipes are now closed; readers unblock and finish
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "git command timed out",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(e),
        }
    }
}

fn read_bounded<R: std::io::Read>(reader: Option<R>, bound: Bound) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    match bound {
        Bound::Limit(limit) => {
            reader
                .take(limit.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "subprocess output exceeded byte limit",
                ));
            }
        }
        Bound::Tail(keep) => {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > 2 * keep {
                    bytes.drain(..bytes.len() - keep);
                }
            }
            bytes.drain(..bytes.len().saturating_sub(keep));
        }
        Bound::None => {
            let mut reader = reader;
            reader.read_to_end(&mut bytes)?;
        }
    }
    Ok(bytes)
}

fn join_reader(
    thread: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    thread
        .join()
        .map_err(|_| std::io::Error::other("subprocess output reader panicked"))?
}

#[cfg(test)]
mod tests {
    use super::read_bounded;
    use std::io::Cursor;

    #[test]
    fn bounded_reader_rejects_oversized_output() {
        let error = read_bounded(Some(Cursor::new(b"four")), super::Bound::Limit(3)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
