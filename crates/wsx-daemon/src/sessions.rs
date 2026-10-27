//! Project, session and pane structure, and the PTY runtime of a pane.

use super::*;

pub(crate) fn synchronize_projects(
    daemon: &Arc<Daemon>,
    specs: Vec<ProjectSpec>,
) -> Result<Response, ApiError> {
    if specs.len() > 256 {
        return Err(api("invalid_projects", "too many projects"));
    }
    let mut canonical = Vec::with_capacity(specs.len());
    let mut seen_projects = HashSet::new();
    let mut seen_worktrees = HashSet::new();
    for spec in specs {
        let project_path = fs::canonicalize(&spec.path).map_err(io_api)?;
        if !seen_projects.insert(project_path.clone()) {
            return Err(api("duplicate_project", "duplicate project path"));
        }
        let name = bounded_label(spec.name)?;
        let mut worktrees = Vec::new();
        for worktree in spec.worktrees {
            let path = match fs::canonicalize(&worktree.path) {
                Ok(path) => path,
                // ^ Removed after the client listed it, as a task that closes itself removes its
                // copy: it is gone, like a worktree left out of the list.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_api(error)),
            };
            if !seen_worktrees.insert(path.clone()) {
                return Err(api("duplicate_worktree", "duplicate worktree path"));
            }
            worktrees.push((path, bounded_label(worktree.branch)?));
        }
        canonical.push((project_path, name, worktrees));
    }

    let mut state = lock(&daemon.state);
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let mut retained_projects = Vec::new();
    let mut retained_worktrees = Vec::new();
    for (path, name, worktrees) in canonical {
        let project_id = match persisted
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id)
        {
            Some(id) => id,
            None => ProjectId(next_id(&mut persisted)?),
        };
        retained_projects.push(project_id);
        if let Some(project) = persisted
            .projects
            .iter_mut()
            .find(|project| project.id == project_id)
        {
            project.name = name;
            project.revision = revision;
        } else {
            persisted.projects.push(Project {
                id: project_id,
                path,
                name,
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                revision,
            });
        }
        for (path, branch) in worktrees {
            let worktree_id = match persisted
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .map(|worktree| worktree.id)
            {
                Some(id) => id,
                None => WorktreeId(next_id(&mut persisted)?),
            };
            retained_worktrees.push(worktree_id);
            if let Some(worktree) = persisted
                .worktrees
                .iter_mut()
                .find(|worktree| worktree.id == worktree_id)
            {
                worktree.project_id = project_id;
                worktree.branch = branch;
                worktree.revision = revision;
            } else {
                persisted.worktrees.push(Worktree {
                    id: worktree_id,
                    project_id,
                    path,
                    branch,
                    revision,
                });
            }
        }
    }
    let session_worktrees = persisted
        .sessions
        .iter()
        .map(|session| session.worktree_id)
        .collect::<HashSet<_>>();
    for project_id in persisted
        .worktrees
        .iter()
        .filter(|worktree| session_worktrees.contains(&worktree.id))
        .map(|worktree| worktree.project_id)
    {
        if !retained_projects.contains(&project_id) {
            retained_projects.push(project_id);
        }
    }
    persisted
        .projects
        .retain(|project| retained_projects.contains(&project.id));
    persisted.worktrees.retain(|worktree| {
        retained_worktrees.contains(&worktree.id) || session_worktrees.contains(&worktree.id)
    });
    commit_publish(
        daemon,
        &mut state,
        persisted,
        revision,
        "projects.synchronized",
        0,
    )?;
    Ok(Response::Ack { revision })
}

