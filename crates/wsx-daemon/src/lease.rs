//! Terminal leases, stream hand-over records and the per-pane operation lock.

use super::*;

pub(crate) fn record_transferred_stream(state: &mut State, stream: StreamLease) {
    while state.transferred_streams.len() >= MAX_CLIENTS {
        state.transferred_streams.pop_front();
    }
    state.transferred_streams.push_back(stream);
}

/// An exchange types into the pane, so a live terminal controller blocks it. With
/// `takeover`, a person's own action, it takes the pane instead: the controller's stream
/// ends with `lease_transferred`, as after `acquire_terminal_lease` with a takeover. The
/// caller holds the pane operation lock, so no controller can come back before the write.
pub(crate) fn take_terminal_lease(
    state: &mut State,
    pane_id: PaneId,
    takeover: bool,
) -> Result<(), ApiError> {
    let Some(lease) = state
        .leases
        .get(&pane_id)
        .copied()
        .filter(|lease| lease.expires_at > Instant::now())
    else {
        return Ok(());
    };
    if !takeover {
        return Err(api("terminal_busy", "pane has another writable controller"));
    }
    state.leases.remove(&pane_id);
    record_transferred_stream(
        state,
        StreamLease {
            pane_id,
            client_id: lease.client_id,
            generation: lease.generation,
        },
    );
    Ok(())
}

pub(crate) fn terminal_stream_lease_error(
    state: &State,
    stream: StreamLease,
    now: Instant,
) -> Option<ApiError> {
    if state.handoff_in_progress || state.stop_reason == Some(StopReason::Handoff) {
        return Some(ApiError::new(
            "daemon_handoff",
            "wsxd is handing this terminal to a newer daemon",
        ));
    }
    if state.stopping {
        return Some(ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        ));
    }
    if state.leases.get(&stream.pane_id).is_some_and(|current| {
        current.client_id == stream.client_id
            && current.generation == stream.generation
            && current.expires_at > now
    }) {
        return None;
    }
    if state.transferred_streams.contains(&stream) {
        Some(ApiError::new(
            "lease_transferred",
            "terminal control moved to another wsx instance",
        ))
    } else {
        Some(ApiError::new(
            "lease_lost",
            "terminal stream no longer owns the pane lease",
        ))
    }
}

pub(crate) fn release_stream_lease(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    lease_generation: u64,
) {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    let stream = StreamLease {
        pane_id,
        client_id,
        generation: lease_generation,
    };
    state
        .transferred_streams
        .retain(|candidate| *candidate != stream);
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.client_id == client_id && lease.generation == lease_generation)
    {
        state.leases.remove(&pane_id);
        let runtime = (!state.stopping && !state.handoff_in_progress)
            .then(|| state.runtimes.get(&pane_id).cloned())
            .flatten();
        drop(state);
        if let Some(runtime) = runtime {
            let _ = runtime.clear_selection();
        }
    }
}

pub(crate) fn release_client_lease(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
) -> Result<u64, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    require_lease(&state, pane_id, LeaseAccess::Client(client_id))?;
    state.leases.remove(&pane_id);
    let revision = state.revision;
    let runtime = state.runtimes.get(&pane_id).cloned();
    drop(state);
    if let Some(runtime) = runtime {
        runtime.clear_selection().map_err(terminal_api)?;
    }
    Ok(revision)
}

