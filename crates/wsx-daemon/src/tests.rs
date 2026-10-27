
use super::*;

fn agent_test_persisted() -> Persisted {
    serde_json::from_str(
            r#"{
                "next_id":6,
                "projects":[{"id":1,"path":"/project","name":"project","revision":7}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":7}],
                "sessions":[{"id":3,"worktree_id":2,"label":"session","primary_pane":4,"focused_pane":4,"panes":[4],"layout":{"kind":"leaf","pane_id":4},"revision":7}],
                "panes":[{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":true,"revision":7}]
            }"#,
        )
        .unwrap()
}

fn agent_test_daemon(persisted: Persisted) -> (Arc<Daemon>, PathBuf) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let path = std::env::temp_dir().join(format!(
        "wsx-daemon-agent-test-{}-{}.json",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    // ^ A save-failure test puts a directory here; one killed before its cleanup leaves it
    // for a later process with the same pid.
    let _ = fs::remove_file(&path);
    let _ = fs::remove_dir(&path);
    let daemon = Arc::new(Daemon {
        state: Mutex::new(State {
            persisted,
            revision: 7,
            runtimes: HashMap::new(),
            runtime_generations: HashMap::from([(
                PaneId(4),
                "0000000000000001:0000000000000001".into(),
            )]),
            agent_wake_leases: HashMap::new(),
            agent_presence_leases: HashMap::new(),
            agent_wake_tokens: HashMap::new(),
            claude_reconciliations: HashMap::new(),
            terminal_operation_locks: HashMap::new(),
            listening_ports: HashMap::new(),
            foreground_jobs: HashSet::new(),
            listener_scan_complete: false,
            leases: HashMap::new(),
            transferred_streams: VecDeque::new(),
            events: VecDeque::new(),
            plugins: Vec::new(),
            plugin_events: VecDeque::new(),
            tui_clients: HashMap::new(),
            legacy_tui_until: None,
            replacement_target: None,
            replacement_daemon_revision: None,
            replacement_protocol: None,
            replacement_executable: None,
            handoff_in_progress: false,
            handoff_started: None,
            last_handoff_error: None,
            stop_reason: None,
            persistence_dirty: false,
            recovery_in_progress: false,
            deliveries_in_flight: 0,
            stopping: false,
        }),
        mutation_lock: Mutex::new(()),
        changed: Condvar::new(),
        plugin_changed: Condvar::new(),
        active_clients: Arc::new(AtomicUsize::new(0)),
        review_jobs: plugins::review::Registry::default(),
        epoch: 1,
        binary_id: "0.22.0:1:1:1:10".into(),
        started_unix_ms: 1,
        recovered_from_backup: false,
        next_runtime_generation: AtomicU64::new(1),
        state_path: path.clone(),
        lifecycle_path: path.with_extension("lifecycle"),
    });
    (daemon, path)
}

#[test]
fn stale_runtime_callback_cannot_publish_for_replacement_generation() {
    let (daemon, path, replacement, generation) = exchange_test_daemon();
    {
        let mut state = lock(&daemon.state);
        state.persisted.panes[0].exited = false;
    }
    let notify = runtime_notify(
        &daemon,
        PaneId(4),
        Some("0000000000000000:0000000000000001".into()),
    );
    assert_ne!(generation, "0000000000000000:0000000000000001");
    let (revision, event_count) = {
        let state = lock(&daemon.state);
        (state.revision, state.events.len())
    };

    notify();

    let state = lock(&daemon.state);
    assert_eq!(state.revision, revision);
    assert_eq!(state.events.len(), event_count);
    assert!(!state.persisted.panes[0].exited);
    assert!(state
        .runtimes
        .get(&PaneId(4))
        .is_some_and(|runtime| Arc::ptr_eq(runtime, &replacement)));
    drop(state);
    replacement.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn pending_agent_recovery_has_a_bounded_readiness_wait() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    assert_eq!(
        wait_for_recovered_agent(
            &daemon,
            PaneId(4),
            "0000000000000001:0000000000000001",
            "claude",
            Duration::ZERO,
        ),
        RecoveryReadiness::TimedOut
    );
    let _ = fs::remove_file(path);
}

#[test]
fn lifecycle_status_is_available_across_protocol_mismatch() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let (mut client, server) = UnixStream::pair().unwrap();
    let serving = thread::spawn(move || serve_client(server, daemon).unwrap());
    client
        .write_all(&encode_line(&Request::Hello { protocol: 999 }).unwrap())
        .unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(matches!(
        serde_json::from_str::<Response>(&line).unwrap(),
        Response::Hello {
            protocol: PROTOCOL_VERSION,
            ..
        }
    ));
    client
        .write_all(&encode_line(&Request::LifecycleStatus).unwrap())
        .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert!(matches!(
        serde_json::from_str::<Response>(&line).unwrap(),
        Response::Lifecycle(DaemonLifecycle {
            protocol: PROTOCOL_VERSION,
            ..
        })
    ));
    serving.join().unwrap();
    let _ = fs::remove_file(path);
}

#[test]
fn newer_replacement_stops_atomically_without_blockers() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let target = "0.23.0:1:2:3:20";
    let response = prepare_replacement(&daemon, target.into()).unwrap();
    assert_eq!(
        response,
        Response::Replacement {
            disposition: ReplacementDisposition::Stopping,
            live_runtimes: 0,
            daemon_version: WSX_VERSION.into(),
            target_version: "0.23.0".into(),
            blockers: vec![],
            use_current_daemon: false,
        }
    );
    let state = lock(&daemon.state);
    assert!(state.stopping);
    assert_eq!(state.stop_reason, Some(StopReason::Replacement));
    drop(state);
    let socket = path.with_extension("sock");
    cleanup(&daemon, &socket);
    assert_eq!(
        fs::read_to_string(path.with_extension("lifecycle"))
            .unwrap()
            .trim(),
        format!("replacement:{target}")
    );
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
    let _ = fs::remove_file(path.with_extension("lifecycle"));
}

#[test]
fn replacement_waits_for_work_and_servers_but_not_blocked_agent() {
    let pane_id = PaneId(4);
    let recovery_recipe = LaunchRecipe {
        command: vec!["/bin/cat".into()],
        initial_input: None,
        rows: 3,
        cols: 4,
    };
    let mut persisted = agent_test_persisted();
    persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
        .expect("test fixture must contain pane 4")
        .recovery = Some(recovery_recipe.clone());
    let (daemon, path) = agent_test_daemon(persisted);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &recovery_recipe,
        )
        .unwrap(),
    );
    let generation = {
        let mut state = lock(&daemon.state);
        state.runtimes.insert(pane_id, Arc::clone(&runtime));
        assert_eq!(
            replacement_blockers(&mut state, "0.23.0:1:2:3:20"),
            vec![ReplacementBlocker::ListenerScanPending]
        );
        state.listener_scan_complete = true;
        state.runtime_generations[&pane_id].clone()
    };
    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(pane_id, Some(generation.clone())),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    assert_eq!(
        prepare_replacement(&daemon, "0.23.0:1:2:3:20".into()).unwrap(),
        Response::Replacement {
            disposition: ReplacementDisposition::Deferred,
            live_runtimes: 1,
            daemon_version: WSX_VERSION.into(),
            target_version: "0.23.0".into(),
            blockers: vec![ReplacementBlocker::WorkingAgent],
            use_current_daemon: false,
        }
    );
    assert!(!lock(&daemon.state).stopping);
    assert!(!runtime.exited());

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(pane_id, Some(generation)),
        "pi".into(),
        AgentState::Blocked,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    {
        let mut state = lock(&daemon.state);
        state.foreground_jobs.insert(pane_id);
        state.listening_ports.insert(pane_id, vec![3000]);
    }
    assert!(!advance_replacement(&daemon));
    assert!(!runtime.exited());
    {
        let mut state = lock(&daemon.state);
        assert_eq!(
            replacement_blockers(&mut state, "0.23.0:1:2:3:20"),
            vec![
                ReplacementBlocker::ForegroundJob,
                ReplacementBlocker::ListeningPort,
            ]
        );
        state.foreground_jobs.clear();
    }
    assert!(!advance_replacement(&daemon));
    lock(&daemon.state).listening_ports.clear();
    assert!(advance_replacement(&daemon));
    assert!(!runtime.exited());

    let socket = path.with_extension("sock");
    cleanup(&daemon, &socket);
    assert!(runtime.exited());

    let persisted = load_state(&path).unwrap();
    let pane = persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("replacement cleanup must preserve pane identity 4");
    assert_eq!(pane.id, PaneId(4));
    let reloaded_recipe = pane
        .recovery
        .as_ref()
        .expect("replacement cleanup must preserve the recovery recipe");
    assert_eq!(reloaded_recipe.command, recovery_recipe.command);
    assert_eq!(reloaded_recipe.initial_input, recovery_recipe.initial_input);
    assert_eq!(reloaded_recipe.rows, recovery_recipe.rows);
    assert_eq!(reloaded_recipe.cols, recovery_recipe.cols);

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
    let _ = fs::remove_file(path.with_extension("lifecycle"));
}

#[test]
fn older_binary_never_downgrades_the_daemon() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    assert_eq!(
        prepare_replacement(&daemon, "0.20.0:1:2:3:20".into()).unwrap(),
        Response::Replacement {
            disposition: ReplacementDisposition::Deferred,
            live_runtimes: 0,
            daemon_version: WSX_VERSION.into(),
            target_version: "0.20.0".into(),
            blockers: vec![],
            use_current_daemon: true,
        }
    );
    assert!(lock(&daemon.state).replacement_target.is_none());
    let _ = fs::remove_file(path);
}

#[test]
fn live_handoff_waits_for_tuis_but_not_terminal_work() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let mut state = lock(&daemon.state);
    state.foreground_jobs.insert(PaneId(4));
    state.listening_ports.insert(PaneId(4), vec![3000]);
    state.tui_clients.insert(
        7,
        TuiPresenceLease {
            target_binary_id: "target".into(),
            target_daemon_revision: DAEMON_REVISION + 1,
            max_protocol: 0,
            follows: false,
            expires_at: Instant::now() + TUI_PRESENCE_TTL,
        },
    );
    assert_eq!(
        handoff_blockers(&mut state, "target"),
        vec![ReplacementBlocker::OtherTui]
    );
    state.tui_clients.clear();
    assert!(handoff_blockers(&mut state, "target").is_empty());
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn handoff_executable_validation_accepts_cross_version_metadata_identity() {
    let executable = std::env::current_exe().unwrap();
    let legacy_identity = binary_identity_with_version(&executable, "0.25.0").unwrap();

    assert_eq!(
        validate_handoff_executable(&executable, &legacy_identity).unwrap(),
        executable.canonicalize().unwrap()
    );
    assert!(validate_handoff_executable(&executable, "0.25.0:1:2:3:4").is_err());
}

#[test]
fn mutation_waiting_at_handoff_barrier_is_rejected_without_state_change() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let barrier = lock(&daemon.mutation_lock);
    let worker_daemon = Arc::clone(&daemon);
    let worker = thread::spawn(move || {
        handle(
            &worker_daemon,
            Request::SessionRename {
                session_id: SessionId(3),
                label: "raced".into(),
                expected_revision: 7,
            },
        )
    });
    lock(&daemon.state).handoff_in_progress = true;
    drop(barrier);

    assert!(matches!(
        worker.join().unwrap(),
        Response::Error(ApiError { code, .. }) if code == "daemon_replacing"
    ));
    assert_eq!(lock(&daemon.state).persisted.sessions[0].label, "session");
    let _ = fs::remove_file(path);
}

#[test]
fn rollback_preserves_remaining_terminal_lease_time() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let socket = path.with_extension("sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let before = Instant::now() + Duration::from_secs(1);
    {
        let mut state = lock(&daemon.state);
        state.handoff_in_progress = true;
        state.handoff_started = Some(Instant::now() - Duration::from_secs(2));
        state.leases.insert(
            PaneId(4),
            Lease {
                client_id: 8,
                generation: 9,
                expires_at: before,
            },
        );
    }
    let mut listener = Some(listener);
    rollback_handoff(
        &daemon,
        &socket,
        &mut listener,
        &[],
        "validate",
        &io::Error::other("test"),
    );
    let state = lock(&daemon.state);
    assert!(!state.handoff_in_progress);
    assert!(state.leases[&PaneId(4)].expires_at >= before + Duration::from_secs(2));
    drop(state);
    drop(listener);
    let _ = fs::remove_file(socket);
    let _ = fs::remove_file(path);
}

#[test]
fn active_other_tui_defers_replacement_until_its_presence_expires() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    record_tui_presence(
        &mut lock(&daemon.state),
        Some(TuiClientPresence {
            instance_id: 7,
            version: "0.20.0".into(),
            target_binary_id: "0.20.0:1:2:3:10".into(),
            target_daemon_revision: 0,
            max_protocol: 0,
            follows: false,
        }),
    )
    .unwrap();
    assert!(matches!(
        prepare_replacement(&daemon, "0.22.0:1:2:3:20".into())
        .unwrap(),
        Response::Replacement {
            disposition: ReplacementDisposition::Deferred,
            blockers,
            ..
        } if blockers == vec![ReplacementBlocker::OtherTui]
    ));
    assert!(!advance_replacement(&daemon));
    lock(&daemon.state)
        .tui_clients
        .get_mut(&7)
        .unwrap()
        .expires_at = Instant::now();
    assert!(advance_replacement(&daemon));
    let _ = fs::remove_file(path);
}

#[test]
fn same_version_different_build_tui_blocks_replacement_until_expiry() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    record_tui_presence(
        &mut lock(&daemon.state),
        Some(TuiClientPresence {
            instance_id: 8,
            version: "0.22.0".into(),
            target_binary_id: "0.22.0:1:2:3:10".into(),
            target_daemon_revision: 0,
            max_protocol: 0,
            follows: false,
        }),
    )
    .unwrap();

    assert!(matches!(
        prepare_replacement(&daemon, "0.22.0:1:2:3:20".into()).unwrap(),
        Response::Replacement {
            disposition: ReplacementDisposition::Deferred,
            blockers,
            ..
        } if blockers == vec![ReplacementBlocker::OtherTui]
    ));
    assert!(!advance_replacement(&daemon));
    lock(&daemon.state)
        .tui_clients
        .get_mut(&8)
        .unwrap()
        .expires_at = Instant::now();
    assert!(advance_replacement(&daemon));
    let _ = fs::remove_file(path);
}

