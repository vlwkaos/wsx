pub mod info;
pub mod worktree;

use crate::process::{run_bounded, Bound, Options, Stderr};
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

/// `git_cmd` for a call to a remote (fetch, ls-remote, push), and the words to add when ssh
/// refuses the keys. ref: docs/tasks.md "Push"
pub(crate) fn remote_cmd(repo: &Path) -> (Command, String) {
    let mut cmd = git_cmd(repo);
    let (socket, words) = ssh_agent();
    if let Some(socket) = socket {
        cmd.env("SSH_AUTH_SOCK", socket);
    }
    (cmd, words)
}

/// The ssh agent socket a command of wsx's should get as `SSH_AUTH_SOCK`, and the words for
/// a server that took no key: which agent ssh asked, or why there is none (no socket).
/// wsxd, its hosts and the web server keep the environment of the shell that first started
/// wsxd, such as an ssh login with no agent; the agent socket of the login session also
/// moves at each login. So the answer is `SSH_AUTH_SOCK` when it answers, else the login
/// session's agent. Every command wsx runs for a project (push, deploy, release, routines)
/// gets it here. ref: docs/tasks.md "Push"
pub fn ssh_agent() -> (Option<std::path::PathBuf>, String) {
    let answers = |path: &std::ffi::OsStr| std::os::unix::net::UnixStream::connect(path).is_ok();
    let inherited = std::env::var_os("SSH_AUTH_SOCK").filter(|path| !path.is_empty());
    if let Some(path) = inherited.as_deref().filter(|path| answers(path)) {
        let agent = format!("the ssh agent at {}", Path::new(path).display());
        return (Some(path.into()), refused_by(&agent));
    }
    // SAFETY: getuid has no failure and touches no memory.
    let uid = unsafe { libc::getuid() };
    let login = login_agent(uid);
    if let Some(path) = login.filter(|path| answers(path.as_os_str())) {
        let agent = format!("the login session's ssh agent at {}", path.display());
        return (Some(path), refused_by(&agent));
    }
    let unset = match &inherited {
        None => "SSH_AUTH_SOCK is not set".to_string(),
        Some(path) => format!(
            "SSH_AUTH_SOCK {} does not answer",
            Path::new(path).display()
        ),
    };
    let none = if cfg!(target_os = "macos") {
        format!(
            "{unset} and the login session has no agent (`launchctl print gui/{uid}/com.openssh.ssh-agent`)"
        )
    } else {
        unset
    };
    (
        None,
        format!("ssh had no agent: {none}; log in to the Mac's desktop, or start wsxd from a shell whose ssh agent holds the key"),
    )
}

/// The environment variable that lets `clone` take a `file://` URL: for the scenarios, which
/// clone a local bare repository. ref: docs/tasks.md "Approvals"
pub const ALLOW_FILE_CLONE_ENV: &str = "WSX_TEST_ALLOW_FILE_CLONE";

fn file_clone_allowed() -> bool {
    std::env::var_os(ALLOW_FILE_CLONE_ENV).is_some()
}

/// A clone URL wsx accepts: `https://`, `ssh://` or scp-style `user@host:path`, and
/// `file://` only under `ALLOW_FILE_CLONE_ENV`. Nothing that git would read as an option
/// or as a transport helper (`ext::`).
pub fn check_clone_url(url: &str) -> anyhow::Result<()> {
    let bad = |why: &str| anyhow::anyhow!("clone URL '{url}' {why}");
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad("is empty or has a space"));
    }
    if url.starts_with('-') {
        return Err(bad("starts with '-'"));
    }
    let file = file_clone_allowed() && url.starts_with("file://");
    let scp = !url.contains("://")
        && url.split_once(':').is_some_and(|(left, right)| {
            !right.is_empty()
                && left.split_once('@').is_some_and(|(user, host)| {
                    !user.is_empty()
                        && !host.is_empty()
                        && !host.starts_with('-')
                        && !host.contains('/')
                })
        });
    if url.starts_with("https://") || url.starts_with("ssh://") || scp || file {
        return Ok(());
    }
    Err(bad("is not ssh or https"))
}

