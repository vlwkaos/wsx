//! The daemon process: startup, the accept loop, client connections and shutdown.

use super::*;

pub fn run() -> io::Result<()> {
    run_daemon(None)
}

pub fn run_routine_daemon(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected routine daemon argument",
        ));
    }
    let root = asched_core::RegistryStore::default_root().map_err(io::Error::other)?;
    asched_core::routine::daemon::serve_from_startup_env(root).map_err(io::Error::other)
}

pub fn run_handoff_import(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    let socket = arguments
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "handoff socket is missing"))?;
    let token = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "handoff token is missing"))?;
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected handoff import argument",
        ));
    }
    run_daemon(Some(handoff::receive(&socket, &token)?))
}

pub(crate) fn new_state(persisted: Persisted) -> State {
    State {
        persisted,
        revision: 1,
        runtimes: HashMap::new(),
        runtime_generations: HashMap::new(),
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
        plugins: plugins::discover(),
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
    }
}

pub(crate) fn run_daemon(import: Option<handoff::Received>) -> io::Result<()> {
    install_shutdown_signals()?;
    raise_descriptor_limit();
    let socket = default_socket_path();
    let state_path = state_path(&socket);
    secure_parent(&socket)?;

    let (singleton_lock, persisted, recovered_from_backup, import) = match import {
        Some(handoff::Received {
            manifest,
            singleton_lock,
            pane_fds,
            stream,
        }) => {
            validate_persisted(&manifest.persisted)?;
            (
                fs::File::from(singleton_lock),
                manifest.persisted,
                false,
                Some((manifest.panes, manifest.exited_panes, pane_fds, stream)),
            )
        }
        None => {
            let singleton_lock = acquire_singleton_lock(&state_path.with_extension("lock"))?;
            prepare_socket(&socket)?;
            let (persisted, recovered) = load_state_with_status(&state_path)?;
            (singleton_lock, persisted, recovered, None)
        }
    };

    let daemon = Arc::new(Daemon {
        state: Mutex::new(new_state(persisted)),
        mutation_lock: Mutex::new(()),
        changed: Condvar::new(),
        plugin_changed: Condvar::new(),
        active_clients: Arc::new(AtomicUsize::new(0)),
        review_jobs: plugins::review::Registry::default(),
        epoch: epoch(),
        binary_id: binary_identity(&std::env::current_exe()?)?,
        started_unix_ms: unix_time_millis(),
        recovered_from_backup,
        next_runtime_generation: AtomicU64::new(1),
        state_path,
        lifecycle_path: lifecycle_marker_path(&socket),
    });

    let cold_start = import.is_none();
    let mut import_stream = None;
    if let Some((panes, exited_panes, pane_fds, mut stream)) = import {
        if panes.len() != pane_fds.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff pane and descriptor counts differ",
            ));
        }
        restore_handoff_runtimes(&daemon, panes, exited_panes, pane_fds)?;
        handoff::report_restored(&mut stream)?;
        handoff::wait_publish(&mut stream)?;
        prepare_socket(&socket)?;
        import_stream = Some(stream);
    }

    // ^ docs/agent-reporting.md: stage before readiness, publish only under ownership.
    let mut reporter = Some(reporter::Publication::prepare(
        &socket,
        &std::env::current_exe()?,
    )?);
    let listener = bind_owner_only(&socket)?;
    listener.set_nonblocking(true)?;
    let mut listener = Some(listener);
    let plugin_dispatcher = match spawn_plugin_dispatcher(&daemon) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };

    if let Some(mut stream) = import_stream {
        write_lifecycle_marker(&daemon.lifecycle_path, "ready")?;
        handoff::report_ready(&mut stream)?;
        handoff::wait_commit(&mut stream)?;
        // ^ docs/daemon-live-upgrade.md "A successor that dies": scripts/runtime-smoke.py
        // ends a successor here, after the commit and before `owned`, which no outside
        // signal can time. Read only by a successor, never by a daemon that starts cold.
        if std::env::var_os("WSX_HANDOFF_FAULT").is_some_and(|fault| fault == "after_commit") {
            std::process::exit(70);
        }
        // Commit is irreversible. A publication failure must not kill imported PTYs.
        if let Err(error) = reporter::publish_pending(&mut reporter) {
            eprintln!("wsxd reporter publication after handoff commit: {error}; retrying under successor ownership");
        }
        let runtimes = lock(&daemon.state)
            .runtimes
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in runtimes {
            runtime.assume_handoff_ownership();
            runtime.activate_handoff();
        }
        let _ = handoff::report_owned(&mut stream);
    } else {
        reporter::publish_pending(&mut reporter)?;
        write_lifecycle_marker(&daemon.lifecycle_path, "ready")?;
    }

    // ^ [[Session Model]] Cold recovery begins only after lifecycle hooks can reach
    // the canonical socket. Agent resumes are queued by recover_runtimes.
    let mut cold_recovery =
        cold_start.then(|| spawn_cold_recovery(&daemon, resume_agents_on_restore()));
    let mut reporter_retry_at = Instant::now() + Duration::from_secs(1);
    let mut next_presence_poll = Instant::now() + AGENT_PRESENCE_POLL;
    let port_scanner = spawn_port_scanner(&daemon);
    let foreground_scanner = spawn_foreground_scanner(&daemon);
    let claude_reconciler = spawn_claude_reconciler(&daemon);
    let wake_controller = spawn_wake_controller(&daemon);

    while !advance_replacement(&daemon) {
        retry_dirty_persistence(&daemon);
        // ^ docs/agent-reporting.md: retain staged publication after an
        // irreversible commit and retry only while this daemon owns the runtime.
        if reporter.is_some() && Instant::now() >= reporter_retry_at {
            if reporter::publish_pending(&mut reporter).is_ok() {
                eprintln!("wsxd reporter publication recovered");
            }
            reporter_retry_at = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= next_presence_poll {
            expire_agent_presence(&daemon, Instant::now());
            next_presence_poll = Instant::now() + AGENT_PRESENCE_POLL;
        }
        if let Some(target) = take_ready_handoff(&daemon) {
            match perform_live_handoff(&daemon, &socket, &singleton_lock, &mut listener, target) {
                Ok(()) => break,
                Err(error) => eprintln!("wsxd handoff: {error}"),
            }
        }
        let Some(active_listener) = listener.as_ref() else {
            // ^ Only a rollback that could not bind the socket again leaves it unbound.
            if let Err(error) = rebind_listener(&socket, &mut listener) {
                eprintln!("wsxd socket: {error}");
                thread::sleep(Duration::from_secs(1));
            }
            continue;
        };
        match active_listener.accept() {
            Ok((stream, _)) => {
                if daemon.active_clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                    daemon.active_clients.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let daemon = Arc::clone(&daemon);
                let clients = Arc::clone(&daemon.active_clients);
                thread::spawn(move || {
                    let _guard = ClientGuard(clients);
                    if let Err(error) = serve_client(stream, daemon) {
                        eprintln!("wsxd client: {error}");
                    }
                });
            }
            Err(error) if accept_error_is_transient(&error) => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                {
                    let mut state = lock(&daemon.state);
                    state.stopping = true;
                    daemon.changed.notify_all();
                }
                if let Some(recovery) = cold_recovery.take() {
                    let _ = recovery.join();
                }
                cleanup(&daemon, &socket);
                let _ = plugin_dispatcher.join();
                let _ = port_scanner.join();
                let _ = foreground_scanner.join();
                let _ = claude_reconciler.join();
                let _ = wake_controller.join();
                return Err(error);
            }
        }
    }
    if let Some(recovery) = cold_recovery.take() {
        let _ = recovery.join();
    }
    cleanup(&daemon, &socket);
    let _ = plugin_dispatcher.join();
    let _ = port_scanner.join();
    let _ = foreground_scanner.join();
    let _ = claude_reconciler.join();
    let _ = wake_controller.join();
    Ok(())
}