#[test]
fn daemon_revision_controls_replacement_and_tui_build_compatibility() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    assert!(matches!(
        prepare_replacement_with_revision(&daemon, "0.23.0:1:2:3:20".into(), DAEMON_REVISION,)
            .unwrap(),
        Response::Replacement {
            use_current_daemon: true,
            ..
        }
    ));
    assert!(lock(&daemon.state).replacement_target.is_none());

    let mut state = lock(&daemon.state);
    state.replacement_target = Some("0.23.0:1:2:3:20".into());
    state.replacement_daemon_revision = Some(DAEMON_REVISION + 1);
    record_tui_presence(
        &mut state,
        Some(TuiClientPresence {
            instance_id: 10,
            version: "0.23.1".into(),
            target_binary_id: "0.23.1:4:5:6:30".into(),
            target_daemon_revision: DAEMON_REVISION + 1,
            max_protocol: 0,
            follows: false,
        }),
    )
    .unwrap();
    assert!(!replacement_blockers(&mut state, "0.23.0:1:2:3:20")
        .contains(&ReplacementBlocker::OtherTui));
    state
        .tui_clients
        .get_mut(&10)
        .unwrap()
        .target_daemon_revision = DAEMON_REVISION;
    assert!(
        replacement_blockers(&mut state, "0.23.0:1:2:3:20").contains(&ReplacementBlocker::OtherTui)
    );
    state.stop_reason = Some(StopReason::Replacement);
    drop(state);
    cleanup(&daemon, &path.with_extension("sock"));
    assert_eq!(
        fs::read_to_string(path.with_extension("lifecycle"))
            .unwrap()
            .trim(),
        format!("replacement:0.23.0:1:2:3:20|{}", DAEMON_REVISION + 1)
    );
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("lifecycle"));
}

#[test]
fn malformed_or_empty_tui_presence_is_rejected() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    for (version, target_binary_id) in [
        ("", "0.22.0:1:2:3:20"),
        ("0.22.0", ""),
        ("not-semver", "0.22.0:1:2:3:20"),
        ("0.22.0", "malformed"),
        ("0.22.0", "0.22.0:1:2:3:not-hex"),
    ] {
        assert!(
            record_tui_presence(
                &mut lock(&daemon.state),
                Some(TuiClientPresence {
                    instance_id: 9,
                    version: version.into(),
                    target_binary_id: target_binary_id.into(),
                    target_daemon_revision: 0,
                    max_protocol: 0,
                    follows: false,
                }),
            )
            .is_err(),
            "presence ({version:?}, {target_binary_id:?}) must be rejected"
        );
    }
    assert!(lock(&daemon.state).tui_clients.is_empty());
    let _ = fs::remove_file(path);
}

#[test]
fn newer_pending_build_supersedes_stale_target_without_ping_pong() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    record_tui_presence(&mut lock(&daemon.state), None).unwrap();
    for target in ["0.22.0:1:2:3:20", "0.22.0:1:2:3:30"] {
        assert!(matches!(
            prepare_replacement(&daemon, target.into()).unwrap(),
            Response::Replacement {
                disposition: ReplacementDisposition::Deferred,
                ..
            }
        ));
    }
    assert_eq!(
        lock(&daemon.state).replacement_target.as_deref(),
        Some("0.22.0:1:2:3:30")
    );
    assert!(matches!(
        prepare_replacement(&daemon, "0.22.0:1:2:3:25".into())
        .unwrap(),
        Response::Replacement { blockers, .. }
            if blockers == vec![ReplacementBlocker::PendingTarget]
    ));
    assert_eq!(
        lock(&daemon.state).replacement_target.as_deref(),
        Some("0.22.0:1:2:3:30")
    );

    assert!(matches!(
        prepare_replacement(&daemon, "0.23.0:1:2:3:10".into()).unwrap(),
        Response::Replacement {
            disposition: ReplacementDisposition::Deferred,
            ..
        }
    ));
    assert_eq!(
        lock(&daemon.state).replacement_target.as_deref(),
        Some("0.23.0:1:2:3:10")
    );
    assert!(matches!(
        prepare_replacement(&daemon, "0.22.0:1:2:3:99".into()).unwrap(),
        Response::Replacement { blockers, .. }
            if blockers == vec![ReplacementBlocker::PendingTarget]
    ));
    assert_eq!(
        lock(&daemon.state).replacement_target.as_deref(),
        Some("0.23.0:1:2:3:10")
    );
    let _ = fs::remove_file(path);
}

#[test]
fn wake_lease_requires_fresh_working_report_and_live_runtime() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    let generation = {
        let mut state = lock(&daemon.state);
        state.runtimes.insert(pane_id, Arc::clone(&runtime));
        state.runtime_generations[&pane_id].clone()
    };

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(pane_id, Some(generation.clone())),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    assert!(has_fresh_working_agent(
        &mut lock(&daemon.state),
        Instant::now()
    ));

    lock(&daemon.state)
        .agent_wake_leases
        .insert(pane_id, Instant::now() - AGENT_WAKE_LEASE_TTL);
    assert!(!has_fresh_working_agent(
        &mut lock(&daemon.state),
        Instant::now()
    ));

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(pane_id, Some(generation)),
        "pi".into(),
        AgentState::Blocked,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    assert!(!has_fresh_working_agent(
        &mut lock(&daemon.state),
        Instant::now()
    ));
    runtime.terminate();
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
}

#[test]
fn stale_stream_generation_cannot_use_or_release_reacquired_same_client_lease() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    {
        let mut state = lock(&daemon.state);
        state.leases.insert(
            pane_id,
            Lease {
                client_id: 9,
                generation: 2,
                expires_at: Instant::now() + LEASE_TTL,
            },
        );
        let stale = LeaseAccess::Stream {
            client_id: 9,
            generation: 1,
        };
        assert_eq!(
            require_lease(&state, pane_id, stale).unwrap_err().code,
            "lease_required"
        );
    }

    release_stream_lease(&daemon, pane_id, 9, 1);
    assert!(lock(&daemon.state).leases.contains_key(&pane_id));
    release_stream_lease(&daemon, pane_id, 9, 2);
    assert!(!lock(&daemon.state).leases.contains_key(&pane_id));
    let _ = fs::remove_file(path);
}

#[test]
fn stream_lease_classifies_live_replacement_as_transfer() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let now = Instant::now();
    let displaced = StreamLease {
        pane_id: PaneId(4),
        client_id: 9,
        generation: 1,
    };
    {
        let mut state = lock(&daemon.state);
        state.leases.insert(
            PaneId(4),
            Lease {
                client_id: 10,
                generation: 2,
                expires_at: now + LEASE_TTL,
            },
        );
        record_transferred_stream(&mut state, displaced);
    }

    let state = lock(&daemon.state);
    assert_eq!(
        terminal_stream_lease_error(&state, displaced, now)
            .unwrap()
            .code,
        "lease_transferred"
    );
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn stream_lease_classifies_expiry_missing_and_stopping_as_lost() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let now = Instant::now();
    let stream = StreamLease {
        pane_id: PaneId(4),
        client_id: 9,
        generation: 1,
    };

    {
        let mut state = lock(&daemon.state);
        state.leases.insert(
            PaneId(4),
            Lease {
                client_id: 9,
                generation: 1,
                expires_at: now,
            },
        );
        assert_eq!(
            terminal_stream_lease_error(&state, stream, now)
                .unwrap()
                .code,
            "lease_lost"
        );

        state.leases.remove(&PaneId(4));
        assert_eq!(
            terminal_stream_lease_error(&state, stream, now)
                .unwrap()
                .code,
            "lease_lost"
        );

        state.leases.insert(
            PaneId(4),
            Lease {
                client_id: 9,
                generation: 1,
                expires_at: now + LEASE_TTL,
            },
        );
        state.stopping = true;
        assert_eq!(
            terminal_stream_lease_error(&state, stream, now)
                .unwrap()
                .code,
            "lease_lost"
        );
    }
    let _ = fs::remove_file(path);
}

#[test]
fn expired_lease_reacquisition_remains_lease_lost() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    let (_, expired_generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    lock(&daemon.state)
        .leases
        .get_mut(&pane_id)
        .unwrap()
        .expires_at = Instant::now();
    let (_, replacement_generation) = acquire_terminal_lease(&daemon, pane_id, 10, true).unwrap();

    let state = lock(&daemon.state);
    assert_eq!(
        terminal_stream_lease_error(
            &state,
            StreamLease {
                pane_id,
                client_id: 9,
                generation: expired_generation,
            },
            Instant::now(),
        )
        .unwrap()
        .code,
        "lease_lost"
    );
    drop(state);

    release_stream_lease(&daemon, pane_id, 10, replacement_generation);
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn repeated_takeovers_bound_provenance_when_displaced_clients_do_not_release() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));

    let (_, first_generation) = acquire_terminal_lease(&daemon, pane_id, 1, true).unwrap();
    let oldest = StreamLease {
        pane_id,
        client_id: 1,
        generation: first_generation,
    };
    let mut latest_displaced = oldest;
    let mut current_generation = first_generation;
    let mut current_client = 1;
    for client_id in 2..=(MAX_CLIENTS as u64 + 2) {
        latest_displaced = StreamLease {
            pane_id,
            client_id: current_client,
            generation: current_generation,
        };
        (_, current_generation) =
            acquire_terminal_lease(&daemon, pane_id, client_id, true).unwrap();
        current_client = client_id;
    }

    let state = lock(&daemon.state);
    assert_eq!(state.transferred_streams.len(), MAX_CLIENTS);
    assert!(!state.transferred_streams.contains(&oldest));
    assert!(state.transferred_streams.contains(&latest_displaced));
    assert_eq!(
        terminal_stream_lease_error(&state, oldest, Instant::now())
            .unwrap()
            .code,
        "lease_lost"
    );
    assert_eq!(
        terminal_stream_lease_error(&state, latest_displaced, Instant::now())
            .unwrap()
            .code,
        "lease_transferred"
    );
    drop(state);

    release_stream_lease(&daemon, pane_id, current_client, current_generation);
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn displaced_terminal_stream_receives_transfer_notice() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    let (_, displaced_generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    let (mut server_stream, client_stream) = UnixStream::pair().unwrap();
    client_stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let resync = Arc::new(AtomicBool::new(true));
    let input_error = Arc::new(Mutex::new(None));
    let stream_daemon = Arc::clone(&daemon);
    let stream_active = Arc::clone(&active);
    let stream_resync = Arc::clone(&resync);
    let stream_input_error = Arc::clone(&input_error);
    let stream_thread = thread::spawn(move || {
        stream_terminal_updates(
            &mut server_stream,
            &stream_daemon,
            StreamLease {
                pane_id,
                client_id: 9,
                generation: displaced_generation,
            },
            &stream_active,
            &stream_resync,
            &stream_input_error,
        )
    });

    let (_, replacement_generation) = acquire_terminal_lease(&daemon, pane_id, 10, true).unwrap();
    daemon.changed.notify_all();
    let mut reader = BufReader::new(client_stream);
    let error = (0..3)
        .find_map(|_| {
            let line = read_bounded_line(&mut reader).unwrap().unwrap();
            match serde_json::from_str::<TerminalServerMessage>(&line).unwrap() {
                TerminalServerMessage::Error(error) => Some(error),
                _ => None,
            }
        })
        .expect("displaced stream transfer error");
    assert_eq!(error.code, "lease_transferred");
    assert_eq!(
        error.message,
        "terminal control moved to another wsx instance"
    );

    active.store(false, Ordering::Release);
    daemon.changed.notify_all();
    stream_thread.join().unwrap().unwrap();
    release_stream_lease(&daemon, pane_id, 10, replacement_generation);
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn explicit_other_client_takeover_rotates_authority_and_fences_every_old_operation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));

    let (_, displaced_generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    let (_, replacement_generation) = acquire_terminal_lease(&daemon, pane_id, 10, true).unwrap();
    assert_ne!(displaced_generation, replacement_generation);
    {
        let state = lock(&daemon.state);
        let lease = state.leases.get(&pane_id).unwrap();
        assert_eq!(lease.client_id, 10);
        assert_eq!(lease.generation, replacement_generation);
    }

    for message in [
        TerminalClientMessage::Input(b"stale input".to_vec()),
        TerminalClientMessage::Resize { rows: 8, cols: 9 },
        TerminalClientMessage::Heartbeat,
    ] {
        assert_eq!(
            handle_terminal_stream_input(&daemon, pane_id, 9, displaced_generation, message,)
                .unwrap_err()
                .code,
            "lease_required"
        );
    }

    for mouse in [
        MouseEvent {
            action: MouseAction::Press,
            button: MouseButton::Left,
            x: 0,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
        MouseEvent {
            action: MouseAction::Motion,
            button: MouseButton::Left,
            x: 2,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
    ] {
        with_stream_runtime(&daemon, pane_id, 10, replacement_generation, |runtime| {
            runtime.mouse(&mouse).map_err(terminal_api)
        })
        .unwrap();
    }
    let selection = || match runtime
        .presentation_sample(None, true)
        .update
        .unwrap()
        .unwrap()
    {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert!(!selection().is_empty());

    release_stream_lease(&daemon, pane_id, 9, displaced_generation);
    assert!(!selection().is_empty());
    let state = lock(&daemon.state);
    let lease = state.leases.get(&pane_id).unwrap();
    assert_eq!(lease.client_id, 10);
    assert_eq!(lease.generation, replacement_generation);
    drop(state);

    release_stream_lease(&daemon, pane_id, 10, replacement_generation);
    runtime.terminate();
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
}

#[test]
fn only_the_current_lease_generation_clears_terminal_selection() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    let (_, generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    for mouse in [
        MouseEvent {
            action: MouseAction::Press,
            button: MouseButton::Left,
            x: 0,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
        MouseEvent {
            action: MouseAction::Motion,
            button: MouseButton::Left,
            x: 2,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
    ] {
        with_stream_runtime(&daemon, pane_id, 9, generation, |runtime| {
            runtime.mouse(&mouse).map_err(terminal_api)
        })
        .unwrap();
    }
    let selection = || match runtime
        .presentation_sample(None, true)
        .update
        .unwrap()
        .unwrap()
    {
        TerminalUpdate::Full(frame) => frame.selection,
        TerminalUpdate::Patch { .. } => panic!("expected full frame"),
    };
    assert!(!selection().is_empty());

    let (_, replacement_generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    assert!(selection().is_empty());
    for mouse in [
        MouseEvent {
            action: MouseAction::Press,
            button: MouseButton::Left,
            x: 0,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
        MouseEvent {
            action: MouseAction::Motion,
            button: MouseButton::Left,
            x: 2,
            y: 0,
            in_bounds: true,
            shift: false,
            control: false,
            alt: false,
            super_key: false,
        },
    ] {
        with_stream_runtime(&daemon, pane_id, 9, replacement_generation, |runtime| {
            runtime.mouse(&mouse).map_err(terminal_api)
        })
        .unwrap();
    }
    assert!(!selection().is_empty());

    release_stream_lease(&daemon, pane_id, 9, generation);
    assert!(!selection().is_empty());
    assert!(lock(&daemon.state).leases.contains_key(&pane_id));

    release_stream_lease(&daemon, pane_id, 9, replacement_generation);
    assert!(selection().is_empty());
    assert!(!lock(&daemon.state).leases.contains_key(&pane_id));

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn terminal_takeover_waits_for_the_inflight_generation_operation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    let (_, generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();
    let operation_lock = terminal_operation_lock(&daemon, pane_id);
    let operation = lock(&operation_lock);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let takeover_daemon = Arc::clone(&daemon);
    let takeover = thread::spawn(move || {
        started_tx.send(()).unwrap();
        completed_tx
            .send(acquire_terminal_lease(&takeover_daemon, pane_id, 9, true))
            .unwrap();
    });

    started_rx.recv().unwrap();
    assert!(completed_rx
        .recv_timeout(Duration::from_millis(50))
        .is_err());
    drop(operation);
    let (_, takeover_generation) = completed_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    takeover.join().unwrap();

    assert_ne!(generation, takeover_generation);
    assert_eq!(
        with_stream_runtime(&daemon, pane_id, 9, generation, |_| Ok(()))
            .unwrap_err()
            .code,
        "lease_required"
    );
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn legacy_agent_report_promotes_conversation_id_to_session_ref() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());

    let response = agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
        "codex".into(),
        AgentState::Idle,
        Some("legacy-id".into()),
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    assert!(matches!(response, Response::Ack { revision: 8 }));

    let state = lock(&daemon.state);
    let agent = state.persisted.panes[0].agent.as_ref().unwrap();
    assert_eq!(agent.conversation_id.as_deref(), Some("legacy-id"));
    assert_eq!(
        agent.session_ref.as_ref().unwrap().kind,
        AgentSessionRefKind::Id
    );
    assert_eq!(agent.session_ref.as_ref().unwrap().value, "legacy-id");
    assert!(agent.capabilities.resume);
    drop(state);
    let persisted = load_state(&path).unwrap();
    let agent = persisted.panes[0].agent.as_ref().unwrap();
    assert_eq!(agent.conversation_id.as_deref(), Some("legacy-id"));
    assert_eq!(
        agent.session_ref.as_ref().unwrap().kind,
        AgentSessionRefKind::Id
    );
    assert_eq!(agent.session_ref.as_ref().unwrap().value, "legacy-id");
    assert!(agent.capabilities.resume);
    fs::remove_file(path).unwrap();
}

#[test]
fn resident_presence_expires_without_erasing_resume_identity_or_event_only_agents() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let id = "9ef119f4-c114-4786-b58b-63ea29f96359";
    {
        let mut state = lock(&daemon.state);
        state.persisted.panes[0].exited = false;
    }
    let invalid = agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "pi".into(),
            state: AgentState::Working,
            attached: true,
            presence_id: Some("unbounded".into()),
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities::default(),
            received: None,
            compacting: false,
        },
    )
    .unwrap_err();
    assert_eq!(invalid.code, "invalid_presence");
    assert!(lock(&daemon.state).persisted.panes[0].agent.is_none());
    agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "pi".into(),
            state: AgentState::Working,
            attached: true,
            presence_id: Some(id.into()),
            conversation_id: Some("resumable".into()),
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities::default(),
            received: None,
            compacting: false,
        },
    )
    .unwrap();
    let generation = lock(&daemon.state).runtime_generations[&pane_id].clone();
    let revision = lock(&daemon.state).revision;
    let persisted_bytes = fs::read(&path).unwrap();
    assert!(
        matches!(renew_agent_presence(&daemon, pane_id, generation.clone(), id.into()),
            Ok(Response::Ack { revision: renewed }) if renewed == revision)
    );
    assert_eq!(fs::read(&path).unwrap(), persisted_bytes);
    assert_eq!(lock(&daemon.state).revision, revision);
    let renewed_at = lock(&daemon.state).agent_presence_leases[&pane_id];
    expire_agent_presence(
        &daemon,
        renewed_at + AGENT_PRESENCE_TTL - Duration::from_millis(1),
    );
    assert!(
        lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .attached
    );
    assert_eq!(
        renew_agent_presence(&daemon, pane_id, "bad:0001".into(), id.into())
            .unwrap_err()
            .code,
        "stale_runtime"
    );
    assert_eq!(
        renew_agent_presence(
            &daemon,
            pane_id,
            generation.clone(),
            "00000000-0000-0000-0000-000000000000".into()
        )
        .unwrap_err()
        .code,
        "stale_presence"
    );
    expire_agent_presence(&daemon, renewed_at + AGENT_PRESENCE_TTL);
    let state = lock(&daemon.state);
    let agent = state.persisted.panes[0].agent.as_ref().unwrap();
    assert!(!agent.attached);
    assert_eq!(agent.state, AgentState::Unknown);
    assert_eq!(agent.presence_id, None);
    assert_eq!(agent.conversation_id.as_deref(), Some("resumable"));
    assert!(!state.agent_wake_leases.contains_key(&pane_id));
    assert!(state.revision > revision);
    drop(state);
    assert!(
        !load_state(&path).unwrap().panes[0]
            .agent
            .as_ref()
            .unwrap()
            .attached
    );
    assert_eq!(
        renew_agent_presence(&daemon, pane_id, generation.clone(), id.into())
            .unwrap_err()
            .code,
        "stale_presence"
    );
    let replacement_id = "2126bf38-610e-42d6-8a8c-1c233bb0a152";
    agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "pi".into(),
            state: AgentState::Idle,
            attached: true,
            presence_id: Some(replacement_id.into()),
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities::default(),
            received: None,
            compacting: false,
        },
    )
    .unwrap();
    assert_eq!(
        renew_agent_presence(&daemon, pane_id, generation.clone(), id.into())
            .unwrap_err()
            .code,
        "stale_presence"
    );
    assert!(matches!(
        renew_agent_presence(&daemon, pane_id, generation, replacement_id.into()),
        Ok(Response::Ack { .. })
    ));
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    expire_agent_presence(&daemon, Instant::now() + Duration::from_secs(3600));
    assert!(
        lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .attached
    );
    let _ = fs::remove_file(path);
}

