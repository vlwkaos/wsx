//! Cold recovery of saved panes and the resume supervisor.

use super::*;

pub(crate) fn recover_runtimes(daemon: &Arc<Daemon>, resume_agents: bool) -> io::Result<()> {
    let mut attempts = prepare_recovery_attempts(daemon, resume_agents)?;
    attempts.sort_by_key(|attempt| attempt.launch.resume_key.is_some());
    let mut attempted_sessions = HashSet::new();

    for attempt in attempts {
        let RecoveryAttempt {
            pane_id,
            terminal_id,
            cwd,
            launch,
        } = attempt;
        let (stopping, pane_exists) = {
            let state = lock(&daemon.state);
            (
                state.stopping,
                state.persisted.panes.iter().any(|pane| pane.id == pane_id),
            )
        };
        if stopping {
            break;
        }
        if !pane_exists {
            continue;
        }
        let had_resume = launch.resume_key.is_some();
        let launch = deduplicate_recovery_launch(launch, &attempted_sessions);
        if had_resume && launch.resume_key.is_none() {
            clear_pending_recovery_agent(daemon, pane_id)?;
        }
        let launch = match prepare_recovery_launch(launch, &cwd) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {error}", pane_id.0);
                clear_pending_recovery_agent(daemon, pane_id)?;
                continue;
            }
        };
        let runtime = match spawn_runtime(daemon, pane_id, terminal_id, &cwd, &launch.recipe) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                eprintln!("wsxd recovery pane {}: {}", pane_id.0, error.message);
                clear_pending_recovery_agent(daemon, pane_id)?;
                continue;
            }
        };
        let generation = {
            let mut state = lock(&daemon.state);
            if !state.persisted.panes.iter().any(|pane| pane.id == pane_id) {
                drop(state);
                runtime.terminate();
                continue;
            }
            let generation = state.runtime_generations.get(&pane_id).cloned();
            state.runtimes.insert(pane_id, Arc::clone(&runtime));
            if runtime.exited() {
                record_terminal_exit(daemon, &mut state, pane_id);
            } else if let Some(pane) = state
                .persisted
                .panes
                .iter_mut()
                .find(|pane| pane.id == pane_id)
            {
                pane.exited = false;
            }
            generation
        };
        let Some(resume_key) = launch.resume_key else {
            continue;
        };
        attempted_sessions.insert(resume_key);
        if !launch.await_readiness {
            continue;
        }
        let Some(provider) = launch.provider.as_deref() else {
            continue;
        };
        let Some(generation) = generation else {
            continue;
        };
        if wait_for_recovered_agent(
            daemon,
            pane_id,
            &generation,
            provider,
            RECOVERY_AGENT_READY_TIMEOUT,
        ) == RecoveryReadiness::TimedOut
        {
            eprintln!(
                "wsxd recovery pane {}: {provider} did not become ready within {} seconds; opening a shell",
                pane_id.0,
                RECOVERY_AGENT_READY_TIMEOUT.as_secs()
            );
            replace_timed_out_recovery_with_shell(
                daemon,
                pane_id,
                terminal_id,
                &cwd,
                launch.recipe.rows,
                launch.recipe.cols,
                &runtime,
            )?;
        }
    }
    let state = lock(&daemon.state);
    save_state(&daemon.state_path, &state.persisted)
}

