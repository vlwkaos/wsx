use super::*;

pub(super) fn exchange_timeout_ms(seconds: u64) -> Result<u64> {
    seconds
        .checked_mul(1000)
        .filter(|value| *value > 0 && *value <= runtime::MAX_AGENT_EXCHANGE_TIMEOUT_MS)
        .context("exchange timeout must be between 1 second and 24 hours")
}

pub(super) fn terminal_frame_text(frame: &runtime::TerminalFrame) -> String {
    frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

pub(super) fn print_agent_exchange(
    exchange: &runtime::AgentExchange,
    frame: Option<&runtime::TerminalFrame>,
    json: bool,
) -> Result<()> {
    print_agent_exchange_with(exchange, frame, None, None, json)
}

/// The watched exchanges of a `wait --any`, by whether they are at a wait boundary.
pub(super) struct WatchedExchanges {
    pub(super) ready: Vec<runtime::AgentExchangeId>,
    pub(super) pending: Vec<runtime::AgentExchangeId>,
}

/// `result` is the value of `exchange_result`. A key is absent when it was not asked for.
pub(super) fn print_agent_exchange_with(
    exchange: &runtime::AgentExchange,
    frame: Option<&runtime::TerminalFrame>,
    result: Option<serde_json::Value>,
    watched: Option<&WatchedExchanges>,
    json: bool,
) -> Result<()> {
    if json {
        let mut output = serde_json::json!({
            "exchange": exchange,
            "fallback": frame.map(|frame| serde_json::json!({
                "kind": "terminal_frame",
                "text": terminal_frame_text(frame),
                "frame_revision": frame.revision,
            })),
        });
        if let Some(result) = result {
            output["result"] = result;
        }
        if let Some(watched) = watched {
            output["ready"] = serde_json::json!(watched.ready);
            output["pending"] = serde_json::json!(watched.pending);
        }
        print_json(&output)?;
    } else {
        println!(
            "exchange {} round {}: {:?}",
            exchange.id, exchange.round, exchange.state
        );
        println!("target: pane {} ({})", exchange.pane_id, exchange.provider);
        println!("evidence: {:?}", exchange.evidence);
        println!("revision: {}", exchange.revision);
        if let Some(frame) = frame {
            let text = terminal_frame_text(frame);
            if !text.is_empty() {
                println!("terminal frame (fallback):\n{text}");
            }
        }
        if let Some(result) = result {
            match result["text"].as_str() {
                Some(text) => println!("result (round {}):\n{text}", exchange.round),
                None => println!(
                    "result unavailable: {}",
                    result["status"].as_str().unwrap_or_default()
                ),
            }
        }
        if let Some(watched) = watched {
            println!("ready: {}", exchange_ids(&watched.ready));
            println!("pending: {}", exchange_ids(&watched.pending));
        }
    }
    Ok(())
}

pub(super) fn exchange_ids(ids: &[runtime::AgentExchangeId]) -> String {
    if ids.is_empty() {
        return "none".into();
    }
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The agent's final message of the exchange's current round, or the reason none can be named.
/// ^ docs/agent-orchestration.md "Result of a round": the round is found in the provider's
/// saved history by the envelope line that wsxd delivered. Text from before that line is
/// never a result.
pub(super) fn exchange_result(
    exchange: &runtime::AgentExchange,
    bytes: usize,
) -> serde_json::Value {
    let unavailable = |status: &str| serde_json::json!({ "status": status, "round": exchange.round, "text": null });
    if !matches!(
        exchange.state,
        runtime::AgentExchangeState::DoneObserved | runtime::AgentExchangeState::Completed
    ) {
        return unavailable("round_not_completed");
    }
    let Ok(snapshot) = ops::runtime_snapshot() else {
        return unavailable("daemon_unavailable");
    };
    let Some(agent) = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == exchange.pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| agent.id == exchange.agent_id)
    else {
        return unavailable("agent_replaced");
    };
    let marker = runtime::agent_exchange_envelope_start(exchange.id, exchange.round);
    // A provider may write its last record a moment after its agent reports done.
    let flushed_by = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = wsx_core::integration::memory::Reader::default().read_reply(
            &agent.provider,
            agent.session_ref.as_ref(),
            &marker,
            bytes,
        );
        let may_follow = matches!(
            reply.status,
            "delivery_not_in_history" | "no_message_after_delivery"
        );
        if !may_follow || Instant::now() >= flushed_by {
            return serde_json::json!({
                "status": reply.status,
                "round": exchange.round,
                "text": reply.text,
                "truncated": reply.truncated,
                "source": reply.source,
                "path": reply.path,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn agent_exchange_response(
    response: runtime::Response,
    json: bool,
) -> Result<runtime::AgentExchange> {
    match response {
        runtime::Response::AgentExchange { exchange, frame } => {
            print_agent_exchange(&exchange, frame.as_ref(), json)?;
            Ok(exchange)
        }
        other => unexpected_response(other, "wsxd returned an unexpected agent exchange response"),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_agent_exchange_request(
    selector: &str,
    prompt: PromptSource,
    timeout: u64,
    writer: bool,
    write_claims: Vec<PathBuf>,
    stash_draft: bool,
    key: Option<String>,
    json: bool,
    scope: &SessionScope,
) -> Result<()> {
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let pane_id = resolve_pane(selector, scope)?;
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let access = if writer {
        runtime::AgentExchangeAccess::Writer
    } else {
        runtime::AgentExchangeAccess::ReadOnly
    };
    let request = exchange_create_request(
        pane_id,
        prompt,
        timeout_ms,
        access,
        write_claims,
        stash_draft,
        key,
    );
    agent_exchange_response(deliver_exchange(&request)?, json)?;
    Ok(())
}

/// The create request for these options. A key selects the keyed variant, which an
/// older wsxd refuses; without a key the request is the one every protocol-16 wsxd knows.
pub(super) fn exchange_create_request(
    pane_id: runtime::PaneId,
    prompt: String,
    timeout_ms: u64,
    access: runtime::AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
    stash_draft: bool,
    key: Option<String>,
) -> runtime::Request {
    match key {
        Some(key) => runtime::Request::AgentExchangeCreateKeyed {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            key,
            stash_draft,
        },
        None if stash_draft => runtime::Request::AgentExchangeCreateStashingDraft {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
        },
        None => runtime::Request::AgentExchangeCreate {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
        },
    }
}

/// Send a create or continue request and say what state a failure left.
/// ^ docs/agent-orchestration.md "Idempotent delivery".
pub(super) fn deliver_exchange(request: &runtime::Request) -> Result<runtime::Response> {
    let keyed = matches!(
        request,
        runtime::Request::AgentExchangeCreateKeyed { .. }
            | runtime::Request::AgentExchangeContinueKeyed { .. }
    );
    match runtime::Client::local().call(request) {
        // An older wsxd cannot decode a keyed request and answers with its parse error.
        Ok(runtime::Response::Error(error))
            if keyed
                && error.code == "invalid_json"
                && error.message.contains("unknown variant") =>
        {
            bail!(
                "unsupported_request: this wsxd is older than --key and delivered nothing; \
                 run `wsx daemon restart`, or repeat the command without --key"
            )
        }
        Ok(response) => Ok(response),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            bail!(
                "reply_timeout: wsxd did not answer in time, and the prompt may be delivered; {}",
                if keyed {
                    "repeat the command with the same --key to get the exchange"
                } else {
                    "run `wsx agent exchanges <session>` before you send it again"
                }
            )
        }
        Err(error) => Err(error.into()),
    }
}

/// What `agent start` made or found. A late failure prints the same object with `error`
/// and `next`, so the caller knows what exists and what to run.
#[derive(serde::Serialize)]
pub(super) struct AgentStart {
    pub(super) project: String,
    pub(super) branch: String,
    pub(super) worktree: PathBuf,
    pub(super) worktree_created: bool,
    pub(super) session_id: Option<runtime::SessionId>,
    pub(super) session_label: String,
    pub(super) session_created: bool,
    pub(super) pane_id: Option<runtime::PaneId>,
    pub(super) agent: Option<String>,
    /// True when this call answered the agent's folder trust question.
    pub(super) folder_trust_accepted: bool,
    pub(super) exchange: Option<runtime::AgentExchange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next: Option<String>,
}

// ^ docs/agent-orchestration.md "Start a worker in one call": every step uses what a
// former run left, so the same command with the same --key continues and delivers once.
pub(super) fn cmd_agent_start(args: AgentStartArgs) -> Result<()> {
    // Refuse what can be refused before anything is made.
    let prompt = args.prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let timeout_ms = exchange_timeout_ms(args.timeout)?;
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, args.project.as_deref())?;
    let project_config = project.config.clone().unwrap_or_default();
    // ^ One owner of the command: `worktree.defaultSession.command`. `--agent` is the same
    // value for one call. A disabled default session gives no command.
    let configured = match project_config
        .initial_session(wsx_core::model::workspace::WorktreeInitialSession::Disabled)
    {
        wsx_core::model::workspace::WorktreeInitialSession::Command(command) => Some(command),
        _ => None,
    };
    let Some(command) = args
        .agent
        .or(configured)
        .filter(|command| !command.trim().is_empty())
    else {
        bail!(
            "no agent command: pass --agent <command>, or set worktree.defaultSession.command for project '{}'",
            project.name
        );
    };
    let label = args
        .name
        .filter(|name| !name.trim().is_empty())
        .or_else(|| command.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default();

    // The worktree. A failure here leaves nothing: see git::worktree::create_worktree.
    let existing = project
        .worktrees
        .iter()
        .find(|worktree| worktree.branch == args.branch);
    let (worktree, worktree_created) = match existing {
        Some(worktree) => (worktree.path.clone(), false),
        None => {
            refuse_start_point_of_existing_branch(project, &args.branch, args.from.as_deref())?;
            // No default session: the agent session below is the only one.
            let (path, warning) = ops::create_worktree(
                &project.path,
                args.from.as_deref().unwrap_or(&project.default_branch),
                &project_config,
                &args.branch,
            )?;
            if let Some(warning) = warning {
                eprintln!("warning: {warning}");
            }
            (path, true)
        }
    };
    let mut start = AgentStart {
        project: project.name.clone(),
        branch: args.branch.clone(),
        worktree,
        worktree_created,
        session_id: None,
        session_label: label,
        session_created: false,
        pane_id: None,
        agent: None,
        folder_trust_accepted: false,
        exchange: None,
        error: None,
        next: None,
    };

    let access = if args.writer {
        runtime::AgentExchangeAccess::Writer
    } else {
        runtime::AgentExchangeAccess::ReadOnly
    };
    let outcome = (|| -> std::result::Result<(), (&'static str, anyhow::Error)> {
        let snapshot = ops::runtime_snapshot().map_err(|error| ("session", error))?;
        let existing = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.path == start.worktree)
            .and_then(|worktree| {
                snapshot.sessions.iter().find(|session| {
                    session.worktree_id == worktree.id && session.label == start.session_label
                })
            });
        let session_id = match existing {
            Some(session) => session.id,
            None => {
                start.session_created = true;
                ops::create_session(
                    &config,
                    &project.name,
                    "",
                    &start.worktree,
                    Some(start.session_label.clone()),
                    Some(command.clone()),
                )
                .map_err(|error| ("session", error))?
                .0
            }
        };
        start.session_id = Some(session_id);

        // A repeat finds the exchange of its key before it waits: its agent may be at work on it.
        if let (Some(key), Some(session)) = (args.key.as_deref(), existing) {
            start.pane_id = Some(session.focused_pane);
            let exchanges =
                list_agent_exchanges(Some(session.focused_pane), RuntimeSessionScope::default())
                    .map_err(|error| ("request", error))?;
            if let Some(exchange) = exchanges
                .into_iter()
                .find(|exchange| exchange.key.as_deref() == Some(key))
            {
                start.agent = Some(exchange.provider.clone());
                start.exchange = Some(exchange);
                return Ok(());
            }
        }

        let (pane_id, provider) = wait_for_prompt_agent(
            session_id,
            Instant::now() + Duration::from_secs(args.ready_timeout),
            args.accept_folder_trust,
            &mut start,
        )
        .map_err(|error| ("ready", error))?;
        start.agent = Some(provider);
        let request = exchange_create_request(
            pane_id,
            prompt,
            timeout_ms,
            access,
            args.write_claims,
            false,
            args.key,
        );
        match deliver_exchange(&request).map_err(|error| ("request", error))? {
            runtime::Response::AgentExchange { exchange, .. } => {
                start.exchange = Some(exchange);
                Ok(())
            }
            runtime::Response::Error(error) => Err(("request", api_error(error))),
            _ => Err((
                "request",
                anyhow::anyhow!("wsxd returned an unexpected agent exchange response"),
            )),
        }
    })();

    let failure = outcome.err();
    if let Some((step, error)) = &failure {
        let mut report = serde_json::json!({ "step": step, "message": error.to_string() });
        let not_ready = error.downcast_ref::<AgentNotReady>();
        if let Some(not_ready) = not_ready {
            report["reason"] = serde_json::json!(not_ready.reason);
            report["screen"] = serde_json::json!(not_ready.screen);
        }
        start.error = Some(report);
        start.next = Some(match (not_ready.map(|not_ready| not_ready.reason), start.session_id) {
            (Some(FOLDER_TRUST_QUESTION), Some(session_id)) => format!(
                "session {session_id} waits on the agent's folder trust question for {}; review that folder, then repeat this command with --accept-folder-trust",
                start.worktree.display()
            ),
            (Some(_), Some(session_id)) => format!(
                "the worktree and session {session_id} are kept; read the screen in this report or `wsx session peek {session_id} --trim`, then repeat this command"
            ),
            _ => "what is listed here is kept; repeat this command".to_owned(),
        });
    }
    if args.json {
        print_json(&start)?;
    } else {
        println!("worktree: {}", start.worktree.display());
        if let Some(session_id) = start.session_id {
            println!("{}", created_session_line(session_id));
        }
        if let Some(pane_id) = start.pane_id {
            println!("pane:     {pane_id}");
        }
        if let Some(exchange) = &start.exchange {
            println!(
                "exchange: {} round {}: {:?}",
                exchange.id, exchange.round, exchange.state
            );
        }
        if let Some(screen) = start
            .error
            .as_ref()
            .and_then(|error| error["screen"].as_str())
        {
            println!("screen:   last lines of the pane");
            for line in screen.lines() {
                println!("  | {line}");
            }
        }
        if let Some(next) = &start.next {
            println!("next:     {next}");
        }
    }
    match failure {
        Some((step, error)) => Err(error.context(format!("agent start stopped at step '{step}'"))),
        None => Ok(()),
    }
}

pub(super) const FOLDER_TRUST_QUESTION: &str = "folder_trust_question";
pub(super) const NOT_READY_SCREEN_LINES: usize = 20;
pub(super) const NOT_READY_SCREEN_BYTES: usize = 2048;

/// Why `agent start` stopped before its agent could take a prompt. `screen` is the end
/// of the pane's visible screen, so the caller sees what the pane waits on in this reply.
#[derive(Debug)]
pub(super) struct AgentNotReady {
    pub(super) reason: &'static str,
    pub(super) message: String,
    pub(super) screen: Option<String>,
}

impl std::fmt::Display for AgentNotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentNotReady {}

/// The last non-empty lines of a screen, at most `NOT_READY_SCREEN_BYTES`.
pub(super) fn screen_excerpt(screen: &str) -> String {
    let lines = screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let excerpt = lines[lines.len().saturating_sub(NOT_READY_SCREEN_LINES)..].join("\n");
    let mut start = excerpt.len().saturating_sub(NOT_READY_SCREEN_BYTES);
    while !excerpt.is_char_boundary(start) {
        start += 1;
    }
    excerpt[start..].to_owned()
}

/// Claude ("Yes, I trust this folder") and Codex ("Trust this folder?") ask this before
/// they load a folder they have not seen. No agent hook runs until it is answered.
pub(super) fn asks_folder_trust(screen: &str) -> bool {
    screen
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .contains("trust this folder")
}

/// Select the trust choice of the question and confirm it. The highlighted choice is the
/// line after the agent's cursor (`❯` Claude, `›` Codex); Enter is sent only while that
/// line is the trust choice. Claude highlights "No, exit" first. False when no trust
/// choice was found.
pub(super) fn answer_folder_trust(pane_id: runtime::PaneId) -> Result<bool> {
    for _ in 0..4 {
        let screen = terminal_frame_text(&pane_frame(pane_id)?);
        let choice = screen
            .lines()
            .map(str::trim_start)
            .find_map(|line| line.strip_prefix('❯').or_else(|| line.strip_prefix('›')))
            .map(|choice| choice.trim().to_lowercase());
        match choice {
            Some(choice)
                if choice.contains("trust")
                    && !choice.starts_with("no")
                    && !choice.contains("not")
                    && !choice.contains("don") =>
            {
                send_pane_bytes(pane_id, b"\r".to_vec())?;
                return Ok(true);
            }
            Some(_) => send_pane_bytes(pane_id, b"\x1b[B".to_vec())?,
            None => return Ok(false),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(false)
}

/// Wait until the session's pane has an attached agent that can take a prompt now.
/// `start.pane_id` keeps the pane ID for the report when the wait fails.
pub(super) fn wait_for_prompt_agent(
    session_id: runtime::SessionId,
    deadline: Instant,
    accept_folder_trust: bool,
    start: &mut AgentStart,
) -> Result<(runtime::PaneId, String)> {
    let mut screen_read = None::<Instant>;
    loop {
        let snapshot = ops::runtime_snapshot()?;
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .context("session_missing: the session was closed")?;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| pane.id == session.focused_pane)
            .context("session_missing: the session has no pane")?;
        start.pane_id = Some(pane.id);
        let not_ready = |reason, message: String| AgentNotReady {
            reason,
            message,
            screen: pane_frame(pane.id)
                .ok()
                .map(|frame| screen_excerpt(&terminal_frame_text(&frame))),
        };
        if pane.exited {
            return Err(not_ready(
                "session_exited",
                format!("session_exited: run `wsx session restart {session_id}`"),
            )
            .into());
        }
        let agent = pane.agent.as_ref().filter(|agent| agent.attached);
        if let Some(agent) = agent.filter(|agent| {
            agent.capabilities.prompt
                && matches!(
                    agent.state,
                    runtime::AgentState::Idle | runtime::AgentState::Done
                )
        }) {
            return Ok((pane.id, agent.provider.clone()));
        }
        // ^ The trust question does not go away by itself: stop at once, or answer it
        // once when the caller asked for that.
        if agent.is_none()
            && screen_read.is_none_or(|read| read.elapsed() >= Duration::from_millis(500))
        {
            screen_read = Some(Instant::now());
            let screen = terminal_frame_text(&pane_frame(pane.id)?);
            if asks_folder_trust(&screen) {
                if !accept_folder_trust || start.folder_trust_accepted {
                    return Err(AgentNotReady {
                        reason: FOLDER_TRUST_QUESTION,
                        message: if start.folder_trust_accepted {
                            "agent_not_ready: the agent still asks whether to trust the folder"
                        } else {
                            "agent_not_ready: the agent asks whether to trust the folder"
                        }
                        .to_owned(),
                        screen: Some(screen_excerpt(&screen)),
                    }
                    .into());
                }
                if !answer_folder_trust(pane.id)? {
                    return Err(not_ready(
                        FOLDER_TRUST_QUESTION,
                        "agent_not_ready: no trust choice was found in the folder trust question"
                            .to_owned(),
                    )
                    .into());
                }
                start.folder_trust_accepted = true;
                // ^ Look again after 2.5 s: the agent needs that long to load the folder.
                screen_read = Some(Instant::now() + Duration::from_secs(2));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(match agent {
                Some(agent) if agent.capabilities.prompt => not_ready(
                    "agent_not_idle",
                    format!("agent_not_ready: {} is {:?}", agent.provider, agent.state),
                ),
                Some(agent) => not_ready(
                    "agent_cannot_prompt",
                    format!(
                        "agent_not_ready: {} is {:?} and its integration cannot take a prompt",
                        agent.provider, agent.state
                    ),
                ),
                None => not_ready(
                    "no_agent_report",
                    "agent_not_ready: no agent reported from the session".to_owned(),
                ),
            }
            .into());
        }
        // ^ Not `poll`: wsxd counts a poll without TUI data as an open TUI.
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

pub(super) fn pane_frame(pane_id: runtime::PaneId) -> Result<runtime::TerminalFrame> {
    match runtime::Client::local().call(&runtime::Request::View {
        pane_ids: vec![pane_id],
    })? {
        runtime::Response::View { mut frames, .. } => {
            frames.pop().context("terminal frame is unavailable")
        }
        other => unexpected_response(other, "wsxd returned an unexpected view response"),
    }
}

pub(super) fn parse_exchange_receipt(
    value: &str,
) -> std::result::Result<runtime::AgentExchangeReceipt, String> {
    match value {
        "accepted" => Ok(runtime::AgentExchangeReceipt::Accepted),
        "completed" => Ok(runtime::AgentExchangeReceipt::Completed),
        _ => Err("receipt must be accepted or completed".into()),
    }
}

// ^ docs/agent-orchestration.md: only adapters correlate native turns with exact requests.
// wsxd owns capability, generation, round and state validation. Never bootstrap or retry a receipt.
pub(super) fn cmd_agent_exchange_receipt(
    exchange_id: runtime::AgentExchangeId,
    round: u32,
    receipt: runtime::AgentExchangeReceipt,
    delivery_sha256: Option<String>,
    input_id: Option<String>,
    json: bool,
) -> Result<()> {
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("exchange receipt requires a managed wsx runtime generation")?;
    let request = match (delivery_sha256, input_id) {
        (Some(delivery_sha256), Some(input_id)) => {
            let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
                .ok()
                .and_then(|value| value.parse::<runtime::PaneId>().ok())
                .context("bound exchange receipt requires a managed wsx pane")?;
            runtime::Request::AgentExchangeBoundReceipt {
                exchange_id,
                pane_id,
                round,
                runtime_generation,
                delivery_sha256,
                input_id,
                receipt,
            }
        }
        (None, None) => runtime::Request::AgentExchangeReceipt {
            exchange_id,
            round,
            runtime_generation,
            receipt,
        },
        _ => bail!("bound receipt requires both delivery digest and native input ID"),
    };
    agent_exchange_response(
        runtime::Client::new(runtime::default_socket_path()).call(&request)?,
        json,
    )?;
    Ok(())
}

pub(super) fn cmd_agent_exchange_inspect(
    exchange_id: runtime::AgentExchangeId,
    frame: bool,
    json: bool,
) -> Result<()> {
    let (exchange, frame) = fetch_agent_exchange(exchange_id, frame)?;
    print_agent_exchange(&exchange, frame.as_ref(), json)
}

pub(super) fn current_agent_exchange(
    exchange_id: runtime::AgentExchangeId,
) -> Result<runtime::AgentExchange> {
    Ok(fetch_agent_exchange(exchange_id, false)?.0)
}

pub(super) fn fetch_agent_exchange(
    exchange_id: runtime::AgentExchangeId,
    include_frame: bool,
) -> Result<(runtime::AgentExchange, Option<runtime::TerminalFrame>)> {
    match runtime::Client::local().call(&runtime::Request::AgentExchangeGet {
        exchange_id,
        include_frame,
    })? {
        runtime::Response::AgentExchange { exchange, frame } => Ok((exchange, frame)),
        other => unexpected_response(other, "wsxd returned an unexpected agent exchange response"),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_agent_exchange_wait(
    exchanges: Vec<runtime::AgentExchangeId>,
    any: bool,
    timeout: u64,
    frame: bool,
    result_bytes: Option<usize>,
    json: bool,
    scope: &SessionScope,
) -> Result<()> {
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (exchange_id, watched) = if any {
        let watched = wait_any_agent_exchange(exchanges, scope, deadline)?;
        (watched.ready[0], Some(watched))
    } else {
        let [exchange_id] = exchanges[..] else {
            bail!("wait takes one exchange; add --any for the first of several or of a -p/-w scope")
        };
        if scope.project.is_some() || scope.worktree.is_some() {
            bail!("-p and -w select exchanges only with --any");
        }
        wait_agent_exchange(exchange_id, deadline)?;
        (exchange_id, None)
    };
    let (exchange, frame) = fetch_agent_exchange(exchange_id, frame)?;
    let result = result_bytes.map(|bytes| exchange_result(&exchange, bytes));
    print_agent_exchange_with(&exchange, frame.as_ref(), result, watched.as_ref(), json)
}

pub(super) fn wait_agent_exchange(
    exchange_id: runtime::AgentExchangeId,
    deadline: Instant,
) -> Result<()> {
    let mut exchange = current_agent_exchange(exchange_id)?;
    while !exchange.is_wait_boundary() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "wait_timeout: exchange {} remains {:?}",
                exchange.id,
                exchange.state
            );
        }
        let wait_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, 30_000);
        exchange = match runtime::Client::local().wait_agent_exchange(
            &runtime::Request::AgentExchangeWait {
                exchange_id,
                after_revision: exchange.revision,
                timeout_ms: wait_ms,
                include_frame: false,
            },
        )? {
            runtime::Response::AgentExchange { exchange, .. } => exchange,
            other => {
                unexpected_response(other, "wsxd returned an unexpected agent exchange response")?
            }
        };
    }
    Ok(())
}

/// The most exchanges that one `wait --any` watches: each one holds a connection to wsxd.
pub(super) const MAX_WATCHED_EXCHANGES: usize = 16;

/// Wait until one of the named exchanges, or one that was active in the scope at the
/// start, is at a wait boundary. `ready` is then not empty.
/// ^ docs/agent-orchestration.md "Wait for the first of several": no daemon request is
/// added. Each pending exchange gets its own `agent_exchange_wait` long-poll, which wsxd
/// answers when that exchange changes. `poll` is not used: wsxd counts a poll without
/// TUI data as an open TUI, and that would defer a daemon upgrade during the wait.
pub(super) fn wait_any_agent_exchange(
    named: Vec<runtime::AgentExchangeId>,
    requested: &SessionScope,
    deadline: Instant,
) -> Result<WatchedExchanges> {
    let snapshot = ops::runtime_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    let mut watched = named;
    if watched.is_empty() {
        watched = list_agent_exchanges(None, scope)?
            .iter()
            .filter(|exchange| !exchange.is_wait_boundary())
            .map(|exchange| exchange.id)
            .collect();
        if watched.is_empty() {
            bail!("no_active_exchange: no exchange in the requested scope waits for its agent");
        }
    }
    if watched.len() > MAX_WATCHED_EXCHANGES {
        bail!("too_many_exchanges: wait --any watches at most {MAX_WATCHED_EXCHANGES} exchanges");
    }
    let sort = || -> Result<WatchedExchanges> {
        let exchanges = list_agent_exchanges(None, scope)?;
        let mut sorted = WatchedExchanges {
            ready: Vec::new(),
            pending: Vec::new(),
        };
        for id in &watched {
            let Some(exchange) = exchanges.iter().find(|exchange| exchange.id == *id) else {
                bail!("not_found: exchange {id} is not retained in the requested scope");
            };
            if exchange.is_wait_boundary() {
                sorted.ready.push(*id);
            } else {
                sorted.pending.push(*id);
            }
        }
        Ok(sorted)
    };
    let sorted = sort()?;
    if !sorted.ready.is_empty() {
        return Ok(sorted);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    for id in sorted.pending {
        let sender = sender.clone();
        // A watcher that is still waiting ends with this process.
        std::thread::spawn(move || {
            let _ = sender.send(wait_agent_exchange(id, deadline));
        });
    }
    drop(sender);
    let first = receiver
        .recv()
        .context("exchange watchers ended without an answer")?;
    let sorted = sort()?;
    match first {
        Err(error) if sorted.ready.is_empty() => {
            if error.to_string().starts_with("wait_timeout") {
                bail!(
                    "wait_timeout: no exchange of {} is at a wait boundary",
                    exchange_ids(&sorted.pending)
                );
            }
            Err(error)
        }
        _ => Ok(sorted),
    }
}

pub(super) fn cmd_agent_exchange_continue(
    exchange_id: runtime::AgentExchangeId,
    prompt: PromptSource,
    timeout: u64,
    stash_draft: bool,
    key: Option<String>,
    json: bool,
) -> Result<()> {
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let current = current_agent_exchange(exchange_id)?;
    let expected_revision = current.revision;
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let request = match key {
        Some(key) => runtime::Request::AgentExchangeContinueKeyed {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            key,
            stash_draft,
        },
        None if stash_draft => runtime::Request::AgentExchangeContinueStashingDraft {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
        },
        None => runtime::Request::AgentExchangeContinue {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
        },
    };
    agent_exchange_response(deliver_exchange(&request)?, json)?;
    Ok(())
}

pub(super) fn cmd_agent_exchange_cancel(
    exchange_id: runtime::AgentExchangeId,
    json: bool,
) -> Result<()> {
    let current = current_agent_exchange(exchange_id)?;
    agent_exchange_response(
        runtime::Client::local().call(&runtime::Request::AgentExchangeCancel {
            exchange_id,
            expected_revision: current.revision,
            takeover: false,
        })?,
        json,
    )?;
    Ok(())
}

/// Retained exchanges of one session or pane, or of the requested project and worktree.
pub(super) fn scoped_agent_exchanges(
    selector: Option<&str>,
    requested: &SessionScope,
) -> Result<Vec<runtime::AgentExchange>> {
    let mut pane_id = None;
    let mut scope = RuntimeSessionScope::default();
    if selector.is_some() || requested.project.is_some() || requested.worktree.is_some() {
        let snapshot = ops::runtime_snapshot()?;
        scope = requested_runtime_scope(&snapshot, requested)?;
        if let Some(selector) = selector {
            pane_id = Some(resolve_session_in_snapshot(&snapshot, selector, scope)?.pane_id);
        }
    }
    list_agent_exchanges(pane_id, scope)
}

/// ^ wsxd filters by pane only. A scope without a session is applied here, on the
/// project and worktree IDs that every exchange carries.
pub(super) fn list_agent_exchanges(
    pane_id: Option<runtime::PaneId>,
    scope: RuntimeSessionScope,
) -> Result<Vec<runtime::AgentExchange>> {
    match runtime::Client::local().call(&runtime::Request::AgentExchangeList { pane_id })? {
        runtime::Response::AgentExchanges(mut exchanges) => {
            exchanges.retain(|exchange| {
                scope.project_id.is_none_or(|id| exchange.project_id == id)
                    && scope
                        .worktree_id
                        .is_none_or(|id| exchange.worktree_id == id)
            });
            Ok(exchanges)
        }
        other => unexpected_response(
            other,
            "wsxd returned an unexpected agent exchange list response",
        ),
    }
}

pub(super) fn cmd_agent_exchange_list(
    selector: Option<&str>,
    json: bool,
    scope: &SessionScope,
) -> Result<()> {
    let exchanges = scoped_agent_exchanges(selector, scope)?;
    if json {
        print_json(&exchanges)?;
        return Ok(());
    }
    for exchange in exchanges {
        println!(
            "{}\tpane {}\tround {}\t{:?}\t{:?}",
            exchange.id, exchange.pane_id, exchange.round, exchange.state, exchange.evidence
        );
    }
    Ok(())
}