#[test]
fn simultaneous_presence_expiry_detaches_each_pane_in_saved_snapshot() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let first = PaneId(4);
    let second = PaneId(6);
    let generation = "0000000000000001:0000000000000002";
    {
        let mut state = lock(&daemon.state);
        let mut pane = state.persisted.panes[0].clone();
        pane.id = second;
        pane.terminal_id = TerminalId(7);
        state.persisted.panes.push(pane);
        state.persisted.sessions[0].panes.push(second);
        assert!(state.persisted.sessions[0]
            .layout
            .split(first, second, SplitAxis::Vertical));
        state.persisted.next_id = 8;
        state.runtime_generations.insert(second, generation.into());
    }
    for (pane_id, presence_id) in [
        (first, "f3d17df9-7ce4-4921-9572-ff5fd6189003"),
        (second, "b40262be-6a45-43f2-a171-6ced0a3eb62a"),
    ] {
        let generation = lock(&daemon.state).runtime_generations[&pane_id].clone();
        agent_report_with_attachment(
            &daemon,
            RuntimeAgentAuthority::new(pane_id, Some(generation)),
            AgentReportInput {
                provider: "pi".into(),
                state: AgentState::Working,
                attached: true,
                presence_id: Some(presence_id.into()),
                conversation_id: None,
                session_ref: None,
                wake_token: None,
                capabilities: AgentCapabilities::default(),
                received: None,
                compacting: false,
            },
        )
        .unwrap();
    }
    let expired_at = lock(&daemon.state)
        .agent_presence_leases
        .values()
        .copied()
        .max()
        .unwrap()
        + AGENT_PRESENCE_TTL;
    let before = lock(&daemon.state).revision;
    expire_agent_presence(&daemon, expired_at);
    let state = lock(&daemon.state);
    assert_eq!(state.revision, before + 2);
    assert!(state
        .persisted
        .panes
        .iter()
        .all(|pane| !pane.agent.as_ref().unwrap().attached));
    drop(state);
    assert!(load_state(&path).unwrap().panes.iter().all(|pane| !pane
        .agent
        .as_ref()
        .unwrap()
        .attached));
    let _ = fs::remove_file(path);
}

#[test]
fn expired_presence_retries_failed_persistence_without_restoring_working() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let id = "c4773b42-8707-476d-a702-7a7c3e9bc67d";
    agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "pi".into(),
            state: AgentState::Working,
            attached: true,
            presence_id: Some(id.into()),
            conversation_id: Some("saved".into()),
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities::default(),
            received: None,
            compacting: false,
        },
    )
    .unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let renewed_at = lock(&daemon.state).agent_presence_leases[&pane_id];
    expire_agent_presence(&daemon, renewed_at + AGENT_PRESENCE_TTL);
    {
        let state = lock(&daemon.state);
        assert!(state.persistence_dirty);
        let agent = state.persisted.panes[0].agent.as_ref().unwrap();
        assert!(!agent.attached);
        assert_eq!(agent.state, AgentState::Unknown);
        assert_eq!(agent.conversation_id.as_deref(), Some("saved"));
    }
    fs::remove_dir(&path).unwrap();
    retry_dirty_persistence(&daemon);
    assert!(!lock(&daemon.state).persistence_dirty);
    let persisted = load_state(&path).unwrap();
    let agent = persisted.panes[0].agent.as_ref().unwrap();
    assert!(!agent.attached);
    assert_eq!(agent.conversation_id.as_deref(), Some("saved"));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
}

#[test]
fn detached_report_preserves_resume_metadata_and_releases_live_authority() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let authority = current_agent_authority(&daemon);
    agent_report(
        &daemon,
        authority,
        "codex".into(),
        AgentState::Working,
        Some(CODEX_SESSION_ID.into()),
        None,
        AgentCapabilities::default(),
    )
    .unwrap();
    assert!(lock(&daemon.state)
        .agent_wake_leases
        .contains_key(&PaneId(4)));

    agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "codex".into(),
            state: AgentState::Done,
            attached: false,
            presence_id: None,
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities::default(),
            received: None,
            compacting: false,
        },
    )
    .unwrap();

    let state = lock(&daemon.state);
    let agent = state.persisted.panes[0].agent.as_ref().unwrap();
    assert!(!agent.attached);
    assert_eq!(agent.state, AgentState::Unknown);
    assert_eq!(agent.conversation_id.as_deref(), Some(CODEX_SESSION_ID));
    assert_eq!(
        agent
            .session_ref
            .as_ref()
            .map(|session_ref| session_ref.value.as_str()),
        Some(CODEX_SESSION_ID)
    );
    assert!(agent.capabilities.resume);
    assert!(!state.agent_wake_leases.contains_key(&PaneId(4)));
    let mut resumable = agent.clone();
    drop(state);

    let launch = recovery_launch(Some(&mut resumable), &recovery_test_recipe(), true);
    assert_eq!(
        launch.recipe.command,
        vec![
            "codex".to_string(),
            "resume".to_string(),
            CODEX_SESSION_ID.to_string(),
        ]
    );
    let _ = fs::remove_file(path);
}

#[test]
fn agent_report_rejects_provider_mismatch_without_mutation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

    let error = agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
        "claude".into(),
        AgentState::Working,
        None,
        Some(AgentSessionRef::path("/absolute/session").unwrap()),
        AgentCapabilities::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, "invalid_agent_session");

    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    assert!(!path.exists());
}

#[test]
fn agent_report_rejects_invalid_structured_id_without_mutation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

    let error = agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
        "codex".into(),
        AgentState::Working,
        None,
        Some(AgentSessionRef {
            kind: AgentSessionRefKind::Id,
            value: "invalid\nid".into(),
            transcript_path: None,
        }),
        AgentCapabilities::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, "invalid_agent_session");

    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    assert!(!path.exists());
}

#[test]
fn terminal_agent_environment_exposes_stable_pane_identity() {
    let environment = terminal_agent_environment(PaneId(42), "0000000000000001:0000000000000001");
    assert!(environment
        .iter()
        .any(|(name, value)| name == WSX_PANE_ID_ENV && value == "42"));
    assert!(environment.iter().any(|(name, value)| {
        name == WSX_RUNTIME_GENERATION_ENV && value == "0000000000000001:0000000000000001"
    }));
}

#[test]
fn agent_report_rejects_missing_or_stale_runtime_generation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

    for generation in [None, Some("0000000000000001:0000000000000002".into())] {
        let error = agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), generation),
            "codex".into(),
            AgentState::Working,
            Some("conversation".into()),
            None,
            AgentCapabilities::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, "stale_runtime");
    }

    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    assert!(!path.exists());
}

#[test]
fn agent_clear_rotates_runtime_authority_before_shell_fallback() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let old = "0000000000000001:0000000000000001";
    let next = "0000000000000001:0000000000000002";
    lock(&daemon.state).claude_reconciliations.insert(
        PaneId(4),
        ClaudeReconciliation {
            event_state: AgentState::Working,
            event_at: Instant::now(),
            terminal_revision: Some(1),
            terminal: Some(claude_status::TerminalEvidence::Working),
            interrupt_requested: false,
            interrupt_revision: None,
        },
    );

    let response = agent_clear(&daemon, PaneId(4), old.into(), next.into()).unwrap();
    assert!(matches!(response, Response::Ack { revision: 8 }));
    {
        let state = lock(&daemon.state);
        assert!(state.persisted.panes[0].agent.is_none());
        assert!(!state.claude_reconciliations.contains_key(&PaneId(4)));
        assert_eq!(
            state
                .runtime_generations
                .get(&PaneId(4))
                .map(String::as_str),
            Some(next)
        );
    }

    let stale = agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(old.into())),
        "pi".into(),
        AgentState::Done,
        None,
        None,
        AgentCapabilities::default(),
    )
    .unwrap_err();
    assert_eq!(stale.code, "stale_runtime");
    assert!(agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(next.into())),
        "pi".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities::default(),
    )
    .is_ok());
    let _ = fs::remove_file(path);
}