pub(crate) fn prepare_recovery_attempts(
    daemon: &Arc<Daemon>,
    resume_agents: bool,
) -> io::Result<Vec<RecoveryAttempt>> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    let session_worktrees = persisted
        .sessions
        .iter()
        .map(|session| (session.id, session.worktree_id))
        .collect::<HashMap<_, _>>();
    let worktree_paths = persisted
        .worktrees
        .iter()
        .map(|worktree| (worktree.id, worktree.path.clone()))
        .collect::<HashMap<_, _>>();
    let mut attempts = Vec::new();
    for pane in &mut persisted.panes {
        pane.exited = true;
        if pane.recovery_quarantined {
            continue;
        }
        if pane.recovery.is_none() {
            pane.recovery = Some(default_launch_recipe());
        }
        let Some(saved_recipe) = pane.recovery.clone() else {
            continue;
        };
        if let Err(error) = validate_recipe(&saved_recipe) {
            eprintln!("wsxd recovery pane {}: {error}", pane.id.0);
            pane.recovery = None;
            pane.recovery_quarantined = true;
            pane.agent = None;
            continue;
        }
        let launch = recovery_launch(pane.agent.as_mut(), &saved_recipe, resume_agents);
        // ^ [[Session Model]] A persisted identity authorizes the queued resume plan.
        // It remains detached until the replacement runtime reports its own generation.
        if launch.resume_key.is_some() {
            if let Some(agent) = pane.agent.as_mut() {
                detach_agent(agent);
            }
        } else {
            pane.agent = None;
        }
        let Some(cwd) = session_worktrees
            .get(&pane.session_id)
            .and_then(|worktree_id| worktree_paths.get(worktree_id))
            .cloned()
        else {
            eprintln!("wsxd recovery pane {}: worktree is absent", pane.id.0);
            pane.agent = None;
            continue;
        };
        attempts.push(RecoveryAttempt {
            pane_id: pane.id,
            terminal_id: pane.terminal_id,
            cwd,
            launch,
        });
    }
    state.persist(&daemon.state_path, persisted)?;
    Ok(attempts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryReadiness {
    Ready,
    Failed,
    Cancelled,
    TimedOut,
}

pub(crate) fn wait_for_recovered_agent(
    daemon: &Daemon,
    pane_id: PaneId,
    generation: &str,
    provider: &str,
    timeout: Duration,
) -> RecoveryReadiness {
    let deadline = Instant::now() + timeout;
    let mut state = lock(&daemon.state);
    loop {
        if state.stopping || !state.persisted.panes.iter().any(|pane| pane.id == pane_id) {
            return RecoveryReadiness::Cancelled;
        }
        if state.runtime_generations.get(&pane_id).map(String::as_str) != Some(generation) {
            return RecoveryReadiness::Failed;
        }
        if state
            .persisted
            .panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .and_then(|pane| pane.agent.as_ref())
            .is_some_and(|agent| agent.attached && agent.provider == provider)
        {
            return RecoveryReadiness::Ready;
        }
        let now = Instant::now();
        if now >= deadline {
            return RecoveryReadiness::TimedOut;
        }
        let wait = (deadline - now).min(RECOVERY_AGENT_POLL_INTERVAL);
        let (next, _) = daemon
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
    }
}

pub(crate) fn clear_pending_recovery_agent(daemon: &Daemon, pane_id: PaneId) -> io::Result<()> {
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
        pane.agent = None;
    }
    state.persist(&daemon.state_path, persisted)?;
    Ok(())
}

pub(crate) fn replace_timed_out_recovery_with_shell(
    daemon: &Arc<Daemon>,
    pane_id: PaneId,
    terminal_id: TerminalId,
    cwd: &Path,
    rows: u16,
    cols: u16,
    runtime: &Arc<TerminalRuntime>,
) -> io::Result<()> {
    {
        let mut state = lock(&daemon.state);
        let owns_runtime = state
            .runtimes
            .get(&pane_id)
            .is_some_and(|current| Arc::ptr_eq(current, runtime));
        if !owns_runtime {
            return Ok(());
        }
        let mut persisted = state.persisted.clone();
        let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) else {
            return Ok(());
        };
        pane.agent = None;
        pane.exited = true;
        state.persist(&daemon.state_path, persisted)?;
        state.runtimes.remove(&pane_id);
        state.runtime_generations.remove(&pane_id);
    }
    runtime.terminate();
    let recipe = shell_launch_recipe(rows, cols);
    let replacement = Arc::new(
        spawn_runtime(daemon, pane_id, terminal_id, cwd, &recipe)
            .map_err(|error| io::Error::other(error.message))?,
    );
    let mut state = lock(&daemon.state);
    let mut persisted = state.persisted.clone();
    if let Some(pane) = persisted.panes.iter_mut().find(|pane| pane.id == pane_id) {
        pane.exited = replacement.exited();
        if let Err(error) = state.persist(&daemon.state_path, persisted) {
            state.runtime_generations.remove(&pane_id);
            drop(state);
            replacement.terminate();
            return Err(error);
        }
        state.runtimes.insert(pane_id, replacement);
    } else {
        state.runtime_generations.remove(&pane_id);
        drop(state);
        replacement.terminate();
    }
    Ok(())
}

