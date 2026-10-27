//! Live handoff to a successor daemon and the replacement status that gates it.

use super::*;

#[derive(Clone)]
pub(crate) struct HandoffTarget {
    binary_id: String,
    version: String,
    protocol: u32,
    daemon_revision: u32,
    executable: PathBuf,
}

pub(crate) fn take_ready_handoff(daemon: &Daemon) -> Option<HandoffTarget> {
    // ^ The accept loop calls this on every pass. A handoff starts between mutations, so
    // while one runs this pass is skipped and the loop keeps accepting connections.
    let _mutation = match daemon.mutation_lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let mut state = lock(&daemon.state);
    // ^ A delivery releases the mutation lock during its PTY write; its PTY must not move
    // to the successor before the outcome is recorded.
    if state.stopping || state.handoff_in_progress || state.deliveries_in_flight > 0 {
        return None;
    }
    let binary_id = state.replacement_target.clone()?;
    let executable = state.replacement_executable.clone()?;
    if !handoff_blockers(&mut state, &binary_id).is_empty() {
        return None;
    }
    let target = HandoffTarget {
        version: binary_identity_version(&binary_id)?.to_string(),
        binary_id,
        protocol: state.replacement_protocol?,
        daemon_revision: state.replacement_daemon_revision?,
        executable,
    };
    // ^ docs/daemon-live-upgrade.md: mark the whole transition before any client is
    // told, so attached clients wait for the successor or the rollback.
    if let Err(error) = write_lifecycle_marker(
        &daemon.lifecycle_path,
        &format!("{HANDOFF_MARKER_PREFIX}{}", unix_time_millis()),
    ) {
        eprintln!("wsxd handoff marker: {error}");
        return None;
    }
    state.handoff_in_progress = true;
    state.handoff_started = Some(Instant::now());
    daemon.changed.notify_all();
    Some(target)
}

