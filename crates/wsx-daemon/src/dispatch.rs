//! Request dispatch and the read-only views: snapshot, poll, terminal views and plugin views.

use super::*;

pub(crate) fn handle(daemon: &Arc<Daemon>, request: Request) -> Response {
    match handle_inner(daemon, request) {
        Ok(response) => response,
        Err(error) => Response::Error(error),
    }
}

pub(crate) fn handle_inner(daemon: &Arc<Daemon>, request: Request) -> Result<Response, ApiError> {
    // ^ An exchange delivery takes the mutation lock itself and releases it while the PTY
    // takes the prompt: see deliver_exchange_round.
    let _mutation = (!matches!(
        &request,
        Request::Hello { .. }
            | Request::Snapshot
            | Request::Poll { .. }
            | Request::View { .. }
            | Request::AgentExchangeWait { .. }
            | Request::PluginList
            | Request::LifecycleStatus
            | Request::AgentExchangeCreate { .. }
            | Request::AgentExchangeCreateStashingDraft { .. }
            | Request::AgentExchangeCreateKeyed { .. }
            | Request::AgentExchangeContinue { .. }
            | Request::AgentExchangeContinueStashingDraft { .. }
            | Request::AgentExchangeContinueKeyed { .. }
            | Request::AgentSteer { .. }
    ))
    .then(|| lock(&daemon.mutation_lock));
    if !matches!(&request, Request::Hello { .. } | Request::LifecycleStatus)
        && lock(&daemon.state).handoff_in_progress
    {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    match request {
        Request::Hello { protocol } => {
            if protocol != PROTOCOL_VERSION {
                return Err(api(
                    "protocol_mismatch",
                    format!("client {protocol}, daemon {PROTOCOL_VERSION}"),
                ));
            }
            Ok(Response::Hello {
                protocol: PROTOCOL_VERSION,
                epoch: daemon.epoch,
                capabilities: capabilities(),
            })
        }
        Request::Snapshot => Ok(Response::Snapshot(snapshot(daemon, &lock(&daemon.state)))),
        Request::Poll {
            after_revision,
            timeout_ms,
            tui,
        } => poll(daemon, after_revision, timeout_ms, tui),
        Request::SynchronizeProjects { projects } => synchronize_projects(daemon, projects),
        Request::SessionCreate {
            worktree_id,
            label,
            command,
            initial_input,
            rows,
            cols,
        } => create_session(
            daemon,
            worktree_id,
            label,
            command,
            initial_input,
            rows,
            cols,
        ),
        Request::SessionRename {
            session_id,
            label,
            expected_revision,
        } => mutate(daemon, |persisted, _revision| {
            let session = session_mut(persisted, session_id)?;
            expect_revision(session.revision, expected_revision)?;
            session.label = bounded_label(label)?;
            Ok(("session.renamed", session_id.0))
        }),
        Request::SessionReorder {
            session_id,
            target_session_id,
            placement,
            expected_revision,
        } => reorder_session(
            daemon,
            session_id,
            target_session_id,
            placement,
            expected_revision,
        ),
        Request::SessionClose {
            session_id,
            expected_revision,
        } => close_session(daemon, session_id, expected_revision),
        Request::SessionRestart {
            pane_id,
            expected_revision,
        } => restart_session(daemon, pane_id, expected_revision),
        Request::PaneSplit {
            session_id,
            target,
            axis,
            label,
            command,
            initial_input,
            rows,
            cols,
            expected_revision,
        } => split_pane(
            daemon,
            session_id,
            target,
            axis,
            label,
            command,
            initial_input,
            rows,
            cols,
            expected_revision,
        ),
        Request::PaneFocus {
            session_id,
            pane_id,
        } => mutate(daemon, |persisted, revision| {
            let session = session_mut(persisted, session_id)?;
            if !session.panes.contains(&pane_id) {
                return Err(api("not_found", "pane is not in session"));
            }
            session.focused_pane = pane_id;
            session.revision = revision;
            Ok(("pane.focused", pane_id.0))
        }),
        Request::PaneClose {
            pane_id,
            expected_revision,
        } => close_pane(daemon, pane_id, expected_revision),
        Request::TerminalAcquire {
            pane_id,
            client_id,
            takeover,
        } => {
            let (revision, _) = acquire_terminal_lease(daemon, pane_id, client_id, takeover)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalRelease { pane_id, client_id } => {
            let revision = release_client_lease(daemon, pane_id, client_id)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalHeartbeat { pane_id, client_id } => mutate_runtime(daemon, |state| {
            refresh_lease(state, pane_id, LeaseAccess::Client(client_id))
        }),
        Request::TerminalSubscribe { .. } | Request::TerminalRawSubscribe { .. } => Err(api(
            "invalid_request",
            "a terminal subscribe is only valid as a connection upgrade",
        )),
        Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        } => {
            if bytes.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "input exceeds limit"));
            }
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.write(&bytes).map_err(terminal_api)?;
            request_claude_reconcile(daemon, pane_id, bytes == [3]);
            Ok(Response::Ack { revision })
        }
        Request::TerminalKey {
            pane_id,
            client_id,
            key,
        } => {
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.key(&key).map_err(terminal_api)?;
            request_claude_reconcile(daemon, pane_id, key_interrupts_agent(&key));
            Ok(Response::Ack { revision })
        }
        Request::TerminalPaste {
            pane_id,
            client_id,
            text,
        } => {
            if text.len() > MAX_INPUT_BYTES {
                return Err(api("invalid_input", "paste exceeds limit"));
            }
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.paste(&text).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalMouse {
            pane_id,
            client_id,
            mouse,
        } => {
            let (runtime, revision) =
                leased_runtime(daemon, pane_id, LeaseAccess::Client(client_id))?;
            runtime.mouse(&mouse).map_err(terminal_api)?;
            Ok(Response::Ack { revision })
        }
        Request::TerminalResize {
            pane_id,
            client_id,
            rows,
            cols,
        } => resize_terminal(daemon, pane_id, LeaseAccess::Client(client_id), rows, cols),
        Request::View { pane_ids } => view(daemon, pane_ids),
        Request::AgentReport {
            pane_id,
            runtime_generation,
            provider,
            state: agent_state,
            attached,
            presence_id,
            conversation_id,
            session_ref,
            wake_token,
            capabilities,
            received,
            compacting,
        } => agent_report_with_attachment(
            daemon,
            RuntimeAgentAuthority::new(pane_id, runtime_generation),
            AgentReportInput {
                provider,
                state: agent_state,
                attached,
                presence_id,
                conversation_id,
                session_ref,
                wake_token,
                capabilities,
                received,
                compacting,
            },
        ),
        Request::AgentPresenceRenew {
            pane_id,
            runtime_generation,
            presence_id,
        } => renew_agent_presence(daemon, pane_id, runtime_generation, presence_id),
        Request::AgentWakeRenew {
            pane_id,
            runtime_generation,
            wake_token,
        } => renew_agent_wake(daemon, pane_id, runtime_generation, wake_token),
        Request::AgentClear {
            pane_id,
            runtime_generation,
            next_runtime_generation,
        } => agent_clear(daemon, pane_id, runtime_generation, next_runtime_generation),
        Request::AgentExchangeCreate {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery::default(),
        ),
        Request::AgentExchangeCreateStashingDraft {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery {
                stash_draft: true,
                key: None,
            },
        ),
        Request::AgentExchangeCreateKeyed {
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            key,
            stash_draft,
            command,
            takeover,
        } => create_agent_exchange_with(
            daemon,
            pane_id,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            access,
            write_claims,
            ExchangeDelivery {
                stash_draft,
                key: Some(key),
            },
        ),
        Request::AgentExchangeContinueStashingDraft {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery {
                stash_draft: true,
                key: None,
            },
        ),
        Request::AgentExchangeContinue {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery::default(),
        ),
        Request::AgentExchangeContinueKeyed {
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            key,
            stash_draft,
            command,
            takeover,
        } => continue_agent_exchange_with(
            daemon,
            exchange_id,
            expected_revision,
            ExchangeInput {
                prompt,
                command,
                takeover,
            },
            timeout_ms,
            ExchangeDelivery {
                stash_draft,
                key: Some(key),
            },
        ),
        Request::AgentExchangeGet {
            exchange_id,
            include_frame,
        } => get_agent_exchange(daemon, exchange_id, include_frame),
        Request::AgentExchangeList { pane_id } => list_agent_exchanges(daemon, pane_id),
        Request::AgentExchangeWait {
            exchange_id,
            after_revision,
            timeout_ms,
            include_frame,
        } => wait_agent_exchange(
            daemon,
            exchange_id,
            after_revision,
            timeout_ms,
            include_frame,
        ),
        Request::AgentExchangeReceipt {
            exchange_id,
            round,
            runtime_generation,
            receipt,
        } => {
            report_agent_exchange_receipt(daemon, exchange_id, round, &runtime_generation, receipt)
        }
        Request::AgentExchangeBoundReceipt {
            exchange_id,
            pane_id,
            round,
            runtime_generation,
            delivery_sha256,
            input_id,
            receipt,
        } => report_agent_exchange_receipt_with_binding(
            daemon,
            exchange_id,
            round,
            &runtime_generation,
            receipt,
            Some((pane_id, &delivery_sha256, &input_id)),
        ),
        Request::AgentExchangeCancel {
            exchange_id,
            expected_revision,
            takeover,
        } => cancel_agent_exchange(daemon, exchange_id, expected_revision, takeover),
        Request::AgentSteer {
            pane_id,
            text,
            takeover,
        } => steer_agent(daemon, pane_id, &text, takeover),
        Request::PluginList => Ok(Response::Plugins(lock(&daemon.state).plugins.clone())),
        Request::PluginReload => {
            let mut state = lock(&daemon.state);
            state.plugins = plugins::discover();
            bump(daemon, &mut state, "plugins", 0);
            Ok(Response::Plugins(state.plugins.clone()))
        }
        Request::PluginReviewCancel { request_id } => {
            daemon.review_jobs.cancel(&request_id);
            Ok(Response::Ack {
                revision: lock(&daemon.state).revision,
            })
        }
        Request::PluginReview {
            plugin_id,
            worktree_id,
            request_id,
            comparison,
            limits,
            operation,
        } => {
            let (plugin, path) = {
                let state = lock(&daemon.state);
                let plugin = state
                    .plugins
                    .iter()
                    .find(|plugin| {
                        plugin.enabled
                            && plugin.id == plugin_id
                            && plugin
                                .worktree_review
                                .as_ref()
                                .is_some_and(|spec| spec.comparisons.contains(&comparison))
                    })
                    .cloned()
                    .ok_or_else(|| api("plugin_unavailable", "review provider unavailable"))?;
                let path = state
                    .persisted
                    .worktrees
                    .iter()
                    .find(|w| w.id == worktree_id)
                    .map(|w| w.path.clone())
                    .ok_or_else(|| api("worktree_not_found", "worktree unavailable"))?;
                (plugin, path)
            };
            let request = ReviewRequest {
                api_version: REVIEW_API_VERSION,
                request_id,
                worktree_id,
                worktree_path: path,
                comparison,
                limits,
                operation,
            };
            request
                .validate()
                .map_err(|message| api("invalid_review_request", message))?;
            let cancelled = daemon
                .review_jobs
                .start(&request.request_id)
                .map_err(|error| api("review_unavailable", error.to_string()))?;
            let result = plugins::review::invoke(&plugin, &request, &cancelled);
            daemon.review_jobs.finish(&request.request_id);
            result
                .map(|response| Response::PluginReview {
                    epoch: daemon.epoch,
                    response,
                })
                .map_err(|error| {
                    api(
                        match error.kind() {
                            io::ErrorKind::TimedOut => "review_timeout",
                            io::ErrorKind::Interrupted => "review_cancelled",
                            io::ErrorKind::InvalidData => "review_invalid_output",
                            _ => "review_unavailable",
                        },
                        error.to_string(),
                    )
                })
        }
        Request::PluginRender {
            plugin_id,
            pane_id,
            columns,
            rows,
            generation,
        } => render_plugin_view(daemon, plugin_id, pane_id, columns, rows, generation),
        Request::LifecycleStatus => {
            let mut state = lock(&daemon.state);
            Ok(Response::Lifecycle(lifecycle_status(daemon, &mut state)))
        }
        Request::PrepareReplacement {
            target_binary_id,
            target_daemon_revision,
        } => prepare_replacement_with_revision(daemon, target_binary_id, target_daemon_revision),
        Request::PrepareHandoff {
            target_binary_id,
            target_version,
            target_protocol,
            target_daemon_revision,
            executable,
            restart,
        } => prepare_handoff(
            daemon,
            target_binary_id,
            target_version,
            target_protocol,
            target_daemon_revision,
            executable,
            restart,
        ),
        Request::Shutdown => {
            let mut state = lock(&daemon.state);
            state.stopping = true;
            state.stop_reason = Some(StopReason::Intentional);
            daemon.changed.notify_all();
            daemon.plugin_changed.notify_all();
            Ok(Response::Ack {
                revision: state.revision,
            })
        }
    }
}

