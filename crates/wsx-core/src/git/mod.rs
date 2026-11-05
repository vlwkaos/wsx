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

/// A lock older than this, with no process holding it open, is left by a git that died.
const STALE_LOCK_AGE: std::time::Duration = std::time::Duration::from_secs(300);
/// How long a refused run waits for a young lock to go, and how often it looks.
const LOCK_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(200);

/// A stale `index.lock` that `run_clearing_stale_lock` removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCleared {
    pub path: std::path::PathBuf,
    pub age_s: u64,
}

/// Run `git -C repo args`. When git refuses with "index.lock: File exists", the repository's
/// own `index.lock` is looked at again and again for up to `LOCK_SETTLE`:
/// - gone, or young (a live git, such as a status refresh of a watcher, holds it for a moment):
///   wait and run again;
/// - over 5 minutes old by mtime and no process has it open (`lsof -t`; a missing or failing
///   lsof is not "no holder"): remove it, once, and run again at once. Removing the file is
///   itself a file event, so a watcher may take a new lock at that moment: the run after it
///   waits like any other (a land of pizzahouse #34 failed so on 2026-11-05);
/// - old and held, or its holder unknown: refuse.
/// A lock that stays refuses, and the output's stderr names it, its age and its holder. No
/// other lock file is touched. ref: docs/tasks.md "Landing"
pub fn run_clearing_stale_lock(
    repo: &Path,
    args: &[&str],
) -> std::io::Result<(std::process::Output, Option<LockCleared>)> {
    let started = std::time::Instant::now();
    let mut cleared = None;
    loop {
        let mut output = git_cmd(repo).args(args).output()?;
        let refused = String::from_utf8_lossy(&output.stderr);
        if output.status.success()
            || !(refused.contains("index.lock") && refused.contains("File exists"))
        {
            return Ok((output, cleared));
        }
        let Some(lock) = index_lock(repo) else {
            return Ok((output, cleared));
        };
        let settling = started.elapsed() < LOCK_SETTLE;
        let Ok(modified) = std::fs::metadata(&lock).and_then(|meta| meta.modified()) else {
            // ^ Gone between git's refusal and now: its holder ended.
            if settling {
                std::thread::sleep(LOCK_RETRY);
                continue;
            }
            return Ok((output, cleared));
        };
        let age = modified.elapsed().unwrap_or_default();
        let holder = lock_holder(&lock);
        let stale = age > STALE_LOCK_AGE;
        if stale && holder == Holder::None && cleared.is_none() {
            std::fs::remove_file(&lock)?;
            cleared = Some(LockCleared {
                path: lock,
                age_s: age.as_secs(),
            });
            continue;
        }
        if settling && !(stale && holder != Holder::None) {
            std::thread::sleep(LOCK_RETRY);
            continue;
        }
        let why = match holder {
            Holder::Pid(pid) => format!("held open by pid {pid}"),
            Holder::Unknown => "its holder could not be checked".into(),
            Holder::None if stale => "taken again after wsx removed a stale one".into(),
            Holder::None => format!("younger than {} s", STALE_LOCK_AGE.as_secs()),
        };
        output.stderr.extend_from_slice(
            format!(
                "\nwsx: {} is {} s old, {why}; wsx removes a lock only when it is older than {} s and no process holds it\n",
                lock.display(),
                age.as_secs(),
                STALE_LOCK_AGE.as_secs()
            )
            .as_bytes(),
        );
        return Ok((output, cleared));
    }
}

/// The `index.lock` of the git dir `repo` uses (a linked worktree's own).
fn index_lock(repo: &Path) -> Option<std::path::PathBuf> {
    let output = git_cmd(repo)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !text.trim().is_empty()).then(|| text.trim().into())
}

#[derive(PartialEq)]
enum Holder {
    None,
    Pid(u32),
    Unknown,
}

/// `lsof -t` exits 0 with the pids of the holders and 1 with no output when there is none.
fn lock_holder(path: &Path) -> Holder {
    let Ok(output) = Command::new("lsof").arg("-t").arg(path).output() else {
        return Holder::Unknown;
    };
    let pids = String::from_utf8_lossy(&output.stdout);
    match (output.status.code(), pids.split_whitespace().next()) {
        (Some(0), Some(pid)) => pid.parse().map_or(Holder::Unknown, Holder::Pid),
        (Some(1), None) if output.stderr.is_empty() => Holder::None,
        _ => Holder::Unknown,
    }
}

/// Spawn `cmd` in its own process group with piped stdout/stderr.
/// Kills the entire group on timeout so ssh + credential helpers are also reaped.
/// After the process ends, waits at most `STREAM_GRACE` for its output streams to close.
pub(crate) fn output_with_timeout(
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
