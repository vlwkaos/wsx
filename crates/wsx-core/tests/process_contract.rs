//! `process::run_bounded` and `process::terminate_group` with real processes: the output a
//! timeout leaves, the bounds, merged stderr, and the group that dies with its leader.
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use wsx_core::process::{run_bounded, terminate_group, Bound, Options, Stderr};

fn options(timeout: Duration, stdout: Bound, stderr: Stderr, reap: bool) -> Options {
    Options {
        timeout,
        stdout,
        stderr,
        reap,
    }
}

fn pid_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wsx-process-{}-{name}", std::process::id()))
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait for `pid` to end, then fail if it did not within 5 s.
fn assert_ends(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if alive(pid) {
        // ^ Which process and in what state (a zombie, stopped, still the forked test binary).
        let ps = Command::new("ps")
            .args(["-o", "pid,ppid,stat,command", "-p", &pid.to_string()])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        panic!("process {pid} is still running:\n{ps}");
    }
}

fn pid_of(file: &Path) -> i32 {
    let pid = std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = std::fs::remove_file(file);
    pid
}

/// A shell that starts a background `sleep`, writes its pid to `file` and then runs `rest`.
fn with_child(file: &Path, rest: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!(
            "sleep 300 & echo $! > '{}'; {rest}",
            file.display()
        ))
        .stdin(Stdio::null());
    command
}

#[test]
fn a_timeout_keeps_the_output_read_and_stops_the_group() {
    let file = pid_file("timeout");
    let mut command = with_child(&file, "echo partial; echo warned >&2; wait");
    let ran = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(1),
            Bound::Unlimited,
            Stderr::Own(Bound::Unlimited),
            false,
        ),
    )
    .unwrap();
    assert!(ran.timed_out);
    assert_eq!(ran.stdout, b"partial\n");
    assert_eq!(ran.stderr, b"warned\n");
    assert_ends(pid_of(&file));
}

#[test]
fn merged_stderr_keeps_the_order_of_the_writes() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "echo one; echo two >&2; echo three"]);
    let ran = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(10),
            Bound::Unlimited,
            Stderr::Merge,
            false,
        ),
    )
    .unwrap();
    assert!(!ran.timed_out && ran.status.success());
    assert_eq!(ran.stdout, b"one\ntwo\nthree\n");
    assert!(ran.stderr.is_empty());
}

#[test]
fn a_tail_keeps_the_last_bytes_and_counts_the_rest() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "yes | head -c 100000"]);
    let ran = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(10),
            Bound::Tail(1000),
            Stderr::Null,
            false,
        ),
    )
    .unwrap();
    assert_eq!(ran.stdout.len(), 1000);
    assert_eq!(ran.stdout_cut, 99_000);
    assert_eq!(ran.stderr_cut, 0);
}

#[test]
fn more_output_than_the_limit_is_an_error() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "yes | head -c 100000"]);
    let error = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(10),
            Bound::Limit(100),
            Stderr::Null,
            false,
        ),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn reap_stops_what_the_command_left_holding_the_pipe() {
    let file = pid_file("reap");
    let mut command = with_child(&file, "echo done");
    let ran = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(10),
            Bound::Unlimited,
            Stderr::Merge,
            true,
        ),
    )
    .unwrap();
    assert!(!ran.timed_out && ran.status.success());
    assert_eq!(ran.stdout, b"done\n");
    assert_ends(pid_of(&file));
}

#[test]
fn without_reap_the_leftover_runs_on_and_the_output_read_is_the_result() {
    let file = pid_file("no-reap");
    let mut command = with_child(&file, "echo done");
    let ran = run_bounded(
        &mut command,
        &options(
            Duration::from_secs(10),
            Bound::Unlimited,
            Stderr::Merge,
            false,
        ),
    )
    .unwrap();
    assert_eq!(ran.stdout, b"done\n");
    let pid = pid_of(&file);
    assert!(alive(pid));
    unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_ends(pid);
}

fn group_leader(script: &str) -> std::process::Child {
    Command::new("/bin/sh")
        .args(["-c", script])
        .process_group(0)
        .spawn()
        .unwrap()
}