pub(crate) extern "C" fn request_signal_shutdown(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

pub(crate) fn install_shutdown_signals() -> io::Result<()> {
    // ^ crates/wsx-core/src/runtime/client.rs detaches automatic starts before exec.
    // Ignore terminal hangup here as well so direct starts keep host-owned PTYs alive.
    if unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    let previous = unsafe {
        libc::signal(
            libc::SIGTERM,
            request_signal_shutdown as *const () as libc::sighandler_t,
        )
    };
    if previous == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ^ A GUI terminal passes launchd's soft limit of 256 descriptors on to wsxd. Each pane
// holds about five and each connection two, so agents reporting from many panes at once
// ran out of descriptors. Raise the soft limit toward the hard one, at most to macOS's
// OPEN_MAX (10240), which macOS requires and which keeps panes, that inherit it, from a
// Linux hard limit of millions. Failure keeps the inherited limit.
pub(crate) fn raise_descriptor_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let target = limit.rlim_max.min(10240);
    if limit.rlim_cur < target {
        limit.rlim_cur = target;
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) };
    }
}

// ^ An accept error that belongs to one connection or to a short resource shortage ends
// that connection only. Before 0.40.0 it stopped wsxd and ended every PTY at once.
pub(crate) fn accept_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::EPROTO)
    )
}