pub(crate) fn create_session(
    daemon: &Arc<Daemon>,
    worktree_id: WorktreeId,
    label: String,
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
) -> Result<Response, ApiError> {
    let label = bounded_label(label)?;
    let recipe = launch_recipe(command, initial_input, rows, cols)?;
    let (cwd, session_id, pane_id, terminal_id, revision) = {
        let mut state = lock(&daemon.state);
        if state.stopping {
            return Err(api("daemon_stopping", "wsxd is stopping"));
        }
        let mut persisted = state.persisted.clone();
        let worktree = persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or_else(|| api("not_found", "worktree not found"))?;
        let cwd = worktree.path.clone();
        let project_id = worktree.project_id;
        let project_index = persisted
            .projects
            .iter()
            .position(|project| project.id == project_id)
            .ok_or_else(|| api("not_found", "project not found"))?;
        let session_id = SessionId(next_id(&mut persisted)?);
        let pane_id = PaneId(next_id(&mut persisted)?);
        let terminal_id = TerminalId(next_id(&mut persisted)?);
        let revision = state.revision.saturating_add(1);
        persisted.projects[project_index].last_terminal_active_unix_ms = Some(unix_time_millis());
        persisted.projects[project_index].revision = revision;
        persisted.panes.push(PersistedPane {
            pane: Pane {
                id: pane_id,
                terminal_id,
                session_id,
                label: "terminal".into(),
                agent: None,
                exited: true,
                revision,
            },
            recovery: Some(recipe.clone()),
            recovery_quarantined: false,
        });
        persisted.sessions.push(Session {
            id: session_id,
            worktree_id,
            label,
            primary_pane: pane_id,
            focused_pane: pane_id,
            panes: vec![pane_id],
            layout: PaneLayout::Leaf { pane_id },
            revision,
        });
        commit_publish(
            daemon,
            &mut state,
            persisted,
            revision,
            "session.created",
            session_id.0,
        )?;
        (cwd, session_id, pane_id, terminal_id, revision)
    };
    let runtime = spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe)?;
    let mut state = lock(&daemon.state);
    if state.stopping
        || !state
            .persisted
            .worktrees
            .iter()
            .any(|worktree| worktree.id == worktree_id)
    {
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "worktree changed while terminal was starting",
        ));
    }
    let runtime = Arc::new(runtime);
    if !runtime.exited() {
        let mut persisted = state.persisted.clone();
        if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
            pane.exited = false;
        }
        if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
            drop(state);
            runtime.terminate();
            return Err(error);
        }
    }
    state.runtimes.insert(pane_id, Arc::clone(&runtime));
    if runtime.exited() {
        record_terminal_exit(daemon, &mut state, pane_id);
    }
    Ok(Response::Created {
        revision,
        id: session_id.0,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn split_pane(
    daemon: &Arc<Daemon>,
    session_id: SessionId,
    target: PaneId,
    axis: SplitAxis,
    label: String,
    command: Vec<String>,
    initial_input: Option<String>,
    rows: u16,
    cols: u16,
    expected: u64,
) -> Result<Response, ApiError> {
    let label = bounded_label(label)?;
    let recipe = launch_recipe(command, initial_input, rows, cols)?;
    let (cwd, pane_id, terminal_id) = {
        let mut state = lock(&daemon.state);
        if state.stopping {
            return Err(api("daemon_stopping", "wsxd is stopping"));
        }
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| api("not_found", "session not found"))?;
        expect_revision(session.revision, expected)?;
        if !session.panes.contains(&target) {
            return Err(api("not_found", "target pane is not in session"));
        }
        let worktree_id = session.worktree_id;
        let cwd = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == worktree_id)
            .ok_or_else(|| api("not_found", "worktree not found"))?
            .path
            .clone();
        let mut persisted = state.persisted.clone();
        let pane_id = PaneId(next_id(&mut persisted)?);
        let terminal_id = TerminalId(next_id(&mut persisted)?);
        commit_persisted(daemon, &mut state, persisted)?;
        (cwd, pane_id, terminal_id)
    };
    let runtime = Arc::new(spawn_runtime(daemon, pane_id, terminal_id, &cwd, &recipe)?);
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let Some(index) = persisted
        .sessions
        .iter()
        .position(|session| session.id == session_id)
    else {
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "session closed while terminal was starting",
        ));
    };
    if state.stopping
        || persisted.sessions[index].revision != expected
        || !persisted.sessions[index].panes.contains(&target)
    {
        drop(state);
        runtime.terminate();
        return Err(api(
            "revision_conflict",
            "session changed while terminal was starting",
        ));
    }
    if !persisted.sessions[index]
        .layout
        .split(target, pane_id, axis)
    {
        drop(state);
        runtime.terminate();
        return Err(api("invalid_layout", "target pane is absent from layout"));
    }
    let revision = state.revision.saturating_add(1);
    let session = &mut persisted.sessions[index];
    session.panes.push(pane_id);
    session.focused_pane = pane_id;
    session.revision = revision;
    persisted.panes.push(PersistedPane {
        pane: Pane {
            id: pane_id,
            terminal_id,
            session_id,
            label,
            agent: None,
            exited: runtime.exited(),
            revision,
        },
        recovery: Some(recipe),
        recovery_quarantined: false,
    });
    if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
        drop(state);
        runtime.terminate();
        return Err(error);
    }
    state.runtimes.insert(pane_id, Arc::clone(&runtime));
    publish(daemon, &mut state, revision, "pane.created", pane_id.0);
    if runtime.exited() {
        record_terminal_exit(daemon, &mut state, pane_id);
    }
    Ok(Response::Created {
        revision,
        id: pane_id.0,
    })
}

