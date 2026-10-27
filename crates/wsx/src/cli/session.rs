use super::*;

#[derive(Clone, Copy, Default)]
pub(super) struct RuntimeSessionScope {
    pub(super) project_id: Option<runtime::ProjectId>,
    pub(super) worktree_id: Option<runtime::WorktreeId>,
}

#[derive(Debug)]
pub(super) struct ResolvedSession {
    pub(super) session_id: runtime::SessionId,
    pub(super) pane_id: runtime::PaneId,
    pub(super) label: String,
}

pub(super) fn runtime_session_scope(
    snapshot: &runtime::Snapshot,
    requested: &SessionScope,
    config: Option<&GlobalConfig>,
) -> Result<RuntimeSessionScope> {
    let project_id = requested
        .project
        .as_deref()
        .map(|name| {
            snapshot
                .projects
                .iter()
                .find(|project| project.name == name)
                .map(|project| project.id)
                .ok_or_else(|| anyhow::anyhow!("project '{}' not found", name))
        })
        .transpose()?;
    let worktree_id = requested
        .worktree
        .as_deref()
        .map(|selector| {
            let matches = snapshot
                .worktrees
                .iter()
                .filter(|worktree| project_id.is_none_or(|id| worktree.project_id == id))
                .filter(|worktree| {
                    let alias = config.and_then(|config| {
                        let project = snapshot
                            .projects
                            .iter()
                            .find(|project| project.id == worktree.project_id)?;
                        config
                            .projects
                            .iter()
                            .find(|entry| entry.path == project.path)?
                            .aliases
                            .get(&worktree.branch)
                    });
                    worktree.branch == selector
                        || worktree.path == Path::new(selector)
                        || alias.is_some_and(|alias| alias == selector)
                })
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [worktree] => Ok(worktree.id),
                [] => bail!("worktree '{}' not found in requested scope", selector),
                _ => bail!("worktree '{}' is ambiguous; add --project", selector),
            }
        })
        .transpose()?;
    Ok(RuntimeSessionScope {
        project_id,
        worktree_id,
    })
}

/// The requested scope in one snapshot. Worktree aliases need the configuration.
pub(super) fn requested_runtime_scope(
    snapshot: &runtime::Snapshot,
    requested: &SessionScope,
) -> Result<RuntimeSessionScope> {
    let config = requested
        .worktree
        .as_ref()
        .map(|_| load_config())
        .transpose()?;
    runtime_session_scope(snapshot, requested, config.as_ref())
}

pub(super) fn session_matches_scope(
    snapshot: &runtime::Snapshot,
    session: &runtime::Session,
    scope: RuntimeSessionScope,
) -> bool {
    let Some(worktree) = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
    else {
        return false;
    };
    scope
        .project_id
        .is_none_or(|project_id| worktree.project_id == project_id)
        && scope
            .worktree_id
            .is_none_or(|worktree_id| worktree.id == worktree_id)
}

pub(super) fn resolve_session_in_snapshot(
    snapshot: &runtime::Snapshot,
    selector: &str,
    scope: RuntimeSessionScope,
) -> Result<ResolvedSession> {
    if let Ok(session_id) = selector.parse::<runtime::SessionId>() {
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            if !session_matches_scope(snapshot, session, scope) {
                bail!("session {} is outside the requested scope", session_id);
            }
            return Ok(ResolvedSession {
                session_id,
                pane_id: session.focused_pane,
                label: session.label.clone(),
            });
        }
    }
    if let Ok(pane_id) = selector.parse::<runtime::PaneId>() {
        if let Some(pane) = snapshot.panes.iter().find(|pane| pane.id == pane_id) {
            let session = snapshot
                .sessions
                .iter()
                .find(|session| session.id == pane.session_id)
                .context("pane session is unavailable")?;
            if !session_matches_scope(snapshot, session, scope) {
                bail!("pane {} is outside the requested scope", pane_id);
            }
            return Ok(ResolvedSession {
                session_id: session.id,
                pane_id,
                label: session.label.clone(),
            });
        }
    }
    let matches = snapshot
        .sessions
        .iter()
        .filter(|session| session.label == selector)
        .filter(|session| session_matches_scope(snapshot, session, scope))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok(ResolvedSession {
            session_id: session.id,
            pane_id: session.focused_pane,
            label: session.label.clone(),
        }),
        [] => bail!("session or pane not found in requested scope: {selector}"),
        _ => bail!(
            "session label is ambiguous: {selector}; add --project/--worktree or use a numeric ID"
        ),
    }
}

