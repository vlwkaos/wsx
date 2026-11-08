use super::*;

pub(crate) fn cmd_routine(command: RoutineCmd) -> Result<()> {
    use wsx_core::routine::{Action, Routine};

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
            if json {
                return print_json(
                    &serde_json::json!({ "revision": revision, "routine": routine }),
                );
            }
            println!(
                "revision {revision}\nname: {}\nenabled: {}\ntrigger: {}\ncommand: {}\nprompt: {}",
                routine.routine.name,
                routine.routine.enabled,
                wsx_core::routine::local::trigger_text(&routine.routine.trigger),
                format_argv(&routine.routine.command),
                routine.routine.prompt
            );
            return Ok(());
        }
        RoutineCmd::Add {
            name,
            trigger,
            command,
            prompt,
            no_post,
            findings_exit,
            project,
            revision,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            let routine = Routine {
                name,
                trigger: trigger.trigger()?,
                command,
                prompt,
                enabled: true,
                post: !no_post,
                findings_exit,
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
            trigger,
            command,
            prompt,
            no_post,
            findings_exit,
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
                trigger: trigger.trigger()?,
                command,
                prompt,
                enabled,
                post: !no_post,
                findings_exit,
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
            let wsx_core::routine::Response::Fire { outcome } = response else {
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

pub(crate) fn routine_project(value: Option<&str>) -> Result<PathBuf> {
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
    action: wsx_core::routine::Action,
) -> Result<wsx_core::routine::Response> {
    send_routine_with(project, action, runtime::ensure_available)
}

/// `availability` is `ensure_available`, or `ensure_background_available` for a timer that
/// must leave a stopped wsxd stopped.
pub(crate) fn send_routine_with(
    project: &Path,
    action: wsx_core::routine::Action,
    availability: fn(&runtime::Client) -> std::io::Result<runtime::Availability>,
) -> Result<wsx_core::routine::Response> {
    // ^ wsxd runs the scheduler: start it, or upgrade one older than the scheduler.
    let client = runtime::Client::local();
    availability(&client)?;
    Ok(wsx_core::routine::request(&client, project, action)?)
}

pub(crate) fn fetch_routines(project: &Path) -> Result<(u64, Vec<wsx_core::routine::RoutineView>)> {
    match send_routine(project, wsx_core::routine::Action::List)? {
        wsx_core::routine::Response::Routines { revision, routines } => Ok((revision, routines)),
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
            wsx_core::routine::Action::Edit {
                revision: revision.unwrap_or(current_revision),
                old_name: name.to_string(),
                routine,
            },
        )?,
        json,
    )
}

pub(super) fn print_fire_outcome(outcome: &wsx_core::routine::FireOutcome) {
    use wsx_core::routine::{FireOutcome, RoutineFire};
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
    response: wsx_core::routine::Response,
    json: bool,
) -> Result<()> {
    use wsx_core::routine::Response;
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
                    wsx_core::routine::local::trigger_text(&view.routine.trigger),
                    format_argv(&view.routine.command)
                );
            }
        }
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
        Response::Ok { revision } => match revision {
            Some(revision) => println!("ok revision={revision}"),
            None => println!("ok"),
        },
        Response::Error(error) => bail!("{error}"),
    }
    Ok(())
}

pub(super) fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}

impl TriggerArgs {
    /// The trigger the flags name; a time must be in the future.
    pub(crate) fn trigger(self) -> Result<wsx_core::routine::Trigger> {
        use wsx_core::routine::Trigger;
        let now = wsx_core::task::now_unix_ms() as i64 / 1000;
        Ok(match (self.cron, self.event, self.at) {
            (Some(cron), _, _) => Trigger::Cron(cron),
            (_, Some(kind), _) => Trigger::Event { kind },
            (_, _, Some(at)) => {
                let epoch_minute =
                    wsx_core::routine::local::local_minute(&at, now).map_err(anyhow::Error::msg)?;
                if epoch_minute * 60 <= now {
                    bail!("--at {at} has passed");
                }
                Trigger::At { epoch_minute }
            }
            (None, None, None) => bail!("one of --cron, --event or --at is required"),
        })
    }
}