// ^ A missing resident-adapter renewal revokes only its verified attachment.
// Event-only adapters never enter this map; a reported shutdown remains immediate.
pub(crate) fn expire_agent_presence(daemon: &Daemon, now: Instant) {
    let mut state = lock(&daemon.state);
    let expired = state
        .agent_presence_leases
        .iter()
        .filter(|(_, renewed_at)| now.saturating_duration_since(**renewed_at) >= AGENT_PRESENCE_TTL)
        .map(|(pane_id, _)| *pane_id)
        .collect::<Vec<_>>();
    let mut changed = Vec::new();
    for pane_id in expired {
        state.agent_presence_leases.remove(&pane_id);
        let revision = state.revision.saturating_add(changed.len() as u64 + 1);
        let Some(pane) = state
            .persisted
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
        else {
            continue;
        };
        let Some(agent) = pane
            .agent
            .as_mut()
            .filter(|agent| agent.attached && agent.presence_id.is_some())
        else {
            continue;
        };
        detach_agent(agent);
        pane.revision = revision;
        observe_agent_exchange(
            &mut state.persisted,
            pane_id,
            AgentState::Unknown,
            false,
            revision,
            unix_time_millis(),
        );
        clear_agent_wake(&mut state, pane_id);
        state.claude_reconciliations.remove(&pane_id);
        changed.push((pane_id, revision));
    }
    if changed.is_empty() {
        return;
    }
    if save_state(&daemon.state_path, &state.persisted).is_err() {
        state.persistence_dirty = true;
    }
    for (pane_id, revision) in changed {
        publish(
            daemon,
            &mut state,
            revision,
            "agent.presence_expired",
            pane_id.0,
        );
    }
}

pub(crate) fn retry_dirty_persistence(daemon: &Daemon) {
    let mut state = lock(&daemon.state);
    if state.persistence_dirty && save_state(&daemon.state_path, &state.persisted).is_ok() {
        state.persistence_dirty = false;
    }
}

pub(crate) fn advance_replacement(daemon: &Daemon) -> bool {
    let mut state = lock(&daemon.state);
    if STOP_REQUESTED.swap(false, Ordering::AcqRel) {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Intentional);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
    }
    let replacement_ready = if state.stopping || state.replacement_executable.is_some() {
        false
    } else {
        state
            .replacement_target
            .clone()
            .is_some_and(|target| replacement_blockers(&mut state, &target).is_empty())
    };
    if replacement_ready {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Replacement);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
    }
    state.stopping
}

pub(crate) struct ClientGuard(Arc<AtomicUsize>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn acquire_singleton_lock(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsxd singleton lock",
        ));
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(file)
    } else {
        let error = io::Error::last_os_error();
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another wsxd owns {}: {error}", path.display()),
        ))
    }
}