#[test]
fn synchronization_save_failure_publishes_no_state_or_event() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
    let result = synchronize_projects(
        &daemon,
        vec![ProjectSpec {
            path: PathBuf::from("/"),
            name: "root".into(),
            worktrees: vec![WorktreeSpec {
                path: PathBuf::from("/"),
                branch: "main".into(),
            }],
        }],
    );
    assert!(result.is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn create_save_failure_publishes_no_session_or_event() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
    let result = create_session(
        &daemon,
        WorktreeId(2),
        "new".into(),
        vec!["/bin/cat".into()],
        None,
        10,
        20,
    );
    assert!(result.is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn durable_mutation_save_failure_publishes_no_state_or_event() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
    let result = mutate(&daemon, |persisted, _revision| {
        session_mut(persisted, SessionId(3))?.label = "changed".into();
        Ok(("session.renamed", 3))
    });
    assert!(result.is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn close_save_failure_keeps_session_and_runtime_maps_untouched() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();
    assert!(close_session(&daemon, SessionId(3), 7).is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn runtime_exit_stays_truthful_and_retries_durability() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    {
        let mut state = lock(&daemon.state);
        state.persisted.next_id = 7;
        state.persisted.panes[0].exited = false;
        state.persisted.panes[0].agent = Some(AgentInfo {
            id: AgentInstanceId(6),
            provider: "claude".into(),
            state: AgentState::Working,
            state_since_unix_ms: None,
            attached: true,
            presence_id: None,
            conversation_id: Some("/absolute/retained-conversation".into()),
            session_ref: Some(AgentSessionRef::path("/absolute/retained-conversation").unwrap()),
            capabilities: claude_capabilities(),
            source: "test".into(),
            compacting: false,
        });
        record_terminal_exit(&daemon, &mut state, PaneId(4));
        let pane = &state.persisted.panes[0];
        assert!(pane.exited);
        let agent = pane.agent.as_ref().unwrap();
        assert!(!agent.attached);
        assert_eq!(agent.state, AgentState::Unknown);
        assert_eq!(
            agent.conversation_id.as_deref(),
            Some("/absolute/retained-conversation")
        );
        assert!(state.persistence_dirty);
    }
    fs::remove_dir(&path).unwrap();
    retry_dirty_persistence(&daemon);
    assert!(!lock(&daemon.state).persistence_dirty);
    assert!(path.is_file());
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("json.backup"));
}

#[test]
fn agent_clear_save_failure_preserves_agent_and_runtime_generation() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = {
        let state = lock(&daemon.state);
        (
            serde_json::to_value(&state.persisted).unwrap(),
            state.runtime_generations.clone(),
        )
    };

    assert!(agent_clear(
        &daemon,
        PaneId(4),
        "0000000000000001:0000000000000001".into(),
        "0000000000000001:0000000000000002".into(),
    )
    .is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before.0);
    assert_eq!(state.runtime_generations, before.1);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

fn report_agent(daemon: &Arc<Daemon>, state: AgentState) -> Result<Response, ApiError> {
    agent_report(
        daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some("0000000000000001:0000000000000001".into())),
        "codex".into(),
        state,
        Some("conversation".into()),
        None,
        AgentCapabilities::default(),
    )
}

#[test]
fn agent_report_save_failure_keeps_live_state_unchanged() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = {
        let state = lock(&daemon.state);
        serde_json::to_value(&state.persisted).unwrap()
    };

    assert!(report_agent(&daemon, AgentState::Working).is_err());
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(&path).unwrap();
}

fn current_agent_authority(daemon: &Daemon) -> RuntimeAgentAuthority {
    let generation = lock(&daemon.state)
        .runtime_generations
        .get(&PaneId(4))
        .cloned();
    RuntimeAgentAuthority::new(PaneId(4), generation)
}

fn terminal_key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        text: String::new(),
        shift: false,
        control: false,
        alt: false,
        super_key: false,
        repeat: false,
    }
}

fn claude_capabilities() -> AgentCapabilities {
    AgentCapabilities {
        escape_interrupts: true,
        ..AgentCapabilities::default()
    }
}

fn report_claude_prompt(
    daemon: &Arc<Daemon>,
    state: AgentState,
    wake_token: Option<&str>,
) -> Result<Response, ApiError> {
    agent_report_with_attachment(
        daemon,
        current_agent_authority(daemon),
        AgentReportInput {
            provider: "claude".into(),
            state,
            attached: true,
            presence_id: None,
            conversation_id: None,
            session_ref: None,
            wake_token: wake_token.map(str::to_string),
            capabilities: claude_capabilities(),
            received: None,
            compacting: false,
        },
    )
}

#[test]
fn claude_wake_heartbeat_is_prompt_and_generation_bound() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let generation = current_agent_authority(&daemon).generation.unwrap();
    report_claude_prompt(&daemon, AgentState::Working, Some("prompt-a")).unwrap();
    let expired = Instant::now() - AGENT_WAKE_LEASE_TTL;
    lock(&daemon.state)
        .agent_wake_leases
        .insert(pane_id, expired);

    assert!(matches!(
        renew_agent_wake(&daemon, pane_id, generation.clone(), "prompt-a".into()),
        Ok(Response::Ack { .. })
    ));
    assert!(lock(&daemon.state).agent_wake_leases[&pane_id] > expired);

    assert_eq!(
        renew_agent_wake(&daemon, pane_id, generation.clone(), "prompt-old".into())
            .unwrap_err()
            .code,
        "stale_wake_heartbeat"
    );
    assert_eq!(
        renew_agent_wake(&daemon, pane_id, generation.clone(), "bad/token".into())
            .unwrap_err()
            .code,
        "invalid_wake_token"
    );
    assert_eq!(
        renew_agent_wake(
            &daemon,
            pane_id,
            "0000000000000001:0000000000000099".into(),
            "prompt-a".into()
        )
        .unwrap_err()
        .code,
        "stale_runtime"
    );

    report_claude_prompt(&daemon, AgentState::Working, Some("prompt-b")).unwrap();
    lock(&daemon.state).agent_wake_tokens.remove(&pane_id);
    assert!(renew_agent_wake(&daemon, pane_id, generation.clone(), "prompt-b".into()).is_ok());
    assert_eq!(lock(&daemon.state).agent_wake_tokens[&pane_id], "prompt-b");
    assert_eq!(
        renew_agent_wake(&daemon, pane_id, generation.clone(), "prompt-a".into())
            .unwrap_err()
            .code,
        "stale_wake_heartbeat"
    );
    assert!(renew_agent_wake(&daemon, pane_id, generation, "prompt-b".into()).is_ok());
    let _ = fs::remove_file(path);
}

#[test]
fn claude_wake_heartbeat_cannot_outlive_authoritative_working_state() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let generation = current_agent_authority(&daemon).generation.unwrap();
    for (index, ending) in [
        AgentState::Idle,
        AgentState::Blocked,
        AgentState::Done,
        AgentState::Error,
        AgentState::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        let wake_token = format!("prompt-{index}");
        report_claude_prompt(&daemon, AgentState::Working, Some(&wake_token)).unwrap();
        report_claude_prompt(&daemon, ending, None).unwrap();
        let state = lock(&daemon.state);
        assert!(!state.agent_wake_leases.contains_key(&pane_id));
        assert!(!state.agent_wake_tokens.contains_key(&pane_id));
        drop(state);
        assert_eq!(
            renew_agent_wake(&daemon, pane_id, generation.clone(), wake_token.clone(),)
                .unwrap_err()
                .code,
            "wake_not_active"
        );
    }

    report_claude_prompt(&daemon, AgentState::Working, Some("prompt-detached")).unwrap();
    agent_report_with_attachment(
        &daemon,
        current_agent_authority(&daemon),
        AgentReportInput {
            provider: "claude".into(),
            state: AgentState::Done,
            attached: false,
            presence_id: None,
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: claude_capabilities(),
            received: None,
            compacting: false,
        },
    )
    .unwrap();
    assert!(!lock(&daemon.state).agent_wake_tokens.contains_key(&pane_id));
    assert_eq!(
        renew_agent_wake(&daemon, pane_id, generation, "prompt-detached".into())
            .unwrap_err()
            .code,
        "wake_not_active"
    );
    let _ = fs::remove_file(path);
}

#[test]
fn claude_wake_authority_clears_on_runtime_replacement_and_exit() {
    let pane_id = PaneId(4);
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let generation = current_agent_authority(&daemon).generation.unwrap();
    report_claude_prompt(&daemon, AgentState::Working, Some("prompt-replaced")).unwrap();
    agent_clear(
        &daemon,
        pane_id,
        generation,
        "0000000000000002:0000000000000002".into(),
    )
    .unwrap();
    let state = lock(&daemon.state);
    assert!(!state.agent_wake_leases.contains_key(&pane_id));
    assert!(!state.agent_wake_tokens.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);

    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    report_claude_prompt(&daemon, AgentState::Working, Some("prompt-exited")).unwrap();
    let mut state = lock(&daemon.state);
    record_terminal_exit(&daemon, &mut state, pane_id);
    assert!(!state.agent_wake_leases.contains_key(&pane_id));
    assert!(!state.agent_wake_tokens.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);
}

fn claude_observation(revision: u64, title: &str, text: &str) -> AgentTerminalObservation {
    AgentTerminalObservation {
        revision,
        title: title.into(),
        text: text.into(),
    }
}

#[test]
fn streamed_escape_requests_claude_reconcile_without_deciding_state() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 3,
                cols: 4,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    lock(&daemon.state)
        .claude_reconciliations
        .get_mut(&pane_id)
        .unwrap()
        .terminal_revision = Some(42);
    let (_, generation) = acquire_terminal_lease(&daemon, pane_id, 9, true).unwrap();

    handle_terminal_stream_input(
        &daemon,
        pane_id,
        9,
        generation,
        TerminalClientMessage::Key(terminal_key(KeyCode::Escape)),
    )
    .unwrap();

    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().state,
        AgentState::Working
    );
    assert_eq!(
        state.claude_reconciliations[&pane_id].terminal_revision,
        None
    );
    assert!(state.agent_wake_leases.contains_key(&pane_id));
    drop(state);
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn idle_terminal_evidence_cannot_override_working_without_interrupt() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    let event_at = lock(&daemon.state).claude_reconciliations[&pane_id].event_at;
    let generation = current_agent_authority(&daemon).generation.unwrap();
    let idle = claude_observation(2, "✳ task", "❯ ");

    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        idle.clone(),
        event_at + claude_status::WORKING_EVENT_LEAD - Duration::from_millis(1),
    )
    .unwrap();
    assert_eq!(
        lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .state,
        AgentState::Working
    );

    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        idle.clone(),
        event_at + claude_status::WORKING_EVENT_LEAD + Duration::from_millis(1),
    )
    .unwrap();
    {
        let state = lock(&daemon.state);
        assert_eq!(
            state.persisted.panes[0].agent.as_ref().unwrap().state,
            AgentState::Working
        );
        assert!(state.agent_wake_leases.contains_key(&pane_id));
    }
    request_claude_reconcile(&daemon, pane_id, true);
    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        idle,
        Instant::now() + claude_status::WORKING_EVENT_LEAD,
    )
    .unwrap();
    {
        let state = lock(&daemon.state);
        assert_eq!(
            state.persisted.panes[0].agent.as_ref().unwrap().state,
            AgentState::Idle
        );
        assert!(!state.agent_wake_leases.contains_key(&pane_id));
    }

    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().state,
        AgentState::Working
    );
    assert!(state.agent_wake_leases.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn claude_reconciler_requires_interrupt_for_real_pty_idle() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/cat".into()],
                initial_input: None,
                rows: 6,
                cols: 40,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    let reconciler = spawn_claude_reconciler(&daemon);

    runtime.write("\x1b]2;✳ task\x07\r\n❯ ".as_bytes()).unwrap();
    thread::sleep(claude_status::WORKING_EVENT_LEAD + Duration::from_millis(150));
    assert_eq!(
        lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .state,
        AgentState::Working
    );
    request_claude_reconcile(&daemon, pane_id, true);
    thread::sleep(CLAUDE_RECONCILE_INTERVAL + Duration::from_millis(50));
    assert_eq!(
        lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .unwrap()
            .state,
        AgentState::Working,
        "the pre-interrupt frame cannot settle an interrupted turn"
    );
    runtime.write(b"\r\n\xe2\x9d\xaf ").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if lock(&daemon.state).persisted.panes[0]
            .agent
            .as_ref()
            .is_some_and(|agent| agent.state == AgentState::Idle)
        {
            break;
        }
        assert!(Instant::now() < deadline, "Claude state remained working");
        thread::sleep(Duration::from_millis(20));
    }

    {
        let mut state = lock(&daemon.state);
        assert!(!state.agent_wake_leases.contains_key(&pane_id));
        state.stopping = true;
    }
    reconciler.join().unwrap();
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn visible_claude_states_reconcile_without_granting_heuristic_wake_authority() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let generation = current_agent_authority(&daemon).generation.unwrap();
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Idle,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();

    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        claude_observation(2, "◐ task", ""),
        Instant::now(),
    )
    .unwrap();
    {
        let state = lock(&daemon.state);
        assert_eq!(
            state.persisted.panes[0].agent.as_ref().unwrap().state,
            AgentState::Working
        );
        assert!(!state.agent_wake_leases.contains_key(&pane_id));
    }

    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        claude_observation(
            3,
            "project",
            "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel",
        ),
        Instant::now(),
    )
    .unwrap();
    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().state,
        AgentState::Blocked
    );
    assert!(!state.agent_wake_leases.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn terminal_mismatch_does_not_erase_claude_done_or_error() {
    for event_state in [AgentState::Done, AgentState::Error] {
        let (daemon, path) = agent_test_daemon(agent_test_persisted());
        let generation = current_agent_authority(&daemon).generation.unwrap();
        agent_report(
            &daemon,
            current_agent_authority(&daemon),
            "claude".into(),
            event_state,
            None,
            None,
            claude_capabilities(),
        )
        .unwrap();

        apply_claude_terminal_observation(
            &daemon,
            PaneId(4),
            &generation,
            claude_observation(2, "◐ task", "❯ "),
            Instant::now() + claude_status::WORKING_EVENT_LEAD,
        )
        .unwrap();

        assert_eq!(
            lock(&daemon.state).persisted.panes[0]
                .agent
                .as_ref()
                .unwrap()
                .state,
            event_state
        );
        let _ = fs::remove_file(path);
    }
}

#[test]
fn ambiguous_or_stale_terminal_evidence_cannot_override_claude_event_state() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    let event_at = lock(&daemon.state).claude_reconciliations[&pane_id].event_at;
    let generation = current_agent_authority(&daemon).generation.unwrap();
    let observation = claude_observation(2, "project", "ordinary output");

    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        "0000000000000001:0000000000000099",
        claude_observation(1, "✳ task", "❯ "),
        event_at + claude_status::WORKING_EVENT_LEAD,
    )
    .unwrap();
    apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        observation,
        event_at + claude_status::WORKING_EVENT_LEAD,
    )
    .unwrap();

    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().state,
        AgentState::Working
    );
    assert!(state.agent_wake_leases.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn failed_claude_reconciliation_preserves_event_state_and_wake_lease() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    agent_report(
        &daemon,
        current_agent_authority(&daemon),
        "claude".into(),
        AgentState::Working,
        None,
        None,
        claude_capabilities(),
    )
    .unwrap();
    let event_at = lock(&daemon.state).claude_reconciliations[&pane_id].event_at;
    let generation = current_agent_authority(&daemon).generation.unwrap();
    request_claude_reconcile(&daemon, pane_id, true);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();

    assert!(apply_claude_terminal_observation(
        &daemon,
        pane_id,
        &generation,
        claude_observation(2, "✳ task", "❯ "),
        event_at + claude_status::WORKING_EVENT_LEAD,
    )
    .is_err());

    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().state,
        AgentState::Working
    );
    assert_eq!(
        state.claude_reconciliations[&pane_id].terminal_revision,
        None
    );
    assert!(state.agent_wake_leases.contains_key(&pane_id));
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn ctrl_c_key_requests_an_agent_reconcile() {
    let mut key = terminal_key(KeyCode::Text);
    key.text = "c".into();
    key.control = true;

    assert!(key_interrupts_agent(&key));
}