pub(crate) fn render_plugin_view(
    daemon: &Daemon,
    plugin_id: String,
    pane_id: PaneId,
    columns: u16,
    rows: u16,
    generation: u64,
) -> Result<Response, ApiError> {
    if !plugins::valid_token(&plugin_id) || columns == 0 || rows == 0 || columns > 500 || rows > 512
    {
        return Err(api(
            "invalid_plugin_view",
            "plugin view dimensions are invalid",
        ));
    }
    let (plugin, worktree_id, worktree_path) = {
        let state = lock(&daemon.state);
        let plugin = state
            .plugins
            .iter()
            .find(|plugin| plugin.enabled && plugin.id == plugin_id && plugin.sidecar.is_some())
            .cloned()
            .ok_or_else(|| api("plugin_unavailable", "plugin sidecar is unavailable"))?;
        let pane = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .ok_or_else(|| api("pane_not_found", "pane does not exist"))?;
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
            .ok_or_else(|| api("session_not_found", "session does not exist"))?;
        let worktree = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .ok_or_else(|| api("worktree_not_found", "worktree does not exist"))?;
        (plugin, worktree.id, worktree.path.clone())
    };
    plugins::render(
        &plugin,
        plugins::RenderContext {
            pane_id,
            worktree_id,
            worktree_path: &worktree_path,
            epoch: daemon.epoch,
            columns,
            rows,
            generation,
        },
    )
    .map(Response::PluginView)
    .map_err(|error| api("plugin_view_failed", error.to_string()))
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        pane_splits: true,
        plugins: true,
        plugin_views: true,
        worktree_review_available: false,
        agent_reports: true,
        agent_session_restore: true,
        resume_shell_fallback: true,
        listening_ports: true,
        foreground_jobs: true,
        process_restore: false,
        lifecycle_coordination: true,
        version_coordination: true,
        daemon_revision_coordination: true,
        live_handoff: true,
        agent_exchanges: true,
        agent_exchange_bound_receipts: true,
        raw_terminal_stream: true,
        daemon_restart: true,
        exchange_command: true,
        exchange_shell: true,
        agent_steer: true,
    }
}