/// Listen on `socket`, which appears only once it is owner-only: a client refuses a socket
/// another user could reach, so one that connected between a bind and a chmod failed.
/// ^ The name gains one character, so it stays within the socket path limit; the
/// singleton lock keeps a second wsxd from using the same name.
pub(crate) fn bind_owner_only(socket: &Path) -> io::Result<UnixListener> {
    let mut name = socket.file_name().unwrap_or_default().to_os_string();
    name.push("~");
    let staging = socket.with_file_name(name);
    let _ = fs::remove_file(&staging);
    let listener = UnixListener::bind(&staging)?;
    if let Err(error) = fs::set_permissions(&staging, fs::Permissions::from_mode(0o600))
        .and_then(|()| fs::rename(&staging, socket))
    {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    Ok(listener)
}

pub(crate) fn cleanup(daemon: &Daemon, socket: &Path) {
    let (runtimes, reason, preserve_runtimes) = {
        let mut state = lock(&daemon.state);
        state.stopping = true;
        state.leases.clear();
        state.transferred_streams.clear();
        state.agent_wake_leases.clear();
        state.agent_presence_leases.clear();
        state.agent_wake_tokens.clear();
        state.claude_reconciliations.clear();
        state.plugin_events.clear();
        let runtimes = std::mem::take(&mut state.runtimes);
        state.runtime_generations.clear();
        let reason = state.stop_reason.unwrap_or(StopReason::Unexpected);
        let preserve_runtimes = reason == StopReason::Handoff;
        if !preserve_runtimes {
            let _ = save_state(&daemon.state_path, &state.persisted);
        }
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        let marker = if matches!(reason, StopReason::Replacement | StopReason::Handoff) {
            let target = state.replacement_target.as_deref().unwrap_or_default();
            match state
                .replacement_daemon_revision
                .filter(|revision| *revision > 0)
            {
                Some(revision) => format!("replacement:{target}|{revision}"),
                None => format!("replacement:{target}"),
            }
        } else {
            reason.marker().to_string()
        };
        (runtimes, marker, preserve_runtimes)
    };
    if !preserve_runtimes {
        for runtime in runtimes.into_values() {
            runtime.terminate();
        }
    }
    if !preserve_runtimes {
        let _ = write_lifecycle_marker(&daemon.lifecycle_path, &reason);
        let _ = fs::remove_file(socket);
    }
}

pub(crate) fn serve_client(mut stream: UnixStream, daemon: Arc<Daemon>) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(line) = read_bounded_line(&mut reader)? else {
        return Ok(());
    };
    let request = match serde_json::from_str::<Request>(&line) {
        Ok(request) => request,
        Err(error) => {
            return write_response(
                &mut stream,
                Response::Error(ApiError::new("invalid_json", error.to_string())),
            )
        }
    };
    let Request::Hello { protocol } = request else {
        return write_response(
            &mut stream,
            Response::Error(ApiError::new(
                "handshake_required",
                "hello must be the first request on a connection",
            )),
        );
    };
    let compatible = protocol == PROTOCOL_VERSION;
    write_response(
        &mut stream,
        Response::Hello {
            protocol: PROTOCOL_VERSION,
            epoch: daemon.epoch,
            capabilities: capabilities(),
        },
    )?;
    if !compatible {
        let Some(line) = read_bounded_line(&mut reader)? else {
            return Ok(());
        };
        // ^ docs/daemon-live-upgrade.md: a request this daemon does not know is answered,
        // never dropped, so a client of another release reads a reason.
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(
                request @ (Request::Shutdown
                | Request::LifecycleStatus
                | Request::PrepareReplacement { .. }
                | Request::PrepareHandoff { .. }),
            ) => handle(&daemon, request),
            Ok(_) => Response::Error(ApiError::new(
                "protocol_mismatch",
                format!("client {protocol}, daemon {PROTOCOL_VERSION}"),
            )),
            Err(error) => Response::Error(ApiError::new("invalid_json", error.to_string())),
        };
        return write_response(&mut stream, response);
    }

    while let Some(line) = read_bounded_line(&mut reader)? {
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stream,
                    Response::Error(ApiError::new("invalid_json", error.to_string())),
                )?;
                continue;
            }
        };
        let (kind, lease, size) = match request {
            Request::TerminalSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            } => (
                TerminalStreamKind::Frames,
                (pane_id, client_id, takeover),
                (rows, cols),
            ),
            Request::TerminalRawSubscribe {
                pane_id,
                client_id,
                takeover,
                rows,
                cols,
            } => (
                TerminalStreamKind::Raw,
                (pane_id, client_id, takeover),
                (rows, cols),
            ),
            request => {
                write_response(&mut stream, handle(&daemon, request))?;
                continue;
            }
        };
        return serve_terminal_stream(stream, daemon, kind, lease, size);
    }
    Ok(())
}

pub(crate) fn read_bounded_line(reader: &mut BufReader<UnixStream>) -> io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_REQUEST_BYTES || !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request is incomplete or exceeds limit",
        ));
    }
    Ok(Some(line))
}

pub(crate) fn write_response(stream: &mut UnixStream, response: Response) -> io::Result<()> {
    let bytes = encode_line(&response).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds limit",
        ));
    }
    stream.write_all(&bytes)
}