#[test]
fn working_agent_reports_set_and_refresh_project_activity() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    {
        let mut state = lock(&daemon.state);
        state.persisted.projects[0].last_agent_active_unix_ms = Some(1);
    }

    report_agent(&daemon, AgentState::Working).unwrap();
    let first_agent_id = {
        let state = lock(&daemon.state);
        assert_ne!(
            state.persisted.projects[0].last_agent_active_unix_ms,
            Some(1)
        );
        assert_eq!(state.persisted.panes[0].revision, 8);
        assert_eq!(state.persisted.projects[0].revision, 8);
        assert_eq!(state.events.len(), 1);
        state.persisted.panes[0].agent.as_ref().unwrap().id
    };

    {
        let mut state = lock(&daemon.state);
        state.persisted.projects[0].last_agent_active_unix_ms = Some(1);
    }
    report_agent(&daemon, AgentState::Working).unwrap();
    let state = lock(&daemon.state);
    assert_ne!(
        state.persisted.projects[0].last_agent_active_unix_ms,
        Some(1)
    );
    assert_eq!(
        state.persisted.panes[0].agent.as_ref().unwrap().id,
        first_agent_id
    );
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn non_working_agent_reports_preserve_project_activity() {
    let mut persisted = agent_test_persisted();
    persisted.projects[0].last_agent_active_unix_ms = Some(123);
    let (daemon, path) = agent_test_daemon(persisted);

    report_agent(&daemon, AgentState::Idle).unwrap();

    let state = lock(&daemon.state);
    assert_eq!(
        state.persisted.projects[0].last_agent_active_unix_ms,
        Some(123)
    );
    assert_eq!(state.persisted.projects[0].revision, 7);
    assert_eq!(state.persisted.panes[0].revision, 8);
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn agent_report_with_missing_ancestry_has_no_mutation() {
    let mut persisted = agent_test_persisted();
    persisted.projects.clear();
    let (daemon, path) = agent_test_daemon(persisted);

    assert_eq!(
        report_agent(&daemon, AgentState::Working).unwrap_err().code,
        "not_found"
    );

    let state = lock(&daemon.state);
    assert!(state.persisted.panes[0].agent.is_none());
    assert_eq!(state.persisted.next_id, 6);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    assert!(!path.exists());
}

#[test]
fn working_activity_survives_state_reload() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    report_agent(&daemon, AgentState::Working).unwrap();

    let reloaded = load_state(&path).unwrap();
    assert!(reloaded.projects[0].last_agent_active_unix_ms.is_some());
    fs::remove_file(path).unwrap();
}

fn exchange_test_daemon() -> (Arc<Daemon>, PathBuf, Arc<TerminalRuntime>, String) {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let fixture_root = fs::canonicalize(path.parent().unwrap()).unwrap();
    let mut state = lock(&daemon.state);
    state.persisted.projects[0].path = fixture_root.clone();
    state.persisted.worktrees[0].path = fixture_root;
    drop(state);
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "trap '' INT; exec /bin/cat".into(),
                ],
                initial_input: None,
                rows: 8,
                cols: 80,
            },
        )
        .unwrap(),
    );
    let generation = {
        let mut state = lock(&daemon.state);
        state.runtimes.insert(pane_id, Arc::clone(&runtime));
        state.runtime_generations[&pane_id].clone()
    };
    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(pane_id, Some(generation.clone())),
        "pi".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    (daemon, path, runtime, generation)
}

fn terminal_text(runtime: &TerminalRuntime) -> String {
    runtime
        .frame()
        .unwrap()
        .cells
        .chunks(80)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_terminal_text(runtime: &TerminalRuntime, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if terminal_text(runtime).contains(expected) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "terminal never contained {expected:?}: {:?}",
        terminal_text(runtime)
    );
}

fn listed_agent_exchange(daemon: &Daemon, exchange_id: AgentExchangeId) -> AgentExchange {
    let Response::AgentExchanges(exchanges) = list_agent_exchanges(daemon, None).unwrap() else {
        panic!("expected exchange list");
    };
    exchanges
        .into_iter()
        .find(|exchange| exchange.id == exchange_id)
        .expect("created exchange must be listed")
}

// The shared exchange fixture keeps a live runtime but leaves the pane marked exited.
// Production marks a pane exited only when its runtime dies, so these helpers restore that.
fn live_agent_daemon() -> (Arc<Daemon>, PathBuf, Arc<TerminalRuntime>) {
    let (daemon, path, runtime, _generation) = exchange_test_daemon();
    let mut state = lock(&daemon.state);
    if let Some(pane) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == PaneId(4))
    {
        pane.exited = false;
    }
    drop(state);
    (daemon, path, runtime)
}

fn record_runtime_death(daemon: &Arc<Daemon>, pane_id: PaneId) {
    let runtime = lock(&daemon.state).runtimes.remove(&pane_id);
    if let Some(runtime) = runtime {
        runtime.terminate();
    }
    let mut state = lock(&daemon.state);
    record_terminal_exit(daemon, &mut state, pane_id);
    assert!(
        state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .is_some_and(|pane| pane.exited),
        "the fixture must record a real runtime death"
    );
    drop(state);
}

fn exited_pane_daemon() -> (Arc<Daemon>, PathBuf) {
    let (daemon, path, _runtime) = live_agent_daemon();
    record_runtime_death(&daemon, PaneId(4));
    (daemon, path)
}

fn restart_pane(daemon: &Arc<Daemon>, pane_id: PaneId) -> u64 {
    let pane = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane exists");
    let Response::Ack { revision } =
        restart_session(daemon, pane_id, pane.revision).expect("restart succeeds")
    else {
        panic!("expected an ack");
    };
    revision
}

fn stop_restarted_runtime(daemon: &Arc<Daemon>, pane_id: PaneId) {
    let runtime = lock(&daemon.state).runtimes.remove(&pane_id);
    if let Some(runtime) = runtime {
        runtime.terminate();
    }
}

#[test]
fn restart_revives_an_exited_pane_and_fences_its_old_stream() {
    let (daemon, path) = exited_pane_daemon();
    let pane_id = PaneId(4);
    let before = {
        let mut state = lock(&daemon.state);
        let before = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .cloned()
            .expect("pane exists");
        assert!(before.exited, "fixture must start from an exited pane");
        state.leases.insert(
            pane_id,
            Lease {
                client_id: 99,
                generation: 4,
                expires_at: Instant::now() + LEASE_TTL,
            },
        );
        state.transferred_streams.push_back(StreamLease {
            pane_id,
            client_id: 99,
            generation: 4,
        });
        before
    };

    let revision = restart_pane(&daemon, pane_id);

    assert!(revision > before.revision);
    let state = lock(&daemon.state);
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("pane kept");
    assert!(!pane.exited, "restart must clear the exited flag");
    assert!(pane.revision > before.revision);
    assert_eq!(pane.session_id, before.session_id);
    assert_eq!(pane.terminal_id, before.terminal_id);
    assert!(state
        .runtimes
        .get(&pane_id)
        .is_some_and(|runtime| !runtime.exited()));
    assert!(state.runtime_generations.contains_key(&pane_id));
    assert!(
        !state.leases.contains_key(&pane_id),
        "the dead pane's lease must not carry authority into the replacement runtime"
    );
    assert!(!state
        .transferred_streams
        .iter()
        .any(|stream| stream.pane_id == pane_id));
    drop(state);
    stop_restarted_runtime(&daemon, pane_id);
    let _ = fs::remove_file(path);
}

#[test]
fn restart_save_failure_leaves_no_runtime_authority() {
    let (daemon, path) = exited_pane_daemon();
    let pane_id = PaneId(4);
    let before = {
        let state = lock(&daemon.state);
        (
            state
                .persisted
                .panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .expect("pane exists")
                .revision,
            serde_json::to_value(&state.persisted).unwrap(),
            state.revision,
        )
    };
    let _ = fs::remove_file(&path);
    fs::create_dir(&path).unwrap();
    assert!(
        restart_session(&daemon, pane_id, before.0).is_err(),
        "an unwritable state store must fail the restart"
    );
    let state = lock(&daemon.state);
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .expect("pane kept");
    assert!(pane.exited, "a failed restart must leave the pane exited");
    assert_eq!(pane.revision, before.0);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before.1);
    assert_eq!(state.revision, before.2);
    assert!(
        !state.runtime_generations.contains_key(&pane_id),
        "a failed restart must not leave a generation that can authorize agent reports"
    );
    assert!(!state.runtimes.contains_key(&pane_id));
    drop(state);
    fs::remove_dir(&path).unwrap();
}

#[test]
fn restart_missing_worktree_record_leaves_state_unchanged() {
    let (daemon, path) = exited_pane_daemon();
    let pane_id = PaneId(4);
    let before = {
        let mut state = lock(&daemon.state);
        state.persisted.worktrees.clear();
        (
            state.persisted.panes[0].revision,
            serde_json::to_value(&state.persisted).unwrap(),
            state.revision,
        )
    };

    assert_eq!(
        restart_session(&daemon, pane_id, before.0)
            .unwrap_err()
            .code,
        "not_found"
    );
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before.1);
    assert_eq!(state.revision, before.2);
    assert!(!state.runtime_generations.contains_key(&pane_id));
    assert!(!state.runtimes.contains_key(&pane_id));
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn restart_refuses_running_stale_stopping_handoff_and_recovering_panes() {
    let (daemon, path, _runtime) = live_agent_daemon();
    let pane_id = PaneId(4);
    let running = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane exists");
    assert!(!running.exited);
    assert_eq!(
        restart_session(&daemon, pane_id, running.revision)
            .unwrap_err()
            .code,
        "invalid_state"
    );
    assert_eq!(
        lock(&daemon.state)
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .expect("pane kept")
            .revision,
        running.revision,
        "a refused restart must not mutate the pane"
    );

    record_runtime_death(&daemon, pane_id);
    let exited = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane kept");
    assert!(exited.exited);
    assert_eq!(
        restart_session(&daemon, pane_id, exited.revision + 1)
            .unwrap_err()
            .code,
        "revision_conflict"
    );
    assert!(
        lock(&daemon.state)
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .expect("pane kept")
            .exited,
        "a stale revision must leave the pane exited"
    );

    lock(&daemon.state).stopping = true;
    assert_eq!(
        restart_session(&daemon, pane_id, exited.revision)
            .unwrap_err()
            .code,
        "daemon_replacing"
    );
    lock(&daemon.state).stopping = false;
    lock(&daemon.state).handoff_in_progress = true;
    assert_eq!(
        restart_session(&daemon, pane_id, exited.revision)
            .unwrap_err()
            .code,
        "daemon_replacing"
    );
    lock(&daemon.state).handoff_in_progress = false;
    lock(&daemon.state).recovery_in_progress = true;
    assert_eq!(
        restart_session(&daemon, pane_id, exited.revision)
            .unwrap_err()
            .code,
        "recovery_in_progress"
    );
    lock(&daemon.state).recovery_in_progress = false;
    assert!(
        lock(&daemon.state)
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .expect("pane kept")
            .exited
    );
    let _ = fs::remove_file(path);
}

#[test]
fn restart_never_falsely_attaches_a_persisted_agent_identity() {
    let (daemon, path) = exited_pane_daemon();
    let pane_id = PaneId(4);
    let attached = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.clone());
    assert!(
        attached.is_some(),
        "fixture must carry a persisted agent identity"
    );

    restart_pane(&daemon, pane_id);

    let revived = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane kept");
    assert!(!revived.exited);
    assert!(
        revived.agent.as_ref().is_none_or(|agent| !agent.attached),
        "a replacement runtime must never inherit an attached identity"
    );
    record_runtime_death(&daemon, pane_id);
    let mut state = lock(&daemon.state);
    if let Some(pane) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
    {
        pane.agent = None;
    }
    drop(state);
    restart_pane(&daemon, pane_id);
    let plain = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane kept");
    assert!(!plain.exited);
    assert!(
        plain.agent.is_none(),
        "a pane without a saved identity restarts without one"
    );
    stop_restarted_runtime(&daemon, pane_id);
    let _ = fs::remove_file(path);
}

#[test]
fn restart_sequence_advances_revision_and_leaves_exchanges_terminal() {
    let (daemon, path, _runtime) = live_agent_daemon();
    let pane_id = PaneId(4);
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        pane_id,
        "survive the exit".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    let exchange_id = exchange.id;
    record_runtime_death(&daemon, pane_id);
    let first_exit = {
        let state = lock(&daemon.state);
        assert_eq!(
            state
                .persisted
                .agent_exchanges
                .iter()
                .find(|candidate| candidate.id == exchange_id)
                .map(|candidate| candidate.state),
            Some(AgentExchangeState::TargetExited)
        );
        state.revision
    };

    let first_restart = restart_pane(&daemon, pane_id);
    assert!(first_restart > first_exit);
    assert_eq!(
        lock(&daemon.state)
            .persisted
            .agent_exchanges
            .iter()
            .find(|candidate| candidate.id == exchange_id)
            .map(|candidate| candidate.state),
        Some(AgentExchangeState::TargetExited),
        "restart must not revive an exchange bound to the dead runtime"
    );

    record_runtime_death(&daemon, pane_id);
    let second_exit = lock(&daemon.state).revision;
    assert!(
        lock(&daemon.state)
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .expect("pane kept")
            .exited
    );
    let second_restart = restart_pane(&daemon, pane_id);
    assert!(second_restart > second_exit);
    let live = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane kept");
    assert!(!live.exited);
    assert_eq!(
        restart_session(&daemon, pane_id, live.revision)
            .unwrap_err()
            .code,
        "invalid_state",
        "a running pane is never restartable"
    );
    stop_restarted_runtime(&daemon, pane_id);
    let _ = fs::remove_file(path);
}