pub(crate) fn reorder_session(
    daemon: &Arc<Daemon>,
    session_id: SessionId,
    target_session_id: SessionId,
    placement: SessionPlacement,
    expected_revision: u64,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    reorder_session_state(
        &mut persisted,
        session_id,
        target_session_id,
        placement,
        expected_revision,
    )?;
    let revision = state.revision.saturating_add(1);
    persisted
        .sessions
        .iter_mut()
        .find(|session| session.id == session_id)
        .expect("reordered session must remain present")
        .revision = revision;
    commit_persisted(daemon, &mut state, persisted)?;
    let bumped = bump(daemon, &mut state, "session.reordered", session_id.0);
    debug_assert_eq!(bumped, revision);
    Ok(Response::Ack { revision })
}

pub(crate) fn reorder_session_state(
    persisted: &mut Persisted,
    session_id: SessionId,
    target_session_id: SessionId,
    placement: SessionPlacement,
    expected_revision: u64,
) -> Result<(), ApiError> {
    if session_id == target_session_id {
        return Err(api(
            "invalid_target",
            "session cannot be reordered relative to itself",
        ));
    }
    let source_index = persisted
        .sessions
        .iter()
        .position(|session| session.id == session_id)
        .ok_or_else(|| api("not_found", "session not found"))?;
    let target = persisted
        .sessions
        .iter()
        .find(|session| session.id == target_session_id)
        .ok_or_else(|| api("not_found", "target session not found"))?;
    let source = &persisted.sessions[source_index];
    expect_revision(source.revision, expected_revision)?;
    if source.worktree_id != target.worktree_id {
        return Err(api(
            "invalid_target",
            "sessions can only be reordered within one worktree",
        ));
    }

    let source = persisted.sessions.remove(source_index);
    let target_index = persisted
        .sessions
        .iter()
        .position(|session| session.id == target_session_id)
        .expect("validated target must remain after removing a distinct session");
    let insert_at = match placement {
        SessionPlacement::Before => target_index,
        SessionPlacement::After => target_index + 1,
    };
    persisted.sessions.insert(insert_at, source);
    Ok(())
}

pub(crate) fn close_session(
    daemon: &Arc<Daemon>,
    id: SessionId,
    expected: u64,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let session = state
        .persisted
        .sessions
        .iter()
        .find(|session| session.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "session not found"))?;
    expect_revision(session.revision, expected)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    terminate_agent_exchanges(
        &mut persisted,
        &session.panes,
        AgentExchangeState::TargetExited,
        revision,
    );
    persisted.panes.retain(|pane| pane.session_id != id);
    persisted.sessions.retain(|session| session.id != id);
    commit_persisted(daemon, &mut state, persisted)?;
    let runtimes = session
        .panes
        .iter()
        .filter_map(|pane_id| state.forget_pane(*pane_id))
        .collect::<Vec<_>>();
    publish(daemon, &mut state, revision, "session.closed", id.0);
    drop(state);
    for runtime in runtimes {
        runtime.terminate();
    }
    Ok(Response::Ack { revision })
}

