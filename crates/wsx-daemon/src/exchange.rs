//! The agent exchange broker: create, deliver, continue, wait, receipts, steer and cancel.

use super::*;

pub(crate) fn terminate_agent_exchanges(
    persisted: &mut Persisted,
    pane_ids: &[PaneId],
    exchange_state: AgentExchangeState,
    revision: u64,
) {
    let now = unix_time_millis();
    for exchange in &mut persisted.agent_exchanges {
        if pane_ids.contains(&exchange.pane_id) && !exchange.is_terminal() {
            exchange.state = exchange_state;
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
}

pub(crate) fn observe_agent_exchange(
    persisted: &mut Persisted,
    pane_id: PaneId,
    agent_state: AgentState,
    attached: bool,
    revision: u64,
    now: u64,
) {
    let receipts = persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| agent.attached && agent.capabilities.prompt_receipts);
    let Some(exchange) = persisted
        .agent_exchanges
        .iter_mut()
        .rev()
        .find(|exchange| exchange.pane_id == pane_id && !exchange.is_terminal())
    else {
        return;
    };
    // ^ With prompt receipts a round follows the agent's turns only once the agent took it:
    // until then they belong to another prompt (docs/agent-orchestration.md "Taken, not typed").
    // A command round may start no prompt at all (Claude's /compact): it follows the lifecycle.
    if attached
        && receipts
        && !exchange.command
        && matches!(
            exchange.state,
            AgentExchangeState::Submitted | AgentExchangeState::Delivered
        )
    {
        return;
    }
    let next = if !attached {
        Some(AgentExchangeState::Interrupted)
    } else if exchange.state == AgentExchangeState::CancelRequested
        && agent_state != AgentState::Working
    {
        Some(AgentExchangeState::Cancelled)
    } else {
        match agent_state {
            AgentState::Working => Some(AgentExchangeState::WorkingObserved),
            AgentState::Blocked => Some(AgentExchangeState::BlockedObserved),
            AgentState::Done => Some(AgentExchangeState::DoneObserved),
            AgentState::Error => Some(AgentExchangeState::ErrorObserved),
            // ^ docs/agent-orchestration.md "Interrupted turn": an agent that was seen at
            // work on this exchange and is idle again was stopped by a person (Escape in
            // its terminal, or a denied dialog). No "done" will follow, so the exchange
            // ends here and the next request is not held back until the deadline.
            AgentState::Idle
                if matches!(
                    exchange.state,
                    AgentExchangeState::WorkingObserved | AgentExchangeState::BlockedObserved
                ) =>
            {
                Some(AgentExchangeState::Interrupted)
            }
            AgentState::Unknown | AgentState::Idle => None,
        }
    };
    if let Some(next) = next {
        exchange.state = next;
        exchange.evidence = AgentExchangeEvidence::PaneLifecycle;
        exchange.updated_unix_ms = now;
        exchange.revision = revision;
    }
}

/// How long a delivered round may stay untaken while its agent is not working.
pub(crate) const TAKE_WAIT_MS: u64 = 5_000;

/// The agent's own hook says which prompt a turn took. The round it names is accepted; a
/// round still waiting when another prompt starts was not taken, so that turn never
/// becomes its result. ref: docs/agent-orchestration.md "Taken, not typed"
pub(crate) fn receive_prompt(
    persisted: &mut Persisted,
    pane_id: PaneId,
    received: Option<PromptReceived>,
    revision: u64,
    now: u64,
) {
    let Some(received) = received else {
        return;
    };
    let Some(exchange) = persisted.agent_exchanges.iter_mut().rev().find(|exchange| {
        exchange.pane_id == pane_id
            && matches!(
                exchange.state,
                AgentExchangeState::Submitted | AgentExchangeState::Delivered
            )
    }) else {
        return;
    };
    let named = received
        == PromptReceived::Round {
            exchange: exchange.id,
            round: exchange.round,
        };
    // ^ A command round has no exchange line: the prompt the agent took is taken as it.
    if named || exchange.command {
        exchange.state = AgentExchangeState::Accepted;
        exchange.evidence = AgentExchangeEvidence::PaneLifecycle;
    } else {
        exchange.state = AgentExchangeState::DeliveryFailed;
        exchange.failure = Some("not_taken: the agent took another prompt first".into());
    }
    exchange.updated_unix_ms = now;
    exchange.revision = revision;
}

/// A delivered round that an agent with prompt receipts has not taken within
/// `TAKE_WAIT_MS` while it is not working, such as a paste dropped as a turn ended, is not
/// taken. Returns whether a round changed.
pub(crate) fn fail_untaken_rounds(daemon: &Daemon, state: &mut State) -> Result<bool, ApiError> {
    let now = unix_time_millis();
    let untaken = |exchange: &AgentExchange, panes: &[PersistedPane]| {
        exchange.state == AgentExchangeState::Delivered
            && !exchange.command
            && now.saturating_sub(exchange.updated_unix_ms) >= TAKE_WAIT_MS
            && panes
                .iter()
                .find(|pane| pane.id == exchange.pane_id)
                .and_then(|pane| pane.agent.as_ref())
                .is_some_and(|agent| {
                    agent.attached
                        && agent.capabilities.prompt_receipts
                        && agent.state != AgentState::Working
                        && agent
                            .state_since_unix_ms
                            .is_none_or(|since| now.saturating_sub(since) >= TAKE_WAIT_MS)
                })
    };
    let persisted = &state.persisted;
    if !persisted
        .agent_exchanges
        .iter()
        .any(|exchange| untaken(exchange, &persisted.panes))
    {
        return Ok(false);
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let panes = persisted.panes.clone();
    for exchange in &mut persisted.agent_exchanges {
        if untaken(exchange, &panes) {
            exchange.state = AgentExchangeState::DeliveryFailed;
            exchange.failure = Some("not_taken: the agent did not take it within 5 s".into());
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.not_taken",
        0,
    )?;
    Ok(true)
}

pub(crate) const MAX_EXCHANGE_COMMAND_BYTES: usize = 256;

pub(crate) fn validate_exchange_prompt(prompt: &str) -> Result<(), ApiError> {
    if prompt.is_empty() || prompt.len() > MAX_AGENT_EXCHANGE_PROMPT_BYTES {
        return Err(api("invalid_prompt", "prompt must be 1..65536 bytes"));
    }
    if prompt
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(api(
            "invalid_prompt",
            "prompt contains terminal control characters",
        ));
    }
    Ok(())
}

pub(crate) fn validate_exchange_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    (timeout_ms > 0 && timeout_ms <= MAX_AGENT_EXCHANGE_TIMEOUT_MS)
        .then_some(timeout_ms)
        .ok_or_else(|| api("invalid_timeout", "timeout must be 1ms..24h"))
}

pub(crate) fn exchange_claims(
    access: AgentExchangeAccess,
    claims: Vec<PathBuf>,
    worktree: &Worktree,
) -> Result<Vec<PathBuf>, ApiError> {
    if access == AgentExchangeAccess::ReadOnly {
        if !claims.is_empty() {
            return Err(api(
                "invalid_claim",
                "read-only exchanges cannot declare write claims",
            ));
        }
        return Ok(Vec::new());
    }
    if claims.len() > MAX_AGENT_EXCHANGE_WRITE_CLAIMS {
        return Err(api("invalid_claim", "too many write claims"));
    }
    let claims = if claims.is_empty() {
        vec![worktree.path.clone()]
    } else {
        claims
    };
    if claims.iter().any(|claim| {
        !claim.is_absolute()
            || !claim.starts_with(&worktree.path)
            || claim
                .components()
                .any(|part| matches!(part, Component::ParentDir))
    }) {
        return Err(api(
            "invalid_claim",
            "write claims must be absolute paths inside the target worktree",
        ));
    }
    let canonical_worktree = fs::canonicalize(&worktree.path)
        .map_err(|error| api("invalid_claim", format!("cannot resolve worktree: {error}")))?;
    let mut unique = claims
        .into_iter()
        .map(|claim| canonical_exchange_claim(&claim, &worktree.path, &canonical_worktree))
        .collect::<Result<Vec<_>, _>>()?;
    unique.sort();
    unique.dedup();
    Ok(unique)
}

pub(crate) fn canonical_exchange_claim(
    claim: &Path,
    worktree: &Path,
    canonical_worktree: &Path,
) -> Result<PathBuf, ApiError> {
    let relative = claim.strip_prefix(worktree).map_err(|_| {
        api(
            "invalid_claim",
            "write claim is outside the target worktree",
        )
    })?;
    let mut resolved = canonical_worktree.to_path_buf();
    let mut components = relative.components();
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            return Err(api(
                "invalid_claim",
                "write claim contains an invalid path component",
            ));
        };
        let candidate = resolved.join(component);
        match fs::canonicalize(&candidate) {
            Ok(path) => {
                if !path.starts_with(canonical_worktree) {
                    return Err(api(
                        "invalid_claim",
                        "write claim resolves outside the target worktree",
                    ));
                }
                resolved = path;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                resolved = candidate;
                for remaining in components {
                    let Component::Normal(remaining) = remaining else {
                        return Err(api(
                            "invalid_claim",
                            "write claim contains an invalid path component",
                        ));
                    };
                    resolved.push(remaining);
                }
                break;
            }
            Err(error) => {
                return Err(api(
                    "invalid_claim",
                    format!("cannot resolve write claim: {error}"),
                ))
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn claims_overlap(left: &[PathBuf], right: &[PathBuf]) -> bool {
    left.iter().any(|left| {
        right
            .iter()
            .any(|right| left.starts_with(right) || right.starts_with(left))
    })
}

pub(crate) fn has_writer_claim_conflict(exchanges: &[AgentExchange], claims: &[PathBuf]) -> bool {
    exchanges.iter().any(|exchange| {
        exchange.access == AgentExchangeAccess::Writer
            && !exchange.is_terminal()
            && claims_overlap(claims, &exchange.write_claims)
    })
}

pub(crate) fn slash_command(line: &str) -> bool {
    line.strip_prefix('/')
        .is_some_and(|rest| rest.starts_with(|character: char| character.is_ascii_alphanumeric()))
}

/// `!` and a command, such as `! git status` or `!ls`.
pub(crate) fn shell_line(line: &str) -> bool {
    line.strip_prefix('!')
        .is_some_and(|rest| !rest.trim().is_empty())
}

/// What one exchange round types into the agent's terminal.
pub(crate) struct ExchangeInput {
    pub(crate) prompt: String,
    /// A one-line slash command, typed as written (docs/agent-orchestration.md "Commands").
    pub(crate) command: bool,
    /// Take the pane from a live terminal controller (docs/agent-orchestration.md
    /// "Terminal control").
    pub(crate) takeover: bool,
}

impl ExchangeInput {
    fn validate(&self) -> Result<(), ApiError> {
        validate_exchange_prompt(&self.prompt)?;
        // ^ An agent runs a command only at the start of its input, so a command round
        // carries no exchange line. It must be one short line that names a command
        // (`/name`) or a shell line (`!` and a command, docs/agent-orchestration.md "Commands").
        if self.command
            && (self.prompt.len() > MAX_EXCHANGE_COMMAND_BYTES
                || self.prompt.contains(['\n', '\t'])
                || !(slash_command(&self.prompt) || shell_line(&self.prompt)))
        {
            return Err(api(
                "invalid_prompt",
                "a command must be one line of at most 256 bytes that starts with /name or !",
            ));
        }
        Ok(())
    }

    fn terminal_text(
        &self,
        id: AgentExchangeId,
        round: u32,
        access: AgentExchangeAccess,
    ) -> String {
        if self.command {
            return self.prompt.clone();
        }
        let access = match access {
            AgentExchangeAccess::ReadOnly => "read-only",
            AgentExchangeAccess::Writer => "writer",
        };
        // ^ wsx_core::runtime::agent_exchange_envelope_start: `agent wait --result` finds
        // the round in the agent's history by this line.
        format!(
            "{}{access}]\n{}",
            agent_exchange_envelope_start(id, round),
            self.prompt
        )
    }
}

// ^ Provider-specific editor protocol, never agent identity or completion.
// Documented Ctrl+S stashes idle Claude input; fresh empty chrome is the ACK.
// See docs/agent-orchestration.md and Claude's interactive-mode documentation.
pub(crate) fn claude_editor(observation: &AgentTerminalObservation) -> Option<&str> {
    if claude_status::classify(&observation.title, &observation.text)
        != Some(claude_status::TerminalEvidence::Idle)
    {
        return None;
    }
    claude_input(observation)
}

/// The text of Claude's input line, at work or idle; `Some("")` when it is empty.
pub(crate) fn claude_input(observation: &AgentTerminalObservation) -> Option<&str> {
    let lines: Vec<_> = observation.text.lines().collect();
    let border = |line: &str| {
        let line = line.trim();
        !line.is_empty() && line.chars().all(|character| matches!(character, '─' | '━'))
    };
    // ^ Claude Code 2.1.3xx draws up to seven footer rows under the editor's bottom border
    // (status line, mode, update notice, background agents): look above that border.
    let prompt = |line: &&str| line.trim_start().starts_with('❯');
    let end = match lines.iter().rposition(|line| border(line)) {
        Some(bottom) if !lines[bottom + 1..].iter().any(prompt) => bottom,
        _ => lines.len(),
    };
    let (index, input) =
        lines[..end]
            .iter()
            .enumerate()
            .rev()
            .take(8)
            .find_map(|(index, line)| {
                line.trim_start()
                    .strip_prefix('❯')
                    .map(|input| (index, input.trim()))
            })?;
    // ^ A blank first line is not an empty multiline editor. Stop at Claude's
    // horizontal input border, not its footer. Ambiguous placeholder UI is refused.
    if input.starts_with("Try ") {
        return None;
    }
    if !input.is_empty() {
        return Some(input);
    }
    for line in &lines[index + 1..] {
        if border(line) {
            break;
        }
        if !line.trim().is_empty() {
            return Some(line.trim());
        }
    }
    Some("")
}

// ^ docs/agent-orchestration.md: an empty Claude editor shows a suggestion such as
// `❯ Try "fix typecheck errors"` or, after a turn, a next prompt in plain words
// (Claude 2.1.300). Claude draws it dimmed; text a person typed is not. Only the
// editor line of the frame, dimmed as a whole, counts as empty, whatever its words.
pub(crate) fn claude_suggestion(
    runtime: &TerminalRuntime,
    observation: &AgentTerminalObservation,
) -> bool {
    if claude_status::classify(&observation.title, &observation.text)
        != Some(claude_status::TerminalEvidence::Idle)
    {
        return false;
    }
    let Ok(frame) = runtime.frame() else {
        return false;
    };
    if frame.cols == 0 {
        return false;
    }
    // The editor is the lowest prompt line; a short conversation leaves blank rows below it.
    let input = |row: &[Cell]| {
        let text: String = row.iter().map(|cell| cell.symbol.as_str()).collect();
        text.trim_start()
            .strip_prefix('❯')
            .map(|input| input.trim().to_owned())
    };
    let editor = frame
        .cells
        .chunks(usize::from(frame.cols))
        .rev()
        .find(|row| input(row).is_some());
    editor.is_some_and(|row| {
        input(row).is_some_and(|input| !input.is_empty())
            && row
                .iter()
                .filter(|cell| !cell.symbol.trim().is_empty() && cell.symbol != "❯")
                .all(|cell| cell.modifiers.dim)
    })
}

pub(crate) fn validate_draft_target(
    agent: &AgentInfo,
    runtime: &TerminalRuntime,
) -> Result<(), ApiError> {
    if agent.provider != "claude" {
        return Err(api(
            "draft_policy_unsupported",
            "draft stashing is supported only for Claude",
        ));
    }
    if !agent.attached || !matches!(agent.state, AgentState::Idle | AgentState::Done) {
        return Err(api(
            "agent_busy",
            "draft stashing requires an idle or done Claude agent",
        ));
    }
    let observation = runtime
        .agent_observation()
        .map_err(|_| api("editor_unavailable", "cannot read current Claude editor"))?;
    if claude_editor(&observation).is_none() && !claude_suggestion(runtime, &observation) {
        return Err(api(
            "editor_unavailable",
            "Claude input editor is not visibly idle",
        ));
    }
    Ok(())
}

pub(crate) fn stash_claude_draft(
    runtime: &TerminalRuntime,
    deadline: Instant,
) -> Result<(), ApiError> {
    let before = runtime
        .agent_observation()
        .map_err(|_| api("editor_unavailable", "cannot observe Claude editor"))?;
    // An empty editor needs no key: Ctrl+S would restore the existing stash.
    if claude_editor(&before) == Some("") || claude_suggestion(runtime, &before) {
        return Ok(());
    }
    runtime
        .write_within(b"\x13", deadline)
        .map_err(|_| api("editor_unavailable", "cannot stash Claude editor"))?;
    let observe_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < observe_until {
        let after = runtime
            .agent_observation()
            .map_err(|_| api("editor_unavailable", "cannot observe stashed Claude editor"))?;
        // ^ The emptied editor may show Claude's dimmed suggestion again.
        if after.revision > before.revision
            && (claude_editor(&after) == Some("") || claude_suggestion(runtime, &after))
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(api(
        "editor_stash_unconfirmed",
        "prompt was not sent because fresh empty input was not observed",
    ))
}

pub(crate) fn deliver_agent_exchange(
    runtime: &TerminalRuntime,
    input: &str,
    deadline: Instant,
) -> Result<(), wsx_terminal::TerminalError> {
    runtime.paste_within(input, deadline)?;
    runtime.write_within(b"\r", deadline)
}

/// Upper bound of the PTY write of one round. A healthy agent takes a 64 KiB prompt in
/// milliseconds. An agent that stops reading must not hold its pane and a live handoff.
pub(crate) const AGENT_EXCHANGE_WRITE_LIMIT: Duration = Duration::from_secs(10);

/// The locks a delivery holds, or the exchange that a repeated key already made.
pub(crate) enum DeliveryLocks<'a> {
    Held(MutexGuard<'a, ()>, MutexGuard<'a, ()>),
    Repeat(Box<AgentExchange>),
}

/// Take the mutation lock and the pane-operation lock of a delivery. Other requests take
/// the mutation lock first and then wait for the pane, so a delivery never waits for its
/// pane while it holds the mutation lock: a slow delivery to that pane would hold every
/// change. `repeat` finds the exchange of a repeated key, which is returned at once: the
/// key is saved with the submitted round, also while that round is still in the PTY. A
/// continuation key is compared before the revision, because a repeat carries a newer one.
pub(crate) fn lock_delivery<'a>(
    daemon: &'a Daemon,
    operation: &'a Mutex<()>,
    repeat: impl Fn(&State) -> Option<&AgentExchange>,
) -> DeliveryLocks<'a> {
    loop {
        let mutation = lock(&daemon.mutation_lock);
        if let Some(exchange) = repeat(&lock(&daemon.state)).cloned() {
            return DeliveryLocks::Repeat(Box::new(exchange));
        }
        match operation.try_lock() {
            Ok(guard) => return DeliveryLocks::Held(mutation, guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return DeliveryLocks::Held(mutation, poisoned.into_inner())
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        drop(mutation);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Ends a delivery's hold on live handoff, also when recording its outcome fails.
pub(crate) struct DeliveryInFlight<'a>(&'a Daemon);

impl Drop for DeliveryInFlight<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.deliveries_in_flight = state.deliveries_in_flight.saturating_sub(1);
    }
}

/// Write the persisted `round` of exchange `id` to its PTY and record whether it arrived.
/// ^ docs/agent-orchestration.md "Idempotent delivery": the mutation lock is released while the PTY
/// takes the prompt, so other changes, among them the agent's own reports, go on while an
/// agent reads slowly. `deliveries_in_flight` keeps a live handoff from starting until the
/// outcome is recorded. The pane-operation lock is released before the mutation lock is
/// taken again, because other requests take the mutation lock first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn deliver_exchange_round(
    daemon: &Daemon,
    mutation: MutexGuard<'_, ()>,
    operation: MutexGuard<'_, ()>,
    mut state: MutexGuard<'_, State>,
    id: AgentExchangeId,
    round: u32,
    runtime: &TerminalRuntime,
    stash_draft: bool,
    input: &str,
) -> Result<Response, ApiError> {
    let delivery_sha256 = format!("{:x}", Sha256::digest(input.as_bytes()));
    let deadline_unix_ms = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map_or(0, |exchange| exchange.deadline_unix_ms);
    state.deliveries_in_flight += 1;
    let _in_flight = DeliveryInFlight(daemon);
    drop(state);
    drop(mutation);
    let remaining = Duration::from_millis(deadline_unix_ms.saturating_sub(unix_time_millis()));
    let deadline = Instant::now() + remaining.min(AGENT_EXCHANGE_WRITE_LIMIT);
    let stashed = if stash_draft {
        stash_claude_draft(runtime, deadline)
    } else {
        Ok(())
    };
    let delivered = stashed.and_then(|()| {
        deliver_agent_exchange(runtime, input, deadline)
            .map_err(|error| api("terminal_write_failed", error.to_string()))
    });
    drop(operation);
    let _mutation = lock(&daemon.mutation_lock);
    let mut state = lock(&daemon.state);
    let current = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    // ^ A change made during the write, such as an expiry, keeps its own outcome.
    if current.state != AgentExchangeState::Submitted || current.round != round {
        return Ok(Response::AgentExchange {
            exchange: current,
            frame: None,
        });
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .expect("exchange found above");
    record_delivery(exchange, delivered, delivery_sha256);
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.delivery",
        id.0,
    )?;
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}

pub(crate) fn expire_due_agent_exchanges(
    daemon: &Daemon,
    state: &mut State,
) -> Result<(), ApiError> {
    let now = unix_time_millis();
    if !state
        .persisted
        .agent_exchanges
        .iter()
        .any(|exchange| !exchange.is_terminal() && exchange.deadline_unix_ms <= now)
    {
        return Ok(());
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    for exchange in &mut persisted.agent_exchanges {
        if !exchange.is_terminal() && exchange.deadline_unix_ms <= now {
            exchange.state = AgentExchangeState::Expired;
            exchange.updated_unix_ms = now;
            exchange.revision = revision;
        }
    }
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.expired",
        0,
    )?;
    Ok(())
}

/// How one round reaches the agent's editor.
#[derive(Default)]
pub(crate) struct ExchangeDelivery {
    pub(crate) stash_draft: bool,
    /// ^ docs/agent-orchestration.md "Idempotent delivery": a repeated key returns the
    /// exchange that the first request made and writes nothing to the PTY.
    pub(crate) key: Option<String>,
}

pub(crate) fn validate_exchange_key(key: Option<&str>) -> Result<(), ApiError> {
    match key {
        Some(key)
            if key.is_empty()
                || key.len() > MAX_AGENT_EXCHANGE_KEY_BYTES
                || !key.bytes().all(|byte| byte.is_ascii_graphic()) =>
        {
            Err(api(
                "invalid_key",
                "key must be 1..128 printable ASCII bytes without spaces",
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(crate) fn create_agent_exchange(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    prompt: String,
    timeout_ms: u64,
    access: AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
) -> Result<Response, ApiError> {
    create_agent_exchange_with(
        daemon,
        pane_id,
        ExchangeInput {
            prompt,
            command: false,
            takeover: false,
        },
        timeout_ms,
        access,
        write_claims,
        ExchangeDelivery::default(),
    )
}

pub(crate) fn create_agent_exchange_with(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    input: ExchangeInput,
    timeout_ms: u64,
    access: AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
    delivery: ExchangeDelivery,
) -> Result<Response, ApiError> {
    let ExchangeDelivery { stash_draft, key } = delivery;
    input.validate()?;
    validate_exchange_key(key.as_deref())?;
    let timeout_ms = validate_exchange_timeout(timeout_ms)?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let (mutation, operation) = match lock_delivery(daemon, &operation_lock, |state| {
        key.as_deref().and_then(|key| {
            state.persisted.agent_exchanges.iter().find(|exchange| {
                exchange.pane_id == pane_id && exchange.key.as_deref() == Some(key)
            })
        })
    }) {
        DeliveryLocks::Held(mutation, operation) => (mutation, operation),
        DeliveryLocks::Repeat(exchange) => {
            return Ok(Response::AgentExchange {
                exchange: *exchange,
                frame: None,
            })
        }
    };
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    expire_due_agent_exchanges(daemon, &mut state)?;
    if state.stopping {
        return Err(api(
            "daemon_unavailable",
            "wsxd cannot start an exchange during shutdown",
        ));
    }
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .cloned()
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let agent = pane
        .agent
        .as_ref()
        .filter(|agent| agent.attached && agent.capabilities.prompt)
        .ok_or_else(|| {
            api(
                "agent_unavailable",
                "pane has no attached prompt-capable agent",
            )
        })?;
    if matches!(
        agent.state,
        AgentState::Working | AgentState::Unknown | AgentState::Error
    ) {
        return Err(api(
            "agent_busy",
            "agent is not ready for an explicit exchange",
        ));
    }
    if state
        .persisted
        .agent_exchanges
        .iter()
        .any(|exchange| exchange.pane_id == pane_id && !exchange.is_terminal())
    {
        return Err(api("exchange_busy", "pane already has an active exchange"));
    }
    let session = state
        .persisted
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id)
        .cloned()
        .ok_or_else(|| api("not_found", "session not found"))?;
    let worktree = state
        .persisted
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .cloned()
        .ok_or_else(|| api("not_found", "worktree not found"))?;
    let claims = exchange_claims(access, write_claims, &worktree)?;
    if access == AgentExchangeAccess::Writer
        && has_writer_claim_conflict(&state.persisted.agent_exchanges, &claims)
    {
        return Err(api(
            "write_claim_conflict",
            "an active writer already owns an overlapping path",
        ));
    }
    let generation = state
        .runtime_generations
        .get(&pane_id)
        .cloned()
        .ok_or_else(|| {
            api(
                "terminal_unavailable",
                "pane has no live runtime generation",
            )
        })?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    if stash_draft {
        validate_draft_target(agent, &runtime)?;
    }
    take_terminal_lease(&mut state, pane_id, input.takeover)?;
    let revision = state.revision.saturating_add(1);
    let now = unix_time_millis();
    let mut persisted = state.persisted.clone();
    while persisted.agent_exchanges.len() >= MAX_AGENT_EXCHANGES {
        let Some(index) = persisted
            .agent_exchanges
            .iter()
            .position(|exchange| exchange.is_terminal())
        else {
            return Err(api("exchange_limit", "too many active agent exchanges"));
        };
        persisted.agent_exchanges.remove(index);
    }
    let id = AgentExchangeId(next_id(&mut persisted)?);
    let deadline = now.saturating_add(timeout_ms);
    persisted.agent_exchanges.push(AgentExchange {
        id,
        pane_id,
        session_id: session.id,
        worktree_id: worktree.id,
        project_id: worktree.project_id,
        agent_id: agent.id,
        provider: agent.provider.clone(),
        runtime_generation: generation,
        round: 1,
        access,
        write_claims: claims,
        state: AgentExchangeState::Submitted,
        evidence: AgentExchangeEvidence::IntentPersisted,
        created_unix_ms: now,
        updated_unix_ms: now,
        deadline_unix_ms: deadline,
        delivery_revision: revision,
        delivery_sha256: None,
        native_input_id: None,
        failure: None,
        key,
        continue_key: None,
        command: input.command,
        revision,
    });
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.submitted",
        id.0,
    )?;

    let text = input.terminal_text(id, 1, access);
    deliver_exchange_round(
        daemon,
        mutation,
        operation,
        state,
        id,
        1,
        &runtime,
        stash_draft,
        &text,
    )
}

#[cfg(test)]
pub(crate) fn continue_agent_exchange(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    prompt: String,
    timeout_ms: u64,
) -> Result<Response, ApiError> {
    let input = ExchangeInput {
        prompt,
        command: false,
        takeover: false,
    };
    continue_agent_exchange_with(
        daemon,
        id,
        expected_revision,
        input,
        timeout_ms,
        ExchangeDelivery::default(),
    )
}

pub(crate) fn continue_agent_exchange_with(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    input: ExchangeInput,
    timeout_ms: u64,
    delivery: ExchangeDelivery,
) -> Result<Response, ApiError> {
    let ExchangeDelivery { stash_draft, key } = delivery;
    input.validate()?;
    validate_exchange_key(key.as_deref())?;
    let timeout_ms = validate_exchange_timeout(timeout_ms)?;
    let pane_id = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map(|exchange| exchange.pane_id)
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let (mutation, operation) = match lock_delivery(daemon, &operation_lock, |state| {
        state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id && key.is_some() && exchange.continue_key == key)
    }) {
        DeliveryLocks::Held(mutation, operation) => (mutation, operation),
        DeliveryLocks::Repeat(exchange) => {
            return Ok(Response::AgentExchange {
                exchange: *exchange,
                frame: None,
            })
        }
    };
    let mut state = lock(&daemon.state);
    if state.handoff_in_progress {
        return Err(api("daemon_replacing", "wsxd live handoff is in progress"));
    }
    let previous = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    expect_revision(previous.revision, expected_revision)?;
    if !previous.can_continue()
        || matches!(
            previous.state,
            AgentExchangeState::Cancelled
                | AgentExchangeState::TargetExited
                | AgentExchangeState::TargetReplaced
        )
    {
        return Err(api(
            "invalid_exchange_state",
            "exchange cannot continue from its current state",
        ));
    }
    expect_runtime_generation(&state, pane_id, Some(&previous.runtime_generation))?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    if stash_draft {
        if state.stopping || state.handoff_in_progress {
            return Err(api(
                "daemon_unavailable",
                "daemon cannot stash a draft during shutdown or handoff",
            ));
        }
        let agent = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.agent.as_ref())
            .filter(|agent| agent.id == previous.agent_id && agent.attached)
            .ok_or_else(|| api("agent_unavailable", "exchange agent is no longer attached"))?;
        validate_draft_target(agent, &runtime)?;
    }
    let next_round = previous
        .round
        .checked_add(1)
        .ok_or_else(|| api("round_exhausted", "exchange round space exhausted"))?;
    take_terminal_lease(&mut state, pane_id, input.takeover)?;
    let revision = state.revision.saturating_add(1);
    let now = unix_time_millis();
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.round = next_round;
    exchange.state = AgentExchangeState::Submitted;
    exchange.evidence = AgentExchangeEvidence::IntentPersisted;
    // ^ A continuation cannot reuse the prior round's native input binding.
    exchange.delivery_sha256 = None;
    exchange.native_input_id = None;
    exchange.continue_key = key;
    exchange.command = input.command;
    exchange.updated_unix_ms = now;
    exchange.deadline_unix_ms = now.saturating_add(timeout_ms);
    exchange.delivery_revision = revision;
    exchange.revision = revision;
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.continued",
        id.0,
    )?;
    let text = input.terminal_text(id, next_round, previous.access);
    deliver_exchange_round(
        daemon,
        mutation,
        operation,
        state,
        id,
        next_round,
        &runtime,
        stash_draft,
        &text,
    )
}

/// The outcome of one round's PTY delivery. Only a successful delivery publishes the
/// round's input digest (docs/agent-orchestration.md); a failed one keeps its reason.
pub(crate) fn record_delivery(
    exchange: &mut AgentExchange,
    delivered: Result<(), ApiError>,
    delivery_sha256: String,
) {
    match delivered {
        Ok(()) => {
            exchange.delivery_sha256 = Some(delivery_sha256);
            exchange.failure = None;
            exchange.evidence = AgentExchangeEvidence::PtyDelivery;
            exchange.state = AgentExchangeState::Delivered;
        }
        Err(error) => {
            exchange.delivery_sha256 = None;
            exchange.failure = Some(format!("{}: {}", error.code, error.message));
            exchange.state = AgentExchangeState::DeliveryFailed;
        }
    }
}

pub(crate) fn expire_agent_exchange(
    daemon: &Daemon,
    state: &mut State,
    id: AgentExchangeId,
) -> Result<(), ApiError> {
    let now = unix_time_millis();
    let should_expire = state.persisted.agent_exchanges.iter().any(|exchange| {
        exchange.id == id && !exchange.is_terminal() && exchange.deadline_unix_ms <= now
    });
    if !should_expire {
        return Ok(());
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.state = AgentExchangeState::Expired;
    exchange.updated_unix_ms = now;
    exchange.revision = revision;
    commit_publish(
        daemon,
        state,
        persisted,
        revision,
        "agent_exchange.expired",
        id.0,
    )?;
    Ok(())
}

pub(crate) fn agent_exchange_response(
    daemon: &Daemon,
    exchange: AgentExchange,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let frame = if include_frame {
        let runtime = lock(&daemon.state).runtimes.get(&exchange.pane_id).cloned();
        runtime
            .filter(|runtime| !runtime.exited())
            .map(|runtime| runtime.frame().map_err(terminal_api))
            .transpose()?
    } else {
        None
    };
    Ok(Response::AgentExchange { exchange, frame })
}

pub(crate) fn get_agent_exchange(
    daemon: &Daemon,
    id: AgentExchangeId,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let exchange = {
        let mut state = lock(&daemon.state);
        expire_agent_exchange(daemon, &mut state, id)?;
        state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id)
            .cloned()
            .ok_or_else(|| api("not_found", "agent exchange not found"))?
    };
    agent_exchange_response(daemon, exchange, include_frame)
}

pub(crate) fn list_agent_exchanges(
    daemon: &Daemon,
    pane_id: Option<PaneId>,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    expire_due_agent_exchanges(daemon, &mut state)?;
    let exchanges = state
        .persisted
        .agent_exchanges
        .iter()
        .filter(|exchange| pane_id.is_none_or(|pane_id| exchange.pane_id == pane_id))
        .cloned()
        .collect();
    Ok(Response::AgentExchanges(exchanges))
}

pub(crate) fn wait_agent_exchange(
    daemon: &Daemon,
    id: AgentExchangeId,
    after_revision: u64,
    timeout_ms: u64,
    include_frame: bool,
) -> Result<Response, ApiError> {
    let timeout = Duration::from_millis(timeout_ms.min(30_000));
    let deadline = Instant::now() + timeout;
    {
        let _mutation = lock(&daemon.mutation_lock);
        expire_agent_exchange(daemon, &mut lock(&daemon.state), id)?;
    }
    let mut state = lock(&daemon.state);
    loop {
        let current = state
            .persisted
            .agent_exchanges
            .iter()
            .find(|exchange| exchange.id == id)
            .ok_or_else(|| api("not_found", "agent exchange not found"))?;
        if current.revision > after_revision || current.is_wait_boundary() {
            break;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        let (next_state, timeout_result) = daemon
            .changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next_state;
        if timeout_result.timed_out() {
            break;
        }
    }
    drop(state);
    {
        let _mutation = lock(&daemon.mutation_lock);
        expire_agent_exchange(daemon, &mut lock(&daemon.state), id)?;
    }
    let exchange = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    agent_exchange_response(daemon, exchange, include_frame)
}

pub(crate) fn report_agent_exchange_receipt(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    round: u32,
    runtime_generation: &str,
    receipt: AgentExchangeReceipt,
) -> Result<Response, ApiError> {
    report_agent_exchange_receipt_with_binding(daemon, id, round, runtime_generation, receipt, None)
}

pub(crate) fn report_agent_exchange_receipt_with_binding(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    round: u32,
    runtime_generation: &str,
    receipt: AgentExchangeReceipt,
    binding: Option<(PaneId, &str, &str)>,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    // ^ docs/agent-orchestration.md: receipts cannot outlive an active request deadline.
    expire_agent_exchange(daemon, &mut state, id)?;
    let previous = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    if previous.runtime_generation != runtime_generation {
        return Err(api(
            "stale_runtime",
            "agent exchange receipt has a stale runtime generation",
        ));
    }
    if previous.round != round {
        return Err(api(
            "stale_exchange_round",
            "agent exchange receipt has a stale round",
        ));
    }
    let supports_receipts = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == previous.pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.id == previous.agent_id && agent.attached && agent.capabilities.exchange_receipts
        });
    if !supports_receipts {
        return Err(api(
            "unsupported_exchange_receipt",
            "bound agent did not advertise request-bound exchange receipts",
        ));
    }
    // ^ docs/agent-orchestration.md: only an exact delivered round may bind native input.
    if let Some((pane_id, digest, input_id)) = binding {
        expect_runtime_generation(&state, previous.pane_id, Some(runtime_generation))?;
        if pane_id != previous.pane_id
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || previous.delivery_sha256.as_deref() != Some(digest)
            || input_id.is_empty()
            || input_id.len() > 128
            || !input_id.bytes().all(|byte| byte.is_ascii_graphic())
            || previous
                .native_input_id
                .as_deref()
                .is_some_and(|id| id != input_id)
        {
            return Err(api(
                "invalid_delivery_binding",
                "receipt does not match the exact delivered native input",
            ));
        }
        if previous.state != AgentExchangeState::Completed
            && previous.deadline_unix_ms <= unix_time_millis()
        {
            return Err(api(
                "receipt_deadline_exceeded",
                "native receipt arrived after its request deadline",
            ));
        }
        if receipt == AgentExchangeReceipt::Completed && previous.native_input_id.is_none() {
            return Err(api(
                "native_input_unaccepted",
                "native completion requires a prior bound acceptance",
            ));
        }
    } else if previous.native_input_id.is_some() {
        return Err(api(
            "bound_receipt_required",
            "native-bound input requires a bound receipt",
        ));
    }
    let next_state = match receipt {
        AgentExchangeReceipt::Accepted
            if matches!(
                previous.state,
                AgentExchangeState::Submitted
                    | AgentExchangeState::Delivered
                    | AgentExchangeState::Accepted
                    | AgentExchangeState::WorkingObserved
                    | AgentExchangeState::BlockedObserved
            ) =>
        {
            if matches!(
                previous.state,
                AgentExchangeState::WorkingObserved | AgentExchangeState::BlockedObserved
            ) {
                previous.state
            } else {
                AgentExchangeState::Accepted
            }
        }
        AgentExchangeReceipt::Completed
            if matches!(
                previous.state,
                AgentExchangeState::Submitted
                    | AgentExchangeState::Delivered
                    | AgentExchangeState::Accepted
                    | AgentExchangeState::WorkingObserved
                    | AgentExchangeState::BlockedObserved
                    | AgentExchangeState::DoneObserved
                    | AgentExchangeState::CancelRequested
                    | AgentExchangeState::Completed
            ) =>
        {
            AgentExchangeState::Completed
        }
        _ => {
            return Err(api(
                "invalid_exchange_state",
                "exchange cannot accept a request-bound receipt in its current state",
            ))
        }
    };
    if previous.state == next_state
        && previous.evidence == AgentExchangeEvidence::RequestBound
        && binding
            .is_none_or(|(_, _, input_id)| previous.native_input_id.as_deref() == Some(input_id))
    {
        return Ok(Response::AgentExchange {
            exchange: previous,
            frame: None,
        });
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .expect("validated exchange remains present");
    exchange.state = next_state;
    exchange.evidence = AgentExchangeEvidence::RequestBound;
    if let Some((_, _, input_id)) = binding {
        exchange.native_input_id = Some(input_id.to_owned());
    }
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.receipt",
        id.0,
    )?;
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}