#[test]
fn ctrl_c_bytes_end_a_pane_and_restart_reuses_its_saved_recipe() {
    let (daemon, path, fixture_runtime, _generation) = exchange_test_daemon();
    let pane_id = PaneId(4);
    let cwd = lock(&daemon.state)
        .persisted
        .worktrees
        .first()
        .expect("fixture worktree")
        .path
        .clone();
    let recipe = LaunchRecipe {
        command: vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()],
        initial_input: None,
        rows: 8,
        cols: 80,
    };
    let runtime =
        Arc::new(spawn_runtime(&daemon, pane_id, TerminalId(5), &cwd, &recipe).expect("spawns"));
    {
        let mut state = lock(&daemon.state);
        state.runtimes.insert(pane_id, Arc::clone(&runtime));
        if let Some(pane) = state
            .persisted
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
        {
            pane.exited = false;
            pane.recovery = Some(recipe.clone());
        }
    }
    fixture_runtime.terminate();

    // ^ A terminal Ctrl+C is the VINTR byte, so the line discipline signals the foreground group.
    runtime.write(&[3]).expect("writes the interrupt byte");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.exited() {
        assert!(
            Instant::now() < deadline,
            "Ctrl+C must end a foreground process that does not trap SIGINT"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let exited = {
        let mut state = lock(&daemon.state);
        state.foreground_jobs.insert(pane_id);
        state.listening_ports.insert(pane_id, vec![3000]);
        record_terminal_exit(&daemon, &mut state, pane_id);
        assert!(
            !state.foreground_jobs.contains(&pane_id)
                && !state.listening_ports.contains_key(&pane_id),
            "an exited pane must not keep reporting a live job or listener"
        );
        state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .cloned()
            .expect("pane kept")
    };
    assert!(exited.exited);

    let revision = restart_pane(&daemon, pane_id);
    assert!(revision > exited.revision);
    let revived = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .expect("pane kept");
    assert!(
        !revived.exited,
        "the reported hollow session must become live"
    );
    assert_eq!(
        revived
            .recovery
            .as_ref()
            .map(|recipe| recipe.command.clone()),
        Some(recipe.command.clone()),
        "restart reuses the pane's saved command"
    );
    assert!(lock(&daemon.state)
        .runtimes
        .get(&pane_id)
        .is_some_and(|runtime| !runtime.exited()));
    stop_restarted_runtime(&daemon, pane_id);
    let _ = fs::remove_file(path);
}

#[test]
fn one_active_exchange_per_pane_releases_after_terminal_state() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let Response::AgentExchange {
        exchange: first, ..
    } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "first request".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap()
    else {
        panic!("expected first exchange");
    };

    assert_eq!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "overlapping request".into(),
            60_000,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap_err()
        .code,
        "exchange_busy"
    );

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
        "pi".into(),
        AgentState::Done,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        listed_agent_exchange(&daemon, first.id).state,
        AgentExchangeState::DoneObserved
    );
    assert!(matches!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "next request".into(),
            60_000,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap(),
        Response::AgentExchange { exchange, .. }
            if exchange.id != first.id && exchange.state == AgentExchangeState::Delivered
    ));

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn request_bound_exchange_receipts_require_capability_and_exact_authority() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "receipt review".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };

    let before_unsupported = listed_agent_exchange(&daemon, exchange.id);
    assert_eq!(
        report_agent_exchange_receipt(
            &daemon,
            exchange.id,
            exchange.round,
            &generation,
            AgentExchangeReceipt::Accepted,
        )
        .unwrap_err()
        .code,
        "unsupported_exchange_receipt"
    );
    assert_eq!(
        listed_agent_exchange(&daemon, exchange.id),
        before_unsupported
    );

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation.clone())),
        "pi".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            exchange_receipts: true,
            ..Default::default()
        },
    )
    .unwrap();
    let before_stale_receipts = listed_agent_exchange(&daemon, exchange.id);

    assert_eq!(
        report_agent_exchange_receipt(
            &daemon,
            exchange.id,
            exchange.round,
            "stale-generation",
            AgentExchangeReceipt::Accepted,
        )
        .unwrap_err()
        .code,
        "stale_runtime"
    );
    assert_eq!(
        listed_agent_exchange(&daemon, exchange.id),
        before_stale_receipts
    );
    assert_eq!(
        report_agent_exchange_receipt(
            &daemon,
            exchange.id,
            exchange.round + 1,
            &generation,
            AgentExchangeReceipt::Accepted,
        )
        .unwrap_err()
        .code,
        "stale_exchange_round"
    );
    assert_eq!(
        listed_agent_exchange(&daemon, exchange.id),
        before_stale_receipts
    );

    let Response::AgentExchange {
        exchange: accepted, ..
    } = report_agent_exchange_receipt(
        &daemon,
        exchange.id,
        exchange.round,
        &generation,
        AgentExchangeReceipt::Accepted,
    )
    .unwrap()
    else {
        panic!("expected accepted exchange");
    };
    assert_eq!(accepted.state, AgentExchangeState::Accepted);
    assert_eq!(accepted.evidence, AgentExchangeEvidence::RequestBound);

    let Response::AgentExchange {
        exchange: duplicate,
        ..
    } = report_agent_exchange_receipt(
        &daemon,
        exchange.id,
        exchange.round,
        &generation,
        AgentExchangeReceipt::Accepted,
    )
    .unwrap()
    else {
        panic!("expected idempotent accepted exchange");
    };
    assert_eq!(duplicate, accepted);
    assert_eq!(listed_agent_exchange(&daemon, exchange.id), accepted);

    let Response::AgentExchange {
        exchange: completed,
        ..
    } = report_agent_exchange_receipt(
        &daemon,
        exchange.id,
        exchange.round,
        &generation,
        AgentExchangeReceipt::Completed,
    )
    .unwrap()
    else {
        panic!("expected completed exchange");
    };
    assert_eq!(completed.state, AgentExchangeState::Completed);
    assert_eq!(completed.evidence, AgentExchangeEvidence::RequestBound);
    assert!(completed.state.is_terminal());

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn exchange_mutations_require_current_revision_without_stale_side_effects() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let worktree_path = lock(&daemon.state).persisted.worktrees[0].path.clone();
    let Response::AgentExchange {
        exchange: delivered,
        ..
    } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "review".into(),
        60_000,
        AgentExchangeAccess::Writer,
        vec![worktree_path.join("src")],
    )
    .unwrap()
    else {
        panic!("expected exchange");
    };
    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
        "pi".into(),
        AgentState::Blocked,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    let blocked = listed_agent_exchange(&daemon, delivered.id);

    assert_eq!(
        continue_agent_exchange(
            &daemon,
            blocked.id,
            delivered.revision,
            "stale continuation".into(),
            60_000,
        )
        .unwrap_err()
        .code,
        "revision_conflict"
    );
    assert_eq!(listed_agent_exchange(&daemon, blocked.id), blocked);
    assert_eq!(
        cancel_agent_exchange(&daemon, blocked.id, delivered.revision, false)
            .unwrap_err()
            .code,
        "revision_conflict"
    );
    assert_eq!(listed_agent_exchange(&daemon, blocked.id), blocked);

    let Response::AgentExchange {
        exchange: continued,
        ..
    } = continue_agent_exchange(
        &daemon,
        blocked.id,
        blocked.revision,
        "current continuation".into(),
        60_000,
    )
    .unwrap()
    else {
        panic!("expected continued exchange");
    };
    assert_eq!(continued.state, AgentExchangeState::Delivered);
    let Response::AgentExchange {
        exchange: cancelling,
        ..
    } = cancel_agent_exchange(&daemon, continued.id, continued.revision, false).unwrap()
    else {
        panic!("expected cancellation request");
    };
    assert_eq!(cancelling.state, AgentExchangeState::CancelRequested);

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn stale_runtime_report_cannot_mutate_or_confirm_exchange_cancellation() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let stale_generation = [
        "0000000000000001:0000000000000001",
        "0000000000000001:0000000000000002",
    ]
    .into_iter()
    .find(|candidate| *candidate != generation)
    .expect("one well-formed generation must differ from the fixture")
    .to_string();
    assert_ne!(stale_generation, generation);
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "cancel me".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    let Response::AgentExchange {
        exchange: cancelling,
        ..
    } = cancel_agent_exchange(&daemon, exchange.id, exchange.revision, false).unwrap()
    else {
        panic!("expected cancellation request");
    };

    assert_eq!(
        agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some(stale_generation)),
            "pi".into(),
            AgentState::Idle,
            None,
            None,
            AgentCapabilities {
                prompt: true,
                lifecycle: true,
                ..Default::default()
            },
        )
        .unwrap_err()
        .code,
        "stale_runtime"
    );
    assert_eq!(listed_agent_exchange(&daemon, cancelling.id), cancelling);

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
        "pi".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        listed_agent_exchange(&daemon, cancelling.id).state,
        AgentExchangeState::Cancelled
    );

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn continue_retains_target_authority_access_and_claims_for_each_wait_boundary() {
    for (lifecycle_state, expected_exchange_state) in [
        (AgentState::Blocked, AgentExchangeState::BlockedObserved),
        (AgentState::Done, AgentExchangeState::DoneObserved),
    ] {
        let (daemon, path, runtime, generation) = exchange_test_daemon();
        let worktree_path = lock(&daemon.state).persisted.worktrees[0].path.clone();
        let Response::AgentExchange {
            exchange: delivered,
            ..
        } = create_agent_exchange(
            &daemon,
            PaneId(4),
            "initial round".into(),
            60_000,
            AgentExchangeAccess::Writer,
            vec![worktree_path.join("src")],
        )
        .unwrap()
        else {
            panic!("expected exchange");
        };
        agent_report(
            &daemon,
            RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
            "pi".into(),
            lifecycle_state,
            None,
            None,
            AgentCapabilities {
                prompt: true,
                lifecycle: true,
                ..Default::default()
            },
        )
        .unwrap();
        let observed = listed_agent_exchange(&daemon, delivered.id);
        assert_eq!(observed.state, expected_exchange_state);

        let Response::AgentExchange {
            exchange: continued,
            ..
        } = continue_agent_exchange(
            &daemon,
            observed.id,
            observed.revision,
            "next round".into(),
            60_000,
        )
        .unwrap()
        else {
            panic!("expected continued exchange");
        };
        assert_eq!(continued.state, AgentExchangeState::Delivered);
        assert_eq!(continued.evidence, AgentExchangeEvidence::PtyDelivery);
        assert_eq!(continued.round, observed.round + 1);
        assert_eq!(continued.pane_id, observed.pane_id);
        assert_eq!(continued.session_id, observed.session_id);
        assert_eq!(continued.worktree_id, observed.worktree_id);
        assert_eq!(continued.project_id, observed.project_id);
        assert_eq!(continued.agent_id, observed.agent_id);
        assert_eq!(continued.provider, observed.provider);
        assert_eq!(continued.runtime_generation, observed.runtime_generation);
        assert_eq!(continued.access, observed.access);
        assert_eq!(continued.write_claims, observed.write_claims);

        runtime.terminate();
        let _ = fs::remove_file(path);
    }
}

