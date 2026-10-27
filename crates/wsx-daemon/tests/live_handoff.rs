#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wsx_core::runtime::{
    binary_identity, AgentCapabilities, AgentState, Client, EventMonitor, EventSignal, KeyCode,
    KeyEvent, PaneId, ProjectSpec, RawTerminalStream, ReplacementBlocker, ReplacementDisposition,
    Request, Response, TerminalClientMessage, TerminalRawServerMessage, TerminalServerMessage,
    TerminalStream, TerminalUpdate, TuiClientPresence, WorktreeSpec, DAEMON_REVISION,
    PROTOCOL_VERSION, WSX_VERSION,
};

static NEXT_FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    socket: PathBuf,
    source: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        // ^ Keep both canonical and handoff socket names below macOS SUN_LEN,
        // including when the repository itself is an isolated candidate worktree.
        // The process ID and sequence make the name unique; the clock part stays short.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/s")
            .join(format!(
                "h{:x}{:x}{sequence:x}",
                std::process::id(),
                nonce & 0xffff_ffff
            ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        // Handoff binaries have their own namespace: XDG_STATE_HOME/wsx is a directory.
        fs::create_dir(root.join("bin")).unwrap();
        fs::copy(
            PathBuf::from(env!("CARGO_BIN_EXE_wsxd")).with_file_name("wsx"),
            root.join("bin/wsx"),
        )
        .expect("build adjacent wsx/wsxd before running daemon lifecycle scenarios");
        let socket = root.join("d.sock");
        Self {
            root,
            socket,
            source: None,
        }
    }

    fn wsxd(&self) -> PathBuf {
        // ^ The daemon re-validates the handoff executable by device, inode, size and mtime
        // after the deferral. Another task's build can relink the shared target/debug/wsxd in
        // that window, so each test hands off its own copy.
        let binary = self.root.join("bin/wsxd");
        fs::copy(env!("CARGO_BIN_EXE_wsxd"), &binary).unwrap();
        binary
    }

    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env("WSX_SOCKET", &self.socket)
            .env("XDG_STATE_HOME", &self.root)
            .env("HOME", &self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(self.root.join("daemon.log"))
                    .unwrap(),
            ));
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.socket.exists() {
            let _ = Client::new(self.socket.clone()).shutdown();
        }
        if let Some(child) = self.source.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        // ^ wsxd binds the socket, then restricts it to its owner: a client that connects
        // between the two steps is refused (client.rs `validate_socket`). Wait for the
        // restricted socket, as a client that retries would.
        if fs::symlink_metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o077 == 0) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "wsxd socket did not appear: {}: {}",
        path.display(),
        fs::read_to_string(path.with_file_name("daemon.log")).unwrap_or_default()
    );
}

fn wait_for_socket_removal(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("wsxd socket remained after shutdown: {}", path.display());
}

