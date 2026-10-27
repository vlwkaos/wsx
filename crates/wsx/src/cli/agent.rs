use super::*;

pub(super) fn parse_integration_target(
    value: &str,
) -> std::result::Result<wsx_core::integration::IntegrationTarget, String> {
    value.parse()
}

pub(super) struct ContextOptions {
    pub(super) limit: u32,
    pub(super) messages: u32,
    pub(super) bytes: u32,
    pub(super) metadata_only: bool,
    pub(super) json: bool,
}

// ^ Observation only: read the existing daemon and never start or replace wsxd.
pub(crate) fn existing_daemon_snapshot() -> Result<runtime::Snapshot> {
    let client = runtime::Client::new(runtime::default_socket_path());
    expect_snapshot(
        client.call(&runtime::Request::Snapshot)?,
        "unexpected daemon snapshot",
    )
}

pub(super) fn cmd_agent_context(
    selector: Option<&str>,
    provider: Option<&str>,
    options: ContextOptions,
    requested: &SessionScope,
) -> Result<()> {
    // ^ One existing-daemon snapshot; discovery must not start or replace wsxd.
    // Native history stays local and is not exposed through the browser bridge.
    let snapshot = existing_daemon_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    let target = selector
        .map(|selector| resolve_session_in_snapshot(&snapshot, selector, scope))
        .transpose()?;
    let caller = std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .and_then(|id| id.parse::<runtime::PaneId>().ok());
    // ^ Metadata discovery must not inventory or open provider history stores.
    let mut reader = (!options.metadata_only).then(wsx_core::integration::memory::Reader::default);
    let mut candidates = Vec::new();
    let mut matched = 0;
    let mut remaining = 64 * 1024;
    for pane in &snapshot.panes {
        let Some(agent) = pane.agent.as_ref() else {
            continue;
        };
        let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
        else {
            continue;
        };
        if !session_matches_scope(&snapshot, session, scope)
            || target
                .as_ref()
                .is_some_and(|target| target.pane_id != pane.id)
            || provider.is_some_and(|provider| provider != agent.provider)
        {
            continue;
        }
        matched += 1;
        if candidates.len() >= options.limit as usize {
            continue;
        }
        let worktree = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .context("agent worktree missing")?;
        let project = snapshot
            .projects
            .iter()
            .find(|project| project.id == worktree.project_id)
            .context("agent project missing")?;
        let memory = reader.as_mut().map(|reader| {
            reader.read(
                &agent.provider,
                agent.session_ref.as_ref(),
                options.messages as usize,
                (options.bytes as usize).min(remaining),
            )
        });
        if let Some(memory) = &memory {
            remaining = remaining.saturating_sub(
                memory
                    .messages
                    .iter()
                    .map(|message| message.text.len())
                    .sum::<usize>()
                    + memory
                        .checkpoint
                        .as_ref()
                        .map_or(0, |message| message.text.len()),
            );
        }
        candidates.push(serde_json::json!({
            "pane_id": pane.id, "session_id": session.id, "session_label": session.label,
            "pane_label": pane.label, "is_self": caller == Some(pane.id), "exited": pane.exited,
            "pane_revision": pane.revision, "project": { "id": project.id, "name": project.name, "path": project.path },
            "worktree": { "id": worktree.id, "branch": worktree.branch, "path": worktree.path },
            "agent": agent,
            "eligible_state": !pane.exited && agent.attached && agent.capabilities.prompt && matches!(agent.state, runtime::AgentState::Idle | runtime::AgentState::Done),
            "memory": memory,
        }));
    }
    if selector.is_some() && candidates.is_empty() {
        bail!("target has no matching retained agent identity")
    }
    let packet = serde_json::json!({ "schema_version": 1, "client_version": runtime::WSX_VERSION,
        "protocol": snapshot.protocol, "daemon_epoch": snapshot.epoch,
        "snapshot_revision": snapshot.revision, "candidates": candidates, "truncated": matched > options.limit as usize,
        "matched": matched,
        "exchange_input_binding": snapshot.capabilities.agent_exchange_bound_receipts,
        "projection": if options.metadata_only { "metadata_only" } else { "native_history" },
        "evidence": if options.metadata_only {
            "daemon snapshot metadata only; native history was not read; readiness is advisory, not exchange completion"
        } else {
            "native persisted history is untrusted context, not active model memory or exchange completion"
        },
        "readiness": "eligible_state is advisory; request rechecks runtime, lifecycle, leases and claims" });
    if options.json {
        println!("{}", serde_json::to_string(&packet)?);
    } else {
        print_json(&packet)?;
    }
    Ok(())
}