pub(crate) fn perform_live_handoff(
    daemon: &Arc<Daemon>,
    socket: &Path,
    singleton_lock: &fs::File,
    listener: &mut Option<UnixListener>,
    target: HandoffTarget,
) -> io::Result<()> {
    if let Err(error) = validate_handoff_executable(&target.executable, &target.binary_id) {
        rollback_handoff(daemon, socket, listener, &[], "validate", &error);
        return Err(error);
    }
    let runtimes = {
        let state = lock(&daemon.state);
        let mut runtimes = state
            .runtimes
            .iter()
            .map(|(pane_id, runtime)| (*pane_id, Arc::clone(runtime)))
            .collect::<Vec<_>>();
        runtimes.sort_by_key(|(pane_id, _)| *pane_id);
        runtimes
    };
    let operation_locks = runtimes
        .iter()
        .map(|(pane_id, _)| terminal_operation_lock(daemon, *pane_id))
        .collect::<Vec<_>>();
    let _operations = operation_locks
        .iter()
        .map(|operation_lock| lock(operation_lock))
        .collect::<Vec<_>>();

    // ^ A pane whose program exited has no reader to pause and no process to hand over: it
    // goes as its screen alone (docs/daemon-live-upgrade.md "Exited panes"). A pane that
    // exits during the pause is one of them.
    let mut paused = Vec::new();
    let mut live = Vec::new();
    let mut exited = Vec::new();
    for (pane_id, runtime) in &runtimes {
        if runtime.exited() {
            exited.push((*pane_id, Arc::clone(runtime)));
            continue;
        }
        if let Err(error) = runtime.pause_handoff_reader() {
            if runtime.exited() {
                exited.push((*pane_id, Arc::clone(runtime)));
                continue;
            }
            let error = io::Error::other(format!("pane {}: {error}", pane_id.0));
            rollback_handoff(daemon, socket, listener, &paused, "pause", &error);
            return Err(error);
        }
        paused.push(Arc::clone(runtime));
        live.push((*pane_id, Arc::clone(runtime)));
    }

    let mut duplicate_fds = Vec::<std::os::fd::OwnedFd>::new();
    let mut source_ownership = Vec::<(Arc<TerminalRuntime>, libc::pid_t)>::new();
    let mut step = "export";
    let result = (|| {
        let (persisted, generations) = {
            let state = lock(&daemon.state);
            save_state(&daemon.state_path, &state.persisted)?;
            (state.persisted.clone(), state.runtime_generations.clone())
        };
        let export = |pane_id: &PaneId, runtime: &TerminalRuntime| {
            runtime
                .handoff_state()
                .map(|state| handoff::PaneHandoff {
                    state,
                    runtime_generation: generations.get(pane_id).cloned(),
                })
                .map_err(|error| io::Error::other(format!("pane {}: {error}", pane_id.0)))
        };
        let mut pane_handoffs = Vec::with_capacity(live.len());
        for (pane_id, runtime) in &live {
            pane_handoffs.push(export(pane_id, runtime)?);
            let fd = runtime
                .duplicate_handoff_fd()
                .map_err(|error| io::Error::other(format!("pane {}: {error}", pane_id.0)))?;
            duplicate_fds.push(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        }
        let exited_handoffs = exited
            .iter()
            .map(|(pane_id, runtime)| export(pane_id, runtime))
            .collect::<io::Result<Vec<_>>>()?;
        let manifest = handoff::manifest(
            persisted,
            pane_handoffs,
            exited_handoffs,
            target.version.clone(),
            target.protocol,
            target.daemon_revision,
        );
        step = "spawn";
        let handoff_path = handoff::socket_path(socket, daemon.epoch)?;
        let handoff_listener = handoff::bind(&handoff_path)?;
        let token = format!(
            "{:016x}{:016x}",
            daemon.epoch,
            unix_time_millis() ^ u64::from(std::process::id())
        );
        let mut child = handoff::spawn_import(&target.executable, &handoff_path, &token)?;
        let pane_fds = duplicate_fds
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>();
        step = "send";
        let mut stream = match handoff::accept_and_send(
            handoff_listener,
            &mut child,
            &handoff_path,
            &token,
            &manifest,
            singleton_lock.as_raw_fd(),
            &pane_fds,
        ) {
            Ok(stream) => stream,
            Err(error) => {
                handoff::cleanup_child(&mut child);
                return Err(error);
            }
        };

        drop(listener.take());
        let _ = fs::remove_file(socket);
        step = "publish";
        handoff::request_publish(&mut stream)?;
        step = "ready";
        if let Err(error) = handoff::wait_ready(&mut stream) {
            handoff::cleanup_child(&mut child);
            return Err(error);
        }
        step = "ownership";
        for (_, runtime) in &live {
            match runtime.prepare_source_handoff() {
                Ok(process_group) => source_ownership.push((Arc::clone(runtime), process_group)),
                Err(error) => {
                    handoff::cleanup_child(&mut child);
                    return Err(io::Error::other(error.to_string()));
                }
            }
        }
        step = "commit";
        if let Err(error) = handoff::commit(&mut stream) {
            handoff::cleanup_child(&mut child);
            return Err(error);
        }
        step = "owned";
        handoff::wait_owned(&mut stream, &mut child)?;
        {
            let mut state = lock(&daemon.state);
            state.stopping = true;
            state.stop_reason = Some(StopReason::Handoff);
            daemon.changed.notify_all();
            daemon.plugin_changed.notify_all();
        }
        Ok(())
    })();

    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            for (runtime, process_group) in source_ownership {
                runtime.rollback_source_handoff(process_group);
            }
            rollback_handoff(daemon, socket, listener, &paused, step, &error);
            Err(error)
        }
    }
}