pub(crate) fn snapshot(daemon: &Daemon, state: &State) -> Snapshot {
    let mut listening_ports = state
        .listening_ports
        .iter()
        .map(|(pane_id, tcp)| PanePorts {
            pane_id: *pane_id,
            tcp: tcp.clone(),
        })
        .collect::<Vec<_>>();
    listening_ports.sort_by_key(|ports| ports.pane_id);
    let mut pane_activity = state
        .foreground_jobs
        .iter()
        .map(|pane_id| PaneActivity {
            pane_id: *pane_id,
            foreground_job: true,
        })
        .collect::<Vec<_>>();
    pane_activity.sort_by_key(|activity| activity.pane_id);
    let mut capabilities = capabilities();
    capabilities.worktree_review_available = state
        .plugins
        .iter()
        .any(|p| p.enabled && p.worktree_review.is_some());
    Snapshot {
        protocol: PROTOCOL_VERSION,
        epoch: daemon.epoch,
        revision: state.revision,
        projects: state.persisted.projects.clone(),
        worktrees: state.persisted.worktrees.clone(),
        sessions: state.persisted.sessions.clone(),
        panes: state
            .persisted
            .panes
            .iter()
            .map(|pane| pane.pane.clone())
            .collect(),
        listening_ports,
        pane_activity,
        plugin_sidecars: plugins::sidecars(&state.plugins),
        capabilities,
    }
}

