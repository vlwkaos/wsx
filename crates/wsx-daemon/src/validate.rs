//! Bounds and shape checks for client-supplied values and persisted state.

use super::*;

pub(crate) fn bounded_label(value: String) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        Err(api("invalid_label", "label must be 1..128 bytes"))
    } else {
        Ok(value.into())
    }
}

/// A non-empty value of at most `max` bytes, every byte accepted by `allowed`.
pub(crate) fn bounded_ascii(
    value: String,
    max: usize,
    allowed: impl Fn(u8) -> bool,
    code: &str,
    message: &str,
) -> Result<String, ApiError> {
    if value.is_empty() || value.len() > max || !value.bytes().all(allowed) {
        Err(api(code, message))
    } else {
        Ok(value)
    }
}

pub(crate) fn bounded_provider(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value.trim().to_ascii_lowercase(),
        64,
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'),
        "invalid_provider",
        "invalid provider name",
    )
}

pub(crate) fn bounded_wake_token(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value,
        128,
        |byte| byte.is_ascii_alphanumeric() || byte == b'-',
        "invalid_wake_token",
        "wake token is malformed",
    )
}

pub(crate) fn validate_presence_id(value: String) -> Result<String, ApiError> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || bytes.iter().enumerate().any(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte != b'-'
            } else {
                !byte.is_ascii_hexdigit()
            }
        })
    {
        Err(api("invalid_presence", "presence ID must be a UUID"))
    } else {
        Ok(value)
    }
}

pub(crate) fn bounded_runtime_generation(value: String) -> Result<String, ApiError> {
    bounded_ascii(
        value,
        128,
        |byte| byte.is_ascii_hexdigit() || byte == b':',
        "invalid_runtime_generation",
        "runtime generation is malformed",
    )
}

pub(crate) fn validated_session_ref(
    session_ref: Option<AgentSessionRef>,
) -> Result<Option<AgentSessionRef>, ApiError> {
    let Some(session_ref) = session_ref else {
        return Ok(None);
    };
    let transcript_path = session_ref.transcript_path;
    let validated = match session_ref.kind {
        AgentSessionRefKind::Id => AgentSessionRef::id(session_ref.value),
        AgentSessionRefKind::Path => AgentSessionRef::path(session_ref.value),
    }
    .ok_or_else(|| api("invalid_agent_session", "invalid agent session reference"))?;
    let validated = if let Some(path) = transcript_path {
        validated
            .with_transcript_path(
                path.to_str()
                    .ok_or_else(|| api("invalid_agent_session", "invalid transcript path"))?
                    .to_owned(),
            )
            .ok_or_else(|| api("invalid_agent_session", "invalid transcript path"))?
    } else {
        validated
    };
    Ok(Some(validated))
}

pub(crate) fn truncate_utf8(mut value: String, max: usize) -> String {
    if value.len() > max {
        let boundary = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= max)
            .last()
            .unwrap_or(0);
        value.truncate(boundary);
    }
    value
}

pub(crate) fn normalize_command(command: Vec<String>) -> Vec<String> {
    if command.is_empty() {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
    } else {
        command
    }
}

pub(crate) fn startup_input(recipe: &LaunchRecipe) -> Option<Vec<u8>> {
    recipe.initial_input.as_ref().map(|input| {
        let mut bytes = input.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    })
}

pub(crate) fn validate_recipe(recipe: &LaunchRecipe) -> Result<(), String> {
    let startup = startup_input(recipe);
    validate_launch(
        recipe.rows,
        recipe.cols,
        &recipe.command,
        startup.as_deref(),
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn launch_recipe(
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<LaunchRecipe, ApiError> {
    let recipe = LaunchRecipe {
        command: normalize_command(command),
        initial_input,
        rows,
        cols,
    };
    validate_recipe(&recipe).map_err(|error| api("invalid_launch", error))?;
    Ok(recipe)
}

pub(crate) fn default_launch_recipe() -> LaunchRecipe {
    launch_recipe(Vec::new(), None, 24, 80).unwrap_or_else(|_| LaunchRecipe {
        command: vec!["/bin/sh".into()],
        initial_input: None,
        rows: 24,
        cols: 80,
    })
}

pub(crate) fn validate_persisted(state: &Persisted) -> io::Result<()> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "inconsistent wsx state");
    let mut ids = HashSet::new();
    let mut max_id = 0;
    let mut insert = |id: u64| {
        max_id = max_id.max(id);
        id > 0 && ids.insert(id)
    };
    for project in &state.projects {
        if !insert(project.id.0) {
            return Err(invalid());
        }
    }
    for worktree in &state.worktrees {
        if !insert(worktree.id.0)
            || !state
                .projects
                .iter()
                .any(|project| project.id == worktree.project_id)
        {
            return Err(invalid());
        }
    }
    for session in &state.sessions {
        if !insert(session.id.0)
            || session.panes.is_empty()
            || !session.panes.contains(&session.primary_pane)
            || !session.panes.contains(&session.focused_pane)
            || !state
                .worktrees
                .iter()
                .any(|worktree| worktree.id == session.worktree_id)
        {
            return Err(invalid());
        }
        let mut layout = Vec::new();
        session.layout.panes(&mut layout);
        let declared = session.panes.iter().copied().collect::<HashSet<_>>();
        let arranged = layout.iter().copied().collect::<HashSet<_>>();
        if declared.len() != session.panes.len()
            || arranged.len() != layout.len()
            || declared != arranged
        {
            return Err(invalid());
        }
    }
    for pane in &state.panes {
        if !insert(pane.id.0)
            || !insert(pane.terminal_id.0)
            || pane.agent.as_ref().is_some_and(|agent| !insert(agent.id.0))
            || !state
                .sessions
                .iter()
                .any(|session| session.id == pane.session_id && session.panes.contains(&pane.id))
        {
            return Err(invalid());
        }
    }
    if state.agent_exchanges.len() > MAX_AGENT_EXCHANGES {
        return Err(invalid());
    }
    for exchange in &state.agent_exchanges {
        if !insert(exchange.id.0)
            || exchange.round == 0
            || exchange.runtime_generation.is_empty()
            || exchange.deadline_unix_ms < exchange.created_unix_ms
            || (exchange.access == AgentExchangeAccess::ReadOnly
                && !exchange.write_claims.is_empty())
            || exchange
                .write_claims
                .iter()
                .any(|claim| !claim.is_absolute())
        {
            return Err(invalid());
        }
        if !exchange.is_terminal() {
            let Some(pane) = state.panes.iter().find(|pane| pane.id == exchange.pane_id) else {
                return Err(invalid());
            };
            let Some(session) = state
                .sessions
                .iter()
                .find(|session| session.id == exchange.session_id)
            else {
                return Err(invalid());
            };
            let Some(worktree) = state
                .worktrees
                .iter()
                .find(|worktree| worktree.id == exchange.worktree_id)
            else {
                return Err(invalid());
            };
            if pane.session_id != session.id
                || session.worktree_id != worktree.id
                || worktree.project_id != exchange.project_id
                || pane.agent.as_ref().map(|agent| agent.id) != Some(exchange.agent_id)
                || exchange
                    .write_claims
                    .iter()
                    .any(|claim| !claim.starts_with(&worktree.path))
            {
                return Err(invalid());
            }
        }
    }
    if state.sessions.iter().any(|session| {
        session.panes.iter().any(|id| {
            !state
                .panes
                .iter()
                .any(|pane| pane.id == *id && pane.session_id == session.id)
        })
    }) || state.next_id == 0
        || state.next_id <= max_id
    {
        return Err(invalid());
    }
    Ok(())
}