pub(crate) fn spawn_cold_recovery(
    daemon: &Arc<Daemon>,
    resume_agents: bool,
) -> thread::JoinHandle<()> {
    {
        let mut state = lock(&daemon.state);
        state.recovery_in_progress = true;
    }
    let daemon = Arc::clone(daemon);
    thread::spawn(move || {
        if let Err(error) = recover_runtimes(&daemon, resume_agents) {
            eprintln!("wsxd recovery: {error}");
        }
        let mut state = lock(&daemon.state);
        state.recovery_in_progress = false;
        daemon.changed.notify_all();
    })
}

pub(crate) fn recovery_launch(
    agent: Option<&mut AgentInfo>,
    saved_recipe: &LaunchRecipe,
    resume_agents: bool,
) -> RecoveryLaunch {
    if !resume_agents {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    }
    let Some(agent) = agent else {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    let session_ref = agent.session_ref.clone().or_else(|| {
        agent
            .conversation_id
            .as_ref()
            .and_then(|value| AgentSessionRef::id(value.clone()))
    });
    let Some(session_ref) = session_ref else {
        return RecoveryLaunch {
            recipe: saved_recipe.clone(),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    agent.session_ref = Some(session_ref.clone());
    let provider = agent.provider.clone();
    let await_readiness = agent.capabilities.lifecycle;
    let Some(plan) = resume::plan(&provider, &session_ref) else {
        return RecoveryLaunch {
            recipe: shell_launch_recipe(saved_recipe.rows, saved_recipe.cols),
            resume_key: None,
            provider: None,
            await_readiness: false,
        };
    };
    RecoveryLaunch {
        recipe: LaunchRecipe {
            command: plan.argv,
            initial_input: None,
            rows: saved_recipe.rows,
            cols: saved_recipe.cols,
        },
        resume_key: Some(plan.dedupe_key),
        provider: Some(provider),
        await_readiness,
    }
}

pub(crate) fn deduplicate_recovery_launch(
    launch: RecoveryLaunch,
    resumed_sessions: &HashSet<String>,
) -> RecoveryLaunch {
    if launch
        .resume_key
        .as_ref()
        .is_some_and(|key| resumed_sessions.contains(key))
    {
        RecoveryLaunch {
            recipe: shell_launch_recipe(launch.recipe.rows, launch.recipe.cols),
            resume_key: None,
            provider: None,
            await_readiness: false,
        }
    } else {
        launch
    }
}

pub(crate) fn shell_launch_recipe(rows: u16, cols: u16) -> LaunchRecipe {
    launch_recipe(Vec::new(), None, rows, cols).unwrap_or_else(|_| default_launch_recipe())
}

pub(crate) fn prepare_recovery_launch(
    mut launch: RecoveryLaunch,
    cwd: &Path,
) -> Result<RecoveryLaunch, String> {
    if launch.resume_key.is_none() {
        return Ok(launch);
    }
    let executable = launch
        .recipe
        .command
        .first()
        .ok_or_else(|| "native resume command is empty".to_string())?;
    let resolved = resolve_executable(cwd, executable)
        .ok_or_else(|| format!("native resume executable is unavailable: {executable}"))?;
    launch.recipe.command[0] = resolved
        .to_str()
        .ok_or_else(|| "native resume executable path is not UTF-8".to_string())?
        .to_string();
    let supervisor = std::env::current_exe()
        .map_err(|error| format!("resolve wsxd resume supervisor: {error}"))?;
    let supervisor = supervisor
        .to_str()
        .ok_or_else(|| "wsxd resume supervisor path is not UTF-8".to_string())?;
    let mut command = vec![supervisor.to_string(), RESUME_SUPERVISOR_ARG.to_string()];
    command.extend(launch.recipe.command);
    launch.recipe.command = command;
    Ok(launch)
}

pub(crate) fn resolve_executable(cwd: &Path, executable: &str) -> Option<PathBuf> {
    let executable_path = Path::new(executable);
    let candidates = if executable_path.components().count() > 1 {
        vec![if executable_path.is_absolute() {
            executable_path.to_path_buf()
        } else {
            cwd.join(executable_path)
        }]
    } else {
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|directory| {
                        if directory.is_absolute() {
                            directory.join(executable_path)
                        } else {
                            cwd.join(directory).join(executable_path)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates.into_iter().find(|candidate| {
        fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

/// Main screen, no kitty keyboard flags, no modifyOtherKeys, no mouse or focus reports, no
/// bracketed paste, normal cursor keys and keypad, a visible cursor.
pub(crate) const AGENT_MODES_RESET: &[u8] = b"\x1b[?1049l\x1b[=0;1u\x1b[>4;0m\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\
\x1b[?1l\x1b>\x1b[?25h";

pub fn run_resume_supervisor(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    let executable = arguments.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "resume supervisor command is empty",
        )
    })?;
    let mut command = Command::new(executable);
    command.args(arguments);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let signals = InteractiveSignalGuard::ignore()?;
    reset_child_interactive_signals(&mut command);
    let status = command.status()?;
    // ^ An agent that quits can leave its alternate screen and its keyboard and mouse modes
    // on; the shell would then get CSI u codes for typed keys and draw on a screen that
    // goes away. Leave them as a fresh terminal has them.
    // ref: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ (CSI = 0 ; 1 u sets no flags)
    let mut out = io::stdout();
    let _ = out.write_all(AGENT_MODES_RESET).and_then(|()| out.flush());
    if !status.success() {
        eprintln!("wsxd resumed agent exited with {status}; opening a shell");
    }
    // ^ The shell is what the pane is for; a record wsxd could not clear (a stale runtime
    // generation, a full disk) must not leave the pane as a dead screen.
    if let Err(error) = transition_resumed_agent_to_shell() {
        eprintln!("wsxd could not clear the resumed agent: {error}; opening a shell");
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
    let mut shell = Command::new(shell);
    // ^ A repeated Ctrl+C that ends the agent must not kill this supervisor before
    // the shell exists, so the default dispositions return only at exec.
    reset_child_interactive_signals(&mut shell);
    let error = shell.exec();
    drop(signals);
    Err(error)
}

pub(crate) fn transition_resumed_agent_to_shell() -> io::Result<()> {
    let pane_id = std::env::var("WSX_PANE_ID")
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "resume pane ID is missing"))?
        .parse::<PaneId>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let runtime_generation = std::env::var(WSX_RUNTIME_GENERATION_ENV).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "resume runtime generation is missing",
        )
    })?;
    let next_runtime_generation =
        format!("{:016x}:{:016x}", epoch(), u64::from(std::process::id()));
    let request = Request::AgentClear {
        pane_id,
        runtime_generation,
        next_runtime_generation: next_runtime_generation.clone(),
    };
    let client = Client::local();
    // ^ [[Session Model]] A recovered agent can exit before startup publishes the wsxd socket.
    let deadline = Instant::now() + RESUME_CLEAR_RETRY_TIMEOUT;
    let response = loop {
        match client.call(&request) {
            Ok(response) => break response,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(RESUME_CLEAR_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    };
    match response {
        Response::Ack { .. } => {
            std::env::set_var(WSX_RUNTIME_GENERATION_ENV, next_runtime_generation);
            Ok(())
        }
        Response::Error(error) => Err(io::Error::other(format!(
            "{}: {}",
            error.code, error.message
        ))),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wsxd returned an unexpected agent clear response",
        )),
    }
}

pub(crate) struct InteractiveSignalGuard {
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl InteractiveSignalGuard {
    fn ignore() -> io::Result<Self> {
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
            // SAFETY: setting a process signal disposition is async-signal-safe;
            // the prior disposition is restored before this supervisor execs a shell.
            let prior = unsafe { libc::signal(signal, libc::SIG_IGN) };
            if prior == libc::SIG_ERR {
                for (restore_signal, disposition) in previous.drain(..).rev() {
                    // SAFETY: restoring a disposition returned by libc::signal.
                    unsafe { libc::signal(restore_signal, disposition) };
                }
                return Err(io::Error::last_os_error());
            }
            previous.push((signal, prior));
        }
        Ok(Self { previous })
    }
}

impl Drop for InteractiveSignalGuard {
    fn drop(&mut self) {
        for (signal, disposition) in self.previous.drain(..).rev() {
            // SAFETY: restoring a disposition returned by libc::signal.
            unsafe { libc::signal(signal, disposition) };
        }
    }
}

pub(crate) fn reset_child_interactive_signals(command: &mut Command) {
    // SAFETY: pre_exec only calls async-signal-safe libc::signal operations.
    unsafe {
        command.pre_exec(|| {
            for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
                if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

pub(crate) fn resume_agents_on_restore() -> bool {
    match GlobalConfig::load() {
        Ok((config, None)) => config.resume_agents_on_restore,
        Ok((_, Some(warning))) => {
            eprintln!("wsxd agent restoration disabled: {warning}");
            false
        }
        Err(error) => {
            eprintln!("wsxd agent restoration disabled: {error:#}");
            false
        }
    }
}