// ^ [[Session Model]] An exited pane keeps its saved recipe, so a restart restores the transcript identity instead of leaving a hollow session.
pub(crate) fn restart_session(
    daemon: &Arc<Daemon>,
    id: PaneId,
    expected: u64,
) -> Result<Response, ApiError> {
    let operation_lock = terminal_operation_lock(daemon, id);
    let _operation = lock(&operation_lock);
    let (cwd, terminal_id, launch, agent) = {
        let state = lock(&daemon.state);
        if state.stopping || state.handoff_in_progress {
            return Err(api(
                "daemon_replacing",
                "wsxd is replacing its runtime owner",
            ));
        }
        if state.recovery_in_progress {
            return Err(api(
                "recovery_in_progress",
                "wsxd is still restoring saved sessions",
            ));
        }
        let pane = state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == id)
            .cloned()
            .ok_or_else(|| api("not_found", "pane not found"))?;
        expect_revision(pane.revision, expected)?;
        if !pane.exited {
            return Err(api(
                "invalid_state",
                "pane is still running; restart accepts only an exited pane",
            ));
        }
        let session = state
            .persisted
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
            .ok_or_else(|| api("not_found", "session not found"))?;
        let mut cwd = state
            .persisted
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .map(|worktree| worktree.path.clone())
            .ok_or_else(|| api("not_found", "worktree not found"))?;
        let recipe = pane
            .recovery
            .clone()
            .filter(|recipe| validate_recipe(recipe).is_ok())
            .unwrap_or_else(default_launch_recipe);
        let mut agent = pane.agent.clone();
        let launch = recovery_launch(agent.as_mut(), &recipe, true);
        let launch = match prepare_recovery_launch(launch, &mut cwd) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("wsxd restart pane {}: {error}", id.0);
                RecoveryLaunch::plain(shell_launch_recipe(recipe.rows, recipe.cols))
            }
        };
        // ^ A saved identity only authorizes a resume plan; the replacement runtime must report its own generation.
        let agent = if launch.resume_key.is_some() {
            agent.map(|mut agent| {
                detach_agent(&mut agent);
                agent
            })
        } else {
            None
        };
        (cwd, pane.terminal_id, launch, agent)
    };
    let runtime = spawn_runtime(daemon, id, terminal_id, &cwd, &launch.recipe)?;
    let runtime = Arc::new(runtime);
    let live = !runtime.exited();
    let mut state = lock(&daemon.state);
    if state.stopping || !state.persisted.panes.iter().any(|pane| pane.id == id) {
        state.runtime_generations.remove(&id);
        drop(state);
        runtime.terminate();
        return Err(api(
            "conflict",
            "session changed while the terminal was restarting",
        ));
    }
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let pane = persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == id)
        .expect("pane was checked above");
    pane.exited = !live;
    pane.revision = revision;
    pane.agent = agent;
    if let Err(error) = commit_persisted(daemon, &mut state, persisted) {
        // ^ A failed restart must not leave a generation that could authorize agent reports.
        state.runtime_generations.remove(&id);
        drop(state);
        runtime.terminate();
        return Err(error);
    }
    // ^ The dead pane's stream has no authority over the replacement runtime.
    state.forget_pane_leases(id);
    state.runtimes.insert(id, Arc::clone(&runtime));
    publish(daemon, &mut state, revision, "session.restarted", id.0);
    if !live {
        record_terminal_exit(daemon, &mut state, id);
    }
    Ok(Response::Ack { revision })
}