/// Back to serving after a failed handoff. The step and the error stay in the lifecycle
/// status as `last_handoff_error`, because the daemon's stderr goes nowhere
/// (docs/daemon-live-upgrade.md "A failed handoff").
// ^ docs/daemon-live-upgrade.md "A stuck handoff": nothing here may leave the handoff open.
// A daemon that kept `handoff_in_progress` after a failed rebind or marker write answered
// every request with `daemon_replacing` for ever while its sessions ran unseen. A socket it
// could not bind again is bound by the main loop (`rebind_listener`).
pub(crate) fn rollback_handoff(
    daemon: &Daemon,
    socket: &Path,
    listener: &mut Option<UnixListener>,
    paused: &[Arc<TerminalRuntime>],
    step: &str,
    error: &io::Error,
) {
    let mut failures = vec![error.to_string()];
    if let Err(rebind_error) = rebind_listener(socket, listener) {
        failures.push(format!("bind the socket again: {rebind_error}"));
    }
    for runtime in paused {
        runtime.resume_handoff_reader();
    }
    if let Err(marker_error) = write_lifecycle_marker(&daemon.lifecycle_path, "ready") {
        failures.push(format!("write the lifecycle marker: {marker_error}"));
    }
    let mut state = lock(&daemon.state);
    state.last_handoff_error = Some(HandoffError {
        unix_ms: unix_time_millis(),
        step: step.to_string(),
        error: failures.join("; rollback could not "),
    });
    if let Some(started) = state.handoff_started.take() {
        let paused_for = started.elapsed();
        for lease in state.leases.values_mut() {
            lease.expires_at += paused_for;
        }
    }
    state.handoff_in_progress = false;
    state.replacement_target = None;
    state.replacement_daemon_revision = None;
    state.replacement_protocol = None;
    state.replacement_executable = None;
    daemon.changed.notify_all();
}

/// Bind the canonical socket again when a handoff gave it up.
pub(crate) fn rebind_listener(
    socket: &Path,
    listener: &mut Option<UnixListener>,
) -> io::Result<()> {
    if listener.is_none() {
        prepare_socket(socket)?;
        let restored = bind_owner_only(socket)?;
        restored.set_nonblocking(true)?;
        *listener = Some(restored);
    }
    Ok(())
}

