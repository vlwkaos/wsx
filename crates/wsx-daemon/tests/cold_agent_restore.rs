use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use wsx_core::runtime::{Client, Request};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TIMEOUT: Duration = Duration::from_secs(5);

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

struct DaemonGuard {
    root: PathBuf,
    socket: PathBuf,
    child: Option<Child>,
}

impl DaemonGuard {
    fn stop(&mut self) -> io::Result<ExitStatus> {
        self.wait_for_socket()?;
        Client::new(&self.socket).shutdown()?;
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            let child = self.child.as_mut().expect("daemon child is missing");
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "wsxd did not exit"));
            }
            thread::sleep(POLL_INTERVAL);
        };
        self.child.take();
        Ok(status)
    }

    fn wait_for_socket(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Client::new(&self.socket).call(&Request::Snapshot).is_ok() {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("daemon child is missing")
                .try_wait()?
            {
                return Err(io::Error::other(format!(
                    "wsxd exited before its socket became ready: {status}; stderr: {}",
                    child_stderr(self)
                )));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "wsxd socket did not become ready",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // A failed assertion must not leave an isolated daemon behind.
            let _ = Client::new(&self.socket).shutdown();
            let deadline = Instant::now() + TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
                    Ok(None) | Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cold_recovery_resumes_codex_with_the_authoritative_session_id() {
    let (log, mut daemon) = start_cold_recovery(None);

    wait_for_invocation(&log, &mut daemon);
    let status = daemon.stop().expect("gracefully shut down wsxd");
    assert!(status.success(), "wsxd exited unsuccessfully: {status}");
    assert_eq!(
        read_invocations(&log),
        vec![vec!["resume".to_owned(), "codex-session-123".to_owned()]]
    );
}

#[test]
fn cold_recovery_releases_duplicate_after_provider_spawn_failure() {
    let (log, mut daemon) = start_cold_recovery_with_seed(None, seed_duplicate_state);

    wait_for_invocation(&log, &mut daemon);
    let status = daemon.stop().expect("gracefully shut down wsxd");
    assert!(status.success(), "wsxd exited unsuccessfully: {status}");
    assert_eq!(
        read_invocations(&log),
        vec![vec!["resume".to_owned(), "codex-session-123".to_owned()]]
    );
}

#[test]
fn cold_recovery_deduplicates_duplicate_valid_cwds() {
    let (log, mut daemon) = start_cold_recovery_with_seed(None, seed_duplicate_valid_state);

    wait_for_invocation(&log, &mut daemon);
    let status = daemon.stop().expect("gracefully shut down wsxd");
    assert!(status.success(), "wsxd exited unsuccessfully: {status}");
    assert_eq!(
        read_invocations(&log),
        vec![vec!["resume".to_owned(), "codex-session-123".to_owned()]]
    );
}

#[test]
fn cold_recovery_uses_generic_recipe_when_resume_on_restore_is_disabled() {
    let (log, mut daemon) = start_cold_recovery(Some(b"resume_agents_on_restore = false\n"));

    wait_for_invocation(&log, &mut daemon);
    let status = daemon.stop().expect("gracefully shut down wsxd");
    assert!(status.success(), "wsxd exited unsuccessfully: {status}");
    assert_eq!(read_invocations(&log), vec![Vec::<String>::new()]);
}

#[test]
fn cold_recovery_fails_closed_to_generic_recipe_for_malformed_config() {
    let (log, mut daemon) = start_cold_recovery(Some(b"resume_agents_on_restore =\n"));

    wait_for_invocation(&log, &mut daemon);
    let status = daemon.stop().expect("gracefully shut down wsxd");
    assert!(status.success(), "wsxd exited unsuccessfully: {status}");
    assert_eq!(read_invocations(&log), vec![Vec::<String>::new()]);
}

fn start_cold_recovery(config: Option<&[u8]>) -> (PathBuf, DaemonGuard) {
    start_cold_recovery_with_seed(config, seed_state)
}

fn start_cold_recovery_with_seed(
    config: Option<&[u8]>,
    seed: fn(&Path, &Path),
) -> (PathBuf, DaemonGuard) {
    let root = test_root();
    private_dir(&root);
    let home = root.join("home");
    let state_home = root.join("state");
    let config_home = root.join("config");
    let cache_home = root.join("cache");
    let worktree = root.join("worktree");
    for directory in [&home, &state_home, &config_home, &cache_home, &worktree] {
        private_dir(directory);
    }

    if let Some(config) = config {
        let wsx_config = effective_config_dir(&home, &config_home);
        private_dir(&wsx_config);
        private_file(&wsx_config.join("config.toml"), config, 0o600);
    }

    let log = root.join("codex-invocations.jsonl");
    let worktree_bin = worktree.join("bin");
    private_dir(&worktree_bin);
    write_fake_codex(&worktree_bin.join("codex"));

    let wsx_state = state_home.join("wsx");
    private_dir(&wsx_state);
    let socket = wsx_state.join("wsx.sock");
    seed(&wsx_state.join("state.json"), &worktree);

    let inherited_path = env::var_os("PATH").unwrap_or_default();
    let mut child_paths = vec![PathBuf::from("bin")];
    child_paths.extend(
        env::split_paths(&inherited_path)
            .filter(|directory| fs::symlink_metadata(directory.join("codex")).is_err()),
    );
    let child_path = env::join_paths(child_paths).expect("PATH components must be valid");

    let child = Command::new(env!("CARGO_BIN_EXE_wsxd"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache_home)
        .env("WSX_SOCKET", &socket)
        .env("WSX_TEST_CODEX_MARKER", &log)
        .env("PATH", child_path)
        .spawn()
        .expect("start wsxd");
    let daemon = DaemonGuard {
        root,
        socket,
        child: Some(child),
    };

    (log, daemon)
}

#[cfg(target_os = "macos")]
fn effective_config_dir(home: &Path, _config_home: &Path) -> PathBuf {
    home.join("Library").join("Application Support").join("wsx")
}

#[cfg(not(target_os = "macos"))]
fn effective_config_dir(_home: &Path, config_home: &Path) -> PathBuf {
    config_home.join("wsx")
}

fn test_root() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("wsx-daemon must be in the workspace")
        .to_path_buf();
    workspace.join(".work/ar").join(format!(
        "{}-{}",
        std::process::id(),
        NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn private_file(path: &Path, bytes: &[u8], mode: u32) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap_or_else(|error| panic!("create {}: {error}", path.display()))
        .write_all(bytes)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn write_fake_codex(path: &Path) {
    private_file(
        path,
        br##"#!/usr/bin/env python3
import json
import os
import sys

log = os.environ["WSX_TEST_CODEX_MARKER"]
with open(log, "a", encoding="utf-8") as output:
    output.write(json.dumps(sys.argv[1:]) + "\n")
    output.flush()
"##,
        0o700,
    );
}

fn seed_state(path: &Path, worktree: &Path) {
    let cwd = worktree.display().to_string();
    let state = serde_json::json!({
        "next_id": 7,
        "projects": [{
            "id": 1,
            "path": cwd,
            "name": "restore-project",
            "revision": 1,
            "last_agent_active_unix_ms": null,
            "last_terminal_active_unix_ms": null
        }],
        "worktrees": [{
            "id": 2,
            "project_id": 1,
            "path": cwd,
            "branch": "restore",
            "revision": 1
        }],
        "sessions": [{
            "id": 3,
            "worktree_id": 2,
            "label": "restore-session",
            "primary_pane": 4,
            "focused_pane": 4,
            "panes": [4],
            "layout": { "kind": "leaf", "pane_id": 4 },
            "revision": 1
        }],
        "panes": [{
            "id": 4,
            "terminal_id": 5,
            "session_id": 3,
            "label": "codex",
            "agent": {
                "id": 6,
                "provider": "codex",
                "state": "unknown",
                "conversation_id": null,
                "session_ref": { "kind": "id", "value": "codex-session-123" },
                "capabilities": { "prompt": false, "resume": true, "lifecycle": false },
                "source": "adapter"
            },
            "exited": false,
            "revision": 1,
            "recovery": {
                "command": ["/bin/sh"],
                "initial_input": "codex",
                "rows": 41,
                "cols": 113
            },
            "recovery_quarantined": false
        }]
    });
    let bytes = serde_json::to_vec_pretty(&state).expect("serialize persisted state");
    private_file(path, &bytes, 0o600);
}

fn seed_duplicate_state(path: &Path, worktree: &Path) {
    let first = worktree.join("first");
    let first_bin = first.join("bin");
    private_dir(&first);
    private_dir(&first_bin);
    symlink("codex", first_bin.join("codex"))
        .unwrap_or_else(|error| panic!("create {}: {error}", first_bin.join("codex").display()));
    seed_duplicate_state_with_cwd(path, worktree, &first);
}

fn seed_duplicate_valid_state(path: &Path, worktree: &Path) {
    seed_duplicate_state_with_cwd(path, worktree, worktree);
}

fn seed_duplicate_state_with_cwd(path: &Path, worktree: &Path, first_cwd: &Path) {
    let valid_cwd = worktree.display().to_string();
    let first_cwd = first_cwd.display().to_string();
    let state = serde_json::json!({
        "next_id": 13,
        "projects": [{
            "id": 1,
            "path": valid_cwd.clone(),
            "name": "restore-project",
            "revision": 1
        }],
        "worktrees": [
            {"id": 2, "project_id": 1, "path": first_cwd, "branch": "first", "revision": 1},
            {"id": 8, "project_id": 1, "path": valid_cwd, "branch": "valid", "revision": 1}
        ],
        "sessions": [
            {"id": 3, "worktree_id": 2, "label": "first", "primary_pane": 4, "focused_pane": 4, "panes": [4], "layout": {"kind": "leaf", "pane_id": 4}, "revision": 1},
            {"id": 9, "worktree_id": 8, "label": "second", "primary_pane": 10, "focused_pane": 10, "panes": [10], "layout": {"kind": "leaf", "pane_id": 10}, "revision": 1}
        ],
        "panes": [
            duplicate_pane(4, 5, 3, 6),
            duplicate_pane(10, 11, 9, 12)
        ]
    });
    let bytes = serde_json::to_vec_pretty(&state).expect("serialize duplicate state");
    private_file(path, &bytes, 0o600);
}

fn duplicate_pane(
    pane_id: u64,
    terminal_id: u64,
    session_id: u64,
    agent_id: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": pane_id,
        "terminal_id": terminal_id,
        "session_id": session_id,
        "label": "codex",
        "agent": {
            "id": agent_id,
            "provider": "codex",
            "state": "unknown",
            "conversation_id": null,
            "session_ref": {"kind": "id", "value": "codex-session-123"},
            "capabilities": {"prompt": false, "resume": true, "lifecycle": false},
            "source": "adapter"
        },
        "exited": false,
        "revision": 1,
        "recovery": {"command": ["/bin/sh"], "initial_input": "codex", "rows": 41, "cols": 113},
        "recovery_quarantined": false
    })
}

fn child_stderr(daemon: &mut DaemonGuard) -> String {
    let Some(stderr) = daemon
        .child
        .as_mut()
        .expect("daemon child is missing")
        .stderr
        .take()
    else {
        return "<unavailable>".to_owned();
    };
    let mut stderr = stderr;
    let mut bytes = Vec::new();
    if let Err(error) = std::io::Read::read_to_end(&mut stderr, &mut bytes) {
        return format!("<failed to read stderr: {error}>");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn read_invocations(log: &Path) -> Vec<Vec<String>> {
    let contents =
        fs::read_to_string(log).unwrap_or_else(|error| panic!("read {}: {error}", log.display()));
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line_number, line)| {
            serde_json::from_str::<Vec<String>>(line).unwrap_or_else(|error| {
                panic!("parse {} line {}: {error}", log.display(), line_number + 1)
            })
        })
        .collect()
}

fn wait_for_invocation(log: &Path, daemon: &mut DaemonGuard) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match fs::read_to_string(log) {
            Ok(contents) => {
                let has_valid_complete_line = contents.split_inclusive('\n').any(|line| {
                    let line = line.strip_suffix('\n').expect("split_inclusive suffix");
                    !line.trim().is_empty() && serde_json::from_str::<Vec<String>>(line).is_ok()
                });
                if has_valid_complete_line {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("read {}: {error}", log.display()),
        }
        if let Some(status) = daemon
            .child
            .as_mut()
            .expect("daemon child is missing")
            .try_wait()
            .expect("poll wsxd")
        {
            panic!(
                "wsxd exited before invoking codex: {status}; stderr: {}",
                child_stderr(daemon)
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fake codex"
        );
        thread::sleep(POLL_INTERVAL);
    }
}
