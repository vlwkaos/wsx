//! Agent lifecycle reports, presence and wake leases, and Claude terminal reconciliation.

use super::*;

pub(crate) fn key_interrupts_agent(key: &KeyEvent) -> bool {
    key.code == KeyCode::Escape
        || (key.code == KeyCode::Text && key.control && key.text.eq_ignore_ascii_case("c"))
}

pub(crate) fn request_claude_reconcile(daemon: &Daemon, pane_id: PaneId, interrupted: bool) {
    if !interrupted {
        return;
    }
    let now = Instant::now();
    let mut state = lock(&daemon.state);
    let Some(agent) = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| {
            agent.attached && agent.provider == "claude" && agent.capabilities.escape_interrupts
        })
    else {
        return;
    };
    let event_state = agent.state;
    let interrupt_revision = state
        .runtimes
        .get(&pane_id)
        .map(|runtime| runtime.revision());
    state
        .claude_reconciliations
        .entry(pane_id)
        .and_modify(|reconciliation| {
            reconciliation.terminal_revision = None;
            reconciliation.interrupt_requested = true;
            reconciliation.interrupt_revision = interrupt_revision;
        })
        .or_insert(ClaudeReconciliation {
            event_state,
            event_at: now
                .checked_sub(claude_status::WORKING_EVENT_LEAD)
                .unwrap_or(now),
            terminal_revision: None,
            terminal: None,
            interrupt_requested: true,
            interrupt_revision,
        });
}

/// How long a compaction that began outside a turn may go without its end. Claude's took 37 to
/// 55 s (real transcripts, 2026-10); a compaction that fails sends no PostCompact.
const COMPACTION_LIMIT: Duration = Duration::from_secs(10 * 60);

/// A compaction outside a turn whose end never came returns its agent to the state before it,
/// so a message does not wait for it for ever. Returns whether an agent changed.
pub(crate) fn end_stale_compactions(daemon: &Daemon, state: &mut State) -> Result<bool, ApiError> {
    let now = Instant::now();
    let stale: Vec<(PaneId, AgentState)> = state
        .compactions
        .iter()
        .filter(|(_, (_, since))| now.saturating_duration_since(*since) >= COMPACTION_LIMIT)
        .map(|(pane_id, (earlier, _))| (*pane_id, *earlier))
        .collect();
    if stale.is_empty() {
        return Ok(false);
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let mut changed = false;
    for (pane_id, earlier) in &stale {
        let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == *pane_id) else {
            continue;
        };
        let Some(agent) = pane.agent.as_mut().filter(|agent| agent.compacting) else {
            continue;
        };
        changed = true;
        agent.compacting = false;
        set_agent_state(agent, *earlier);
        pane.revision = revision;
        observe_agent_exchange(
            &mut persisted,
            *pane_id,
            *earlier,
            true,
            revision,
            unix_time_millis(),
        );
    }
    if changed {
        commit_publish(
            daemon,
            state,
            persisted,
            revision,
            "agent.compaction_ended",
            0,
        )?;
    }
    for (pane_id, earlier) in stale {
        state.compactions.remove(&pane_id);
        // ^ The screen reconciler must not take the compaction's Working back.
        if let Some(reconciliation) = state.claude_reconciliations.get_mut(&pane_id) {
            reconciliation.event_state = earlier;
        }
    }
    Ok(changed)
}

// ^ [[Agent Lifecycle Authority]] wsx-core normalizes provider events and bounded
// companion evidence; this boundary owns reconciliation, generation checks,
// persistence, wake authority, and resume identity.
pub(crate) struct AgentReportInput {
    pub(crate) provider: String,
    pub(crate) state: AgentState,
    pub(crate) attached: bool,
    pub(crate) presence_id: Option<String>,
    pub(crate) conversation_id: Option<String>,
    pub(crate) session_ref: Option<AgentSessionRef>,
    pub(crate) wake_token: Option<String>,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) received: Option<PromptReceived>,
    pub(crate) compacting: bool,
    pub(crate) compacted: bool,
}

