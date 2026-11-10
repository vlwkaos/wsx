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
mod instructions;
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
use args::{DeployArgs, ReleaseArgs};
use cmd::approve::{approve, deny};
use cmd::chat::{boundary_profile, chat, dismiss, manager_command, role, say_command, search};
use cmd::check::check;
pub(crate) use cmd::check::fail_check;
use cmd::deploy::{deploy, deployed, push, trial_command};
use cmd::land::{after_command, hold_command, land_command, run_batch};
use cmd::new::new;
use cmd::project::project;
use cmd::routine::{checkin, digest, post, routine_command};
use cmd::work::{
    add_command, close_command, hand, handback, list, preview_command, show, steer_command,
    stop_command,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::task::owner::{HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::{
    call_host,
    store::{self, TaskDir},
    Actor, DeployKind, HostRequest, HostResponse,
};
use wsx_core::{ops, runtime};

/// Ends the evidence name of a check-run pass the host recorded in a held result; the
/// manager's ready event repeats it. ref: docs/tasks.md "Check-run"
const RECORDED_BY_HOST: &str = "(recorded by the host)";
const HOST_START_WAIT: Duration = Duration::from_secs(10);
pub fn run(command: TaskCmd) -> Result<()> {
    match command {
        TaskCmd::New(args) => new(args),
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
        } => steer_command(&task, text, project.as_deref(), json),
        TaskCmd::Add {
            task,
            text,
            files,
            project,
            json,
        } => add_command(&task, text, &files, project.as_deref(), json),
        TaskCmd::Stop {
            task,
            project,
            json,
        } => stop_command(&task, project.as_deref(), json),
        TaskCmd::Check {
            action,
            project,
            json,
        } => check(action, project.as_deref(), json),
        TaskCmd::Land {
            task,
            tried,
            said,
            project,
            json,
        } => land_command(task, tried, said, project.as_deref(), json),
        TaskCmd::LandBatch { tasks, project } => run_batch(project.as_deref(), &tasks),
        TaskCmd::Hold {
            task,
            reason,
            off: _,
            project,
            json,
        } => hold_command(&task, reason, project.as_deref(), json),
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
        } => trial_command(&task, end, project.as_deref(), json),
        TaskCmd::Close {
            task,
            discard,
            said,
            project,
            json,
        } => close_command(&task, discard, said, project.as_deref(), json),
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
        TaskCmd::Deploy(args) => deploy(DeployKind::Deploy, args),
        TaskCmd::Release(ReleaseArgs {
            commit,
            run,
            said,
            project,
            json,
        }) => deploy(
            DeployKind::Release,
            DeployArgs {
                commit,
                if_new: false,
                trial: None,
                run,
                said,
                project,
                json,
            },
        ),
        TaskCmd::Push(args) => push(args),
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
        TaskCmd::Project { action } => project(action),
        TaskCmd::Ledger { project, .. } => ledger::ledger(project.as_deref()),
        TaskCmd::Routine { action, owner_said } => routine_command(action, owner_said),
        TaskCmd::Manager { project, json } => manager_command(project.as_deref(), json),
        TaskCmd::Say {
            text,
            role,
            from_manager,
            from_role,
            reply,
            project,
            json,
        } => say_command(
            project.as_deref(),
            role.as_deref(),
            text,
            (from_manager, from_role, reply),
            json,
        ),
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
        } => preview_command(&task, stop, project.as_deref(), json),
        TaskCmd::PreviewHost { project_id, task } => {
            preview::host(&project_id, store::parse_task_number(&task)?)
        }
        TaskCmd::ManagerHost { project_id, role } => manager::run(&project_id, role.as_deref()),
        TaskCmd::BoundaryProfile { project } => boundary_profile(project.as_deref()),
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