pub(super) fn resolve_session(selector: &str, requested: &SessionScope) -> Result<ResolvedSession> {
    let snapshot = ops::runtime_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    resolve_session_in_snapshot(&snapshot, selector, scope)
}

pub(super) fn resolve_pane(selector: &str, scope: &SessionScope) -> Result<runtime::PaneId> {
    Ok(resolve_session(selector, scope)?.pane_id)
}

pub(super) fn send_pane_bytes(pane_id: runtime::PaneId, bytes: Vec<u8>) -> Result<()> {
    let client = runtime::Client::local();
    let client_id = runtime::new_client_id();
    expect_ack(
        client.call(&runtime::Request::TerminalAcquire {
            pane_id,
            client_id,
            takeover: false,
        })?,
        "wsxd returned an unexpected lease response",
    )?;
    let result = expect_ack(
        client.call(&runtime::Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        })?,
        "wsxd returned an unexpected input response",
    )
    .map(|_| ());
    let _ = client.call(&runtime::Request::TerminalRelease { pane_id, client_id });
    result
}

pub(super) fn cmd_session_create(
    scope: SessionScope,
    name: Option<String>,
    command: Option<String>,
    json: bool,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let cwd = std::env::current_dir().ok();
    let (project, worktree) = resolve_creation_worktree(&workspace, &scope, cwd.as_deref())?;
    let (session_id, label) = ops::create_session(
        &config,
        &project.name,
        &worktree.session_slug(&project.name),
        &worktree.path,
        name,
        command,
    )?;
    if json {
        let (pane_id, agent) = created_session_pane(session_id);
        print_json(&serde_json::json!({
            "session_id": session_id,
            "pane_id": pane_id,
            "agent": agent,
            "label": label,
            "project": project.name,
            "worktree": worktree.path,
            "branch": worktree.branch,
        }))?;
    } else {
        println!("{}", created_session_line(session_id));
        println!("label:    {label}");
        println!("worktree: {}", worktree.path.display());
    }
    Ok(())
}

pub(super) fn cmd_session_current(json: bool) -> Result<()> {
    let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .filter(|id| !id.is_empty())
        .context("not inside a wsx-managed pane: WSX_PANE_ID is not set")?
        .parse::<runtime::PaneId>()
        .map_err(|_| anyhow::anyhow!("WSX_PANE_ID is not a valid pane ID"))?;
    let snapshot = existing_daemon_snapshot()?;
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} is not present in wsxd"))?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id)
        .context("pane session missing")?;
    let worktree = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .context("pane worktree missing")?;
    let project = snapshot
        .projects
        .iter()
        .find(|project| project.id == worktree.project_id)
        .context("pane project missing")?;
    let agent = pane.agent.as_ref().filter(|agent| agent.attached);
    if json {
        print_json(&serde_json::json!({
            "pane_id": pane.id,
            "session_id": session.id,
            "label": session.label,
            "project": project.name,
            "worktree": worktree.path,
            "branch": worktree.branch,
            "agent": agent.map(|agent| &agent.provider),
            "agent_status": agent.map(|agent| agent.state),
        }))?;
    } else {
        println!("pane:     {}", pane.id);
        println!("{}", created_session_line(session.id));
        println!("label:    {}", session.label);
        println!("project:  {}", project.name);
        println!("worktree: {}", worktree.path.display());
        println!("branch:   {}", worktree.branch);
        if let Some(agent) = agent {
            println!(
                "agent:    {} ({})",
                agent.provider,
                format!("{:?}", agent.state).to_lowercase()
            );
        }
    }
    Ok(())
}