#[cfg(test)]
pub(crate) fn agent_report(
    daemon: &Arc<Daemon>,
    runtime: RuntimeAgentAuthority,
    provider: String,
    state: AgentState,
    conversation_id: Option<String>,
    session_ref: Option<AgentSessionRef>,
    capabilities: AgentCapabilities,
) -> Result<Response, ApiError> {
    agent_report_with_attachment(
        daemon,
        runtime,
        AgentReportInput {
            provider,
            state,
            attached: true,
            presence_id: None,
            conversation_id,
            session_ref,
            wake_token: None,
            capabilities,
            received: None,
            compacting: false,
            compacted: false,
        },
    )
}

pub(crate) fn agent_report_with_attachment(
    daemon: &Arc<Daemon>,
    runtime: RuntimeAgentAuthority,
    report: AgentReportInput,
) -> Result<Response, ApiError> {
    let AgentReportInput {
        provider,
        state: agent_state,
        attached,
        presence_id,
        conversation_id,
        session_ref,
        wake_token,
        mut capabilities,
        received,
        compacting,
        compacted,
    } = report;
    let RuntimeAgentAuthority {
        pane_id,
        generation: runtime_generation,
    } = runtime;
    let provider = bounded_provider(provider)?;
    let presence_id = presence_id.map(validate_presence_id).transpose()?;
    if !attached && presence_id.is_some() {
        return Err(api(
            "invalid_presence",
            "detached reports cannot claim presence",
        ));
    }
    let wake_token = match wake_token {
        Some(wake_token)
            if attached && agent_state == AgentState::Working && provider == "claude" =>
        {
            Some(bounded_wake_token(wake_token)?)
        }
        Some(_) => {
            return Err(api(
                "invalid_wake_token",
                "wake token requires an attached Claude Working report",
            ))
        }
        None => None,
    };
    let conversation_id = conversation_id.map(|value| truncate_utf8(value, 512));
    let explicit_session_ref = validated_session_ref(session_ref)?;
    if explicit_session_ref.as_ref().is_some_and(|session_ref| {
        resume::plan(&provider, session_ref).is_none()
            || session_ref.launch.as_ref().is_some_and(|launch| {
                let variables = integration::root_variables(&provider);
                !launch
                    .environment
                    .keys()
                    .all(|name| variables.contains(&name.as_str()))
            })
    }) {
        return Err(api(
            "invalid_agent_session",
            "provider does not support this agent session reference",
        ));
    }
    let mut session_ref = explicit_session_ref.or_else(|| {
        conversation_id
            .as_ref()
            .and_then(|value| AgentSessionRef::id(value.clone()))
            .filter(|session_ref| resume::plan(&provider, session_ref).is_some())
    });
    let mut conversation_id = conversation_id;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, runtime_generation.as_deref())?;
    let mut persisted = state.persisted.clone();

    // ^ [[Session Model]] Agent activity belongs to the project reached through
    // pane/session/worktree ownership. Resolve ancestry before changing persisted state.
    let pane_index = persisted
        .panes
        .iter()
        .position(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let project_index = project_index_for_pane(&persisted, pane_id)?;

    let previous_agent = persisted.panes[pane_index].agent.clone();
    // ^ docs/agent-reporting.md "Claude compaction": every compaction works from its PreCompact
    // report. One that began outside a turn (an idle one, or the person's /compact) returns to
    // the state before it on its PostCompact (`compacted`) report; one inside a turn leaves the
    // turn going on. Any other report ends what is kept here.
    let before = previous_agent
        .as_ref()
        .filter(|previous| previous.attached && previous.provider == provider);
    let outside_turn = state
        .compactions
        .get(&pane_id)
        .copied()
        .filter(|_| before.is_some_and(|previous| previous.compacting));
    let compacting = attached && compacting && !compacted;
    let mut kept_compaction = None;
    let agent_state = if !attached {
        agent_state
    } else if compacted {
        match (outside_turn, before) {
            (Some((earlier, _)), _) => earlier,
            (None, Some(previous)) => previous.state,
            (None, None) => agent_state,
        }
    } else if compacting {
        kept_compaction = outside_turn.or_else(|| {
            let earlier = before.map_or(AgentState::Idle, |previous| previous.state);
            (earlier != AgentState::Working).then(|| (earlier, Instant::now()))
        });
        AgentState::Working
    } else {
        agent_state
    };
    let agent_id = match previous_agent.as_ref().map(|agent| agent.id) {
        Some(id) => id,
        None => AgentInstanceId(next_id(&mut persisted)?),
    };
    // ^ Preserve native metadata only for the same attached provider/session.
    if attached {
        if let (Some(current), Some(previous)) = (
            &mut session_ref,
            previous_agent
                .as_ref()
                .filter(|previous| previous.attached && previous.provider == provider),
        ) {
            if let Some(old) = &previous.session_ref {
                if current.kind == old.kind && current.value == old.value {
                    if current.transcript_path.is_none() {
                        current.transcript_path = old.transcript_path.clone();
                    }
                    // ^ The session's first report knows where it started; a later hook
                    // may run after the agent changed directory.
                    if old.launch.is_some() {
                        current.launch = old.launch.clone();
                    }
                }
            }
        }
    }
    if !attached {
        if let Some(previous) = previous_agent
            .as_ref()
            .filter(|agent| agent.provider == provider)
        {
            conversation_id = conversation_id.or_else(|| previous.conversation_id.clone());
            session_ref = session_ref.or_else(|| previous.session_ref.clone());
            capabilities = previous.capabilities.clone();
        }
    }
    capabilities.resume |= session_ref.is_some();
    let now = Instant::now();
    let is_claude = provider == "claude";
    let claude_reconciliation = (is_claude && attached).then(|| {
        let mut reconciliation = state
            .claude_reconciliations
            .get(&pane_id)
            .copied()
            .unwrap_or(ClaudeReconciliation {
                event_state: agent_state,
                event_at: now,
                terminal_revision: None,
                terminal: None,
                interrupt_requested: false,
                interrupt_revision: None,
            });
        reconciliation.event_state = agent_state;
        reconciliation.interrupt_requested = false;
        reconciliation.interrupt_revision = None;
        reconciliation.event_at = now;
        reconciliation.terminal_revision = None;
        reconciliation.terminal = None;
        reconciliation
    });
    let reported_state = if attached {
        claude_reconciliation.map_or(agent_state, |reconciliation| reconciliation.effective(now))
    } else {
        AgentState::Unknown
    };
    // A report of the state the agent is already in keeps when that state began.
    let state_since_unix_ms = previous_agent
        .as_ref()
        .filter(|previous| previous.id == agent_id && previous.state == reported_state)
        .and_then(|previous| previous.state_since_unix_ms)
        .unwrap_or_else(unix_time_millis);
    let revision = state.revision.saturating_add(1);
    persisted.panes[pane_index].agent = Some(AgentInfo {
        id: agent_id,
        provider,
        state: reported_state,
        state_since_unix_ms: Some(state_since_unix_ms),
        attached,
        presence_id: presence_id.clone(),
        conversation_id,
        session_ref,
        capabilities,
        source: "adapter".into(),
        compacting,
    });
    persisted.panes[pane_index].revision = revision;
    // ^ A compaction outside a turn is Claude's own upkeep, not work on the project.
    if attached && agent_state == AgentState::Working && kept_compaction.is_none() {
        persisted.projects[project_index].last_agent_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
    }
    receive_prompt(
        &mut persisted,
        pane_id,
        received,
        revision,
        unix_time_millis(),
    );
    observe_agent_exchange(
        &mut persisted,
        pane_id,
        reported_state,
        attached,
        revision,
        unix_time_millis(),
    );
    commit_persisted(daemon, &mut state, persisted)?;
    match kept_compaction {
        Some(kept) => state.compactions.insert(pane_id, kept),
        None => state.compactions.remove(&pane_id),
    };
    if presence_id.is_some() {
        state.agent_presence_leases.insert(pane_id, now);
    } else {
        state.agent_presence_leases.remove(&pane_id);
    }
    if let Some(reconciliation) = claude_reconciliation {
        state.claude_reconciliations.insert(pane_id, reconciliation);
    } else {
        state.claude_reconciliations.remove(&pane_id);
    }
    if attached && agent_state == AgentState::Working && reported_state == AgentState::Working {
        state.agent_wake_leases.insert(pane_id, now);
        if let Some(wake_token) = wake_token {
            state.agent_wake_tokens.insert(pane_id, wake_token);
        } else {
            state.agent_wake_tokens.remove(&pane_id);
        }
    } else {
        clear_agent_wake(&mut state, pane_id);
    }
    let revision = bump(daemon, &mut state, "agent.reported", pane_id.0);
    Ok(Response::Ack { revision })
}

