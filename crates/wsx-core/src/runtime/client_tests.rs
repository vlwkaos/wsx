use super::*;
use crate::runtime::{
    Cell, Cursor, PaneId, TerminalFrame, TerminalId, TerminalSelectionRange, TerminalUpdate,
};
use std::{
    os::unix::{fs::PermissionsExt, net::UnixListener},
    sync::atomic::AtomicUsize,
};

fn test_listener(_name: &str) -> (PathBuf, UnixListener) {
    let dir = std::env::current_dir().unwrap().join(".work/s");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join(format!("{:x}-{:x}", std::process::id(), new_client_id()));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (path, listener)
}

fn send_response(stream: &mut UnixStream, response: &Response) {
    stream.write_all(&encode_line(response).unwrap()).unwrap();
}

fn current_capabilities() -> super::super::domain::Capabilities {
    super::super::domain::Capabilities {
        resume_shell_fallback: true,
        foreground_jobs: true,
        lifecycle_coordination: true,
        agent_exchanges: true,
        ..Default::default()
    }
}

#[test]
fn compatible_daemon_is_reused_without_shutdown() {
    let (path, listener) = test_listener("compatible-reuse");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        ));
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                capabilities: current_capabilities(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Snapshot
        );
        send_response(
            &mut stream,
            &Response::Snapshot(super::super::domain::Snapshot {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                revision: 1,
                projects: Vec::new(),
                worktrees: Vec::new(),
                sessions: Vec::new(),
                panes: Vec::new(),
                listening_ports: Vec::new(),
                pane_activity: Vec::new(),
                plugin_sidecars: Vec::new(),
                capabilities: current_capabilities(),
            }),
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    assert_eq!(
        Client::new(path).status().unwrap(),
        DaemonStatus::Compatible
    );
    assert!(daemon_needs_start(ExistingDaemon::Missing).unwrap());
    server.join().unwrap();
}

#[test]
fn pending_handoff_target_does_not_supersede_a_different_target() {
    let executable = std::env::current_exe().unwrap();
    let current_id = binary_identity(&executable).unwrap();
    let status = super::super::domain::DaemonLifecycle {
        protocol: 15,
        epoch: 7,
        binary_id: "0.28.0:1:2:3:1".into(),
        version: "0.28.0".into(),
        daemon_revision: 13,
        started_unix_ms: 1,
        phase: super::super::domain::DaemonPhase::ReplacementPending,
        live_runtimes: 1,
        active_clients: 1,
        active_tuis: 0,
        recovered_from_backup: false,
        replacement_target: Some("99.0.0:1:2:3:9".into()),
        replacement_target_version: "99.0.0".into(),
        replacement_blockers: vec![super::super::domain::ReplacementBlocker::ForegroundJob],
        last_handoff_error: None,
    };

    assert_eq!(
        pending_handoff_target(&status, &current_id, super::super::protocol::WSX_VERSION),
        None
    );
}

#[test]
fn older_pending_target_is_superseded_but_equal_or_newer_is_not() {
    let executable = std::env::current_exe().unwrap();
    let current_id = binary_identity(&executable).unwrap();
    let mut status = super::super::domain::DaemonLifecycle {
        protocol: 15,
        epoch: 7,
        binary_id: "0.26.2:1:2:3:1".into(),
        version: "0.26.2".into(),
        daemon_revision: 8,
        started_unix_ms: 1,
        phase: super::super::domain::DaemonPhase::ReplacementPending,
        live_runtimes: 11,
        active_clients: 1,
        active_tuis: 0,
        recovered_from_backup: false,
        replacement_target: Some("0.28.0:1:2:3:2".into()),
        replacement_target_version: "0.28.0".into(),
        replacement_blockers: vec![super::super::domain::ReplacementBlocker::WorkingAgent],
        last_handoff_error: None,
    };
    let candidate = |status: &super::super::domain::DaemonLifecycle| {
        pending_handoff_target(status, &current_id, super::super::protocol::WSX_VERSION)
    };
    assert_eq!(
        candidate(&status),
        Some((
            current_id.clone(),
            super::super::protocol::WSX_VERSION.into()
        ))
    );
    status.replacement_target_version = "0.28.1".into();
    assert_eq!(
        candidate(&status),
        None,
        "inconsistent lifecycle target fails closed"
    );
    status.replacement_target = Some(format!("{}:1:2:3:2", super::super::protocol::WSX_VERSION));
    assert_eq!(
        candidate(&status),
        None,
        "same-version different build remains protected"
    );
    status.replacement_target_version = "99.0.0".into();
    status.replacement_target = Some("99.0.0:1:2:3:2".into());
    assert_eq!(
        candidate(&status),
        None,
        "newer pending target remains protected"
    );
    status.replacement_target_version.clear();
    status.replacement_target = Some("malformed".into());
    assert_eq!(
        candidate(&status),
        None,
        "unknown target version remains protected"
    );
}

