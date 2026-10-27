use super::*;

pub(crate) fn cmd_routine(command: RoutineCmd) -> Result<()> {
    use asched_core::routine::{ipc::Action, Routine, Trigger};

    let (project_arg, action, json) = match command {
        RoutineCmd::List { project, json } => (project, Action::List, json),
        RoutineCmd::Show {
            name,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let (revision, routines) = fetch_routines(&path)?;
            let routine = routines
                .into_iter()
                .find(|view| view.routine.name == name)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            return print_routine_response(
                asched_core::routine::ipc::Response::Routine {
                    revision,
                    routine: Box::new(routine),
                },
                json,
            );
        }
        RoutineCmd::Add {
            name,
            cron,
            command,
            prompt,
            project,
            revision,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            let routine = Routine {
                name,
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled: true,
            }
            .validated()?;
            return print_routine_response(
                send_routine(&path, Action::Add { revision, routine })?,
                json,
            );
        }
        RoutineCmd::Edit {
            name,
            new_name,
            cron,
            command,
            prompt,
            project,
            revision,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let (current_revision, routines) = fetch_routines(&path)?;
            let enabled = routines
                .iter()
                .find(|view| view.routine.name == name)
                .map(|view| view.routine.enabled)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            let routine = Routine {
                name: new_name.unwrap_or_else(|| name.clone()),
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled,
            }
            .validated()?;
            return print_routine_response(
                send_routine(
                    &path,
                    Action::Edit {
                        revision: revision.unwrap_or(current_revision),
                        old_name: name,
                        routine,
                    },
                )?,
                json,
            );
        }
        RoutineCmd::Delete {
            name,
            project,
            revision,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            return print_routine_response(
                send_routine(&path, Action::Delete { revision, name })?,
                json,
            );
        }
        RoutineCmd::Enable {
            name,
            project,
            revision,
            json,
        } => return set_routine_enabled(project.as_deref(), &name, revision, true, json),
        RoutineCmd::Disable {
            name,
            project,
            revision,
            json,
        } => return set_routine_enabled(project.as_deref(), &name, revision, false, json),
        RoutineCmd::Run {
            name,
            project,
            json,
        } => (project, Action::Run { name }, json),
        RoutineCmd::Cancel {
            name,
            project,
            json,
        } => (project, Action::Cancel { name }, json),
        RoutineCmd::Logs {
            name,
            project,
            json,
        } => (project, Action::Logs { name }, json),
        RoutineCmd::Fire {
            kind,
            event_id,
            payload,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let payload = serde_json::from_str(&payload)
                .map_err(|error| anyhow::anyhow!("event payload must be JSON: {error}"))?;
            let response = send_routine(
                &path,
                Action::Fire {
                    kind,
                    payload,
                    event_id,
                },
            )?;
            let asched_core::routine::ipc::Response::Fire { outcome } = response else {
                bail!("routine daemon returned an unexpected fire response")
            };
            if json {
                print_json(&outcome)?;
            } else {
                print_fire_outcome(&outcome);
            }
            return Ok(());
        }
    };
    let path = routine_project(project_arg.as_deref())?;
    print_routine_response(send_routine(&path, action)?, json)
}

pub(super) fn routine_root() -> Result<PathBuf> {
    Ok(asched_core::registry::RegistryStore::default_root()?)
}

/// The projects the scheduler runs routines for; `None` without a registry: every project.
pub(crate) fn registered_routine_paths() -> Result<Option<Vec<PathBuf>>> {
    let store = asched_core::registry::RegistryStore::new(routine_root()?);
    if !store.path().exists() {
        return Ok(None);
    }
    Ok(Some(
        store
            .load()?
            .projects
            .into_iter()
            .map(|project| project.working_dir)
            .collect(),
    ))
}