pub(crate) fn clear_agent_wake(state: &mut State, pane_id: PaneId) {
    state.agent_wake_leases.remove(&pane_id);
    state.agent_wake_tokens.remove(&pane_id);
}

// ^ crates/wsx-core/src/runtime/protocol.rs owns the opt-in identity; the
// resident assets in crates/wsx-core/integrations renew without disk writes.
pub(crate) fn renew_agent_presence(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: String,
    presence_id: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let presence_id = validate_presence_id(presence_id)?;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let matches = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id && !pane.exited)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| agent.attached && agent.presence_id.as_deref() == Some(&presence_id));
    if !matches {
        return Err(api(
            "stale_presence",
            "agent attachment no longer owns the pane",
        ));
    }
    let renewed_at = state
        .agent_presence_leases
        .get_mut(&pane_id)
        .ok_or_else(|| api("stale_presence", "agent presence lease is no longer active"))?;
    if Instant::now().saturating_duration_since(*renewed_at) >= AGENT_PRESENCE_TTL {
        return Err(api("stale_presence", "agent presence lease expired"));
    }
    *renewed_at = Instant::now();
    Ok(Response::Ack {
        revision: state.revision,
    })
}

// ^ [[Wake Mode and Report Lease]] Renewal is ephemeral and cannot mutate agent state.
pub(crate) fn renew_agent_wake(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: String,
    wake_token: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let wake_token = bounded_wake_token(wake_token)?;
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let active = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.attached && agent.provider == "claude" && agent.state == AgentState::Working
        });
    if !active {
        return Err(api(
            "wake_not_active",
            "pane does not have an attached working Claude agent",
        ));
    }
    match state.agent_wake_tokens.get(&pane_id) {
        Some(active_token) if active_token != &wake_token => {
            return Err(api(
                "stale_wake_heartbeat",
                "wake heartbeat does not belong to the active Claude prompt",
            ));
        }
        Some(_) => {}
        None => {
            state.agent_wake_tokens.insert(pane_id, wake_token);
        }
    }
    state.agent_wake_leases.insert(pane_id, Instant::now());
    Ok(Response::Ack {
        revision: state.revision,
    })
}