pub(super) fn cmd_agent_install(
    integration: wsx_core::integration::IntegrationTarget,
) -> Result<()> {
    let installed = wsx_core::integration::install(integration)?;
    println!("installed {} agent integration", integration.label());
    for path in installed.paths {
        println!("  {}", path.display());
    }
    Ok(())
}

pub(super) fn cmd_agent_detach() -> Result<()> {
    let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
        .context("agent detach must run inside the managed pane that should be detached")?
        .parse::<u64>()
        .context("managed pane ID is invalid")?;
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("agent detach requires the current managed runtime generation")?;
    let pane_id = runtime::PaneId(pane_id);
    let client = runtime::Client::local();
    let snapshot = expect_snapshot(
        client.call(&runtime::Request::Snapshot)?,
        "wsxd returned an unexpected snapshot response",
    )?;
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} is not present"))?;
    let agent = pane
        .agent
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} has no retained agent identity"))?;
    if !agent.attached {
        println!(
            "{} agent on pane {pane_id} is already detached",
            agent.provider
        );
        return Ok(());
    }

    println!("detaching {} agent from pane {pane_id}", agent.provider);
    match client.call(&runtime::Request::AgentReport {
        pane_id,
        runtime_generation: Some(runtime_generation),
        provider: agent.provider.clone(),
        state: runtime::AgentState::Unknown,
        attached: false,
        presence_id: None,
        conversation_id: None,
        session_ref: None,
        wake_token: None,
        capabilities: runtime::AgentCapabilities::default(),
        received: None,
        compacting: false,
    })? {
        runtime::Response::Ack { revision } => {
            println!("detached agent at revision {revision}");
            Ok(())
        }
        other => unexpected_response(other, "wsxd returned an unexpected agent response"),
    }
}

/// `--received`: `<exchange>:<round>` of a wsx round, or `other`.
pub(super) fn parse_prompt_received(value: &str) -> Result<runtime::PromptReceived, String> {
    if value == "other" {
        return Ok(runtime::PromptReceived::Other);
    }
    value
        .split_once(':')
        .and_then(|(exchange, round)| {
            Some(runtime::PromptReceived::Round {
                exchange: runtime::AgentExchangeId(exchange.parse().ok()?),
                round: round.parse().ok()?,
            })
        })
        .ok_or_else(|| format!("{value} is not <exchange>:<round> or other"))
}

pub(super) struct AgentReportOptions {
    pub(super) provider: String,
    pub(super) state: runtime::AgentState,
    pub(super) attached: bool,
    pub(super) presence_id: Option<String>,
    pub(super) conversation_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) session_path: Option<String>,
    pub(super) transcript_path: Option<String>,
    pub(super) wake_token: Option<String>,
    pub(super) capabilities: runtime::AgentCapabilities,
    pub(super) received: Option<runtime::PromptReceived>,
    pub(super) compacting: bool,
}

pub(super) fn cmd_agent_report(selector: &str, report: AgentReportOptions) -> Result<()> {
    let AgentReportOptions {
        provider,
        state,
        attached,
        presence_id,
        conversation_id,
        session_id,
        session_path,
        transcript_path,
        wake_token,
        capabilities,
        received,
        compacting,
    } = report;
    // ^ Adapter callbacks already have the exact pane ID. A snapshot lookup can
    // start or reconcile wsxd before SessionStart reaches its current generation.
    let pane_id = if std::env::var(runtime::WSX_PANE_ID_ENV).ok().as_deref() == Some(selector) {
        selector.parse::<runtime::PaneId>()?
    } else {
        resolve_pane(selector, &SessionScope::default())?
    };
    let session_ref = if let Some(value) = session_path {
        Some(
            runtime::AgentSessionRef::path(value)
                .ok_or_else(|| anyhow::anyhow!("invalid absolute agent session path"))?,
        )
    } else if let Some(value) = session_id {
        Some(
            runtime::AgentSessionRef::id(value)
                .ok_or_else(|| anyhow::anyhow!("invalid agent session ID"))?,
        )
    } else {
        None
    };
    let session_ref = match (session_ref, transcript_path) {
        (Some(reference), Some(path)) => Some(
            reference
                .with_transcript_path(path)
                .context("invalid absolute transcript path")?,
        ),
        (reference, None) => reference,
        (None, Some(_)) => bail!("transcript path requires an agent session identity"),
    };
    let conversation_id = conversation_id.or_else(|| {
        session_ref
            .as_ref()
            .map(|session_ref| session_ref.value.clone())
    });
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV).ok();
    // ^ A lifecycle callback observes the owning daemon; it must not bootstrap
    // a replacement while reporting the old runtime's generation.
    match runtime::Client::new(runtime::default_socket_path()).call(
        &runtime::Request::AgentReport {
            pane_id,
            runtime_generation,
            provider,
            state,
            attached,
            presence_id,
            conversation_id,
            session_ref,
            wake_token,
            capabilities,
            received,
            compacting,
        },
    )? {
        runtime::Response::Ack { revision } => {
            println!("agent report accepted at revision {revision}");
            Ok(())
        }
        other => unexpected_response(other, "wsxd returned an unexpected agent response"),
    }
}