#[test]
fn matching_daemon_revision_reuses_a_different_wsx_build() {
    let (path, listener) = test_listener("matching-daemon-revision");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello { .. }
        ));
        let mut capabilities = current_capabilities();
        capabilities.daemon_revision_coordination = true;
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 7,
                capabilities,
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::LifecycleStatus
        );
        send_response(
            &mut stream,
            &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                protocol: PROTOCOL_VERSION,
                epoch: 7,
                binary_id: "0.22.1:1:2:3:10".into(),
                version: "0.22.1".into(),
                daemon_revision: super::super::protocol::DAEMON_REVISION,
                started_unix_ms: 1,
                phase: super::super::domain::DaemonPhase::Ready,
                live_runtimes: 1,
                active_clients: 1,
                active_tuis: 1,
                recovered_from_backup: false,
                replacement_target: None,
                replacement_target_version: String::new(),
                replacement_blockers: vec![],
                last_handoff_error: None,
            }),
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });
    let ready = ExistingDaemon::Ready {
        lifecycle_coordination: true,
        version_coordination: true,
        daemon_revision_coordination: true,
        live_handoff: false,
    };

    assert_eq!(
        ready_without_transition(&Client::new(path), &ready, Some("0.22.2:4:5:6:20")).unwrap(),
        Some(Availability::Current)
    );
    server.join().unwrap();
}

#[test]
fn lifecycle_ready_and_deferred_are_typed_healthy_outcomes() {
    let (path, listener) = test_listener("lifecycle-outcomes");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        for (phase, live_runtimes) in [
            (super::super::domain::DaemonPhase::Ready, 0),
            (super::super::domain::DaemonPhase::ReplacementPending, 2),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { .. }
            ));
            send_response(
                &mut stream,
                &Response::Hello {
                    protocol: PROTOCOL_VERSION,
                    epoch: 7,
                    capabilities: current_capabilities(),
                },
            );
            assert_eq!(
                read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
                Request::LifecycleStatus
            );
            send_response(
                &mut stream,
                &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                    protocol: PROTOCOL_VERSION,
                    epoch: 7,
                    binary_id: "target".into(),
                    version: "0.21.0".into(),
                    daemon_revision: 0,
                    started_unix_ms: 1,
                    phase,
                    live_runtimes,
                    active_clients: 1,
                    active_tuis: 1,
                    recovered_from_backup: false,
                    replacement_target: None,
                    replacement_target_version: "0.21.0".into(),
                    replacement_blockers: vec![],
                    last_handoff_error: None,
                }),
            );
        }
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });
    let client = Client::new(path);
    let ready = ExistingDaemon::Ready {
        lifecycle_coordination: true,
        version_coordination: true,
        daemon_revision_coordination: false,
        live_handoff: false,
    };
    assert_eq!(
        ready_without_transition(&client, &ready, Some("target")).unwrap(),
        Some(Availability::Current)
    );
    assert_eq!(
        ready_without_transition(&client, &ready, Some("target")).unwrap(),
        Some(Availability::ReplacementDeferred {
            daemon_version: "0.21.0".into(),
            target_version: "0.21.0".into(),
            live_runtimes: 2,
            blockers: vec![],
        })
    );
    server.join().unwrap();
}

#[test]
fn legacy_client_count_excludes_the_requester_and_own_tui_monitor() {
    assert!(!active_client_count_has_other_tui(1, 0));
    assert!(!active_client_count_has_other_tui(2, 1));
    assert!(active_client_count_has_other_tui(2, 0));
    assert!(active_client_count_has_other_tui(3, 1));
}

