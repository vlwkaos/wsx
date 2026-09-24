#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use wsx_core::runtime::{
    binary_identity, AgentCapabilities, AgentState, Client, PaneId, ProjectSpec, Request, Response,
    TerminalClientMessage, TerminalServerMessage, TerminalStream, TerminalUpdate, WorktreeSpec,
    DAEMON_REVISION, PROTOCOL_VERSION, WSX_VERSION,
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
        let root = std::env::current_dir()
            .unwrap()
            .join(".work/s")
            .join(format!("h{:x}{nonce:x}{sequence:x}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("d.sock");
        Self {
            root,
            socket,
            source: None,
        }
    }

    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env("WSX_SOCKET", &self.socket)
            .env("XDG_STATE_HOME", &self.root)
            .env("HOME", &self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("wsxd socket did not appear: {}", path.display());
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
    let source_binary = PathBuf::from(env!("CARGO_BIN_EXE_wsxd"));
    let mut fixture = Fixture::new();
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
                "printf 'pid=[%s] gen=[%s]\\n' $$ \"$WSX_RUNTIME_GENERATION\"; while IFS= read -r line; do printf 'echo:%s:pid=[%s]\\n' \"$line\" $$; done".into(),
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

    let target_binary = fixture.root.join("wsxd-next");
    fs::write(&target_binary, fs::read(&source_binary).unwrap()).unwrap();
    let mut permissions = fs::metadata(&target_binary).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&target_binary, permissions).unwrap();
    let target_identity = binary_identity(&target_binary).unwrap();

    let response = client
        .call(&Request::PrepareHandoff {
            target_binary_id: target_identity,
            target_version: WSX_VERSION.into(),
            target_protocol: PROTOCOL_VERSION,
            target_daemon_revision: DAEMON_REVISION,
            executable: target_binary,
        })
        .unwrap();
    assert!(
        matches!(response, Response::Replacement { .. }),
        "unexpected handoff response: {response:?}"
    );
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
            "handoff successor did not become ready"
        );
        thread::sleep(Duration::from_millis(25));
    };
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

    assert!(matches!(
        client
            .call(&Request::AgentReport {
                pane_id: pane,
                runtime_generation: Some(runtime_generation),
                provider: "pi".into(),
                state: AgentState::Done,
                attached: true,
                presence_id: Some(presence_id.into()),
                conversation_id: None,
                session_ref: None,
                wake_token: None,
                capabilities: AgentCapabilities {
                    lifecycle: true,
                    ..Default::default()
                },
            })
            .unwrap(),
        Response::Ack { .. }
    ));
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
    let source_binary = PathBuf::from(env!("CARGO_BIN_EXE_wsxd"));
    let mut fixture = Fixture::new();
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

    let target_binary = fixture.root.join("wsxd-next");
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
            "history-cohort handoff successor did not become ready"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(after.panes.len(), PANES);

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