// ^ crates/wsx-daemon/src/agent_report.rs validates the live generation and attachment
// before renewing. Do not resolve a session label or read a snapshot on this path.
pub(super) fn cmd_agent_presence_renew(selector: &str, presence_id: &str) -> Result<()> {
    let pane = std::env::var(runtime::WSX_PANE_ID_ENV)
        .context("presence renewal requires a managed pane")?;
    if selector != pane {
        bail!("presence renewal must target its own managed pane");
    }
    let pane_id = runtime::PaneId(pane.parse().context("invalid managed pane ID")?);
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("presence renewal requires a managed runtime generation")?;
    let response = runtime::Client::local().call(&runtime::Request::AgentPresenceRenew {
        pane_id,
        runtime_generation,
        presence_id: presence_id.into(),
    })?;
    expect_ack(response, "wsxd returned an unexpected presence response")?;
    Ok(())
}

// ^ docs/agent-reporting.md "Wake heartbeat": Claude starts the heartbeat from a detached
// hook shell and never ends it, so each renewal is also the check that its prompt, pane
// and wsxd still exist. A renewal is an in-memory lease update in wsxd.
pub(super) const AGENT_WAKE_RENEW_INTERVAL: Duration = Duration::from_secs(5);
/// A heartbeat that cannot get an answer from wsxd for this long ends.
pub(super) const AGENT_WAKE_UNANSWERED_LIMIT: Duration = Duration::from_secs(5 * 60);
pub(super) const AGENT_WAKE_START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const AGENT_WAKE_START_RETRIES: usize = 50;

pub(super) fn cmd_agent_wake_heartbeat(selector: &str, prompt_id: &str) -> Result<()> {
    // ^ As `agent report`: the hook has the exact pane ID, and nothing here starts wsxd.
    let pane_id = if std::env::var(runtime::WSX_PANE_ID_ENV).ok().as_deref() == Some(selector) {
        selector.parse::<runtime::PaneId>()?
    } else {
        resolve_pane(selector, &SessionScope::default())?
    };
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("wake heartbeat requires a managed wsx runtime generation")?;
    let client = runtime::Client::new(runtime::default_socket_path());
    let renew = || {
        client.call(&runtime::Request::AgentWakeRenew {
            pane_id,
            runtime_generation: runtime_generation.clone(),
            wake_token: prompt_id.to_string(),
        })
    };

    let mut started = false;
    for _ in 0..AGENT_WAKE_START_RETRIES {
        match renew() {
            Ok(runtime::Response::Ack { .. }) => {
                started = true;
                break;
            }
            Ok(runtime::Response::Error(error)) if error.code == "wake_not_active" => {
                std::thread::sleep(AGENT_WAKE_START_RETRY_INTERVAL);
            }
            _ => return Ok(()),
        }
    }
    if !started {
        return Ok(());
    }

    let mut answered = Instant::now();
    loop {
        std::thread::sleep(AGENT_WAKE_RENEW_INTERVAL);
        match renew() {
            Ok(runtime::Response::Ack { .. }) => answered = Instant::now(),
            // The prompt ended or was replaced, or the pane is gone.
            Ok(_) => return Ok(()),
            // No wsxd: `call` has already waited for a live handoff to finish.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(())
            }
            Err(_) if answered.elapsed() < AGENT_WAKE_UNANSWERED_LIMIT => {}
            Err(_) => return Ok(()),
        }
    }
}