pub(crate) fn poll(
    daemon: &Daemon,
    after: u64,
    timeout_ms: u64,
    tui: Option<TuiClientPresence>,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    record_tui_presence(&mut state, tui)?;
    if state.revision <= after && !state.stopping {
        state = daemon
            .changed
            .wait_timeout(state, Duration::from_millis(timeout_ms.min(30_000)))
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
    let earliest = state
        .events
        .front()
        .map(event_revision)
        .unwrap_or(state.revision);
    if after.saturating_add(1) < earliest {
        return Ok(Response::Events {
            revision: state.revision,
            events: vec![Event::ResyncRequired {
                revision: state.revision,
            }],
        });
    }
    Ok(Response::Events {
        revision: state.revision,
        events: state
            .events
            .iter()
            .filter(|event| event_revision(event) > after)
            .cloned()
            .collect(),
    })
}

pub(crate) fn record_tui_presence(
    state: &mut State,
    tui: Option<TuiClientPresence>,
) -> Result<(), ApiError> {
    let now = Instant::now();
    expire_tui_presence(state, now);
    let Some(tui) = tui else {
        state.legacy_tui_until = Some(now + TUI_PRESENCE_TTL);
        return Ok(());
    };
    if tui.instance_id == 0
        || tui.version.len() > 64
        || compare_wsx_versions(&tui.version, &tui.version).is_none()
        || tui.target_binary_id.is_empty()
        || tui.target_binary_id.len() > 512
        || binary_identity_version(&tui.target_binary_id) != Some(tui.version.as_str())
        || compare_binary_identities(&tui.target_binary_id, &tui.target_binary_id).is_none()
    {
        return Err(api("invalid_tui", "TUI presence is invalid"));
    }
    state.tui_clients.insert(
        tui.instance_id,
        TuiPresenceLease {
            target_binary_id: tui.target_binary_id,
            target_daemon_revision: tui.target_daemon_revision,
            max_protocol: tui.max_protocol,
            follows: tui.follows,
            expires_at: now + TUI_PRESENCE_TTL,
        },
    );
    Ok(())
}

pub(crate) fn expire_tui_presence(state: &mut State, now: Instant) {
    state
        .tui_clients
        .retain(|_, client| client.expires_at > now);
    if state.legacy_tui_until.is_some_and(|expires| expires <= now) {
        state.legacy_tui_until = None;
    }
}

pub(crate) fn event_revision(event: &Event) -> u64 {
    match event {
        Event::Changed { revision, .. }
        | Event::Exited { revision, .. }
        | Event::ResyncRequired { revision } => *revision,
    }
}

pub(crate) fn view(daemon: &Daemon, pane_ids: Vec<PaneId>) -> Result<Response, ApiError> {
    if pane_ids.len() > MAX_VIEW_PANES {
        return Err(api("invalid_request", "too many panes requested"));
    }
    let pane_ids = unique_view_pane_ids(pane_ids);
    let (snapshot, runtimes) = {
        let state = lock(&daemon.state);
        let runtimes = pane_ids
            .into_iter()
            .filter_map(|id| state.runtimes.get(&id).cloned())
            .collect::<Vec<_>>();
        (snapshot(daemon, &state), runtimes)
    };
    let mut frames = Vec::with_capacity(runtimes.len());
    let mut total_cells = 0;
    for runtime in runtimes {
        let frame = runtime.frame().map_err(terminal_api)?;
        total_cells = add_view_cells(total_cells, frame.cells.len())?;
        frames.push(frame);
    }
    Ok(Response::View { snapshot, frames })
}

pub(crate) fn unique_view_pane_ids(pane_ids: Vec<PaneId>) -> Vec<PaneId> {
    let mut seen = HashSet::new();
    pane_ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

pub(crate) fn add_view_cells(total: usize, next: usize) -> Result<usize, ApiError> {
    let total = total
        .checked_add(next)
        .filter(|total| *total <= MAX_VIEW_CELLS)
        .ok_or_else(|| api("response_too_large", "terminal view exceeds cell limit"))?;
    Ok(total)
}

pub(crate) fn spawn_plugin_dispatcher(daemon: &Arc<Daemon>) -> io::Result<thread::JoinHandle<()>> {
    let daemon = Arc::clone(daemon);
    thread::Builder::new()
        .name("wsx-plugin-dispatch".into())
        .spawn(move || loop {
            let next = {
                let mut state = lock(&daemon.state);
                while state.plugin_events.is_empty() && !state.stopping {
                    state = daemon
                        .plugin_changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if state.stopping {
                    return;
                }
                state
                    .plugin_events
                    .pop_front()
                    .map(|(name, payload)| (state.plugins.clone(), name, payload))
            };
            if let Some((plugins, name, payload)) = next {
                plugins::emit(&plugins, &name, &payload);
            }
        })
}