pub(crate) fn close_pane(
    daemon: &Arc<Daemon>,
    id: PaneId,
    expected: u64,
) -> Result<Response, ApiError> {
    let mut state = lock(&daemon.state);
    let pane = state
        .persisted
        .panes
        .iter()
        .find(|pane| pane.id == id)
        .cloned()
        .ok_or_else(|| api("not_found", "pane not found"))?;
    expect_revision(pane.revision, expected)?;
    let revision = state.revision.saturating_add(1);
    let mut persisted = state.persisted.clone();
    let session = persisted
        .sessions
        .iter_mut()
        .find(|session| session.id == pane.session_id)
        .ok_or_else(|| api("not_found", "session not found"))?;
    if session.panes.len() == 1 {
        return Err(api("last_pane", "close the session instead"));
    }
    session.layout.remove(id);
    session.panes.retain(|pane_id| *pane_id != id);
    if session.focused_pane == id {
        session.focused_pane = session.panes[0];
    }
    if session.primary_pane == id {
        session.primary_pane = session.panes[0];
    }
    session.revision = revision;
    terminate_agent_exchanges(
        &mut persisted,
        &[id],
        AgentExchangeState::TargetExited,
        revision,
    );
    persisted.panes.retain(|candidate| candidate.id != id);
    commit_persisted(daemon, &mut state, persisted)?;
    let runtime = state.forget_pane(id);
    publish(daemon, &mut state, revision, "pane.closed", id.0);
    drop(state);
    if let Some(runtime) = runtime {
        runtime.terminate();
    }
    Ok(Response::Ack { revision })
}

pub(crate) fn project_index_for_pane(
    state: &Persisted,
    pane_id: PaneId,
) -> Result<usize, ApiError> {
    let pane = state
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| api("not_found", "pane not found"))?;
    let session = state
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id && session.panes.contains(&pane_id))
        .ok_or_else(|| api("not_found", "session not found"))?;
    let worktree = state
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .ok_or_else(|| api("not_found", "worktree not found"))?;
    state
        .projects
        .iter()
        .position(|project| project.id == worktree.project_id)
        .ok_or_else(|| api("not_found", "project not found"))
}

pub(crate) fn touch_terminal_project(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
) -> Result<(), ApiError> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let project_index = project_index_for_pane(&persisted, pane_id)?;
    let revision = state.revision.saturating_add(1);
    persisted.projects[project_index].last_terminal_active_unix_ms = Some(unix_time_millis());
    persisted.projects[project_index].revision = revision;
    commit_persisted(daemon, &mut state, persisted)?;
    bump(daemon, &mut state, "project.terminal_entered", pane_id.0);
    Ok(())
}

// ^ [[Session Model]] Agent adapters need stable pane identity from this spawn
// boundary; crates/wsx-core/src/integration owns installation and bundled assets.
pub(crate) fn terminal_agent_environment(
    pane_id: PaneId,
    runtime_generation: &str,
) -> Vec<(String, String)> {
    let mut environment = vec![
        (WSX_PANE_ID_ENV.into(), pane_id.to_string()),
        (
            WSX_RUNTIME_GENERATION_ENV.into(),
            runtime_generation.to_string(),
        ),
    ];
    // ^ docs/daemon-discovery.md: a shell profile may change XDG_STATE_HOME or HOME per
    // directory; the pane's clients keep the socket of the daemon that owns the pane.
    let socket = default_socket_path();
    environment.push(("WSX_SOCKET".into(), socket.to_string_lossy().into_owned()));
    environment.push((
        "WSX_AGENT_REPORT_BIN".into(),
        socket
            .with_extension("reporter")
            .to_string_lossy()
            .into_owned(),
    ));
    // ^ docs/terminal-context.md "Claude in a pane": Claude's inline renderer leaves a full
    // copy of a tall reply in the scrollback on every resize (anthropics/claude-code#103181),
    // and a phone and a desktop resize a pane often. Its full-screen renderer draws on the
    // alternate screen instead. An owner's own value, here or set later by the shell, wins.
    if std::env::var_os(CLAUDE_FULLSCREEN_ENV).is_none() {
        environment.push((CLAUDE_FULLSCREEN_ENV.into(), "1".into()));
    }
    environment
}

/// Claude Code's switch to its full-screen renderer for one process
/// (https://code.claude.com/docs/en/fullscreen).
pub(crate) const CLAUDE_FULLSCREEN_ENV: &str = "CLAUDE_CODE_NO_FLICKER";