pub(super) fn routine_project(value: Option<&str>) -> Result<PathBuf> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    if let Some(value) = value {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
        return config
            .projects
            .iter()
            .find(|p| p.name == value)
            .map(|p| p.path.clone())
            .ok_or_else(|| anyhow::anyhow!("project '{value}' not found"));
    }
    match config.projects.as_slice() {
        [project] => Ok(project.path.clone()),
        [] => Ok(std::env::current_dir()?),
        many => bail!(
            "multiple projects — use -p to specify: {}",
            many.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn send_routine(
    project: &Path,
    action: asched_core::routine::ipc::Action,
) -> Result<asched_core::routine::ipc::Response> {
    let request = asched_core::routine::ipc::Request::new(project.to_path_buf(), action);
    // ^ wsxd hosts the asched-core scheduler while this client retains its socket protocol.
    Ok(routine_client()?.request_with_start(&request, runtime::routine_daemon_command())?)
}

pub(super) fn routine_client() -> Result<asched_core::routine::RoutineClient> {
    Ok(asched_core::routine::RoutineClient::new(routine_root()?))
}

pub(super) fn fetch_routines(
    project: &Path,
) -> Result<(u64, Vec<asched_core::routine::ipc::RoutineView>)> {
    match send_routine(project, asched_core::routine::ipc::Action::List)? {
        asched_core::routine::ipc::Response::Routines { revision, routines } => {
            Ok((revision, routines))
        }
        other => bail!("routine daemon returned unexpected response: {other:?}"),
    }
}

pub(super) fn fetch_revision(project: &Path) -> Result<u64> {
    Ok(fetch_routines(project)?.0)
}

pub(super) fn set_routine_enabled(
    project: Option<&str>,
    name: &str,
    revision: Option<u64>,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let path = routine_project(project)?;
    let (current_revision, routines) = fetch_routines(&path)?;
    let mut routine = routines
        .into_iter()
        .find(|view| view.routine.name == name)
        .map(|view| view.routine)
        .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
    routine.enabled = enabled;
    print_routine_response(
        send_routine(
            &path,
            asched_core::routine::ipc::Action::Edit {
                revision: revision.unwrap_or(current_revision),
                old_name: name.to_string(),
                routine,
            },
        )?,
        json,
    )
}

pub(super) fn print_fire_outcome(outcome: &asched_core::routine::FireOutcome) {
    use asched_core::routine::{FireOutcome, RoutineFire};
    match outcome {
        FireOutcome::Deduplicated => println!("deduplicated"),
        FireOutcome::NoMatch => println!("no matching routines"),
        FireOutcome::Handled { routines } => {
            for routine in routines {
                match routine {
                    RoutineFire::Started { name } => println!("started\t{name}"),
                    RoutineFire::AlreadyRunning { name } => {
                        println!("already running\t{name}")
                    }
                }
            }
        }
    }
}

pub(super) fn print_routine_response(
    response: asched_core::routine::ipc::Response,
    json: bool,
) -> Result<()> {
    use asched_core::routine::ipc::Response;
    if json {
        print_json(&response)?;
        return Ok(());
    }
    match response {
        Response::Routines { revision, routines } => {
            println!("revision {revision}");
            for view in routines {
                println!(
                    "{}\t{}\t{}\t{}",
                    view.routine.name,
                    if view.routine.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    wsx_core::task::ledger::trigger_text(&view.routine.trigger),
                    format_argv(&view.routine.command)
                );
            }
        }
        Response::Routine { revision, routine } => println!(
            "revision {revision}\nname: {}\nenabled: {}\ntrigger: {}\ncommand: {}\nprompt: {}",
            routine.routine.name,
            routine.routine.enabled,
            wsx_core::task::ledger::trigger_text(&routine.routine.trigger),
            format_argv(&routine.routine.command),
            routine.routine.prompt
        ),
        Response::Runs { runs } => {
            for run in runs {
                println!(
                    "{}\t{:?}\t{}",
                    run.id,
                    run.status,
                    run.final_output.replace('\n', " ")
                );
            }
        }
        Response::Fire { outcome } => print_fire_outcome(&outcome),
        Response::Daemon { protocol, pid } => {
            println!("asched daemon pid={pid} protocol={protocol}")
        }
        Response::Ok { revision } => match revision {
            Some(revision) => println!("ok revision={revision}"),
            None => println!("ok"),
        },
        Response::Error { kind, message } => bail!("{kind}: {message}"),
    }
    Ok(())
}

pub(super) fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}