fn group_gone(pgid: i32) -> bool {
    let signalled = unsafe { libc::kill(-pgid, 0) };
    signalled != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// A killed process that is not this test's child is reaped by init a moment later.
fn assert_group_ends(pgid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !group_gone(pgid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(group_gone(pgid), "group {pgid} is still running");
}

#[test]
fn terminate_group_gives_the_grace_then_kills_what_ignores_sigterm() {
    let mut child = group_leader("trap '' TERM; while :; do sleep 1; done");
    let pgid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    terminate_group(pgid, Duration::from_millis(400));
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(group_gone(pgid));
    let _ = child.wait(); // reaped by terminate_group already
}

#[test]
fn terminate_group_returns_at_once_when_the_group_ends_on_sigterm() {
    let mut child = group_leader("sleep 300");
    let pgid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    terminate_group(pgid, Duration::from_secs(30));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(group_gone(pgid));
    let _ = child.wait(); // reaped by terminate_group already
}

#[test]
fn terminate_group_with_no_grace_kills_and_refuses_the_callers_group() {
    let mut child = group_leader("trap '' TERM; sleep 300");
    let pgid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(200));
    terminate_group(pgid, Duration::ZERO);
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert_group_ends(pgid);
    // Neither init's group nor this test's own gets a signal.
    terminate_group(1, Duration::ZERO);
    terminate_group(unsafe { libc::getpgrp() }, Duration::ZERO);
}

/// `stop_naming` (`wsx task reap`): a process naming the folder in its arguments or environment
/// ends with its children, in either macOS form of the path, also after the folder is gone; a
/// sibling folder that shares the name's start, or a longer path that contains it, does not.
/// ref: docs/tasks.md "Scenarios"
#[cfg(target_os = "macos")]
#[test]
fn reap_stops_what_names_the_folder_and_spares_its_siblings() {
    use wsx_core::process::stop_naming;
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("reap-names-{}", std::process::id()));
    let root = base.join("tk12");
    let sibling = base.join("tk123");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    // ^ The `/var/...` form a process may hold when the canonical form is `/private/var/...`.
    let short = |path: &Path| {
        PathBuf::from(
            path.to_str()
                .unwrap()
                .strip_prefix("/private")
                .unwrap_or(path.to_str().unwrap()),
        )
    };
    // ^ `sleep 60; :` keeps the shell from exec-ing `sleep`, so the shell is the process
    // that names the path and `sleep` is its child.
    let spawn = |script: String, env: Option<(&str, String)>| {
        let mut command = Command::new("sh");
        command
            .args(["-c", &script])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some((key, value)) = env {
            command.env(key, value);
        }
        command.spawn().unwrap()
    };
    let mut semicolon = spawn(format!("cd {}; sleep 60; :", short(&root).display()), None);
    // ^ macOS hides the environment of Apple-signed binaries (`/bin/sh`, `/bin/sleep`) from
    // other processes, so this test binary is the process that holds the folder in its
    // environment: it runs only the `sleeper` below.
    let mut in_env = Command::new(std::env::current_exe().unwrap())
        .args(["sleeper", "--exact", "--ignored", "--nocapture"])
        .env("REAP_SLEEPER", "1")
        .env("WSX_TEST_WORK", format!("{}/out", root.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let spared = [
        spawn(format!("sleep 60; : {}", sibling.display()), None),
        spawn(format!("sleep 60; : /zz{}", root.display()), None),
        spawn(format!("sleep 60; : {}.log", root.display()), None),
    ];
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_dir_all(&base).unwrap();

    let stopped = stop_naming(&root, Duration::from_secs(5)).unwrap();
    // ^ The shell, its `sleep` child and the sleeper.
    assert_eq!(stopped, 3, "stopped {stopped}");
    assert_ends(semicolon.id() as i32);
    assert_ends(in_env.id() as i32);
    // ^ `stop_naming` may have collected them already: either way no zombie stays.
    let _ = semicolon.wait();
    let _ = in_env.wait();
    for mut child in spared {
        assert!(alive(child.id() as i32), "a sibling path was stopped");
        // ^ The shell leads its own group: the group's end takes its `sleep` too.
        terminate_group(child.id() as i32, Duration::ZERO);
        child.wait().unwrap();
    }
}

/// The process of the reap test that names the folder only in its environment.
#[test]
#[ignore = "run by reap_stops_what_names_the_folder_and_spares_its_siblings"]
fn sleeper() {
    if std::env::var_os("REAP_SLEEPER").is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}