/// docs/agent-orchestration.md "Steer": a working Claude takes a message typed into its input
/// line at its next step, fires `UserPromptSubmit` for it at once and saves it as a
/// `queued_command` (Claude Code 2.1.309). No exchange: the turn that runs keeps its round.
pub(crate) fn steer_agent(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    text: &str,
    takeover: bool,
) -> Result<Response, ApiError> {
    if text.trim().is_empty() || text.len() > MAX_AGENT_EXCHANGE_PROMPT_BYTES {
        return Err(api(
            "invalid_request",
            "a steer message must have text and fit an exchange prompt",
        ));
    }
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let runtime = {
        let mut state = lock(&daemon.state);
        let agent = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .ok_or_else(|| api("not_found", "pane not found"))?
            .agent
            .clone()
            .filter(|agent| agent.attached)
            .ok_or_else(|| api("agent_unavailable", "pane has no attached agent"))?;
        if agent.provider != "claude" {
            return Err(api(
                "steer_unsupported",
                "only Claude takes a message during its turn",
            ));
        }
        if agent.state != AgentState::Working {
            return Err(api("agent_not_working", "the agent is not at work"));
        }
        // ^ A round still waiting to be taken would read this prompt's receipt as its own
        // refusal (receive_prompt).
        if state.persisted.agent_exchanges.iter().any(|exchange| {
            exchange.pane_id == pane_id
                && matches!(
                    exchange.state,
                    AgentExchangeState::Submitted | AgentExchangeState::Delivered
                )
        }) {
            return Err(api("exchange_busy", "a delivered message is not taken yet"));
        }
        let runtime = state
            .runtimes
            .get(&pane_id)
            .filter(|runtime| !runtime.exited())
            .cloned()
            .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
        let observation = runtime
            .agent_observation()
            .map_err(|_| api("editor_unavailable", "cannot observe Claude's input line"))?;
        // ^ Typed text in the input line would be sent with this message.
        if claude_input(&observation) != Some("") {
            return Err(api(
                "editor_unavailable",
                "Claude's input line is not empty",
            ));
        }
        take_terminal_lease(&mut state, pane_id, takeover)?;
        runtime
    };
    deliver_agent_exchange(&runtime, text, Instant::now() + AGENT_EXCHANGE_WRITE_LIMIT)
        .map_err(terminal_api)?;
    let revision = lock(&daemon.state).revision;
    Ok(Response::Ack { revision })
}