fn frame_text(stream: &TerminalStream, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut latest = String::new();
    while Instant::now() < deadline {
        match stream.try_recv() {
            Ok(TerminalServerMessage::Update(TerminalUpdate::Full(frame))) => {
                latest = frame.cells.into_iter().map(|cell| cell.symbol).collect();
            }
            Ok(TerminalServerMessage::Update(TerminalUpdate::Patch { changed_rows, .. })) => {
                latest.push_str(
                    &changed_rows
                        .into_iter()
                        .flat_map(|row| row.cells)
                        .map(|cell| cell.symbol)
                        .collect::<String>(),
                );
            }
            Ok(TerminalServerMessage::Error(error)) => {
                panic!("terminal stream failed: {}: {}", error.code, error.message)
            }
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
        if latest.contains(expected) {
            return latest;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("terminal did not render {expected:?}: {latest:?}");
}

/// Reads a raw stream until its replay plus later output contains `expected`,
/// or until the stream ends. Returns that text and the code that ended it.
fn raw_text(stream: &RawTerminalStream, expected: &str) -> (String, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    loop {
        match stream.recv_timeout(Duration::from_millis(50)) {
            Ok(TerminalRawServerMessage::Replay(replay)) => bytes = replay.ansi,
            Ok(TerminalRawServerMessage::Output(output)) => bytes.extend(output),
            Ok(TerminalRawServerMessage::Error(error)) => {
                return (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    Some(error.code),
                )
            }
            Ok(TerminalRawServerMessage::Exited) => {
                return (
                    String::from_utf8_lossy(&bytes).into_owned(),
                    Some("exited".into()),
                )
            }
            Ok(_) | Err(_) => {}
        }
        let text = String::from_utf8_lossy(&bytes);
        if !expected.is_empty() && text.contains(expected) {
            return (text.into_owned(), None);
        }
        assert!(
            Instant::now() < deadline,
            "raw stream never produced {expected:?}: {text:?}"
        );
    }
}

fn reported_value(text: &str, marker: &str) -> String {
    let start = text.find(marker).expect("terminal marker") + marker.len();
    let end = text[start..].find(']').expect("terminal marker terminator");
    text[start..start + end].to_string()
}

fn reported_pid(text: &str) -> String {
    reported_value(text, "pid=[")
}

#[test]
fn daemon_handoff_preserves_live_shell_pid_and_io() {
    let mut fixture = Fixture::new();
    let source_binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&source_binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());

    let worktree = std::env::current_dir().unwrap();
    assert!(matches!(
        client
            .call(&Request::SynchronizeProjects {
                projects: vec![ProjectSpec {
                    path: worktree.clone(),
                    name: "handoff".into(),
                    worktrees: vec![WorktreeSpec {
                        path: worktree,
                        branch: "test".into(),
                    }],
                }],
            })
            .unwrap(),
        Response::Ack { .. }
    ));
    let snapshot = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    let worktree_id = snapshot.worktrees[0].id;
    client
        .call(&Request::SessionCreate {
            worktree_id,
            label: "live".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "test \"$WSX_AGENT_REPORT_BIN\" = \"${WSX_SOCKET%.sock}.reporter\" || exit 23; printf 'pid=[%s] gen=[%s]\\n' $$ \"$WSX_RUNTIME_GENERATION\"; while IFS= read -r line; do printf 'echo:%s:pid=[%s]\\n' \"$line\" $$; done".into(),
            ],
            initial_input: None,
            rows: 12,
            cols: 60,
        })
        .unwrap();
    let before = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    let project_id = before.projects[0].id;
    let project_path = before.projects[0].path.clone();
    let worktree_id = before.worktrees[0].id;
    let worktree_path = before.worktrees[0].path.clone();
    let session_id = before.sessions[0].id;
    let session_label = before.sessions[0].label.clone();
    let pane: PaneId = before.panes[0].id;
    let terminal_id = before.panes[0].terminal_id;
    assert_ne!(
        before.panes[0].agent.as_ref().map(|agent| agent.state),
        Some(AgentState::Error)
    );
    let stream = TerminalStream::connect(&client, pane, 100, true, 12, 60).unwrap();
    let initial_output = frame_text(&stream, "gen=[");
    let pid = reported_pid(&initial_output);
    let runtime_generation = reported_value(&initial_output, "gen=[");
    let old_epoch = stream.epoch();
    let presence_id = "924fe57b-48e4-416c-b4a7-f451c028dc58";
    assert!(matches!(
        client
            .call(&Request::AgentReport {
                pane_id: pane,
                runtime_generation: Some(runtime_generation.clone()),
                provider: "pi".into(),
                state: AgentState::Working,
                attached: true,
                presence_id: Some(presence_id.into()),
                conversation_id: None,
                session_ref: None,
                wake_token: None,
                capabilities: AgentCapabilities {
                    lifecycle: true,
                    ..Default::default()
                },
                received: None,
                compacting: false,
            })
            .unwrap(),
        Response::Ack { .. }
    ));

    let rejected_binary = fixture.root.join("wsxd-rejected");
    fs::write(&rejected_binary, fs::read(&source_binary).unwrap()).unwrap();
    let mut permissions = fs::metadata(&rejected_binary).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o700);
    fs::set_permissions(&rejected_binary, permissions).unwrap();
    let rejected_identity = binary_identity(&rejected_binary).unwrap();
    assert!(matches!(
        client
            .call(&Request::PrepareHandoff {
                target_binary_id: rejected_identity,
                target_version: WSX_VERSION.into(),
                target_protocol: PROTOCOL_VERSION + 1,
                target_daemon_revision: DAEMON_REVISION,
                executable: rejected_binary,
                restart: false,
            })
            .unwrap(),
        Response::Replacement { .. }
    ));
    drop(stream);
    let rollback_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Response::Lifecycle(status)) = client.call(&Request::LifecycleStatus) {
            if status.epoch == old_epoch && status.replacement_target.is_none() {
                break;
            }
        }
        assert!(
            Instant::now() < rollback_deadline,
            "old daemon did not resume after rejected handoff"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let stream = TerminalStream::connect(&client, pane, 100, true, 12, 60).unwrap();
    stream
        .try_send(TerminalClientMessage::Input(b"rollback\n".to_vec()))
        .unwrap();
    let rollback_output = frame_text(&stream, "echo:rollback:pid=[");
    assert_eq!(
        reported_pid(&rollback_output[rollback_output.find("echo:rollback").unwrap()..]),
        pid
    );

    let target_binary = fixture.root.join("bin/wsxd-next");
    fs::write(&target_binary, fs::read(&source_binary).unwrap()).unwrap();
    let mut permissions = fs::metadata(&target_binary).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&target_binary, permissions).unwrap();
    let target_identity = binary_identity(&target_binary).unwrap();

    // A TUI of the target protocol stays attached: its presence must not defer the
    // handoff, and its stream must learn that the daemon, not the terminal, is leaving.
    // The TUI identifies its adjacent daemon; only this test reads the override.
    std::env::set_var("WSX_DAEMON_BIN", &source_binary);
    let (tui, tui_signals) = EventMonitor::start(client.clone()).unwrap();
    assert_eq!(
        tui_signals.recv_timeout(Duration::from_secs(5)).unwrap(),
        EventSignal::Connected(None)
    );
    let response = client
        .call(&Request::PrepareHandoff {
            target_binary_id: target_identity,
            target_version: WSX_VERSION.into(),
            target_protocol: PROTOCOL_VERSION,
            target_daemon_revision: DAEMON_REVISION,
            executable: target_binary,
            restart: false,
        })
        .unwrap();
    assert!(
        matches!(
            &response,
            Response::Replacement { blockers, .. } if blockers.is_empty()
        ),
        "an attached current TUI must not defer the handoff: {response:?}"
    );
    let stream_deadline = Instant::now() + Duration::from_secs(10);
    let stream_end = loop {
        match stream.try_recv() {
            Ok(TerminalServerMessage::Error(error)) => break error.code,
            Ok(_) | Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break "closed".into(),
        }
        assert!(
            Instant::now() < stream_deadline,
            "stream outlived the handoff"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(stream_end, "daemon_handoff");
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(10);
    let after = loop {
        if let Ok(Response::Snapshot(snapshot)) = client.call(&Request::Snapshot) {
            if snapshot.epoch != old_epoch {
                break snapshot;
            }
        }
        assert!(
            Instant::now() < deadline,
            "handoff successor did not become ready: {}",
            fs::read_to_string(fixture.root.join("daemon.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(25));
    };
    // The attached TUI reconnects to the successor without reporting a failure.
    let reconnect_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match tui_signals.recv_timeout(Duration::from_millis(100)) {
            Ok(EventSignal::Connected(_)) => break,
            Ok(EventSignal::Disconnected(error)) => {
                panic!("an attached TUI saw the handoff as a failure: {error}")
            }
            Ok(EventSignal::Dirty) | Err(_) => {}
        }
        assert!(
            Instant::now() < reconnect_deadline,
            "an attached TUI did not reconnect to the successor"
        );
    }
    drop(tui);
    assert_eq!(after.projects.len(), 1);
    assert_eq!(after.projects[0].id, project_id);
    assert_eq!(after.projects[0].path, project_path);
    assert_eq!(after.worktrees.len(), 1);
    assert_eq!(after.worktrees[0].id, worktree_id);
    assert_eq!(after.worktrees[0].path, worktree_path);
    assert_eq!(after.sessions.len(), 1);
    assert_eq!(after.sessions[0].id, session_id);
    assert_eq!(after.sessions[0].label, session_label);
    assert_eq!(after.panes.len(), 1);
    assert_eq!(after.panes[0].id, pane);
    assert_eq!(after.panes[0].terminal_id, terminal_id);
    assert_eq!(
        after.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .presence_id
            .as_deref(),
        Some(presence_id)
    );
    assert!(matches!(
        client
            .call(&Request::AgentPresenceRenew {
                pane_id: pane,
                runtime_generation: runtime_generation.clone(),
                presence_id: presence_id.into(),
            })
            .unwrap(),
        Response::Ack { .. }
    ));

    let reporter = fixture.socket.with_extension("reporter");
    assert_eq!(
        fs::read_link(&reporter).unwrap(),
        fixture.root.join("bin/wsx").canonicalize().unwrap()
    );
    let pane_selector = pane.0.to_string();
    let rejected = fixture
        .command(&reporter)
        .env("WSX_PANE_ID", &pane_selector)
        .env("WSX_RUNTIME_GENERATION", "stale-generation")
        .args([
            "agent",
            "report",
            &pane_selector,
            "--provider",
            "pi",
            "--state",
            "done",
            "--lifecycle",
        ])
        .status()
        .unwrap();
    assert!(
        !rejected.success(),
        "stable reporter must retain generation fencing"
    );
    let unchanged = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    assert_eq!(
        unchanged.panes[0].agent.as_ref().unwrap().state,
        AgentState::Working
    );
    let report = fixture
        .command(&reporter)
        .env("WSX_PANE_ID", &pane_selector)
        .env("WSX_RUNTIME_GENERATION", &runtime_generation)
        .args([
            "agent",
            "report",
            &pane_selector,
            "--provider",
            "pi",
            "--state",
            "done",
            "--lifecycle",
            "--presence-id",
            presence_id,
        ])
        .status()
        .unwrap();
    assert!(report.success(), "stable reporter failed after handoff");
    let projected = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    assert_eq!(
        projected.panes[0].agent.as_ref().unwrap().state,
        AgentState::Done
    );

    // The shell survives an adapter crash; no further presence renewal arrives.
    // The successor must detach the provider without terminating its PTY.
    let expiry_deadline = Instant::now() + Duration::from_secs(35);
    loop {
        let current = match client.call(&Request::Snapshot).unwrap() {
            Response::Snapshot(snapshot) => snapshot,
            response => panic!("unexpected snapshot response: {response:?}"),
        };
        assert!(!current.panes[0].exited);
        if !current.panes[0].agent.as_ref().unwrap().attached {
            assert_eq!(
                current.panes[0].agent.as_ref().unwrap().state,
                AgentState::Unknown
            );
            break;
        }
        assert!(
            Instant::now() < expiry_deadline,
            "stale agent remained attached"
        );
        thread::sleep(Duration::from_millis(100));
    }

    let stream = TerminalStream::connect(&client, pane, 101, true, 12, 60).unwrap();
    stream
        .try_send(TerminalClientMessage::Input(b"after\n".to_vec()))
        .unwrap();
    let output = frame_text(&stream, "echo:after:pid=[");
    assert_eq!(
        reported_pid(&output[output.find("echo:after").unwrap()..]),
        pid
    );
    drop(stream);
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}

#[test]
fn daemon_handoff_restores_a_legacy_sized_history_cohort() {
    const PANES: usize = 43;
    let mut fixture = Fixture::new();
    let source_binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&source_binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());
    let worktree = std::env::current_dir().unwrap();
    client
        .call(&Request::SynchronizeProjects {
            projects: vec![ProjectSpec {
                path: worktree.clone(),
                name: "handoff-cohort".into(),
                worktrees: vec![WorktreeSpec {
                    path: worktree,
                    branch: "test".into(),
                }],
            }],
        })
        .unwrap();
    let worktree_id = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot.worktrees[0].id,
        response => panic!("unexpected snapshot response: {response:?}"),
    };

    let mut identities = Vec::with_capacity(PANES);
    for index in 0..PANES {
        client
            .call(&Request::SessionCreate {
                worktree_id,
                label: format!("history-{index}"),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "i=0; while [ $i -lt 1024 ]; do printf 'history-{index}-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' \"$i\"; i=$((i+1)); done; printf 'ready-{index}:pid=[%s]\\n' $$; while IFS= read -r line; do printf 'echo-{index}:%s:pid=[%s]\\n' \"$line\" $$; done"
                    ),
                ],
                initial_input: None,
                rows: 12,
                cols: 100,
            })
            .unwrap();
        let pane = match client.call(&Request::Snapshot).unwrap() {
            Response::Snapshot(snapshot) => snapshot
                .sessions
                .iter()
                .find(|session| session.label == format!("history-{index}"))
                .map(|session| session.primary_pane)
                .expect("created history pane"),
            response => panic!("unexpected snapshot response: {response:?}"),
        };
        let stream =
            TerminalStream::connect(&client, pane, 10_000 + index as u64, true, 12, 100).unwrap();
        let output = frame_text(&stream, &format!("ready-{index}:pid=["));
        identities.push((pane, reported_pid(&output)));
    }
    let old_epoch = match client.call(&Request::LifecycleStatus).unwrap() {
        Response::Lifecycle(status) => status.epoch,
        response => panic!("unexpected lifecycle response: {response:?}"),
    };

    // docs/terminal-raw-stream.md: a raw stream attached during the handoff learns that
    // the daemon, not the terminal, is leaving. Its replay carries the scrollback.
    let (raw_pane, raw_pid) = identities.first().unwrap().clone();
    let raw = RawTerminalStream::connect_raw(&client, raw_pane, 30_000, true, 0, 0).unwrap();
    let (replay, ended) = raw_text(&raw, "ready-0:pid=[");
    assert_eq!(ended, None);
    // More history than the 12 screen rows, in order, up to the newest line.
    assert!(replay.matches("history-0-").count() > 100 && replay.contains("history-0-1023-"));

    let target_binary = fixture.root.join("bin/wsxd-next");
    fs::write(&target_binary, fs::read(&source_binary).unwrap()).unwrap();
    let mut permissions = fs::metadata(&target_binary).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o700);
    fs::set_permissions(&target_binary, permissions).unwrap();
    let response = client
        .call(&Request::PrepareHandoff {
            target_binary_id: binary_identity(&target_binary).unwrap(),
            target_version: WSX_VERSION.into(),
            target_protocol: PROTOCOL_VERSION,
            target_daemon_revision: DAEMON_REVISION,
            executable: target_binary,
            restart: false,
        })
        .unwrap();
    assert!(matches!(response, Response::Replacement { .. }));

    let deadline = Instant::now() + Duration::from_secs(20);
    let after = loop {
        if let Ok(Response::Snapshot(snapshot)) = client.call(&Request::Snapshot) {
            if snapshot.epoch != old_epoch {
                break snapshot;
            }
        }
        assert!(
            Instant::now() < deadline,
            "history-cohort handoff successor did not become ready: {}",
            fs::read_to_string(fixture.root.join("daemon.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(after.panes.len(), PANES);
    assert_eq!(raw_text(&raw, "").1.as_deref(), Some("daemon_handoff"));
    drop(raw);
    // The successor's stream starts from a new replay of the restored screen.
    let raw = RawTerminalStream::connect_raw(&client, raw_pane, 30_001, true, 0, 0).unwrap();
    assert_eq!(raw_text(&raw, "ready-0:pid=[").1, None);
    raw.try_send(TerminalClientMessage::Input(b"raw\n".to_vec()))
        .unwrap();
    let (output, ended) = raw_text(&raw, "echo-0:raw:pid=[");
    assert_eq!(ended, None);
    assert_eq!(
        reported_pid(&output[output.find("echo-0:raw:pid=[").unwrap()..]),
        raw_pid
    );
    drop(raw);

    for (client_id, (pane, pid)) in [identities.first().unwrap(), identities.last().unwrap()]
        .into_iter()
        .enumerate()
    {
        let stream =
            TerminalStream::connect(&client, *pane, 20_000 + client_id as u64, true, 12, 100)
                .unwrap();
        stream
            .try_send(TerminalClientMessage::Input(b"after\n".to_vec()))
            .unwrap();
        let output = frame_text(&stream, "after:pid=[");
        assert_eq!(
            reported_pid(&output[output.find("after:pid=[").unwrap()..]),
            *pid
        );
    }
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}

// docs/daemon-live-upgrade.md: an explicit restart hands off into the running build, a TUI
// that follows does not defer it, and a deferred handoff continues when the blocker leaves.
#[test]
fn restart_into_the_same_build_waits_only_for_a_tui_that_cannot_follow() {
    let mut fixture = Fixture::new();
    let binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());
    let worktree = std::env::current_dir().unwrap();
    client
        .call(&Request::SynchronizeProjects {
            projects: vec![ProjectSpec {
                path: worktree.clone(),
                name: "restart".into(),
                worktrees: vec![WorktreeSpec {
                    path: worktree,
                    branch: "test".into(),
                }],
            }],
        })
        .unwrap();
    let worktree_id = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot.worktrees[0].id,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    client
        .call(&Request::SessionCreate {
            worktree_id,
            label: "live".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'pid=[%s]\\n' $$; while IFS= read -r line; do printf 'echo:%s:pid=[%s]\\n' \"$line\" $$; done".into(),
            ],
            initial_input: None,
            rows: 12,
            cols: 60,
        })
        .unwrap();
    let (pane, revision) = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => (snapshot.panes[0].id, snapshot.revision),
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    let stream = TerminalStream::connect(&client, pane, 100, true, 12, 60).unwrap();
    let pid = reported_pid(&frame_text(&stream, "pid=["));
    let old_epoch = stream.epoch();
    drop(stream);

    let identity = binary_identity(&binary).unwrap();
    let handoff = |restart| Request::PrepareHandoff {
        target_binary_id: identity.clone(),
        target_version: WSX_VERSION.into(),
        target_protocol: PROTOCOL_VERSION,
        target_daemon_revision: DAEMON_REVISION,
        executable: binary.clone(),
        restart,
    };
    assert!(
        matches!(
            client.call(&handoff(false)).unwrap(),
            Response::Error(error) if error.code == "invalid_request"
        ),
        "an upgrade request must not select the running build"
    );

    // Both TUIs are older than the target protocol. Only one of them follows.
    let presence = |instance_id, follows| Request::Poll {
        after_revision: revision,
        timeout_ms: 0,
        tui: Some(TuiClientPresence {
            instance_id,
            version: WSX_VERSION.into(),
            target_binary_id: identity.clone(),
            target_daemon_revision: DAEMON_REVISION,
            max_protocol: PROTOCOL_VERSION - 1,
            follows,
        }),
    };
    assert!(matches!(
        client.call(&presence(1, true)).unwrap(),
        Response::Events { .. }
    ));
    assert!(matches!(
        client.call(&presence(2, false)).unwrap(),
        Response::Events { .. }
    ));
    let deferred = client.call(&handoff(true)).unwrap();
    assert!(
        matches!(
            &deferred,
            Response::Replacement {
                disposition: ReplacementDisposition::Deferred,
                blockers,
                ..
            } if blockers == &[ReplacementBlocker::OtherTui]
        ),
        "a TUI that cannot follow must defer the restart: {deferred:?}"
    );

    // The TUI that cannot follow quits: its presence expires. The following TUI keeps
    // renewing its presence, which lasts three seconds, until the successor answers.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut following_tui_renewed = Instant::now();
    loop {
        if matches!(client.call(&presence(1, true)), Ok(Response::Events { .. })) {
            following_tui_renewed = Instant::now();
        }
        if let Ok(Response::Lifecycle(status)) = client.call(&Request::LifecycleStatus) {
            if status.epoch != old_epoch {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the deferred restart did not continue by itself: {}",
            fs::read_to_string(fixture.root.join("daemon.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        following_tui_renewed.elapsed() < Duration::from_secs(2),
        "the restart must proceed while the following TUI is still attached"
    );

    let restarted = TerminalStream::connect(&client, pane, 101, true, 12, 60).unwrap();
    restarted
        .try_send(TerminalClientMessage::Input(b"after\n".to_vec()))
        .unwrap();
    let output = frame_text(&restarted, "echo:after:pid=[");
    assert_eq!(
        reported_pid(&output[output.find("echo:after").unwrap()..]),
        pid
    );
    drop(restarted);
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}

/// One project with one worktree, and a session per `(label, shell command)`; the primary
/// pane of each, in order.
fn sessions(client: &Client, name: &str, commands: &[(&str, &str)]) -> Vec<PaneId> {
    let worktree = std::env::current_dir().unwrap();
    client
        .call(&Request::SynchronizeProjects {
            projects: vec![ProjectSpec {
                path: worktree.clone(),
                name: name.into(),
                worktrees: vec![WorktreeSpec {
                    path: worktree,
                    branch: "test".into(),
                }],
            }],
        })
        .unwrap();
    let worktree_id = match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot.worktrees[0].id,
        response => panic!("unexpected snapshot response: {response:?}"),
    };
    for (label, command) in commands {
        client
            .call(&Request::SessionCreate {
                worktree_id,
                label: (*label).into(),
                command: vec!["/bin/sh".into(), "-c".into(), (*command).into()],
                initial_input: None,
                rows: 12,
                cols: 60,
            })
            .unwrap();
    }
    let Response::Snapshot(snapshot) = client.call(&Request::Snapshot).unwrap() else {
        panic!("no snapshot");
    };
    commands
        .iter()
        .map(|(label, _)| {
            snapshot
                .sessions
                .iter()
                .find(|session| session.label == *label)
                .unwrap()
                .primary_pane
        })
        .collect()
}

fn pane_exited(client: &Client, pane: PaneId) -> bool {
    match client.call(&Request::Snapshot).unwrap() {
        Response::Snapshot(snapshot) => snapshot
            .panes
            .iter()
            .find(|candidate| candidate.id == pane)
            .is_some_and(|candidate| candidate.exited),
        response => panic!("unexpected snapshot response: {response:?}"),
    }
}

fn lifecycle(client: &Client) -> wsx_core::runtime::DaemonLifecycle {
    match client.call(&Request::LifecycleStatus).unwrap() {
        Response::Lifecycle(status) => status,
        response => panic!("unexpected lifecycle response: {response:?}"),
    }
}

// docs/daemon-live-upgrade.md "Exited panes": a pane whose program exited used to fail the
// whole handoff (its reader was gone, so the pause wrote into a closed pipe). It now goes
// as its last screen; the live pane keeps its process.
#[test]
fn an_exited_pane_hands_off_as_its_last_screen() {
    let mut fixture = Fixture::new();
    let binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());
    let panes = sessions(
        &client,
        "exited",
        &[
            (
                "live",
                "printf 'pid=[%s]\\n' $$; while IFS= read -r line; do printf 'echo:%s:pid=[%s]\\n' \"$line\" $$; done",
            ),
            ("gone", "printf 'bye from the exited pane\\n'; exit 3"),
        ],
    );
    let (live, gone) = (panes[0], panes[1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pane_exited(&client, gone) {
        assert!(Instant::now() < deadline, "the gone pane did not exit");
        thread::sleep(Duration::from_millis(50));
    }
    let stream = TerminalStream::connect(&client, live, 200, true, 12, 60).unwrap();
    let pid = reported_pid(&frame_text(&stream, "pid=["));
    drop(stream);
    let old_epoch = lifecycle(&client).epoch;

    // ^ Another file is another binary id: the handoff of an upgrade, not a restart.
    let next = fixture.root.join("bin/next-wsxd");
    fs::copy(&binary, &next).unwrap();
    let identity = binary_identity(&next).unwrap();
    assert_ne!(identity, lifecycle(&client).binary_id);
    let restart = client
        .call(&Request::PrepareHandoff {
            target_binary_id: identity.clone(),
            target_version: WSX_VERSION.into(),
            target_protocol: PROTOCOL_VERSION,
            target_daemon_revision: DAEMON_REVISION,
            executable: next,
            restart: true,
        })
        .unwrap();
    assert!(
        matches!(
            restart,
            Response::Replacement {
                disposition: ReplacementDisposition::Stopping,
                ..
            }
        ),
        "{restart:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Response::Lifecycle(status)) = client.call(&Request::LifecycleStatus) {
            if status.epoch != old_epoch {
                break;
            }
            assert!(
                status.last_handoff_error.is_none(),
                "the handoff rolled back: {:?}",
                status.last_handoff_error
            );
        }
        assert!(
            Instant::now() < deadline,
            "the handoff did not finish: {}",
            fs::read_to_string(fixture.root.join("daemon.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    }

    // Both panes are listed; the exited one still reads exited and shows its last screen.
    assert_eq!(lifecycle(&client).binary_id, identity);
    assert!(pane_exited(&client, gone));
    assert!(!pane_exited(&client, live));
    let raw = RawTerminalStream::connect_raw(&client, gone, 30_100, true, 0, 0).unwrap();
    let (text, _) = raw_text(&raw, "bye from the exited pane");
    assert!(text.contains("bye from the exited pane"), "{text:?}");
    drop(raw);
    let restarted = TerminalStream::connect(&client, live, 201, true, 12, 60).unwrap();
    restarted
        .try_send(TerminalClientMessage::Input(b"after\n".to_vec()))
        .unwrap();
    let output = frame_text(&restarted, "echo:after:pid=[");
    assert_eq!(
        reported_pid(&output[output.find("echo:after").unwrap()..]),
        pid
    );
    drop(restarted);
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}

// docs/daemon-live-upgrade.md "A failed handoff": a target that exits before it takes the
// handoff ends the wait at once; the daemon keeps serving and keeps the step and the error.
#[test]
fn a_failed_handoff_keeps_its_reason() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new();
    let binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());
    let panes = sessions(
        &client,
        "failed",
        &[("live", "printf 'pid=[%s]\\n' $$; exec cat")],
    );
    let target = fixture.root.join("bin/broken-wsxd");
    fs::write(&target, "#!/bin/sh\nexit 3\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let old_epoch = lifecycle(&client).epoch;
    let started = Instant::now();
    let response = client
        .call(&Request::PrepareHandoff {
            target_binary_id: binary_identity(&target).unwrap(),
            target_version: WSX_VERSION.into(),
            target_protocol: PROTOCOL_VERSION,
            target_daemon_revision: DAEMON_REVISION,
            executable: target,
            restart: true,
        })
        .unwrap();
    assert!(
        matches!(
            response,
            Response::Replacement {
                disposition: ReplacementDisposition::Stopping,
                ..
            }
        ),
        "{response:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let failure = loop {
        if let Ok(Response::Lifecycle(status)) = client.call(&Request::LifecycleStatus) {
            if let Some(failure) = status.last_handoff_error.clone() {
                if status.replacement_target.is_none() {
                    assert_eq!(status.epoch, old_epoch);
                    break failure;
                }
            }
        }
        assert!(Instant::now() < deadline, "the handoff did not roll back");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the rollback waited out the timeout"
    );
    assert_eq!(failure.step, "send");
    assert!(
        failure
            .error
            .contains("the new wsxd exited before it took the handoff"),
        "{failure:?}"
    );
    assert!(failure.unix_ms > 0);
    // The daemon serves as before: the live pane is still there and running.
    assert!(!pane_exited(&client, panes[0]));
    let stream = TerminalStream::connect(&client, panes[0], 202, true, 12, 60).unwrap();
    frame_text(&stream, "pid=[");
    drop(stream);
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}

// docs/agent-orchestration.md: when a resumed agent quits, its supervisor opens a shell in
// the same pane. Clearing the agent's record first can fail (a stale runtime generation, a
// full disk), and the agent can quit in its alternate screen with its keyboard modes on;
// the pane must still get a shell that takes typed keys, not a dead or empty screen.
#[test]
fn a_resumed_agent_that_quits_leaves_a_usable_shell() {
    let mut fixture = Fixture::new();
    let binary = fixture.wsxd();
    fixture.source = Some(fixture.command(&binary).spawn().unwrap());
    wait_for_socket(&fixture.socket);
    let client = Client::new(fixture.socket.clone());
    let command = format!(
        "SHELL=/bin/sh WSX_RUNTIME_GENERATION=stale exec '{}' __wsx_resume_supervisor -- /bin/sh -c 'printf \"agent-quit\\033[?1049h\\033[>31u\\033[?1000h\"'",
        binary.display()
    );
    let pane = sessions(&client, "resume", &[("agent", &command)])[0];
    let stream = TerminalStream::connect(&client, pane, 300, true, 12, 60).unwrap();
    // Keys as the TUI sends them: the pane's emulator encodes them in its current modes.
    let key = |code, text: &str| {
        TerminalClientMessage::Key(KeyEvent {
            code,
            text: text.into(),
            shift: false,
            control: false,
            alt: false,
            super_key: false,
            repeat: false,
        })
    };
    thread::sleep(Duration::from_millis(500));
    for character in "echo shell-$((40+2))".chars() {
        stream
            .try_send(key(KeyCode::Text, &character.to_string()))
            .unwrap();
    }
    stream.try_send(key(KeyCode::Enter, "")).unwrap();
    frame_text(&stream, "shell-42");
    assert!(!pane_exited(&client, pane));
    drop(stream);
    client.shutdown().unwrap();
    wait_for_socket_removal(&fixture.socket);
}