#[test]
fn newer_daemon_is_reused_without_a_downgrade_request() {
    let (path, listener) = test_listener("newer-daemon");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello { .. }
        ));
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 7,
                capabilities: current_capabilities(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::LifecycleStatus
        );
        send_response(
            &mut stream,
            &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                protocol: PROTOCOL_VERSION,
                epoch: 7,
                binary_id: "99.0.0:1:2:3:20".into(),
                version: "99.0.0".into(),
                daemon_revision: 0,
                started_unix_ms: 1,
                phase: super::super::domain::DaemonPhase::Ready,
                live_runtimes: 2,
                active_clients: 2,
                active_tuis: 1,
                recovered_from_backup: false,
                replacement_target: None,
                replacement_target_version: String::new(),
                replacement_blockers: vec![],
                last_handoff_error: None,
            }),
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });
    let ready = ExistingDaemon::Ready {
        lifecycle_coordination: true,
        version_coordination: true,
        daemon_revision_coordination: false,
        live_handoff: false,
    };
    assert_eq!(
        ready_without_transition(&Client::new(path), &ready, Some("0.22.0:1:2:3:10")).unwrap(),
        Some(Availability::NewerDaemon {
            daemon_version: "99.0.0".into()
        })
    );
    server.join().unwrap();
}