pub(crate) fn acquire_terminal_lease(
    daemon: &Daemon,
    pane_id: PaneId,
    client_id: u64,
    takeover: bool,
) -> Result<(u64, u64), ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let state = lock(&daemon.state);
    if state.stopping || state.handoff_in_progress {
        return Err(api(
            "daemon_replacing",
            "wsxd is replacing its runtime owner",
        ));
    }
    let runtime = Arc::clone(require_runtime(&state, pane_id)?);
    if state
        .leases
        .get(&pane_id)
        .is_some_and(|lease| lease.expires_at > Instant::now() && lease.client_id != client_id)
        && !takeover
    {
        return Err(api("terminal_busy", "pane has another writable controller"));
    }
    drop(state);

    // ^ Selection cleanup can notify through the runtime callback, which reacquires daemon state.
    runtime.clear_selection().map_err(terminal_api)?;

    let mut state = lock(&daemon.state);
    require_runtime(&state, pane_id)?;
    let now = Instant::now();
    let displaced = state.leases.get(&pane_id).and_then(|lease| {
        (lease.expires_at > now && lease.client_id != client_id).then_some(StreamLease {
            pane_id,
            client_id: lease.client_id,
            generation: lease.generation,
        })
    });
    if let Some(displaced) = displaced {
        record_transferred_stream(&mut state, displaced);
    }
    let generation = NEXT_LEASE_GENERATION.fetch_add(1, Ordering::Relaxed);
    state.leases.insert(
        pane_id,
        Lease {
            client_id,
            generation,
            expires_at: now + LEASE_TTL,
        },
    );
    Ok((state.revision, generation))
}

pub(crate) fn resize_terminal(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let (runtime, _) = leased_runtime(daemon, pane_id, access)?;
    runtime.resize(rows, cols).map_err(terminal_api)?;
    let mut state = lock(&daemon.state);
    if let Some(recipe) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.recovery.as_mut())
    {
        recipe.rows = rows;
        recipe.cols = cols;
        if let Err(error) = save_state(&daemon.state_path, &state.persisted) {
            state.persistence_dirty = true;
            return Err(api(
                "durability_degraded",
                format!(
                    "terminal resized but its recovery dimensions are not yet durable: {error}"
                ),
            ));
        }
    }
    Ok(Response::Ack {
        revision: state.revision,
    })
}

/// The pane's operation lock. A pane id with no runtime and no persisted record gets a lock that nothing keeps:
/// the caller's own check then answers with its usual error, and a client that sends made-up
/// pane ids cannot grow `terminal_operation_locks`.
pub(crate) fn terminal_operation_lock(daemon: &Daemon, pane_id: PaneId) -> Arc<Mutex<()>> {
    let mut state = lock(&daemon.state);
    let known = state.runtimes.contains_key(&pane_id)
        || state.persisted.panes.iter().any(|pane| pane.id == pane_id);
    if !known {
        return Arc::new(Mutex::new(()));
    }
    Arc::clone(
        state
            .terminal_operation_locks
            .entry(pane_id)
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

pub(crate) fn leased_runtime(
    daemon: &Daemon,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(Arc<TerminalRuntime>, u64), ApiError> {
    let mut state = lock(&daemon.state);
    if state.stopping || state.handoff_in_progress {
        return Err(api(
            "daemon_replacing",
            "wsxd is replacing its runtime owner",
        ));
    }
    refresh_lease(&mut state, pane_id, access)?;
    Ok((
        Arc::clone(require_runtime(&state, pane_id)?),
        state.revision,
    ))
}

pub(crate) fn require_runtime(
    state: &State,
    pane_id: PaneId,
) -> Result<&Arc<TerminalRuntime>, ApiError> {
    state
        .runtimes
        .get(&pane_id)
        .ok_or_else(|| api("terminal_unavailable", "terminal process is not running"))
}

pub(crate) fn require_lease(
    state: &State,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(), ApiError> {
    let owned = state.leases.get(&pane_id).is_some_and(|lease| {
        let identity_matches = match access {
            LeaseAccess::Client(client_id) => lease.client_id == client_id,
            LeaseAccess::Stream {
                client_id,
                generation,
            } => lease.client_id == client_id && lease.generation == generation,
        };
        identity_matches && lease.expires_at > Instant::now()
    });
    if owned {
        Ok(())
    } else {
        Err(api(
            "lease_required",
            "client does not own an active terminal lease",
        ))
    }
}

pub(crate) fn refresh_lease(
    state: &mut State,
    pane_id: PaneId,
    access: LeaseAccess,
) -> Result<(), ApiError> {
    require_lease(state, pane_id, access)?;
    if let Some(lease) = state.leases.get_mut(&pane_id) {
        lease.expires_at = Instant::now() + LEASE_TTL;
    }
    Ok(())
}
