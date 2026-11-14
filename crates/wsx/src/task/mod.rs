//! `wsx task`: start a headless worker and steer, read and close it.
//! ref: docs/tasks.md

mod agent;
mod args;
mod batch;
mod boundary;
pub mod broker;
mod claude;
mod cmd;
mod egress;
mod floor;
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
pub use args::{Common, TaskCmd};
use args::{DeployArgs, ReleaseArgs};
use cmd::approve::{approve, deny};
use cmd::chat::{
    boundary_profile, chat, chat_command, dismiss, manager_command, say_command, search,
};
use cmd::check::check;
use cmd::commit::commit;
use cmd::deploy::{deploy, deployed, push, trial_command};
use cmd::land::{after_command, hold_command, land_command, run_batch};
use cmd::new::new;
use cmd::project::project;
use cmd::routine::{checkin, digest, post, routine_command};
use cmd::work::{
    add_command, close_command, hand, handback, list, preview_command, review, show, steer_command,
    stop_command, why,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::task::owner::{HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::policy::Trust;
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
pub fn run(command: TaskCmd, common: Common) -> Result<()> {
    let Common { project, json } = common;
    let project = project.as_deref();
    match command {
        TaskCmd::New(args) => new(args, project, json),
        TaskCmd::List { all } => list(project, all, json),
        TaskCmd::Show { task } => show(&open(project, &task)?, json),
        TaskCmd::Why { task } => why(&open(project, &task)?, json),
        TaskCmd::Review { task } => review(&open(project, &task)?, json),
        TaskCmd::Commit {
            files,
            message,
            said,
        } => commit(&files, &message, said.owner_said, project, json),
        TaskCmd::Steer { task, text } => steer_command(&task, text, project, json),
        TaskCmd::Add { task, text, files } => add_command(&task, text, &files, project, json),
        TaskCmd::Stop { task } => stop_command(&task, project, json),
        TaskCmd::Check { action } => check(action, project, json),
        TaskCmd::Land { task, tried, said } => land_command(task, tried, said, project, json),
        TaskCmd::LandBatch { tasks } => run_batch(project, &tasks),
        TaskCmd::Hold {
            task,
            reason,
            off: _,
        } => hold_command(&task, reason, project, json),
        TaskCmd::After {
            task,
            on,
            then,
            because,
            off: _,
        } => after_command(project, &task, on, then, because, json),
        TaskCmd::Trial { task, end } => trial_command(&task, end, project, json),
        TaskCmd::Close {
            task,
            discard,
            said,
        } => close_command(&task, discard, said, project, json),
        TaskCmd::Hand { task, back: false } => hand(&open(project, &task)?, json),
        TaskCmd::Hand { task, back: true } | TaskCmd::Handback { task } => {
            handback(&open(project, &task)?, json)
        }
        TaskCmd::Approve { id } => approve(project, &id, json),
        TaskCmd::Deny { id, note } => deny(project, &id, note, json),
        TaskCmd::Deploy(args) => deploy(DeployKind::Deploy, args, project, json),
        TaskCmd::Release(ReleaseArgs { commit, run, said }) => deploy(
            DeployKind::Release,
            DeployArgs {
                commit,
                if_new: false,
                quiet: None,
                trial: None,
                run,
                said,
            },
            project,
            json,
        ),
        TaskCmd::Push(args) => push(args, project, json),
        TaskCmd::Deployed => deployed(project, json),
        TaskCmd::Digest { since } => digest(project, since),
        TaskCmd::Post { title, level, key } => post(project, &title, &level, key.as_deref()),
        TaskCmd::Checkin { after, at, why } => {
            checkin(project, after.as_deref(), at.as_deref(), &why)
        }
        TaskCmd::Project { action } => self::project(action, project, json),
        TaskCmd::Ledger => ledger::ledger(project),
        TaskCmd::Routine {
            mut action,
            owner_said,
        } => {
            action.inherit(project, json);
            routine_command(action, owner_said)
        }
        TaskCmd::Manager => manager_command(project, json),
        TaskCmd::Say {
            text,
            to,
            from_chat,
            reply,
        } => say_command(project, to.as_deref(), text, (from_chat, reply), json),
        TaskCmd::Chat {
            after,
            around,
            chat: name,
            action,
        } => match action {
            Some(action) => chat_command(action, project, json),
            None => chat(project, name.as_deref(), (after, around), json),
        },
        TaskCmd::Search {
            words,
            task,
            all,
            limit,
            reindex,
        } => search(
            project,
            &words.join(" "),
            task.as_deref(),
            all,
            limit,
            reindex,
            json,
        ),
        TaskCmd::Dismiss { id } => dismiss(project, id, json),
        // ^ Under a task host's broker the CLI forwards it before parsing (broker.rs).
        TaskCmd::CheckRun { .. } => {
            bail!("`wsx task check-run` runs only inside a task's worker, through its host")
        }
        TaskCmd::Preview { task, stop } => preview_command(&task, stop, project, json),
        TaskCmd::PreviewHost { project_id, task } => {
            preview::host(&project_id, store::parse_task_number(&task)?)
        }
        TaskCmd::ManagerHost { project_id, chat } => manager::run(&project_id, chat.as_deref()),
        TaskCmd::BoundaryProfile { untrusted } => boundary_profile(project, untrusted),
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

/// Create the manager's or a chat's hidden session (its label), starting wsxd when it does
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

/// True when the manager's or a chat's agent runs this command (`WSX_TASK_ACTOR`).
/// ^ A guard against the agents' mistakes, not a security boundary: anything that runs
/// commands as the owner can unset it (docs/tasks.md "Approvals").
fn acting_as_manager() -> bool {
    actor() != Actor::Owner
}

fn actor() -> Actor {
    Actor::from_env()
}

/// Who a task created in `project` is by: the acting actor, or, for a manager that the broker
/// of another project's manager host runs here (`WSX_TASK_FROM`), that project's manager.
/// ref: docs/tasks.md "Managers talk"
fn creator(project: &str) -> Actor {
    match (actor(), std::env::var(broker::FROM_ENV)) {
        (Actor::Manager, Ok(from)) if !from.is_empty() && from != project => Actor::ManagerOf(from),
        (actor, _) => actor,
    }
}

/// Whether the manager's agent must ask the owner for an outward command: its configured
/// `tasks.*.approve`, or always when the agent is untrusted (`WSX_TASK_TRUST`, set by its
/// broker). ref: docs/tasks.md "Permissions"
fn asks_owner(configured: bool) -> bool {
    acting_as_manager() && (configured || Trust::from_env() == Trust::Untrusted)
}

/// `--owner-said` and `--after` of an action the manager runs: the owner's message (trusted
/// agents only) and the task whose landing the approved action waits for. `--after` is the
/// manager's and needs the message. ref: docs/tasks.md "Approvals"
fn outward_said(said: &args::OwnerSaidAfter) -> Result<(Option<u64>, Option<u32>)> {
    let owner = owner_said(said.said.owner_said);
    if said.after.is_some() && !(acting_as_manager() && owner.is_some()) {
        bail!("--after is the manager's, with --owner-said from a trusted agent; nothing was recorded");
    }
    Ok((owner, said.after))
}

/// The owner's message that approves an action; an untrusted agent has none: a model's word
/// that the owner asked is no approval.
fn owner_said(said: Option<u64>) -> Option<u64> {
    said.filter(|_| Trust::from_env() == Trust::Trusted)
}