#[test]
fn exchange_prompt_and_timeout_boundaries_are_exact_and_typed() {
    let (daemon, path, runtime, _) = exchange_test_daemon();
    assert_eq!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            String::new(),
            1,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap_err()
        .code,
        "invalid_prompt"
    );
    assert_eq!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "x".repeat(MAX_AGENT_EXCHANGE_PROMPT_BYTES + 1),
            1,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap_err()
        .code,
        "invalid_prompt"
    );
    for timeout_ms in [0, MAX_AGENT_EXCHANGE_TIMEOUT_MS + 1] {
        assert_eq!(
            create_agent_exchange(
                &daemon,
                PaneId(4),
                "valid".into(),
                timeout_ms,
                AgentExchangeAccess::ReadOnly,
                Vec::new(),
            )
            .unwrap_err()
            .code,
            "invalid_timeout"
        );
    }
    assert!(matches!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "x".repeat(MAX_AGENT_EXCHANGE_PROMPT_BYTES),
            MAX_AGENT_EXCHANGE_TIMEOUT_MS,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap(),
        Response::AgentExchange { exchange, .. }
            if exchange.state == AgentExchangeState::Delivered
    ));

    runtime.terminate();
    let _ = fs::remove_file(path);

    let (daemon, path, runtime, _) = exchange_test_daemon();
    assert!(matches!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "minimum timeout".into(),
            1,
            AgentExchangeAccess::ReadOnly,
            Vec::new(),
        )
        .unwrap(),
        Response::AgentExchange { exchange, .. }
            if exchange.state == AgentExchangeState::Delivered
    ));
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn exchange_delivery_promotes_only_observed_lifecycle_and_continues_same_target() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let response = create_agent_exchange(
        &daemon,
        PaneId(4),
        "inspect the boundary".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap();
    let Response::AgentExchange {
        exchange,
        frame: None,
    } = response
    else {
        panic!("unexpected exchange response: {response:?}");
    };
    assert_eq!(exchange.state, AgentExchangeState::Delivered);
    assert_eq!(exchange.evidence, AgentExchangeEvidence::PtyDelivery);
    assert!(exchange.write_claims.is_empty());
    wait_for_terminal_text(&runtime, "[wsx exchange 7, round 1, read-only]");
    wait_for_terminal_text(&runtime, "inspect the boundary");

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation.clone())),
        "pi".into(),
        AgentState::Working,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    let working = lock(&daemon.state).persisted.agent_exchanges[0].clone();
    assert_eq!(working.state, AgentExchangeState::WorkingObserved);
    assert_eq!(working.evidence, AgentExchangeEvidence::PaneLifecycle);

    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation.clone())),
        "pi".into(),
        AgentState::Blocked,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    let blocked = lock(&daemon.state).persisted.agent_exchanges[0].clone();
    assert_eq!(blocked.state, AgentExchangeState::BlockedObserved);
    assert!(matches!(
        wait_agent_exchange(&daemon, blocked.id, working.revision, 1, false).unwrap(),
        Response::AgentExchange { exchange, .. }
            if exchange.state == AgentExchangeState::BlockedObserved
    ));
    let continued = continue_agent_exchange(
        &daemon,
        blocked.id,
        blocked.revision,
        "verify once more".into(),
        60_000,
    )
    .unwrap();
    assert!(
        matches!(continued, Response::AgentExchange { exchange, .. } if exchange.round == 2 && exchange.state == AgentExchangeState::Delivered)
    );
    wait_for_terminal_text(&runtime, "[wsx exchange 7, round 2, read-only]");
    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
        "pi".into(),
        AgentState::Done,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        lock(&daemon.state).persisted.agent_exchanges[0].state,
        AgentExchangeState::DoneObserved
    );

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn exchange_wait_releases_the_mutation_fence_for_lifecycle_reports() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "wait concurrently".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    let waiter_daemon = Arc::clone(&daemon);
    let waiter = thread::spawn(move || {
        handle(
            &waiter_daemon,
            Request::AgentExchangeWait {
                exchange_id: exchange.id,
                after_revision: exchange.revision,
                timeout_ms: 1_000,
                include_frame: false,
            },
        )
    });
    thread::sleep(Duration::from_millis(20));
    let report = handle(
        &daemon,
        Request::AgentReport {
            pane_id: PaneId(4),
            runtime_generation: Some(generation),
            provider: "pi".into(),
            state: AgentState::Blocked,
            attached: true,
            presence_id: None,
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: AgentCapabilities {
                prompt: true,
                lifecycle: true,
                ..Default::default()
            },
            received: None,
            compacting: false,
        },
    );
    assert!(matches!(report, Response::Ack { .. }));
    assert!(matches!(
        waiter.join().unwrap(),
        Response::AgentExchange { exchange, .. }
            if exchange.state == AgentExchangeState::BlockedObserved
    ));
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn exchange_cancel_is_requested_until_lifecycle_confirms_and_stale_generation_fails() {
    let (daemon, path, runtime, generation) = exchange_test_daemon();
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "long review".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    lock(&daemon.state)
        .runtime_generations
        .insert(PaneId(4), "0000000000000002:0000000000000002".into());
    assert_eq!(
        cancel_agent_exchange(&daemon, exchange.id, exchange.revision, false)
            .unwrap_err()
            .code,
        "stale_runtime"
    );
    lock(&daemon.state)
        .runtime_generations
        .insert(PaneId(4), generation.clone());
    let Response::AgentExchange {
        exchange: cancelling,
        ..
    } = cancel_agent_exchange(&daemon, exchange.id, exchange.revision, false).unwrap()
    else {
        panic!("expected cancellation");
    };
    assert_eq!(cancelling.state, AgentExchangeState::CancelRequested);
    agent_report(
        &daemon,
        RuntimeAgentAuthority::new(PaneId(4), Some(generation)),
        "pi".into(),
        AgentState::Idle,
        None,
        None,
        AgentCapabilities {
            prompt: true,
            lifecycle: true,
            ..Default::default()
        },
    )
    .unwrap();
    let cancelled = lock(&daemon.state).persisted.agent_exchanges[0].clone();
    assert_eq!(cancelled.state, AgentExchangeState::Cancelled);

    assert_eq!(
        continue_agent_exchange(
            &daemon,
            cancelled.id,
            cancelled.revision,
            "retry".into(),
            60_000
        )
        .unwrap_err()
        .code,
        "invalid_exchange_state"
    );
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn writer_claims_reject_outside_symlinks_and_normalize_in_worktree_descendants() {
    let (_, state_path) = agent_test_daemon(agent_test_persisted());
    let worktree_path = state_path.with_extension("worktree");
    let outside_path = state_path.with_extension("outside");
    fs::create_dir(&worktree_path).unwrap();
    fs::create_dir(&outside_path).unwrap();
    let outside_link = worktree_path.join("outside");
    std::os::unix::fs::symlink(&outside_path, &outside_link).unwrap();
    let allowed_directory = worktree_path.join("allowed");
    fs::create_dir(&allowed_directory).unwrap();
    let worktree = Worktree {
        id: WorktreeId(2),
        project_id: ProjectId(1),
        path: worktree_path.clone(),
        branch: "main".into(),
        revision: 7,
    };

    assert_eq!(
        exchange_claims(
            AgentExchangeAccess::Writer,
            vec![outside_link.join("claimed")],
            &worktree,
        )
        .unwrap_err()
        .code,
        "invalid_claim"
    );

    let future_claim = allowed_directory.join("future");
    assert_eq!(
        exchange_claims(AgentExchangeAccess::Writer, vec![future_claim], &worktree,).unwrap(),
        vec![worktree_path.canonicalize().unwrap().join("allowed/future")]
    );

    fs::remove_dir_all(&worktree_path).unwrap();
    fs::remove_dir_all(&outside_path).unwrap();
}

#[test]
fn writer_claim_conflicts_are_nested_active_and_release_at_terminal_state() {
    let exchange = AgentExchange {
        id: AgentExchangeId(7),
        pane_id: PaneId(4),
        session_id: SessionId(3),
        worktree_id: WorktreeId(2),
        project_id: ProjectId(1),
        agent_id: AgentInstanceId(6),
        provider: "codex".into(),
        runtime_generation: "0000000000000001:0000000000000001".into(),
        round: 1,
        access: AgentExchangeAccess::Writer,
        write_claims: vec![PathBuf::from("/project/src")],
        state: AgentExchangeState::Delivered,
        evidence: AgentExchangeEvidence::PtyDelivery,
        created_unix_ms: 1,
        updated_unix_ms: 1,
        deadline_unix_ms: 2,
        delivery_revision: 8,
        delivery_sha256: None,
        native_input_id: None,
        failure: None,
        key: None,
        continue_key: None,
        command: false,
        revision: 9,
    };
    assert!(has_writer_claim_conflict(
        std::slice::from_ref(&exchange),
        &[PathBuf::from("/project/src/runtime")]
    ));
    assert!(has_writer_claim_conflict(
        std::slice::from_ref(&exchange),
        &[PathBuf::from("/project/src")]
    ));
    assert!(!has_writer_claim_conflict(
        std::slice::from_ref(&exchange),
        &[PathBuf::from("/project/src2")]
    ));
    assert!(!has_writer_claim_conflict(
        std::slice::from_ref(&exchange),
        &[PathBuf::from("/project/docs")]
    ));

    let mut child = exchange.clone();
    child.write_claims = vec![PathBuf::from("/project/src/runtime")];
    assert!(has_writer_claim_conflict(
        &[child],
        &[PathBuf::from("/project/src")]
    ));

    let mut done = exchange.clone();
    done.state = AgentExchangeState::DoneObserved;
    assert!(!has_writer_claim_conflict(
        &[done],
        &[PathBuf::from("/project/src")]
    ));
    let mut expired = exchange;
    expired.state = AgentExchangeState::Expired;
    assert!(!has_writer_claim_conflict(
        &[expired],
        &[PathBuf::from("/project/src")]
    ));
}

#[test]
fn writer_claim_conflict_matrix_distinguishes_ancestors_duplicates_and_siblings() {
    let exchange = AgentExchange {
        id: AgentExchangeId(7),
        pane_id: PaneId(4),
        session_id: SessionId(3),
        worktree_id: WorktreeId(2),
        project_id: ProjectId(1),
        agent_id: AgentInstanceId(6),
        provider: "codex".into(),
        runtime_generation: "0000000000000001:0000000000000001".into(),
        round: 1,
        access: AgentExchangeAccess::Writer,
        write_claims: vec![PathBuf::from("/project/src")],
        state: AgentExchangeState::Delivered,
        evidence: AgentExchangeEvidence::PtyDelivery,
        created_unix_ms: 1,
        updated_unix_ms: 1,
        deadline_unix_ms: 2,
        delivery_revision: 8,
        delivery_sha256: None,
        native_input_id: None,
        failure: None,
        key: None,
        continue_key: None,
        command: false,
        revision: 9,
    };

    for (active_claim, requested_claim, must_conflict) in [
        ("/project/src", "/project/src/child", true),
        ("/project/src/child", "/project/src", true),
        ("/project/src", "/project/src", true),
        ("/project/src", "/project/src2", false),
        ("/project/src", "/project/docs", false),
    ] {
        let mut active = exchange.clone();
        active.write_claims = vec![PathBuf::from(active_claim)];
        assert_eq!(
            has_writer_claim_conflict(&[active], &[PathBuf::from(requested_claim)]),
            must_conflict,
            "active {active_claim:?}, requested {requested_claim:?}"
        );
    }

    let mut read_only = exchange;
    read_only.access = AgentExchangeAccess::ReadOnly;
    read_only.write_claims.clear();
    assert!(read_only.write_claims.is_empty());
    assert!(!has_writer_claim_conflict(
        &[read_only],
        &[PathBuf::from("/project/src")]
    ));
}

#[test]
fn continue_rejects_noncontinuable_terminal_states_without_mutation() {
    let (daemon, path, runtime, _) = exchange_test_daemon();
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "terminal state".into(),
        60_000,
        AgentExchangeAccess::ReadOnly,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };

    for terminal_state in [
        AgentExchangeState::Cancelled,
        AgentExchangeState::TargetExited,
        AgentExchangeState::TargetReplaced,
    ] {
        let before = {
            let mut state = lock(&daemon.state);
            let record = state
                .persisted
                .agent_exchanges
                .iter_mut()
                .find(|record| record.id == exchange.id)
                .unwrap();
            record.state = terminal_state;
            record.clone()
        };
        assert_eq!(
            continue_agent_exchange(
                &daemon,
                before.id,
                before.revision,
                "must not continue".into(),
                60_000,
            )
            .unwrap_err()
            .code,
            "invalid_exchange_state"
        );
        let state = lock(&daemon.state);
        assert_eq!(
            state
                .persisted
                .agent_exchanges
                .iter()
                .find(|record| record.id == before.id)
                .unwrap(),
            &before
        );
    }

    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn expired_exchange_releases_its_writer_claim_on_list() {
    let (daemon, path, runtime, _) = exchange_test_daemon();
    let worktree_path = lock(&daemon.state).persisted.worktrees[0].path.clone();
    let Response::AgentExchange { exchange, .. } = create_agent_exchange(
        &daemon,
        PaneId(4),
        "bounded work".into(),
        60_000,
        AgentExchangeAccess::Writer,
        Vec::new(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    lock(&daemon.state).persisted.agent_exchanges[0].deadline_unix_ms = exchange.created_unix_ms;
    let Response::AgentExchanges(exchanges) = list_agent_exchanges(&daemon, None).unwrap() else {
        panic!("expected exchange list");
    };
    assert_eq!(exchanges[0].state, AgentExchangeState::Expired);
    assert!(!has_writer_claim_conflict(&exchanges, &[worktree_path]));
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn exchange_takes_terminal_lease_only_on_takeover_and_cold_recovery_interrupts_delivery() {
    let (daemon, path, runtime, _) = exchange_test_daemon();
    let worktree_path = lock(&daemon.state).persisted.worktrees[0].path.clone();
    lock(&daemon.state).leases.insert(
        PaneId(4),
        Lease {
            client_id: 99,
            generation: 1,
            expires_at: Instant::now() + LEASE_TTL,
        },
    );
    assert_eq!(
        create_agent_exchange(
            &daemon,
            PaneId(4),
            "blocked".into(),
            60_000,
            AgentExchangeAccess::ReadOnly,
            Vec::new()
        )
        .unwrap_err()
        .code,
        "terminal_busy"
    );
    assert!(lock(&daemon.state).leases.contains_key(&PaneId(4)));
    let Response::AgentExchange { exchange, .. } = create_agent_exchange_with(
        &daemon,
        PaneId(4),
        ExchangeInput {
            prompt: "persist me".into(),
            command: false,
            takeover: true,
        },
        60_000,
        AgentExchangeAccess::Writer,
        Vec::new(),
        ExchangeDelivery::default(),
    )
    .unwrap() else {
        panic!("expected exchange");
    };
    let displaced = StreamLease {
        pane_id: PaneId(4),
        client_id: 99,
        generation: 1,
    };
    {
        let state = lock(&daemon.state);
        assert!(!state.leases.contains_key(&PaneId(4)));
        assert_eq!(
            terminal_stream_lease_error(&state, displaced, Instant::now())
                .unwrap()
                .code,
            "lease_transferred"
        );
    }
    assert_eq!(exchange.write_claims, [worktree_path]);
    let restored = load_state(&path).unwrap();
    assert_eq!(
        restored.agent_exchanges[0].state,
        AgentExchangeState::Interrupted
    );
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn ids_are_monotonic_and_exhaustion_is_typed() {
    let mut state = Persisted::default();
    assert_eq!(next_id(&mut state).unwrap(), 1);
    assert_eq!(next_id(&mut state).unwrap(), 2);
    state.next_id = u64::MAX;
    assert_eq!(next_id(&mut state).unwrap_err().code, "id_exhausted");
}
fn reorder_test_persisted() -> Persisted {
    let mut persisted = agent_test_persisted();
    for (session_id, pane_id, terminal_id, label) in [(6, 8, 9, "second"), (7, 10, 11, "third")] {
        persisted.sessions.push(Session {
            id: SessionId(session_id),
            worktree_id: WorktreeId(2),
            label: label.into(),
            primary_pane: PaneId(pane_id),
            focused_pane: PaneId(pane_id),
            panes: vec![PaneId(pane_id)],
            layout: PaneLayout::Leaf {
                pane_id: PaneId(pane_id),
            },
            revision: 7,
        });
        persisted.panes.push(PersistedPane {
            pane: Pane {
                id: PaneId(pane_id),
                terminal_id: TerminalId(terminal_id),
                session_id: SessionId(session_id),
                label: "terminal".into(),
                agent: None,
                exited: true,
                revision: 7,
            },
            recovery_quarantined: false,
            recovery: None,
        });
    }
    persisted.next_id = 12;
    persisted
}

#[test]
fn session_reorder_is_daemon_owned_and_persists_by_stable_identity() {
    let (daemon, path) = agent_test_daemon(reorder_test_persisted());

    let response = handle(
        &daemon,
        Request::SessionReorder {
            session_id: SessionId(3),
            target_session_id: SessionId(6),
            placement: SessionPlacement::After,
            expected_revision: 7,
        },
    );

    assert!(
        matches!(response, Response::Ack { revision: 8 }),
        "unexpected reorder response: {response:?}"
    );
    assert_eq!(
        lock(&daemon.state)
            .persisted
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        [SessionId(6), SessionId(3), SessionId(7)]
    );
    let reloaded = load_state(&path).unwrap();
    assert_eq!(
        reloaded
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        [SessionId(6), SessionId(3), SessionId(7)]
    );
    assert_eq!(reloaded.sessions[1].revision, 8);
    fs::remove_file(path).unwrap();
}

#[test]
fn session_reorder_rejects_stale_and_cross_worktree_targets_without_mutation() {
    let (daemon, path) = agent_test_daemon(reorder_test_persisted());
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

    let stale = handle(
        &daemon,
        Request::SessionReorder {
            session_id: SessionId(3),
            target_session_id: SessionId(6),
            placement: SessionPlacement::After,
            expected_revision: 6,
        },
    );
    assert!(matches!(stale, Response::Error(ApiError { code, .. }) if code == "revision_conflict"));

    {
        let mut state = lock(&daemon.state);
        state.persisted.sessions[1].worktree_id = WorktreeId(99);
    }
    let cross_worktree = handle(
        &daemon,
        Request::SessionReorder {
            session_id: SessionId(3),
            target_session_id: SessionId(6),
            placement: SessionPlacement::After,
            expected_revision: 7,
        },
    );
    assert!(
        matches!(cross_worktree, Response::Error(ApiError { code, .. }) if code == "invalid_target")
    );
    {
        let mut state = lock(&daemon.state);
        state.persisted.sessions[1].worktree_id = WorktreeId(2);
        assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
        assert_eq!(state.revision, 7);
        assert!(state.events.is_empty());
    }
    assert!(!path.exists());
}

#[test]
fn session_reorder_save_failure_preserves_live_order_and_revision() {
    let (daemon, path) = agent_test_daemon(reorder_test_persisted());
    fs::create_dir(&path).unwrap();
    let before = serde_json::to_value(&lock(&daemon.state).persisted).unwrap();

    let response = handle(
        &daemon,
        Request::SessionReorder {
            session_id: SessionId(3),
            target_session_id: SessionId(6),
            placement: SessionPlacement::After,
            expected_revision: 7,
        },
    );

    assert!(matches!(response, Response::Error(ApiError { code, .. }) if code == "io"));
    let state = lock(&daemon.state);
    assert_eq!(serde_json::to_value(&state.persisted).unwrap(), before);
    assert_eq!(state.revision, 7);
    assert!(state.events.is_empty());
    drop(state);
    fs::remove_dir(path).unwrap();
}

#[test]
fn revisions_conflict_explicitly() {
    assert_eq!(expect_revision(2, 1).unwrap_err().code, "revision_conflict");
}
#[test]
fn utf8_truncation_never_splits_codepoints() {
    let value = truncate_utf8("가".repeat(300), 512);
    assert!(value.len() <= 512);
    assert!(value.chars().all(|character| character == '가'));
}
#[test]
fn default_state_is_valid() {
    validate_persisted(&Persisted::default()).unwrap();
}

#[test]
fn legacy_project_without_activity_timestamp_loads() {
    let (_, path) = agent_test_daemon(Persisted::default());
    fs::write(
            &path,
            r#"{
                "next_id":6,
                "projects":[{"id":1,"path":"/project","name":"project","revision":1}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":1}],
                "sessions":[{"id":3,"worktree_id":2,"label":"legacy","primary_pane":4,"focused_pane":4,"panes":[4],"layout":{"kind":"leaf","pane_id":4},"revision":1}],
                "panes":[{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":false,"revision":1}]
            }"#,
        )
        .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let state = load_state(&path).unwrap();

    assert_eq!(state.projects[0].last_agent_active_unix_ms, None);
    assert_eq!(state.panes.len(), 1);
    assert!(state.panes[0].recovery.is_none());
    assert!(!state.panes[0].recovery_quarantined);
    fs::remove_file(path).unwrap();
}

#[test]
fn malformed_recovery_recipe_is_quarantined() {
    let pane = serde_json::from_str::<PersistedPane>(
            r#"{"id":4,"terminal_id":5,"session_id":3,"label":"terminal","agent":null,"exited":true,"revision":1,"recovery":{"command":"not-an-argv","rows":24,"cols":80}}"#,
        )
        .unwrap();

    assert!(pane.recovery.is_none());
    assert!(pane.recovery_quarantined);
    let encoded = serde_json::to_string(&pane).unwrap();
    assert!(encoded.contains("recovery_quarantined"));
    assert!(
        serde_json::from_str::<PersistedPane>(&encoded)
            .unwrap()
            .recovery_quarantined
    );
}

#[test]
fn recovered_state_with_agent_metadata_is_valid() {
    let state = serde_json::from_str::<Persisted>(
            r#"{
                "next_id":19,
                "projects":[{"id":1,"path":"/project","name":"project","revision":2}],
                "worktrees":[{"id":2,"project_id":1,"path":"/project","branch":"main","revision":2}],
                "sessions":[{"id":15,"worktree_id":2,"label":"recoverable","primary_pane":16,"focused_pane":16,"panes":[16],"layout":{"kind":"leaf","pane_id":16},"revision":26}],
                "panes":[{"id":16,"terminal_id":17,"session_id":15,"label":"terminal","agent":{"id":18,"provider":"codex","state":"unknown","conversation_id":"conversation","capabilities":{"prompt":false,"resume":false,"lifecycle":false},"source":"persisted"},"exited":false,"revision":35,"recovery":{"command":["/bin/sh"],"initial_input":"printf ok","rows":12,"cols":40}}]
            }"#,
        )
        .unwrap();

    validate_persisted(&state).unwrap();
    assert_eq!(state.panes[0].recovery.as_ref().unwrap().rows, 12);
    assert!(!state.panes[0].recovery_quarantined);
}

#[test]
fn launch_recipe_rejects_unbounded_or_nul_startup_input() {
    assert!(launch_recipe(Vec::new(), Some("x".repeat(MAX_INPUT_BYTES)), 24, 80).is_err());
    assert!(launch_recipe(Vec::new(), Some("bad\0input".into()), 24, 80).is_err());
}

#[test]
fn terminal_view_deduplicates_panes_and_bounds_aggregate_cells() {
    assert_eq!(
        unique_view_pane_ids(vec![PaneId(1), PaneId(1), PaneId(2)]),
        vec![PaneId(1), PaneId(2)]
    );
    assert_eq!(add_view_cells(0, MAX_VIEW_CELLS).unwrap(), MAX_VIEW_CELLS);
    assert_eq!(
        add_view_cells(MAX_VIEW_CELLS, 1).unwrap_err().code,
        "response_too_large"
    );
}

#[test]
fn state_save_does_not_follow_the_legacy_predictable_temp_symlink() {
    use std::os::unix::fs::symlink;

    let directory = std::env::current_dir().unwrap().join(format!(
        ".work/daemon-tests/state-save-{}-{}",
        std::process::id(),
        epoch()
    ));
    fs::create_dir_all(&directory).unwrap();
    let state_path = directory.join("state.json");
    let victim = directory.join("victim");
    fs::write(&victim, b"unchanged").unwrap();
    symlink(&victim, state_path.with_extension("json.tmp")).unwrap();

    save_state(&state_path, &Persisted::default()).unwrap();

    assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
    assert_eq!(
        fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!fs::read_dir(&directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("state.json.tmp.")
    }));
    fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn foreground_job_changes_without_waiting_for_listener_scan() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let pane_id = PaneId(4);
    let runtime = Arc::new(
        spawn_runtime(
            &daemon,
            pane_id,
            TerminalId(5),
            Path::new("/"),
            &LaunchRecipe {
                command: vec!["/bin/sh".into()],
                initial_input: None,
                rows: 8,
                cols: 40,
            },
        )
        .unwrap(),
    );
    lock(&daemon.state)
        .runtimes
        .insert(pane_id, Arc::clone(&runtime));
    let scanner = spawn_foreground_scanner(&daemon);
    runtime.write(b"sleep 4\r").unwrap();
    let started = Instant::now();
    while !lock(&daemon.state).foreground_jobs.contains(&pane_id) {
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "foreground job was not observed live"
        );
        thread::sleep(Duration::from_millis(10));
    }
    runtime.write(b"\x03").unwrap();
    let interrupted = Instant::now();
    while lock(&daemon.state).foreground_jobs.contains(&pane_id) {
        assert!(
            interrupted.elapsed() < Duration::from_secs(1),
            "interrupted job stayed active"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!lock(&daemon.state).listener_scan_complete);
    lock(&daemon.state).stopping = true;
    scanner.join().unwrap();
    runtime.terminate();
    let _ = fs::remove_file(path);
}

#[test]
fn failed_listener_scan_preserves_prior_server_fence() {
    let (daemon, path) = agent_test_daemon(agent_test_persisted());
    let mut state = lock(&daemon.state);
    state.listener_scan_complete = true;
    state.listening_ports.insert(PaneId(4), vec![3000]);

    assert!(!apply_port_scan(&mut state, None));
    assert!(state.listener_scan_complete);
    assert_eq!(state.listening_ports[&PaneId(4)], vec![3000]);
    assert!(replacement_blockers(&mut state, "0.23.0:1:2:3:20")
        .contains(&ReplacementBlocker::ListeningPort));
    drop(state);
    let _ = fs::remove_file(path);
}

#[test]
fn listener_scan_finds_current_process_group_port_when_lsof_is_available() {
    if Command::new("lsof").arg("-v").output().is_err() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let pid = std::process::id() as libc::pid_t;
    let group = unsafe { libc::getpgrp() };

    let listeners = scan_listening_ports().unwrap();

    assert!(listeners.iter().any(|process| {
        process.pid == pid && process.group == group && process.ports.contains(&port)
    }));
}

#[test]
fn lsof_parser_groups_and_deduplicates_tcp_listeners() {
    let listeners = parse_lsof_ports(
        "p100\ng42\nf8\nn127.0.0.1:3000\nf9\nn*:3000\np101\ng43\nf7\nn[::1]:5173\n",
    )
    .unwrap();

    assert_eq!(
        listeners,
        vec![
            ListenerProcess {
                pid: 100,
                group: 42,
                ports: vec![3000],
            },
            ListenerProcess {
                pid: 101,
                group: 43,
                ports: vec![5173],
            },
        ]
    );
}

#[test]
fn listener_attribution_accepts_descendant_groups_on_a_unique_pane_tty() {
    let pane = PaneId(7);
    let process_groups = HashMap::from([(pane, 42)]);
    let listeners = vec![ListenerProcess {
        pid: 101,
        group: 99,
        ports: vec![5173, 8081, 5173],
    }];
    let terminals = parse_process_terminals("100 42 ttys007\n101 99 ttys007\n").unwrap();

    let attributed = attribute_listening_ports(&process_groups, &listeners, Some(&terminals));

    assert_eq!(attributed.get(&pane), Some(&vec![5173, 8081]));
}

#[test]
fn listener_attribution_rejects_ambiguous_or_missing_ttys_but_keeps_exact_groups() {
    let first = PaneId(7);
    let second = PaneId(8);
    let process_groups = HashMap::from([(first, 42), (second, 43)]);
    let listeners = vec![
        ListenerProcess {
            pid: 101,
            group: 99,
            ports: vec![5173],
        },
        ListenerProcess {
            pid: 102,
            group: 42,
            ports: vec![9000],
        },
    ];
    let terminals =
        parse_process_terminals("100 42 ttys007\n103 43 ttys007\n101 99 ttys007\n104 100 ??\n")
            .unwrap();

    let attributed = attribute_listening_ports(&process_groups, &listeners, Some(&terminals));

    assert_eq!(attributed.get(&first), Some(&vec![9000]));
    assert!(!attributed.contains_key(&second));
}

const CODEX_SESSION_ID: &str = "0195d8f4-8c88-7b32-8aee-7d3a6c32c5f8";

fn recovery_test_agent(provider: &str, conversation_id: Option<&str>) -> AgentInfo {
    serde_json::from_value(serde_json::json!({
        "id": 18,
        "provider": provider,
        "state": "unknown",
        "conversation_id": conversation_id,
        "capabilities": {"prompt": false, "resume": false, "lifecycle": false},
        "source": "persisted"
    }))
    .unwrap()
}

fn recovery_test_recipe() -> LaunchRecipe {
    LaunchRecipe {
        command: vec!["saved-agent".into(), "--restore".into()],
        initial_input: Some("saved agent-start input".into()),
        rows: 37,
        cols: 113,
    }
}

fn assert_recipe_matches(actual: &LaunchRecipe, expected: &LaunchRecipe) {
    assert_eq!(actual.command, expected.command);
    assert_eq!(actual.initial_input, expected.initial_input);
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.cols, expected.cols);
}

fn assert_clean_shell_recipe(recipe: &LaunchRecipe, saved_recipe: &LaunchRecipe) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    assert_eq!(recipe.command, vec![shell]);
    assert_eq!(recipe.initial_input, None);
    assert_eq!(recipe.rows, saved_recipe.rows);
    assert_eq!(recipe.cols, saved_recipe.cols);
}

#[test]
fn recovery_launch_valid_codex_id_provides_a_resume_key() {
    let saved_recipe = recovery_test_recipe();
    let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

    let launch = recovery_launch(Some(&mut agent), &saved_recipe, true);

    assert_eq!(
        launch.recipe.command,
        vec![
            "codex".to_string(),
            "resume".to_string(),
            CODEX_SESSION_ID.to_string(),
        ]
    );
    assert_eq!(launch.recipe.initial_input, None);
    assert_eq!(launch.recipe.rows, saved_recipe.rows);
    assert_eq!(launch.recipe.cols, saved_recipe.cols);
    assert!(launch.resume_key.is_some());
}

#[test]
fn recovery_launch_disabled_leaves_saved_recipe_without_a_resume_key() {
    let saved_recipe = recovery_test_recipe();
    let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

    let launch = recovery_launch(Some(&mut agent), &saved_recipe, false);

    assert_recipe_matches(&launch.recipe, &saved_recipe);
    assert!(launch.resume_key.is_none());
}

#[test]
fn recovery_launch_without_agent_or_session_ref_leaves_saved_recipe_unchanged() {
    let saved_recipe = recovery_test_recipe();

    let launch = recovery_launch(None, &saved_recipe, true);
    assert_recipe_matches(&launch.recipe, &saved_recipe);
    assert!(launch.resume_key.is_none());

    let mut agent_without_ref = recovery_test_agent("codex", None);
    let launch = recovery_launch(Some(&mut agent_without_ref), &saved_recipe, true);
    assert_recipe_matches(&launch.recipe, &saved_recipe);
    assert!(launch.resume_key.is_none());
}

#[test]
fn recovery_launch_legacy_codex_conversation_migrates_to_session_ref() {
    let saved_recipe = recovery_test_recipe();
    let mut agent = recovery_test_agent("codex", Some(CODEX_SESSION_ID));

    let launch = recovery_launch(Some(&mut agent), &saved_recipe, true);

    assert_eq!(
        launch.recipe.command,
        vec![
            "codex".to_string(),
            "resume".to_string(),
            CODEX_SESSION_ID.to_string(),
        ]
    );
    assert!(agent.session_ref.is_some());
}

#[test]
fn recovery_launch_unsupported_or_malformed_ref_uses_clean_shell() {
    let saved_recipe = recovery_test_recipe();
    let mut seeded = recovery_test_agent("codex", Some(CODEX_SESSION_ID));
    recovery_launch(Some(&mut seeded), &saved_recipe, true);

    let mut unsupported = seeded.clone();
    unsupported.provider = "unsupported".into();
    let unsupported_launch = recovery_launch(Some(&mut unsupported), &saved_recipe, true);
    assert_clean_shell_recipe(&unsupported_launch.recipe, &saved_recipe);
    assert!(unsupported_launch.resume_key.is_none());

    let malformed_id = format!("{CODEX_SESSION_ID}\\n");
    let encoded = serde_json::to_string(&seeded)
        .unwrap()
        .replace(CODEX_SESSION_ID, &malformed_id);
    let mut malformed: AgentInfo = serde_json::from_str(&encoded).unwrap();
    let malformed_launch = recovery_launch(Some(&mut malformed), &saved_recipe, true);
    assert_clean_shell_recipe(&malformed_launch.recipe, &saved_recipe);
    assert!(malformed_launch.resume_key.is_none());
}