/// `git clone <url> <dest>` with the login session's ssh agent (as push has it). `dest` is
/// not there before; a failed or timed out clone leaves nothing of it.
/// ref: docs/tasks.md "Approvals"
pub fn clone(url: &str, dest: &Path) -> anyhow::Result<()> {
    check_clone_url(url)?;
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent folder", dest.display()))?;
    std::fs::create_dir_all(parent)?;
    let (mut cmd, no_key) = remote_cmd(parent);
    let allowed = if file_clone_allowed() {
        "https:ssh:file"
    } else {
        "https:ssh"
    };
    cmd.env("GIT_ALLOW_PROTOCOL", allowed)
        .args(["clone", "--", url])
        .arg(dest);
    let ran = output_with_timeout_tail(&mut cmd, CLONE_TIMEOUT, 4096);
    let failed = match &ran {
        Ok(output) if output.status.success() => return Ok(()),
        Ok(output) => {
            let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if said.contains("Permission denied (publickey") {
                format!("{said}\n{no_key}")
            } else {
                said
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            format!("timed out after {} s", CLONE_TIMEOUT.as_secs())
        }
        Err(error) => error.to_string(),
    };
    let _ = std::fs::remove_dir_all(dest);
    anyhow::bail!("git clone of {url} failed: {failed}")
}

const CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn refused_by(agent: &str) -> String {
    format!("ssh offered the keys of {agent} and the server took none: add the key to that agent in your shell (`ssh-add`, with `--apple-use-keychain` for a key in the keychain)")
}

/// The socket of the login session's ssh agent: on macOS launchd's `com.openssh.ssh-agent`
/// of the user's GUI domain, which a process outside that session (an ssh login) can ask.
fn login_agent(uid: libc::uid_t) -> Option<std::path::PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let target = format!("gui/{uid}/com.openssh.ssh-agent");
    let output = output_with_timeout(
        Command::new("/bin/launchctl")
            .args(["print", &target])
            .stdin(std::process::Stdio::null()),
        std::time::Duration::from_secs(5),
    )
    .ok()
    .filter(|output| output.status.success())?;
    // ^ `inherited environment = { SSH_AUTH_SOCK => /var/run/com.apple.launchd.<id>/Listeners }`
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("SSH_AUTH_SOCK => "))
        .map(|path| path.trim().into())
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
///
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
/// After the process ends, waits at most two seconds for its output streams to close.
pub(crate) fn output_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    output_with_bound(cmd, timeout, Bound::Unlimited)
}

/// Timeout-bounded subprocess output with a per-stream byte limit.
pub fn output_with_timeout_limit(
    cmd: &mut Command,
    timeout: std::time::Duration,
    max_stream_bytes: usize,
) -> std::io::Result<std::process::Output> {
    output_with_bound(cmd, timeout, Bound::Limit(max_stream_bytes))
}

/// Timeout-bounded subprocess output that keeps the last `tail_bytes` of each stream. The
/// process writes as much as it likes and runs to its end: a check's verdict is its exit.
pub fn output_with_timeout_tail(
    cmd: &mut Command,
    timeout: std::time::Duration,
    tail_bytes: usize,
) -> std::io::Result<std::process::Output> {
    output_with_bound(cmd, timeout, Bound::Tail(tail_bytes))
}

fn output_with_bound(
    cmd: &mut Command,
    timeout: std::time::Duration,
    bound: Bound,
) -> std::io::Result<std::process::Output> {
    let run = run_bounded(
        cmd,
        &Options {
            timeout,
            stdout: bound,
            stderr: Stderr::Own(bound),
            reap: false,
        },
    )?;
    if run.timed_out {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "git command timed out",
        ));
    }
    Ok(std::process::Output {
        status: run.status,
        stdout: run.stdout,
        stderr: run.stderr,
    })
}