#[test]
fn bootstrap_lock_serializes_startup_owners() {
    let (path, listener) = test_listener("bootstrap-lock");
    drop(listener);
    let _ = std::fs::remove_file(&path);
    let first = acquire_bootstrap_lock(&path).unwrap();
    let contender_path = path.clone();
    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
    let contender = thread::spawn(move || {
        let lock = acquire_bootstrap_lock(&contender_path).unwrap();
        acquired_tx.send(()).unwrap();
        drop(lock);
    });
    assert!(acquired_rx
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    drop(first);
    acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    contender.join().unwrap();
    let _ = std::fs::remove_file(path.with_extension("bootstrap.lock"));
}

#[test]
fn successor_waits_for_the_daemon_singleton_lock_not_only_socket_removal() {
    let (seed, listener) = test_listener("singleton-release");
    drop(listener);
    let _ = std::fs::remove_file(&seed);
    let directory = seed.with_extension("state");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join("wsx.sock");
    let lock_path = directory.join("state.lock");
    let owner = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let contender_path = path.clone();
    let (released_tx, released_rx) = std::sync::mpsc::channel();
    let contender = thread::spawn(move || {
        wait_for_singleton_release(&contender_path).unwrap();
        released_tx.send(()).unwrap();
    });
    assert!(released_rx
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    drop(owner);
    released_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    contender.join().unwrap();
    let _ = std::fs::remove_file(lock_path);
    let _ = std::fs::remove_dir(directory);
}

#[test]
fn shared_start_budget_blocks_a_loop_and_explicit_recovery_resets_it() {
    let (path, listener) = test_listener("start-budget");
    drop(listener);
    let _ = std::fs::remove_file(&path);
    let mut lock = acquire_bootstrap_lock(&path).unwrap();
    for _ in 0..MAX_START_ATTEMPTS {
        record_start_attempt(&mut lock.file, false).unwrap();
    }
    assert!(record_start_attempt(&mut lock.file, false)
        .unwrap_err()
        .to_string()
        .contains("crash_loop"));
    record_start_attempt(&mut lock.file, true).unwrap();
    drop(lock);
    let _ = std::fs::remove_file(path.with_extension("bootstrap.lock"));
}

#[test]
fn custom_client_never_spawns_a_default_daemon() {
    let (path, listener) = test_listener("custom-no-recovery");
    drop(listener);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        ensure_background_available(&Client::new(path.clone()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!path.exists());
}

#[test]
fn intentional_stop_marker_disables_background_recovery() {
    let (path, listener) = test_listener("intentional-marker");
    drop(listener);
    std::fs::remove_file(&path).unwrap();
    let marker = path.with_extension("lifecycle");
    std::fs::write(&marker, "intentional\n").unwrap();
    std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!background_recovery_allowed(&path));
    assert_eq!(
        ensure_background_available(&Client {
            socket: path.clone(),
            automatic_start: true,
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::ConnectionAborted
    );
    assert_eq!(
        consume_planned_marker(&path, Some("target"), 1).unwrap(),
        PlannedStart::Intentional
    );
    assert_eq!(lifecycle_marker_reason(&path).as_deref(), Some("starting"));
    assert_eq!(
        consume_planned_marker(&path, Some("target"), 1).unwrap(),
        PlannedStart::None
    );
    std::fs::write(&marker, "login_ended\n").unwrap();
    assert!(!background_recovery_allowed(&path));
    std::fs::write(&marker, "replacement:other\n").unwrap();
    assert!(consume_planned_marker(&path, Some("target"), 1).is_err());
    std::fs::write(&marker, "replacement:other|1\n").unwrap();
    assert_eq!(
        consume_planned_marker(&path, Some("target"), 1).unwrap(),
        PlannedStart::Replacement
    );
    std::fs::write(&marker, "unexpected\n").unwrap();
    assert!(background_recovery_allowed(&path));
    let _ = std::fs::remove_file(marker);
    let _ = std::fs::remove_file(path);
}

#[test]
fn detached_spawn_is_session_leader_and_survives_hangup() {
    let mut command = Command::new("sh");
    command.arg("-c").arg("kill -HUP $$; exec sleep 5");

    let mut child = spawn_detached(&mut command).unwrap();
    thread::sleep(Duration::from_millis(20));
    let pid = child.id() as libc::pid_t;
    let session_id = unsafe { libc::getsid(pid) };
    let status = child.try_wait().unwrap();
    if status.is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
    }

    assert_eq!(session_id, pid, "detached daemon must own its session");
    assert!(status.is_none(), "detached daemon exited after SIGHUP");
}

#[cfg(target_os = "macos")]
#[test]
fn local_socket_peer_matches_the_current_user() {
    let (client, server) = UnixStream::pair().unwrap();
    validate_peer_owner(&server).unwrap();
    validate_peer_owner(&client).unwrap();
}

#[test]
fn same_protocol_daemon_without_required_capabilities_is_not_stopped() {
    let (path, listener) = test_listener("missing-capabilities");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    let error = daemon_needs_start(probe_existing_daemon(&Client::new(path)).unwrap()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("missing required capabilities"));
    assert!(error.to_string().contains("refusing automatic shutdown"));
    assert!(error.to_string().contains("wsx daemon stop"));
    server.join().unwrap();
}

#[test]
fn daemon_status_reports_an_answering_incompatible_daemon_as_running() {
    let (path, listener) = test_listener("status-incompatible");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        let capabilities = super::super::domain::Capabilities {
            lifecycle_coordination: true,
            ..Default::default()
        };
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: 15,
                epoch: 1,
                capabilities,
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::LifecycleStatus
        );
        send_response(
            &mut stream,
            &Response::Lifecycle(super::super::domain::DaemonLifecycle {
                protocol: 15,
                epoch: 1,
                binary_id: "0.28.0:1:2:3:4".into(),
                version: "0.28.0".into(),
                daemon_revision: 13,
                started_unix_ms: 1,
                phase: super::super::domain::DaemonPhase::ReplacementPending,
                live_runtimes: 3,
                active_clients: 2,
                active_tuis: 1,
                recovered_from_backup: false,
                replacement_target: Some("0.28.1:1:2:3:5".into()),
                replacement_target_version: "0.28.1".into(),
                replacement_blockers: vec![super::super::domain::ReplacementBlocker::WorkingAgent],
                last_handoff_error: None,
            }),
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    assert!(matches!(
        Client::new(path).status().unwrap(),
        DaemonStatus::Incompatible {
            daemon_protocol: Some(15),
            lifecycle: Some(lifecycle),
            lifecycle_error: None,
        } if lifecycle.phase == super::super::domain::DaemonPhase::ReplacementPending
            && lifecycle.live_runtimes == 3
            && lifecycle.active_clients == 2
            && lifecycle.active_tuis == 1
            && lifecycle.replacement_blockers
                == [super::super::domain::ReplacementBlocker::WorkingAgent]
    ));
    server.join().unwrap();
}

#[test]
fn daemon_status_reports_a_missing_socket_as_stopped() {
    let directory = std::env::current_dir()
        .unwrap()
        .join(".work/tests")
        .join(format!("missing-status-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("wsx.sock");
    let _ = std::fs::remove_file(&path);
    assert_eq!(Client::new(path).status().unwrap(), DaemonStatus::Missing);
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn protocol_mismatch_does_not_send_shutdown() {
    let (path, listener) = test_listener("protocol-skew");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        send_response(
            &mut stream,
            &Response::Error(super::super::protocol::ApiError::new(
                "protocol_mismatch",
                format!("client {PROTOCOL_VERSION}, daemon 8"),
            )),
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    let error = daemon_needs_start(probe_existing_daemon(&Client::new(path)).unwrap()).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("protocol mismatch"));
    assert!(error.to_string().contains("daemon protocol unknown"));
    assert!(error.to_string().contains("refusing automatic shutdown"));
    assert!(error.to_string().contains("wsx daemon stop"));
    server.join().unwrap();
}

#[test]
fn graceful_shutdown_waits_for_socket_cleanup() {
    let (path, listener) = test_listener("graceful-shutdown");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Shutdown
        );
        send_response(&mut stream, &Response::Ack { revision: 1 });
        drop(stream);
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    Client::new(path).shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn graceful_shutdown_surfaces_daemon_rejection() {
    let (path, listener) = test_listener("shutdown-rejection");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        ));
        send_response(
            &mut stream,
            &Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Shutdown
        );
        send_response(
            &mut stream,
            &Response::Error(super::super::protocol::ApiError::new(
                "shutdown_blocked",
                "still busy",
            )),
        );
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    let error = Client::new(path).shutdown().unwrap_err();
    assert!(error.to_string().contains("shutdown_blocked: still busy"));
    server.join().unwrap();
}

#[test]
fn graceful_shutdown_accepts_an_already_stopped_daemon() {
    let path = std::env::current_dir()
        .unwrap()
        .join(".work/s/already-stopped.sock");
    let _ = std::fs::remove_file(&path);
    Client::new(path).shutdown().unwrap();
}

#[test]
fn advertised_incompatible_daemon_shuts_down_on_the_handshake_connection() {
    let (path, listener) = test_listener("advertised-upgrade");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        ));
        let legacy_hello = serde_json::json!({
            "type": "hello",
            "data": {
                "protocol": PROTOCOL_VERSION - 1,
                "epoch": 1,
                "capabilities": {
                    "pane_splits": true,
                    "plugins": true,
                    "agent_reports": true,
                    "process_restore": false
                }
            }
        });
        stream
            .write_all(&encode_line(&legacy_hello).unwrap())
            .unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Shutdown
        );
        send_response(&mut stream, &Response::Ack { revision: 1 });
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    Client::new(path).shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn unadvertised_protocol_two_daemon_shuts_down_on_the_handshake_connection() {
    let (path, listener) = test_listener("protocol-two-upgrade");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut mismatch, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut mismatch, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        send_response(
            &mut mismatch,
            &Response::Error(super::super::protocol::ApiError::new(
                "protocol_mismatch",
                format!("client {PROTOCOL_VERSION}, daemon 2"),
            )),
        );

        for protocol in (3..PROTOCOL_VERSION).rev() {
            let (mut probe, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut probe, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { protocol }
            );
            send_response(
                &mut probe,
                &Response::Error(super::super::protocol::ApiError::new(
                    "protocol_mismatch",
                    format!("client {protocol}, daemon 2"),
                )),
            );
        }

        let (mut protocol_two, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut protocol_two, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello { protocol: 2 }
        );
        send_response(
            &mut protocol_two,
            &Response::Hello {
                protocol: 2,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            },
        );
        assert_eq!(
            read_json_line::<Request>(&mut protocol_two, MAX_RESPONSE_BYTES).unwrap(),
            Request::Shutdown
        );
        send_response(&mut protocol_two, &Response::Ack { revision: 1 });
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    Client::new(path).shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn legacy_daemon_upgrade_uses_separate_request_connection() {
    let (path, listener) = test_listener("legacy-upgrade");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut mismatch, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut mismatch, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        );
        send_response(
            &mut mismatch,
            &Response::Error(super::super::protocol::ApiError::new(
                "protocol_mismatch",
                format!("client {PROTOCOL_VERSION}, daemon 1"),
            )),
        );

        for protocol in (2..PROTOCOL_VERSION).rev() {
            let (mut probe, _) = listener.accept().unwrap();
            assert_eq!(
                read_json_line::<Request>(&mut probe, MAX_RESPONSE_BYTES).unwrap(),
                Request::Hello { protocol }
            );
            send_response(
                &mut probe,
                &Response::Error(super::super::protocol::ApiError::new(
                    "protocol_mismatch",
                    format!("client {protocol}, daemon 1"),
                )),
            );
        }

        let (mut hello, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut hello, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello { protocol: 1 }
        );
        send_response(
            &mut hello,
            &Response::Hello {
                protocol: 1,
                epoch: 1,
                capabilities: super::super::domain::Capabilities::default(),
            },
        );
        drop(hello);

        let (mut shutdown, _) = listener.accept().unwrap();
        assert_eq!(
            read_json_line::<Request>(&mut shutdown, MAX_RESPONSE_BYTES).unwrap(),
            Request::Shutdown
        );
        send_response(&mut shutdown, &Response::Ack { revision: 1 });
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    Client::new(path).shutdown().unwrap();
    server.join().unwrap();
}

#[test]
fn large_json_line_uses_bounded_buffered_reads() {
    struct CountingReader {
        inner: io::Cursor<Vec<u8>>,
        reads: Arc<AtomicUsize>,
    }
    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.read(buffer)
        }
    }

    let expected = "x".repeat(1024 * 1024);
    let mut encoded = serde_json::to_vec(&expected).unwrap();
    encoded.push(b'\n');
    let reads = Arc::new(AtomicUsize::new(0));
    let source = CountingReader {
        inner: io::Cursor::new(encoded),
        reads: Arc::clone(&reads),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, source);
    let actual: String = read_buffered_json_line(&mut reader, 2 * 1024 * 1024).unwrap();

    assert_eq!(actual, expected);
    assert!(reads.load(Ordering::Relaxed) < 32);
}

