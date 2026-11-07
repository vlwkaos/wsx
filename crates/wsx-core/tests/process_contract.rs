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
    assert!(!alive(pid), "process {pid} is still running");
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