pub(crate) fn runtime_notify(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    runtime_generation: Option<String>,
) -> Arc<dyn Fn() + Send + Sync> {
    let weak: Weak<Daemon> = Arc::downgrade(daemon);
    Arc::new(move || {
        let Some(daemon) = weak.upgrade() else { return };
        let mut state = lock(&daemon.state);
        // ^ Bind the callback itself to the registered runtime generation. Looking up only the
        // current runtime would let an abandoned callback publish a change for its successor.
        if state.runtime_generations.get(&pane_id) != runtime_generation.as_ref() {
            return;
        }
        let Some(runtime) = state.runtimes.get(&pane_id).cloned() else {
            return;
        };
        if runtime.exited() {
            record_terminal_exit(&daemon, &mut state, pane_id);
        } else {
            let revision = state.revision.saturating_add(1);
            state.revision = revision;
            state.push_terminal_changed(revision, pane_id);
            daemon.changed.notify_all();
        }
    })
}

pub(crate) fn spawn_runtime(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: &Path,
    recipe: &LaunchRecipe,
) -> Result<TerminalRuntime, ApiError> {
    let runtime_generation = next_runtime_generation(daemon);
    let notify = runtime_notify(daemon, pane_id, Some(runtime_generation.clone()));
    {
        let mut state = lock(&daemon.state);
        clear_agent_wake(&mut state, pane_id);
        state.agent_presence_leases.remove(&pane_id);
        state.claude_reconciliations.remove(&pane_id);
        state
            .runtime_generations
            .insert(pane_id, runtime_generation.clone());
    }
    let startup = startup_input(recipe);
    let runtime = TerminalRuntime::spawn(
        pane_id,
        terminal_id,
        cwd,
        &recipe.command,
        &terminal_agent_environment(pane_id, &runtime_generation),
        startup.as_deref(),
        recipe.rows,
        recipe.cols,
        notify,
    );
    if runtime.is_err() {
        let mut state = lock(&daemon.state);
        if state.runtime_generations.get(&pane_id) == Some(&runtime_generation) {
            state.runtime_generations.remove(&pane_id);
        }
    }
    runtime.map_err(terminal_api)
}

pub(crate) fn next_runtime_generation(daemon: &Daemon) -> String {
    let sequence = daemon
        .next_runtime_generation
        .fetch_add(1, Ordering::Relaxed);
    format!("{:016x}:{sequence:016x}", daemon.epoch)
}

pub(crate) fn record_terminal_exit(daemon: &Daemon, state: &mut State, pane_id: PaneId) {
    state.forget_pane_observations(pane_id);
    let Some(pane) = state
        .persisted
        .panes
        .iter_mut()
        .find(|pane| pane.id == pane_id && !pane.exited)
    else {
        return;
    };
    state.revision = state.revision.saturating_add(1);
    let revision = state.revision;
    pane.exited = true;
    if let Some(agent) = pane.agent.as_mut() {
        detach_agent(agent);
    }
    pane.revision = revision;
    terminate_agent_exchanges(
        &mut state.persisted,
        &[pane_id],
        AgentExchangeState::TargetExited,
        revision,
    );
    state.push_event(Event::Exited { revision, pane_id });
    state.push_plugin_event("terminal.exited", revision, pane_id.0);
    state.persistence_dirty = true;
    daemon.changed.notify_all();
    daemon.plugin_changed.notify_one();
}

pub(crate) fn session_mut(state: &mut Persisted, id: SessionId) -> Result<&mut Session, ApiError> {
    state
        .sessions
        .iter_mut()
        .find(|session| session.id == id)
        .ok_or_else(|| api("not_found", "session not found"))
}

pub(crate) fn next_id(state: &mut Persisted) -> Result<u64, ApiError> {
    let id = state.next_id.max(1);
    state.next_id = id
        .checked_add(1)
        .ok_or_else(|| api("id_exhausted", "stable identifier space exhausted"))?;
    Ok(id)
}

pub(crate) fn expect_revision(actual: u64, expected: u64) -> Result<(), ApiError> {
    if actual == expected {
        Ok(())
    } else {
        Err(api(
            "revision_conflict",
            format!("expected {expected}, current {actual}"),
        ))
    }
}