pub(super) fn cmd_session_delete(selector: &str, scope: &SessionScope, json: bool) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    ops::kill_session(session.session_id)?;
    if json {
        print_json(&serde_json::json!({
            "deleted": true,
            "session_id": session.session_id,
            "label": session.label,
        }))?;
    } else {
        println!(
            "deleted session: {} ({})",
            session.session_id, session.label
        );
    }
    Ok(())
}

pub(super) fn cmd_session_restart(selector: &str, json: bool, scope: &SessionScope) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    let revision = ops::restart_session(session.pane_id)?;
    if json {
        print_json(&serde_json::json!({
            "restarted": true,
            "session_id": session.session_id,
            "pane_id": session.pane_id,
            "label": session.label,
            "revision": revision,
        }))?;
    } else {
        println!(
            "restarted session: {} ({})",
            session.session_id, session.label
        );
    }
    Ok(())
}

pub(super) fn cmd_session_send_text(
    selector: &str,
    text: &str,
    no_enter: bool,
    scope: &SessionScope,
) -> Result<()> {
    let pane_id = resolve_pane(selector, scope)?;
    let mut bytes = text.as_bytes().to_vec();
    if !no_enter {
        bytes.push(b'\r');
    }
    send_pane_bytes(pane_id, bytes)
}

pub(super) fn cmd_session_prompt(
    selector: &str,
    prompt: PromptSource,
    scope: &SessionScope,
) -> Result<()> {
    // ^ wsxd accepts 64 KiB of terminal input, and Enter is one byte of it.
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES - 1)?;
    cmd_session_send_text(selector, &prompt, false, scope)
}

// ^ [[Session Peek]] Reads are bounded projections of authoritative semantic frames.
pub(super) fn cmd_session_peek(
    selector: &str,
    lines: Option<u32>,
    trim: bool,
    agent: bool,
    scope: &SessionScope,
) -> Result<()> {
    let frame = pane_frame(resolve_pane(selector, scope)?)?;
    let mut rendered = frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>();
    let requested = lines.unwrap_or(u32::from(frame.rows));
    let keep_end = rendered.len();
    let keep_start = keep_end.saturating_sub(requested as usize);
    rendered = rendered[keep_start..keep_end].to_vec();
    let mut output = rendered.join("\n");
    if trim || agent {
        output = output.trim_end().to_string();
    }
    print!("{}", output);
    Ok(())
}

pub(super) fn cmd_session_rename(
    selector: &str,
    new_label: &str,
    scope: &SessionScope,
) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    ops::rename_session(session.session_id, new_label)?;
    println!("renamed session {} to '{}'", session.session_id, new_label);
    Ok(())
}

pub(super) fn cmd_session_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let mut sessions = Vec::new();
        for project in &projects {
            for worktree in &project.worktrees {
                for session in &worktree.sessions {
                    // ^ Added keys only: a row says where it is without a second call.
                    let mut row = serde_json::to_value(session)?;
                    row["project"] = serde_json::json!(project.name);
                    row["branch"] = serde_json::json!(worktree.branch);
                    row["worktree"] = serde_json::json!(worktree.path);
                    sessions.push(row);
                }
            }
        }
        print_json(&sessions)?;
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "sessions"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees
                    .iter()
                    .map(move |wt| vec![p.name.clone(), wt.branch.clone(), sessions_inline(wt)])
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            for s in &wt.sessions {
                println!(
                    "{}/{} — {} ({})",
                    p.name,
                    wt.branch,
                    s.pane_id,
                    activity_label(s)
                );
            }
        }
    }
    Ok(())
}