pub(crate) fn cancel_agent_exchange(
    daemon: &Arc<Daemon>,
    id: AgentExchangeId,
    expected_revision: u64,
    takeover: bool,
) -> Result<Response, ApiError> {
    let pane_id = lock(&daemon.state)
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .map(|exchange| exchange.pane_id)
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    let operation_lock = terminal_operation_lock(daemon, pane_id);
    let _operation = lock(&operation_lock);
    let mut state = lock(&daemon.state);
    let exchange = state
        .persisted
        .agent_exchanges
        .iter()
        .find(|exchange| exchange.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "agent exchange not found"))?;
    expect_revision(exchange.revision, expected_revision)?;
    if exchange.is_terminal() {
        return Err(api(
            "invalid_exchange_state",
            "exchange is already terminal",
        ));
    }
    expect_runtime_generation(&state, pane_id, Some(&exchange.runtime_generation))?;
    let runtime = state
        .runtimes
        .get(&pane_id)
        .filter(|runtime| !runtime.exited())
        .cloned()
        .ok_or_else(|| api("terminal_unavailable", "pane has no live terminal"))?;
    // ^ docs/agent-orchestration.md "Cancel": the provider's own interrupt. Escape ends a
    // Claude, Pi or omp turn and leaves the agent ready. Ctrl+C does not stop a working
    // Pi, and in Claude its second press quits. Both were checked against the real
    // programs on 2026-10-12. Other providers get Ctrl+C.
    let escape = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .is_some_and(|agent| {
            agent.attached
                && (agent.capabilities.escape_interrupts
                    || matches!(agent.provider.as_str(), "pi" | "omp"))
        });
    take_terminal_lease(&mut state, pane_id, takeover)?;
    runtime
        .write(if escape { b"\x1b" } else { b"\x03" })
        .map_err(terminal_api)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let exchange = persisted
        .agent_exchanges
        .iter_mut()
        .find(|exchange| exchange.id == id)
        .unwrap();
    exchange.state = AgentExchangeState::CancelRequested;
    exchange.updated_unix_ms = unix_time_millis();
    exchange.revision = revision;
    let response = exchange.clone();
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "agent_exchange.cancel_requested",
        id.0,
    )?;
    drop(state);
    // Claude has no interrupt hook: only its screen shows that the turn ended. Without
    // this the agent stays working here and the exchange stays in cancel_requested.
    request_claude_reconcile(daemon, pane_id, true);
    Ok(Response::AgentExchange {
        exchange: response,
        frame: None,
    })
}