pub(crate) fn agent_clear(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    runtime_generation: String,
    next_runtime_generation: String,
) -> Result<Response, ApiError> {
    let runtime_generation = bounded_runtime_generation(runtime_generation)?;
    let next_runtime_generation = bounded_runtime_generation(next_runtime_generation)?;
    if runtime_generation == next_runtime_generation {
        return Err(api(
            "invalid_runtime_generation",
            "replacement runtime generation must be distinct",
        ));
    }
    let mut state = lock(&daemon.state);
    expect_runtime_generation(&state, pane_id, Some(&runtime_generation))?;
    let mut persisted = state.persisted.clone();
    let pane = persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let revision = state.revision.saturating_add(1);
    pane.agent = None;
    pane.revision = revision;
    terminate_agent_exchanges(
        &mut persisted,
        &[pane_id],
        AgentExchangeState::TargetReplaced,
        revision,
    );
    commit_persisted(daemon, &mut state, persisted)?;
    clear_agent_wake(&mut state, pane_id);
    state.agent_presence_leases.remove(&pane_id);
    state.claude_reconciliations.remove(&pane_id);
    state
        .runtime_generations
        .insert(pane_id, next_runtime_generation);
    let revision = bump(daemon, &mut state, "agent.cleared", pane_id.0);
    Ok(Response::Ack { revision })
}

