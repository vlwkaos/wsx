//! `wsx task`: start a headless worker and steer, read and close it.
//! ref: docs/tasks.md

mod agent;
mod args;
mod batch;
mod boundary;
pub mod broker;
mod claude;
mod cmd;
pub mod host;
mod hosting;
mod ledger;
pub(crate) mod limit;
pub mod manager;
mod notes;
mod pi;
mod preview;
mod slot;
#[cfg(test)]
mod stream_tests;
mod trial;

use crate::cli::load_config;
use anyhow::{bail, Context, Result};
pub use args::TaskCmd;
use cmd::approve::{approve, ask_covering, ask_owner, deny, owner_only, request_of};
use cmd::chat::{chat, dismiss, ensure_manager, role, say, search, From};
use cmd::check::check;
pub(crate) use cmd::check::fail_check;
use cmd::deploy::{deploy, deployed, push, trial_command};
use cmd::land::{after_command, land_batch};
use cmd::new::{new, Size, Steps};
use cmd::routine::{checkin, digest, post};
use cmd::work::{close, hand, handback, list, show};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::manager::ApprovalAction;
use wsx_core::task::owner::{self, HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::view::checked_files;
use wsx_core::task::{
    call_host,
    store::{self, TaskDir},
    Actor, DeployKind, HostRequest, HostResponse, MAX_MESSAGE_BYTES,
};
use wsx_core::{ops, runtime};

/// Ends the evidence name of a check-run pass the host recorded in a held result; the
/// manager's ready event repeats it. ref: docs/tasks.md "Check-run"
const RECORDED_BY_HOST: &str = "(recorded by the host)";
const HOST_START_WAIT: Duration = Duration::from_secs(10);
pub fn run(command: TaskCmd) -> Result<()> {
    match command {
        TaskCmd::New {
            prompt,
            from,
            into,
            title,
            model,
            size,
            hold,
            files,
            plan,
            skip,
            no_steps,
            project,
            json,
        } => new(
            prompt,
            from.as_deref(),
            into,
            title.as_deref(),
            model.as_deref(),
            hold,
            files,
            plan,
            match (no_steps, size) {
                (true, _) => Steps::None,
                (false, Size::Small) => Steps::Small,
                (false, Size::Full) => Steps::Skip(skip),
            },
            project.as_deref(),
            json,
        ),
        TaskCmd::List { project, all, json } => list(project.as_deref(), all, json),
        TaskCmd::Show {
            task,
            project,
            json,
        } => show(&open(project.as_deref(), &task)?, json),
        TaskCmd::Steer {
            task,
            text,
            project,
            json,
        } => {
            let dir = open_owned(project.as_deref(), &task)?;
            let text = text.read(MAX_MESSAGE_BYTES)?;
            send(&dir, HostRequest::Steer { text }, json)
        }
        TaskCmd::Add {
            task,
            text,
            files,
            project,
            json,
        } => {
            let dir = open_owned(project.as_deref(), &task)?;
            let text = text.read(MAX_MESSAGE_BYTES)?;
            let files = checked_files(&dir.read_record()?.files, &files)?;
            send(&dir, HostRequest::Add { text, files }, json)
        }
        TaskCmd::Stop {
            task,
            project,
            json,
        } => send(
            &open_owned(project.as_deref(), &task)?,
            HostRequest::Stop,
            json,
        ),
        TaskCmd::Check {
            action,
            project,
            json,
        } => check(action, project.as_deref(), json),
        TaskCmd::Land {
            task,
            tried,
            said: _,
            project,
            json,
        } if task.len() > 1 => land_batch(project.as_deref(), &task, tried, json),
        TaskCmd::LandBatch { tasks, project } => batch::run(
            tasks
                .iter()
                .map(|task| open(project.as_deref(), task))
                .collect::<Result<_>>()?,
        ),
        TaskCmd::Land {
            task,
            tried,
            said,
            project,
            json,
        } => {
            let dir = open_owned(project.as_deref(), &task[0])?;
            // ^ Only the owner tries a trial, so only the owner passes one.
            if tried && acting_as_manager() {
                bail!("only the owner passes a trial; ask the owner to try it");
            }
            let record = dir.read_record()?;
            // ^ A role lands only into a parent task; the manager lands onto the default branch.
            if actor().role().is_some() && record.parent.is_none() {
                bail!(
                    "a role lands only a child into its parent task; #{} lands onto {}, which is the manager's",
                    record.id,
                    record.default_branch
                );
            }
            let settings = load_task_config(&record.project_path)
                .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
            if acting_as_manager()
                && settings.land_approve
                && ask_owner(
                    &dir.project_id,
                    ApprovalAction::Land,
                    Some(&record),
                    None,
                    None,
                    None,
                    said.owner_said,
                    json,
                )?
            {
                return Ok(());
            }
            send(
                &dir,
                HostRequest::Land {
                    tried,
                    batch_tree: None,
                },
                json,
            )
        }
        TaskCmd::Hold {
            task,
            reason,
            off: _,
            project,
            json,
        } => send(
            &open_owned(project.as_deref(), &task)?,
            HostRequest::Hold {
                reason,
                by: actor(),
            },
            json,
        ),
        TaskCmd::After {
            task,
            on,
            then,
            because,
            off: _,
            project,
            json,
        } => after_command(project.as_deref(), &task, on, then, because, json),
        TaskCmd::Trial {
            task,
            end,
            project,
            json,
        } => {
            not_for_roles("trial")?;
            trial_command(&open(project.as_deref(), &task)?, end, json)
        }
        TaskCmd::Close {
            task,
            discard,
            said,
            project,
            json,
        } => {
            let dir = open_owned(project.as_deref(), &task)?;
            if discard {
                not_for_roles("close --discard")?;
            }
            // ^ Discarding work cannot be undone: the manager always asks first, or names
            // the owner's message that asked for it.
            let record = dir.read_record()?;
            if discard
                && acting_as_manager()
                && ask_covering(
                    &record.project_path,
                    &dir.project_id,
                    request_of(ApprovalAction::Discard, Some(&record), None, None, None),
                    said.owner_said,
                    json,
                )?
            {
                return Ok(());
            }
            close(&dir, discard, json)
        }
        TaskCmd::Hand {
            task,
            project,
            json,
        } => hand(&open(project.as_deref(), &task)?, json),
        TaskCmd::Handback {
            task,
            project,
            json,
        } => handback(&open(project.as_deref(), &task)?, json),
        TaskCmd::Approve { id, project, json } => approve(project.as_deref(), &id, json),
        TaskCmd::Deny {
            id,
            note,
            project,
            json,
        } => deny(project.as_deref(), &id, note, json),
        TaskCmd::Deploy {
            commit,
            if_new,
            trial,
            run,
            said,
            project,
            json,
        } => deploy(
            DeployKind::Deploy,
            project.as_deref(),
            commit.as_deref(),
            if_new,
            trial,
            run.run_locked,
            said.owner_said,
            json,
        ),
        TaskCmd::Release {
            commit,
            run,
            said,
            project,
            json,
        } => deploy(
            DeployKind::Release,
            project.as_deref(),
            commit.as_deref(),
            false,
            None,
            run.run_locked,
            said.owner_said,
            json,
        ),
        TaskCmd::Push {
            commit,
            force,
            lease,
            said,
            project,
            json,
        } => push(
            project.as_deref(),
            commit.as_deref(),
            force,
            lease.as_deref(),
            said.owner_said,
            json,
        ),
        TaskCmd::Deployed { project, json } => deployed(project.as_deref(), json),
        TaskCmd::Digest { project, since } => digest(project.as_deref(), since),
        TaskCmd::Post {
            project,
            title,
            level,
            key,
        } => post(project.as_deref(), &title, &level, key.as_deref()),
        TaskCmd::Checkin {
            project,
            after,
            at,
            why,
        } => checkin(project.as_deref(), after.as_deref(), at.as_deref(), &why),
        TaskCmd::Role { action } => role(action),
        TaskCmd::Ledger { project, .. } => ledger::ledger(project.as_deref()),
        TaskCmd::Routine { action, owner_said } => {
            use crate::cli::RoutineCmd::{Add, Delete, Disable, Edit, Enable};
            // ^ A role changes nothing of the project's own (docs/tasks.md "Roles").
            if let Add { .. } | Edit { .. } | Delete { .. } | Enable { .. } | Disable { .. } =
                &action
            {
                not_for_roles("routine changes")?;
            }
            // ^ wsxd runs a routine outside the agent boundary: only the owner writes one.
            if let Add { project, .. } | Edit { project, .. } | Delete { project, .. } = &action {
                if acting_as_manager() {
                    let said = owner_said.context(
                        "the owner adds, edits and deletes routines: pass --owner-said <id> \
                         when the owner's message of this turn asks for it; nothing was run",
                    )?;
                    let (name, main) = project_entry(project.as_deref())?;
                    owner::check_owner_said(&store::project_id(&name, &main), said)?;
                }
            }
            crate::cli::cmd_routine(action)
        }
        TaskCmd::Manager { project, json } => {
            let (started, dir) = ensure_manager(project.as_deref())?;
            let state = if started { "started" } else { "running" };
            if json {
                let (project, main) = project_entry(project.as_deref())?;
                let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
                let name = settings.manager_shown_name(&project);
                print_json(&json!({ "state": state, "socket": dir.socket_path(), "name": name }))
            } else {
                println!("manager {state}");
                Ok(())
            }
        }
        TaskCmd::Say {
            text,
            role,
            from_manager,
            from_role,
            reply,
            project,
            json,
        } => {
            if reply && !from_manager && !from_role {
                bail!("--reply answers with --from-manager or --from-role");
            }
            let from = match (from_manager, from_role) {
                (true, _) => Some(From::Manager(reply)),
                (_, true) => Some(From::Role(reply)),
                _ => None,
            };
            say(
                project.as_deref(),
                role.as_deref(),
                text.read(MAX_MESSAGE_BYTES)?,
                from,
                json,
            )
        }
        TaskCmd::Chat {
            after,
            find,
            around,
            role,
            project,
            json,
        } => chat(
            project.as_deref(),
            role.as_deref(),
            (after, find.as_deref(), around),
            json,
        ),
        TaskCmd::Search {
            words,
            task,
            all,
            limit,
            reindex,
            project,
            json,
        } => search(
            project.as_deref(),
            &words.join(" "),
            task.as_deref(),
            all,
            limit,
            reindex,
            json,
        ),
        TaskCmd::Dismiss { id, project, json } => dismiss(project.as_deref(), id, json),
        // ^ Under a task host's broker the CLI forwards it before parsing (broker.rs).
        TaskCmd::CheckRun { .. } => {
            bail!("`wsx task check-run` runs only inside a task's worker, through its host")
        }
        TaskCmd::Preview {
            task,
            stop,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            let (value, message) = if stop {
                let stopped = preview::stop(&dir)?;
                let message = match stopped {
                    true => "stopped the preview",
                    false => "no preview was running",
                };
                (
                    json!({ "stopped": stopped, "message": message }),
                    message.to_string(),
                )
            } else {
                let running = preview::start(&dir)?;
                let message = format!("try it at {} until it stops in an hour", running.url);
                let mut value = serde_json::to_value(&running)?;
                value["message"] = json!(message);
                (value, message)
            };
            if json {
                return print_json(&value);
            }
            println!("{message}");
            Ok(())
        }
        TaskCmd::PreviewHost { project_id, task } => {
            preview::host(&project_id, store::parse_task_number(&task)?)
        }
        TaskCmd::ManagerHost { project_id, role } => manager::run(&project_id, role.as_deref()),
        TaskCmd::BoundaryProfile { project } => {
            let (name, main) = project_entry(project.as_deref())?;
            manager::print_profile(&name, &main)
        }
        TaskCmd::Host { project_id, task } => {
            host::run(&project_id, store::parse_task_number(&task)?)
        }
    }
}

/// The registered project by name, by the current folder, or the only one.
pub(crate) fn project_entry(name: Option<&str>) -> Result<(String, PathBuf)> {
    let config = load_config()?;
    let projects = &config.projects;
    if let Some(name) = name {
        return projects
            .iter()
            .find(|project| project.name == name)
            .map(|project| (project.name.clone(), project.path.clone()))
            .with_context(|| format!("project '{name}' is not registered"));
    }
    let cwd = std::env::current_dir().ok();
    if let Some(project) = cwd.as_deref().and_then(|cwd| {
        projects
            .iter()
            .filter(|project| cwd.starts_with(&project.path))
            .max_by_key(|project| project.path.as_os_str().len())
    }) {
        return Ok((project.name.clone(), project.path.clone()));
    }
    match projects.as_slice() {
        [project] => Ok((project.name.clone(), project.path.clone())),
        [] => bail!("no projects are registered"),
        _ => bail!(
            "several projects are registered; use -p with one of: {}",
            projects
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The registered project whose task folder is `project_id`.
fn project_by_id(project_id: &str) -> Result<(String, PathBuf)> {
    load_config()?
        .projects
        .into_iter()
        .find(|project| store::project_id(&project.name, &project.path) == project_id)
        .map(|project| (project.name, project.path))
        .with_context(|| format!("no registered project has the task ID {project_id}"))
}

fn open(project: Option<&str>, task: &str) -> Result<TaskDir> {
    let (name, path) = project_entry(project)?;
    TaskDir::open(
        &store::project_id(&name, &path),
        store::parse_task_number(task)?,
    )
}

/// `open` for a command that changes the task: a role acts only on the tasks it created,
/// whose events go to it. ref: docs/tasks.md "Roles"
fn open_owned(project: Option<&str>, task: &str) -> Result<TaskDir> {
    let dir = open(project, task)?;
    let actor = actor();
    if let Some(role) = actor.role() {
        let record = dir.read_record()?;
        if record.created_by != actor {
            bail!(
                "role {role} acts only on the tasks it created; #{} is the {}'s",
                record.id,
                String::from(record.created_by)
            );
        }
    }
    Ok(dir)
}

/// Refuse what only the owner or the manager does: a role has no approvals, deploys,
/// trials, routines or roles of its own. ref: docs/tasks.md "Roles"
fn not_for_roles(what: &str) -> Result<()> {
    if let Some(role) = actor().role() {
        bail!("{what} is not for a role; ask the manager (`wsx task say --from-role`); role {role} ran nothing");
    }
    Ok(())
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn wait_for_host(dir: &TaskDir) -> Option<HostResponse> {
    let deadline = Instant::now() + HOST_START_WAIT;
    while Instant::now() < deadline {
        if let Ok(response) = call_host(&dir.socket_path(), &HostRequest::Status, HOST_TIMEOUT) {
            return Some(response);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn send(dir: &TaskDir, request: HostRequest, json: bool) -> Result<()> {
    report_response(dir, ask_host(dir, &request)?, json)
}

/// Print a host's answer; a refusal is an error.
fn report_response(dir: &TaskDir, response: HostResponse, json: bool) -> Result<()> {
    if json {
        print_json(&serde_json::to_value(&response)?)?;
    } else {
        println!(
            "t{} {}: {}",
            dir.id,
            response.state.as_str(),
            response.message
        );
    }
    if !response.ok {
        bail!(
            "task t{} refused the request and is still {}",
            dir.id,
            response.state.as_str()
        );
    }
    Ok(())
}

/// One request to the task's host (`call_task_host`); an error says why none answered and
/// that the task is unchanged.
fn ask_host(dir: &TaskDir, request: &HostRequest) -> Result<HostResponse> {
    // ^ A landing or a trial rebases the branch on the host before it answers.
    let timeout = if matches!(request, HostRequest::Land { .. } | HostRequest::Trial) {
        LAND_TIMEOUT
    } else {
        HOST_TIMEOUT
    };
    wsx_core::task::call_task_host(dir, request, timeout).map_err(|error| {
        let state = dir.read_record().map_or_else(
            |_| "unknown".to_string(),
            |record| record.state.as_str().to_string(),
        );
        anyhow::anyhow!("{error:#}; task t{} is {state} and unchanged", dir.id)
    })
}

/// `ask_host`, where a refusal is an error.
fn ask_host_ok(dir: &TaskDir, request: &HostRequest) -> Result<HostResponse> {
    let response = ask_host(dir, request)?;
    if !response.ok {
        bail!(
            "task t{} refused: {}; it is still {}",
            dir.id,
            response.message,
            response.state.as_str()
        );
    }
    Ok(response)
}

/// Create the manager's or a role's hidden session (its label), starting wsxd when it does
/// not run.
pub(crate) fn launch_manager(main: &Path) -> impl FnOnce(String, Vec<String>) -> Result<()> + '_ {
    move |label, argv| {
        ops::create_session_launch(
            &runtime::Client::local(),
            &load_config()?,
            main,
            label,
            argv,
            None,
        )
        .map(drop)
    }
}

/// True when the manager's or a role's agent runs this command (`WSX_TASK_ACTOR`).
/// ^ A guard against the agents' mistakes, not a security boundary: anything that runs
/// commands as the owner can unset it (docs/tasks.md "Approvals").
fn acting_as_manager() -> bool {
    actor() != Actor::Owner
}

fn actor() -> Actor {
    Actor::from_env()
}
