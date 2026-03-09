pub mod info;
pub mod ops;
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
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o ConnectTimeout=5");
    cmd
}

/// Spawn `cmd` in its own process group with piped stdout/stderr.
/// Kills the entire group on timeout so ssh + credential helpers are also reaped.
/// Joins reader threads after kill to prevent thread leaks.
pub fn output_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    // ! own process group so killpg doesn't hit the parent
    cmd.process_group(0);
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let child_pgid = child.id() as libc::pid_t;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut buf);
        }
        buf
    });

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_thread.join().unwrap_or_default();
                let stderr = stderr_thread.join().unwrap_or_default();
                return Ok(std::process::Output { status, stdout, stderr });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    if let Ok(Some(status)) = child.try_wait() {
                        let stdout = stdout_thread.join().unwrap_or_default();
                        let stderr = stderr_thread.join().unwrap_or_default();
                        return Ok(std::process::Output { status, stdout, stderr });
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