pub(crate) fn expect_runtime_generation(
    state: &State,
    pane_id: PaneId,
    provided: Option<&str>,
) -> Result<(), ApiError> {
    let expected = state.runtime_generations.get(&pane_id).ok_or_else(|| {
        api(
            "terminal_unavailable",
            "pane has no live runtime generation",
        )
    })?;
    if provided == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(api(
            "stale_runtime",
            "agent mutation does not belong to the live pane runtime",
        ))
    }
}

/// Ends an agent's attachment: its runtime is gone or has not yet confirmed itself.
pub(crate) fn detach_agent(agent: &mut AgentInfo) {
    agent.attached = false;
    set_agent_state(agent, AgentState::Unknown);
    agent.presence_id = None;
}

/// Sets an agent's state; a changed state records when it began.
pub(crate) fn set_agent_state(agent: &mut AgentInfo, state: AgentState) {
    if agent.state != state || agent.state_since_unix_ms.is_none() {
        agent.state = state;
        agent.state_since_unix_ms = Some(unix_time_millis());
    }
}

pub(crate) fn apply_claude_terminal_observation(
    daemon: &Daemon,
    pane_id: PaneId,
    runtime_generation: &str,
    observation: AgentTerminalObservation,
    now: Instant,
) -> Result<(), ApiError> {
    let terminal = claude_status::classify(&observation.title, &observation.text);
    let mut state = lock(&daemon.state);
    if state.runtime_generations.get(&pane_id).map(String::as_str) != Some(runtime_generation) {
        return Ok(());
    }
    let Some(pane_index) = state
        .persisted
        .panes
        .iter()
        .position(|pane| pane.id == pane_id)
    else {
        return Ok(());
    };
    let Some(agent) = state.persisted.panes[pane_index]
        .agent
        .as_ref()
        .filter(|agent| agent.attached && agent.provider == "claude")
    else {
        state.claude_reconciliations.remove(&pane_id);
        return Ok(());
    };
    let event_at = now
        .checked_sub(claude_status::WORKING_EVENT_LEAD)
        .unwrap_or(now);
    let mut reconciliation = state
        .claude_reconciliations
        .get(&pane_id)
        .copied()
        .unwrap_or(ClaudeReconciliation {
            event_state: agent.state,
            event_at,
            terminal_revision: None,
            terminal: None,
            interrupt_requested: false,
            interrupt_revision: None,
        });
    // An old prompt cannot confirm that Ctrl+C settled the current turn.
    let terminal = if terminal == Some(claude_status::TerminalEvidence::Idle)
        && reconciliation
            .interrupt_revision
            .is_some_and(|revision| observation.revision <= revision)
    {
        None
    } else {
        terminal
    };
    reconciliation.terminal_revision = Some(observation.revision);
    reconciliation.terminal = terminal;
    if terminal.is_none() {
        state.claude_reconciliations.insert(pane_id, reconciliation);
        return Ok(());
    }
    let effective = reconciliation.effective(now);
    if effective == agent.state {
        state.claude_reconciliations.insert(pane_id, reconciliation);
        return Ok(());
    }

    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    set_agent_state(
        persisted.panes[pane_index]
            .agent
            .as_mut()
            .expect("validated Claude agent must remain present"),
        effective,
    );
    persisted.panes[pane_index].revision = revision;
    if effective == AgentState::Working {
        let project_index = project_index_for_pane(&persisted, pane_id)?;
        persisted.projects[project_index].last_agent_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
    }
    // A state read from the screen ends a cancelled exchange like a reported one does.
    observe_agent_exchange(
        &mut persisted,
        pane_id,
        effective,
        true,
        revision,
        unix_time_millis(),
    );
    commit_persisted(daemon, &mut state, persisted)?;
    state.claude_reconciliations.insert(pane_id, reconciliation);
    if effective != AgentState::Working {
        clear_agent_wake(&mut state, pane_id);
    }
    bump(daemon, &mut state, "agent.reconciled", pane_id.0);
    Ok(())
}