#[test]
fn terminal_stream_preserves_selection_updates_and_clipboard_order_after_subscribe_ack() {
    let (path, listener) = test_listener("buffered-subscribe");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::Hello {
                protocol: PROTOCOL_VERSION
            }
        ));
        let mut bytes = encode_line(&Response::Hello {
            protocol: PROTOCOL_VERSION,
            epoch: 1,
            capabilities: super::super::domain::Capabilities::default(),
        })
        .unwrap();
        stream.write_all(&bytes).unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::TerminalSubscribe {
                rows: 24,
                cols: 80,
                ..
            }
        ));
        bytes = encode_line(&Response::Ack { revision: 1 }).unwrap();
        bytes.extend(
            encode_line(&TerminalServerMessage::Update(TerminalUpdate::Full(
                TerminalFrame {
                    pane_id: PaneId(1),
                    terminal_id: TerminalId(2),
                    revision: 1,
                    cols: 1,
                    rows: 1,
                    cells: vec![Cell::default()],
                    cursor: Cursor {
                        x: 0,
                        y: 0,
                        visible: false,
                        blinking: false,
                        shape: 0,
                    },
                    selection: vec![TerminalSelectionRange {
                        row: 0,
                        start_col: 0,
                        end_col: 0,
                    }],
                },
            )))
            .unwrap(),
        );
        bytes.extend(
            encode_line(&TerminalServerMessage::Update(TerminalUpdate::Patch {
                pane_id: PaneId(1),
                terminal_id: TerminalId(2),
                base_revision: 1,
                revision: 2,
                cols: 1,
                rows: 1,
                changed_rows: Vec::new(),
                cursor: Cursor {
                    x: 0,
                    y: 0,
                    visible: false,
                    blinking: false,
                    shape: 0,
                },
                selection: Vec::new(),
            }))
            .unwrap(),
        );
        bytes.extend(
            encode_line(&TerminalServerMessage::ClipboardWrite(b"copied".to_vec())).unwrap(),
        );
        bytes.extend(encode_line(&TerminalServerMessage::Exited).unwrap());
        stream.write_all(&bytes).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    let stream = TerminalStream::connect(
        &Client::new(path),
        super::super::domain::PaneId(1),
        7,
        false,
        24,
        80,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut selections = Vec::new();
    let mut clipboard = None;
    loop {
        match stream.try_recv() {
            Ok(TerminalServerMessage::Update(TerminalUpdate::Full(frame))) => {
                selections.push(frame.selection)
            }
            Ok(TerminalServerMessage::Update(TerminalUpdate::Patch { selection, .. })) => {
                selections.push(selection)
            }
            Ok(TerminalServerMessage::ClipboardWrite(text)) => clipboard = Some(text),
            Ok(TerminalServerMessage::Exited) => break,
            Ok(message) => panic!("unexpected terminal message: {message:?}"),
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("terminal update missing after ACK: {error}"),
        }
    }
    assert_eq!(
        selections,
        vec![
            vec![TerminalSelectionRange {
                row: 0,
                start_col: 0,
                end_col: 0,
            }],
            Vec::new(),
        ]
    );
    assert_eq!(clipboard.as_deref(), Some(b"copied".as_slice()));
    drop(stream);
    server.join().unwrap();
}