pub(crate) fn restore_handoff_runtimes(
    daemon: &Arc<Daemon>,
    panes: Vec<handoff::PaneHandoff>,
    exited_panes: Vec<handoff::PaneHandoff>,
    pane_fds: Vec<OwnedFd>,
) -> io::Result<()> {
    let persisted_ids = lock(&daemon.state)
        .persisted
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for pane in panes.iter().chain(&exited_panes) {
        if !persisted_ids.contains(&pane.state.pane_id) || !seen.insert(pane.state.pane_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff pane is absent from persisted state or duplicated",
            ));
        }
    }
    // ^ An exited pane comes back as its last screen, without a process
    // (docs/daemon-live-upgrade.md "Exited panes").
    let exited = exited_panes
        .into_iter()
        .map(|pane| {
            let pane_id = pane.state.pane_id;
            TerminalRuntime::exited_from_handoff(
                pane.state,
                runtime_notify(daemon, pane_id, pane.runtime_generation.clone()),
            )
            .map(|runtime| (pane_id, pane.runtime_generation, Arc::new(runtime)))
            .map_err(|error| {
                io::Error::other(format!("restore exited pane {}: {error}", pane_id.0))
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    {
        let mut state = lock(&daemon.state);
        for (pane_id, generation, runtime) in exited {
            if let Some(generation) = generation {
                state.runtime_generations.insert(pane_id, generation);
            }
            state.runtimes.insert(pane_id, runtime);
        }
    }
    if panes.is_empty() {
        return Ok(());
    }

    let worker_count = panes.len().min(HANDOFF_RESTORE_WORKERS);
    let mut buckets: Vec<Vec<(handoff::PaneHandoff, OwnedFd)>> =
        (0..worker_count).map(|_| Vec::new()).collect();
    for (index, pair) in panes.into_iter().zip(pane_fds).enumerate() {
        buckets[index % worker_count].push(pair);
    }

    let restored = thread::scope(|scope| -> io::Result<Vec<_>> {
        let handles = buckets
            .into_iter()
            .map(|bucket| {
                let daemon = Arc::clone(daemon);
                scope.spawn(move || -> io::Result<Vec<_>> {
                    let mut restored = Vec::with_capacity(bucket.len());
                    for (pane, fd) in bucket {
                        let pane_id = pane.state.pane_id;
                        let runtime = unsafe {
                            TerminalRuntime::from_handoff_fd(
                                fd.into_raw_fd(),
                                pane.state,
                                runtime_notify(&daemon, pane_id, pane.runtime_generation.clone()),
                            )
                        }
                        .map_err(|error| {
                            io::Error::other(format!("restore handoff pane {}: {error}", pane_id.0))
                        })?;
                        runtime.protect_handoff_import();
                        restored.push((pane_id, pane.runtime_generation, Arc::new(runtime)));
                    }
                    Ok(restored)
                })
            })
            .collect::<Vec<_>>();
        let mut restored = Vec::new();
        for handle in handles {
            let worker = handle
                .join()
                .map_err(|_| io::Error::other("handoff restore worker panicked"))??;
            restored.extend(worker);
        }
        Ok(restored)
    })?;

    let mut state = lock(&daemon.state);
    for (pane_id, generation, runtime) in restored {
        if let Some(generation) = generation {
            state.runtime_generations.insert(pane_id, generation);
            if state
                .persisted
                .panes
                .iter()
                .find(|pane| pane.id == pane_id)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| agent.attached && agent.presence_id.is_some())
            {
                state.agent_presence_leases.insert(pane_id, Instant::now());
            }
        }
        state.runtimes.insert(pane_id, runtime);
    }
    Ok(())
}

pub(crate) fn live_runtime_count(state: &State) -> usize {
    state
        .runtimes
        .values()
        .filter(|runtime| !runtime.exited())
        .count()
}

pub(crate) fn lifecycle_status(daemon: &Daemon, state: &mut State) -> DaemonLifecycle {
    expire_tui_presence(state, Instant::now());
    let target = state.replacement_target.clone();
    let blockers = target
        .as_deref()
        .map(|target| {
            if state.replacement_executable.is_some() {
                handoff_blockers(state, target)
            } else {
                replacement_blockers(state, target)
            }
        })
        .unwrap_or_default();
    let target_version = target
        .as_deref()
        .and_then(binary_identity_version)
        .unwrap_or_default()
        .to_string();
    DaemonLifecycle {
        protocol: PROTOCOL_VERSION,
        epoch: daemon.epoch,
        binary_id: daemon.binary_id.clone(),
        version: WSX_VERSION.to_string(),
        daemon_revision: DAEMON_REVISION,
        started_unix_ms: daemon.started_unix_ms,
        phase: if state.stopping {
            DaemonPhase::Stopping
        } else if target.is_some() {
            DaemonPhase::ReplacementPending
        } else {
            DaemonPhase::Ready
        },
        live_runtimes: live_runtime_count(state),
        active_clients: daemon.active_clients.load(Ordering::Acquire),
        active_tuis: state.tui_clients.len() + usize::from(state.legacy_tui_until.is_some()),
        recovered_from_backup: daemon.recovered_from_backup,
        replacement_target: target,
        replacement_target_version: target_version,
        replacement_blockers: blockers,
        last_handoff_error: state.last_handoff_error.clone(),
    }
}

pub(crate) fn replacement_blockers(
    state: &mut State,
    target_binary_id: &str,
) -> Vec<ReplacementBlocker> {
    let now = Instant::now();
    expire_tui_presence(state, now);
    let mut blockers = Vec::new();
    let target_daemon_revision = state.replacement_daemon_revision.unwrap_or_default();
    if state.legacy_tui_until.is_some()
        || state.tui_clients.values().any(|client| {
            if target_daemon_revision == 0 {
                client.target_binary_id != target_binary_id
            } else {
                client.target_daemon_revision != target_daemon_revision
            }
        })
    {
        blockers.push(ReplacementBlocker::OtherTui);
    }
    if has_fresh_working_agent(state, now) {
        blockers.push(ReplacementBlocker::WorkingAgent);
    }
    if state.recovery_in_progress {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    if live_runtime_count(state) > 0 && !state.listener_scan_complete {
        blockers.push(ReplacementBlocker::ListenerScanPending);
    }
    if !state.foreground_jobs.is_empty() {
        blockers.push(ReplacementBlocker::ForegroundJob);
    }
    if state
        .listening_ports
        .values()
        .any(|ports| !ports.is_empty())
    {
        blockers.push(ReplacementBlocker::ListeningPort);
    }
    blockers
}

pub(crate) fn replacement_response(
    state: &State,
    disposition: ReplacementDisposition,
    target_version: String,
    blockers: Vec<ReplacementBlocker>,
    use_current_daemon: bool,
) -> Response {
    Response::Replacement {
        disposition,
        live_runtimes: live_runtime_count(state),
        daemon_version: WSX_VERSION.to_string(),
        target_version,
        blockers,
        use_current_daemon,
    }
}

#[cfg(test)]
pub(crate) fn prepare_replacement(
    daemon: &Daemon,
    target_binary_id: String,
) -> Result<Response, ApiError> {
    prepare_replacement_with_revision(daemon, target_binary_id, 0)
}

pub(crate) fn prepare_replacement_with_revision(
    daemon: &Daemon,
    target_binary_id: String,
    target_daemon_revision: u32,
) -> Result<Response, ApiError> {
    if target_binary_id.is_empty() || target_binary_id.len() > 512 {
        return Err(api(
            "invalid_request",
            "replacement binary identity is invalid",
        ));
    }
    let target_version = binary_identity_version(&target_binary_id)
        .ok_or_else(|| {
            api(
                "invalid_request",
                "replacement binary identity has no valid wsx version",
            )
        })?
        .to_string();
    let candidate_order = compare_binary_identities(&target_binary_id, &daemon.binary_id)
        .ok_or_else(|| {
            api(
                "invalid_request",
                "replacement binary identity is malformed",
            )
        })?;
    let mut state = lock(&daemon.state);
    if (target_daemon_revision > 0 && target_daemon_revision <= DAEMON_REVISION)
        || (target_daemon_revision == 0 && candidate_order != std::cmp::Ordering::Greater)
    {
        return Ok(replacement_response(
            &state,
            ReplacementDisposition::Deferred,
            target_version,
            Vec::new(),
            true,
        ));
    }

    if let Some(current) = state.replacement_target.as_deref() {
        let order = match (state.replacement_daemon_revision, target_daemon_revision) {
            (Some(current), target) if current > 0 && target > 0 => target.cmp(&current),
            _ => compare_binary_identities(&target_binary_id, current)
                .ok_or_else(|| api("invalid_request", "pending binary identity is malformed"))?,
        };
        if order != std::cmp::Ordering::Greater {
            return Ok(replacement_response(
                &state,
                ReplacementDisposition::Deferred,
                target_version,
                vec![ReplacementBlocker::PendingTarget],
                false,
            ));
        }
    }
    state.replacement_target = Some(target_binary_id.clone());
    state.replacement_daemon_revision = Some(target_daemon_revision);
    let blockers = replacement_blockers(&mut state, &target_binary_id);
    let disposition = if blockers.is_empty() {
        state.stopping = true;
        state.stop_reason = Some(StopReason::Replacement);
        daemon.changed.notify_all();
        daemon.plugin_changed.notify_all();
        ReplacementDisposition::Stopping
    } else {
        ReplacementDisposition::Deferred
    };
    Ok(replacement_response(
        &state,
        disposition,
        target_version,
        blockers,
        false,
    ))
}

pub(crate) fn prepare_handoff(
    daemon: &Daemon,
    target_binary_id: String,
    target_version: String,
    target_protocol: u32,
    target_daemon_revision: u32,
    executable: PathBuf,
    restart: bool,
) -> Result<Response, ApiError> {
    if target_protocol == 0 || target_daemon_revision < DAEMON_REVISION {
        return Err(api("invalid_request", "handoff target is not newer"));
    }
    if binary_identity_version(&target_binary_id) != Some(target_version.as_str()) {
        return Err(api(
            "invalid_request",
            "handoff target version does not match its identity",
        ));
    }
    // ^ docs/daemon-live-upgrade.md: only an explicit restart may hand off into this
    // daemon's own version, including the same file. It never selects an older version.
    let newer = compare_binary_identities(&target_binary_id, &daemon.binary_id)
        == Some(std::cmp::Ordering::Greater);
    let same_version_restart = restart && target_version == WSX_VERSION;
    if !newer && !same_version_restart {
        return Err(api("invalid_request", "handoff target is not newer"));
    }
    let executable = validate_handoff_executable(&executable, &target_binary_id)
        .map_err(|error| api("invalid_handoff_target", error.to_string()))?;
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api(
            "handoff_in_progress",
            "wsxd handoff is already in progress",
        ));
    }
    if let Some(current_revision) = state.replacement_daemon_revision {
        if current_revision > target_daemon_revision {
            return Ok(replacement_response(
                &state,
                ReplacementDisposition::Deferred,
                target_version,
                vec![ReplacementBlocker::PendingTarget],
                false,
            ));
        }
    }
    state.replacement_target = Some(target_binary_id.clone());
    state.replacement_daemon_revision = Some(target_daemon_revision);
    state.replacement_protocol = Some(target_protocol);
    state.replacement_executable = Some(executable);
    let blockers = handoff_blockers(&mut state, &target_binary_id);
    daemon.changed.notify_all();
    Ok(replacement_response(
        &state,
        if blockers.is_empty() {
            ReplacementDisposition::Stopping
        } else {
            ReplacementDisposition::Deferred
        },
        target_version,
        blockers,
        false,
    ))
}

pub(crate) fn validate_handoff_executable(
    path: &Path,
    expected_identity: &str,
) -> io::Result<PathBuf> {
    let canonical = path.canonicalize()?;
    let metadata = canonical.metadata()?;
    let owner = metadata.uid();
    if !metadata.is_file()
        || (owner != unsafe { libc::geteuid() } && owner != 0)
        || metadata.mode() & 0o022 != 0
        || !binary_identity_matches_file(&canonical, expected_identity)?
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsxd handoff executable",
        ));
    }
    Ok(canonical)
}

pub(crate) fn handoff_blockers(
    state: &mut State,
    _target_binary_id: &str,
) -> Vec<ReplacementBlocker> {
    expire_tui_presence(state, Instant::now());
    // ^ docs/daemon-live-upgrade.md: a TUI that speaks the target protocol reconnects
    // to the successor on its own, and a TUI that follows re-executes into the installed
    // wsx. Only a TUI that can do neither must leave first.
    let target_protocol = state.replacement_protocol.unwrap_or(u32::MAX);
    let another_tui = state.legacy_tui_until.is_some()
        || state
            .tui_clients
            .values()
            .any(|client| client.max_protocol < target_protocol && !client.follows);
    let mut blockers = Vec::new();
    if another_tui {
        blockers.push(ReplacementBlocker::OtherTui);
    }
    if state.recovery_in_progress {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    if state.runtimes.len() > handoff::MAX_HANDOFF_PANES {
        blockers.push(ReplacementBlocker::HandoffUnavailable);
    }
    blockers
}