pub(crate) fn spawn_claude_reconciler(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || loop {
        let now = Instant::now();
        let targets = {
            let state = lock(&daemon.state);
            if state.stopping {
                return;
            }
            state
                .persisted
                .panes
                .iter()
                .filter_map(|pane| {
                    let agent = pane
                        .agent
                        .as_ref()
                        .filter(|agent| agent.attached && agent.provider == "claude")?;
                    let runtime = state.runtimes.get(&pane.id)?.clone();
                    let generation = state.runtime_generations.get(&pane.id)?.clone();
                    let runtime_revision = runtime.revision();
                    let reconciliation = state.claude_reconciliations.get(&pane.id).copied();
                    let grace_due = reconciliation.is_some_and(|reconciliation| {
                        reconciliation.event_state == AgentState::Working
                            && reconciliation.terminal
                                == Some(claude_status::TerminalEvidence::Idle)
                            && now.saturating_duration_since(reconciliation.event_at)
                                >= claude_status::WORKING_EVENT_LEAD
                            && reconciliation.effective(now) != agent.state
                    });
                    (reconciliation.is_none()
                        || reconciliation.and_then(|value| value.terminal_revision)
                            != Some(runtime_revision)
                        || grace_due)
                        .then_some((pane.id, generation, runtime))
                })
                .collect::<Vec<_>>()
        };
        for (pane_id, generation, runtime) in targets {
            let Ok(observation) = runtime.agent_observation() else {
                continue;
            };
            let _ = apply_claude_terminal_observation(
                &daemon,
                pane_id,
                &generation,
                observation,
                Instant::now(),
            );
        }
        thread::sleep(CLAUDE_RECONCILE_INTERVAL);
    })
}

pub(crate) fn has_fresh_working_agent(state: &mut State, now: Instant) -> bool {
    let live_working = state
        .persisted
        .panes
        .iter()
        .filter(|pane| {
            pane.agent
                .as_ref()
                .is_some_and(|agent| agent.state == AgentState::Working)
                && state.runtime_generations.contains_key(&pane.id)
                && state
                    .runtimes
                    .get(&pane.id)
                    .is_some_and(|runtime| !runtime.exited())
        })
        .map(|pane| pane.id)
        .collect::<HashSet<_>>();
    state.agent_wake_leases.retain(|pane_id, renewed_at| {
        live_working.contains(pane_id)
            && now.saturating_duration_since(*renewed_at) < AGENT_WAKE_LEASE_TTL
    });
    state
        .agent_wake_tokens
        .retain(|pane_id, _| state.agent_wake_leases.contains_key(pane_id));
    !state.agent_wake_leases.is_empty()
}

#[cfg(target_os = "macos")]
pub(crate) fn spawn_wake_controller(daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    let daemon = Arc::clone(daemon);
    thread::spawn(move || {
        let mut controller = wake::Controller::new();
        let mut reported_config_error = None;
        loop {
            let now = Instant::now();
            let (stopping, has_working_agent) = {
                let mut state = lock(&daemon.state);
                (state.stopping, has_fresh_working_agent(&mut state, now))
            };
            if stopping {
                return;
            }
            if !has_working_agent {
                controller.reconcile(false, now);
                thread::sleep(WAKE_POLL_INTERVAL);
                continue;
            }
            let enabled = match GlobalConfig::load() {
                Ok((config, None)) => {
                    reported_config_error = None;
                    config.wake_mode
                }
                Ok((_, Some(warning))) => {
                    if reported_config_error.as_deref() != Some(warning.as_str()) {
                        eprintln!("wsxd wake mode disabled: {warning}");
                        reported_config_error = Some(warning);
                    }
                    false
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    if reported_config_error.as_deref() != Some(&error) {
                        eprintln!("wsxd wake mode disabled: {error}");
                        reported_config_error = Some(error);
                    }
                    false
                }
            };
            controller.reconcile(enabled && has_working_agent, now);
            thread::sleep(WAKE_POLL_INTERVAL);
        }
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn spawn_wake_controller(_daemon: &Arc<Daemon>) -> thread::JoinHandle<()> {
    thread::spawn(|| {})
}