#[test]
fn terminal_reader_shutdown_interrupts_full_update_queue() {
    let (mut server, client) = UnixStream::pair().unwrap();
    let (updates, receiver) = mpsc::sync_channel(1);
    updates.send(TerminalServerMessage::Exited).unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let reader_stopping = Arc::clone(&stopping);
    let (done, finished) = mpsc::channel();
    let reader = thread::spawn(move || {
        terminal_reader(BufReader::new(client), updates, &reader_stopping);
        done.send(()).unwrap();
    });

    server
        .write_all(&encode_line(&TerminalServerMessage::Exited).unwrap())
        .unwrap();
    thread::sleep(Duration::from_millis(50));
    stopping.store(true, Ordering::Release);

    assert!(finished.recv_timeout(Duration::from_secs(1)).is_ok());
    drop(receiver);
    reader.join().unwrap();
}

// ^ A frame stream from a daemon that sends heartbeats ends
// as `stream_disconnected` when nothing, not even a heartbeat, arrives within the idle limit.
#[test]
fn terminal_stream_ends_when_a_heartbeat_daemon_goes_silent() {
    let (path, listener) = test_listener("silent-subscribe");
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap();
        let hello = Response::Hello {
            protocol: PROTOCOL_VERSION,
            epoch: 1,
            capabilities: super::super::domain::Capabilities {
                stream_heartbeat: true,
                ..Default::default()
            },
        };
        stream.write_all(&encode_line(&hello).unwrap()).unwrap();
        assert!(matches!(
            read_json_line::<Request>(&mut stream, MAX_RESPONSE_BYTES).unwrap(),
            Request::TerminalSubscribe {
                heartbeat: true,
                ..
            }
        ));
        stream
            .write_all(&encode_line(&Response::Ack { revision: 1 }).unwrap())
            .unwrap();
        // The connection stays open and sends nothing until the client has given up. The client's
        // own heartbeats and its detach are read and dropped.
        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        while std::io::BufRead::read_line(&mut reader, &mut line).is_ok_and(|read| read > 0) {
            line.clear();
        }
        drop(listener);
        std::fs::remove_file(server_path).unwrap();
    });

    let stream = TerminalStream::connect(
        &Client::new(path),
        super::super::domain::PaneId(1),
        7,
        false,
        24,
        80,
    )
    .unwrap();
    let started = Instant::now();
    let message = loop {
        match stream.try_recv() {
            Ok(message) => break message,
            Err(mpsc::TryRecvError::Empty) if started.elapsed() < Duration::from_secs(20) => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("no end of a silent stream: {error}"),
        }
    };
    let TerminalServerMessage::Error(error) = message else {
        panic!("unexpected terminal message: {message:?}");
    };
    assert_eq!(error.code, "stream_disconnected");
    assert!(error.message.contains("sent nothing"), "{}", error.message);
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "{:?}",
        started.elapsed()
    );
    drop(stream);
    server.join().unwrap();
}

#[test]
fn event_monitor_reports_one_reason_per_outage_when_the_restart_fails() {
    // ^ A custom client cannot restart wsxd, so each pass fails the poll and then the restart.
    // The monitor must say it once, not flip between two texts.
    let (path, listener) = test_listener("monitor-one-reason");
    drop(listener);
    std::fs::remove_file(&path).unwrap();
    let (monitor, signals) = EventMonitor::start(Client::new(path)).unwrap();
    thread::sleep(Duration::from_millis(2_500));
    drop(monitor);
    let reasons: Vec<EventSignal> = signals.try_iter().collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    match &reasons[0] {
        EventSignal::Disconnected(reason) => assert!(reason.contains("restart failed"), "{reason}"),
        other => panic!("{other:?}"),
    }
}
