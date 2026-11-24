//! `wsx task`: start a headless worker, message, read and close it.
//! ref: docs/tasks.md

mod agent;
mod args;
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
mod repo;
mod slot;
#[cfg(test)]
mod stream_tests;

use crate::cli::load_config;
use crate::runtime_util::print_json;
use anyhow::{bail, Context, Result};
pub use args::{moved, ChatGroup, Common, Invocation, ProjectGroup, TaskCmd};
use args::{DeployArgs, ReleaseArgs};
use cmd::approve::{approve, deny};
use cmd::chat::{
    boundary_profile, chat, chat_command, dismiss, manager_command, say_command, search,
};
use cmd::check::check;
use cmd::commit::commit;
use cmd::deploy::{deploy, deployed, push, sync};
use cmd::land::{after_command, hold_command, land_command};
use cmd::new::new;
use cmd::project::project;
use cmd::routine::{checkin, digest, post, routine_command};
use cmd::work::{
    add_command, close_command, hand, handback, list, move_command, review, show, stop_command, why,
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

/// Ends the evidence name of a check-run pass the host recorded in a done result; the
/// manager's ready event repeats it. ref: docs/tasks.md "Check-run"
const RECORDED_BY_HOST: &str = "(recorded by the host)";
const HOST_START_WAIT: Duration = Duration::from_secs(10);
pub fn run(command: Invocation, common: Common) -> Result<()> {
    let Common { project, json } = common;
    let project = project.as_deref();
    match command {
        Invocation::Task(TaskCmd::New(args)) => new(args, project, json),
        Invocation::Task(TaskCmd::List { all }) => list(project, all, json),
        Invocation::Task(TaskCmd::Show { task }) => show(&open(project, &task)?, json),
        Invocation::Task(TaskCmd::Why { task }) => why(&open(project, &task)?, json),
        Invocation::Task(TaskCmd::Review { task }) => review(&open(project, &task)?, json),
        Invocation::Project(ProjectGroup::Commit {
            files,
            message,
            said,
        }) => commit(&files, &message, said.owner_said, project, json),
        Invocation::Task(TaskCmd::Add {
            task,
            text,
            files,
            title,
            now,
        }) => add_command(&task, text, &files, title.as_deref(), now, project, json),
        Invocation::Task(TaskCmd::Stop { task }) => stop_command(&task, project, json),
        Invocation::Project(ProjectGroup::Check { action }) => check(action, project, json),
        Invocation::Task(TaskCmd::Land { task, said }) => land_command(task, said, project, json),
        Invocation::Task(TaskCmd::Move { task, chat }) => move_command(&task, &chat, project, json),
        Invocation::Task(TaskCmd::Hold {
            task,
            reason,
            off: _,
        }) => hold_command(&task, reason, project, json),
        Invocation::Task(TaskCmd::After {
            task,
            on,
            then,
            because,
            off: _,
        }) => after_command(project, &task, on, then, because, json),
        Invocation::Task(TaskCmd::Close {
            task,
            discard,
            said,
        }) => close_command(&task, discard, said, project, json),
        Invocation::Task(TaskCmd::Hand { task, back: false }) => hand(&open(project, &task)?, json),
        Invocation::Task(TaskCmd::Hand { task, back: true }) => {
            handback(&open(project, &task)?, json)
        }
        Invocation::Chat(ChatGroup::Approve { id }) => approve(project, &id, json),
        Invocation::Chat(ChatGroup::Deny { id, note }) => deny(project, &id, note, json),
        Invocation::Project(ProjectGroup::Deploy(args)) => {
            deploy(DeployKind::Deploy, args, project, json)
        }
        Invocation::Project(ProjectGroup::Release(ReleaseArgs { commit, run, said })) => deploy(
            DeployKind::Release,
            DeployArgs {
                commit,
                if_new: false,
                quiet: None,
                run,
                said,
            },
            project,
            json,
        ),
        Invocation::Project(ProjectGroup::Push(args)) => push(args, project, json),
        Invocation::Project(ProjectGroup::Sync) => sync(project, json),
        Invocation::Project(ProjectGroup::Deployed) => deployed(project, json),
        Invocation::Task(TaskCmd::Digest { since }) => digest(project, since),
        Invocation::Task(TaskCmd::Post { title, level, key }) => {
            post(project, &title, &level, key.as_deref())
        }
        Invocation::Chat(ChatGroup::Checkin { after, at, why }) => {
            checkin(project, after.as_deref(), at.as_deref(), &why)
        }
        Invocation::Project(ProjectGroup::Manage(action)) => self::project(action, project, json),
        Invocation::Project(ProjectGroup::Run { cwd, why, argv }) => {
            cmd::run::ask(project, cwd, why, argv, json)
        }
        Invocation::Task(TaskCmd::Ran { id }) => cmd::run::ran(project, &id),
        Invocation::Chat(ChatGroup::Ledger) => ledger::ledger(project),
        Invocation::Project(ProjectGroup::Routine {
            mut action,
            owner_said,
        }) => {
            action.inherit(project, json);
            routine_command(action, owner_said)
        }
        Invocation::Chat(ChatGroup::Lead) => manager_command(project, json),
        Invocation::Chat(ChatGroup::Say {
            text,
            from_chat,
            uses: Some(dep),
            ..
        }) => cmd::chat::mail_command(project, &dep, text, from_chat, json),
        Invocation::Chat(ChatGroup::Say {
            text,
            to,
            from_chat,
            reply,
            uses: None,
        }) => say_command(project, to.as_deref(), text, (from_chat, reply), json),
        Invocation::Chat(ChatGroup::Read {
            after,
            around,
            chat: name,
        }) => chat(project, name.as_deref(), (after, around), json),
        Invocation::Chat(ChatGroup::Manage(action)) => chat_command(action, project, json),
        Invocation::Chat(ChatGroup::Search {
            words,
            task,
            all,
            limit,
            reindex,
        }) => search(
            project,
            &words.join(" "),
            task.as_deref(),
            all,
            limit,
            reindex,
            json,
        ),
        Invocation::Chat(ChatGroup::Dismiss { id }) => dismiss(project, id, json),
        // ^ Under a task host's broker the CLI forwards it before parsing (broker.rs).
        Invocation::Task(TaskCmd::CheckRun { .. }) => {
            bail!("`wsx task check-run` runs only inside a task's worker, through its host")
        }
        Invocation::Task(TaskCmd::ManagerHost { project_id, chat }) => {
            manager::run_old(&project_id, chat.as_deref())
        }
        Invocation::Task(TaskCmd::ChatHost { chat }) => manager::run(&chat),
        Invocation::Task(TaskCmd::RepoHost { project_id }) => repo::run(&project_id),
        Invocation::Task(TaskCmd::Slot { command }) => std::process::exit(slot::run(&command)?),
        Invocation::Task(TaskCmd::TouchedScenarios { since }) => {
            cmd::deploy::touched_scenarios(since.as_deref())
        }
        Invocation::Task(TaskCmd::Reap { folder }) => {
            let stopped = wsx_core::process::stop_naming(&folder, wsx_core::process::STOP_GRACE)?;
            println!("stopped {stopped} process(es) naming {}", folder.display());
            Ok(())
        }
        Invocation::Task(TaskCmd::BoundaryProfile { untrusted }) => {
            boundary_profile(project, untrusted)
        }
        Invocation::Task(TaskCmd::Host { project_id, task }) => {
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
    // ^ A landing rebases the branch on the host before it answers.
    let timeout = if matches!(request, HostRequest::Land) {
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
pub(crate) fn launch_manager(main: &Path) -> impl Fn(String, Vec<String>) -> Result<()> + '_ {
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
fn acting_as_chat() -> bool {
    actor() != Actor::Owner
}

fn actor() -> Actor {
    Actor::from_env()
}

/// Whether the manager's agent must ask the owner for an outward command: its configured
/// `tasks.*.approve`, or always when the agent is untrusted (`WSX_TASK_TRUST`, set by its
/// broker). ref: docs/tasks.md "Permissions"
fn asks_owner(configured: bool) -> bool {
    acting_as_chat() && (configured || Trust::from_env() == Trust::Untrusted)
}

/// `--owner-said` and `--after` of an action the manager runs: the owner's message (trusted
/// agents only) and the task whose landing the approved action waits for. `--after` is the
/// manager's and needs the message. ref: docs/tasks.md "Approvals"
fn outward_said(said: &args::OwnerSaidAfter) -> Result<(Option<u64>, Option<u32>)> {
    let owner = owner_said(said.said.owner_said);
    if said.after.is_some() && !(acting_as_chat() && owner.is_some()) {
        bail!("--after is the manager's, with --owner-said from a trusted agent; nothing was recorded");
    }
    Ok((owner, said.after))
}

/// The owner's message that approves an action; an untrusted agent has none: a model's word
/// that the owner asked is no approval.
fn owner_said(said: Option<u64>) -> Option<u64> {
    said.filter(|_| Trust::from_env() == Trust::Trusted)
}
