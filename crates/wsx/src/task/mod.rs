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
use args::RoleCmd;
pub use args::TaskCmd;
use cmd::check::check;
pub(crate) use cmd::check::fail_check;
use cmd::land::{after_command, land_batch};
use cmd::new::{new, Size, Steps};
use cmd::work::{close, hand, handback, list, show};
use serde_json::json;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::git::output_with_timeout_tail;
use wsx_core::task::chat::{self, chat_rows, Names, Window};
use wsx_core::task::checks::{self};
use wsx_core::task::ledger::{default_ref, deploy_status, kind_name};
use wsx_core::task::manager::{
    ApprovalAction, ApprovalLine, Author, Level, ManagerEntry, ManagerEvent, ManagerRequest,
    ManagerResponse,
};
use wsx_core::task::owner::{self, action_name, find_request, HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::roles;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{checked_files, digest_lines, short_commit};
use wsx_core::task::{
    call_host, copy, now_unix_ms,
    store::{self, TaskDir},
    Actor, DeployKind, DeployRecord, HostRequest, HostResponse, TaskRecord, MAX_MESSAGE_BYTES,
};
use wsx_core::task::{search, text};
use wsx_core::{ops, runtime};

/// Ends the evidence name of a check-run pass the host recorded in a held result; the
/// manager's ready event repeats it. ref: docs/tasks.md "Check-run"
const RECORDED_BY_HOST: &str = "(recorded by the host)";
const HOST_START_WAIT: Duration = Duration::from_secs(10);
const DAY_MS: u64 = 24 * 3600 * 1000;
/// The events message of a digest in the chat.
const DIGEST_EVENT: &str = "daily digest";
/// How long a digest waits for a manager turn to end, and how often it asks again.
const DIGEST_TURN_WAIT: Duration = Duration::from_secs(30 * 60);
const DIGEST_TURN_POLL: Duration = Duration::from_secs(5);
/// ^ A deploy or release longer than this is killed with its process group and recorded as
/// failed, so a hung command frees `deploy.lock`. wsx's own release takes about 30 minutes.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

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

/// The remote a push goes to.
const PUSH_REMOTE: &str = "origin";

/// Push the default branch tip, or `commit`, to origin's default branch, then move the local
/// default branch there. A fast-forward of both, unless `force`: then it may rewrite them, with
/// `--force-with-lease` on origin's tip seen when it was asked (`lease`). The manager always
/// asks the owner (or names the owner's message); its broker runs this in the manager host,
/// outside the boundary, so the owner's ssh setup applies. ref: docs/tasks.md "Push"
fn push(
    project: Option<&str>,
    commit: Option<&str>,
    force: bool,
    lease: Option<&str>,
    owner_said: Option<u64>,
    json: bool,
) -> Result<()> {
    not_for_roles("push")?;
    let (project_name, main) = project_entry(project)?;
    let project_id = store::project_id(&project_name, &main);
    let default = default_ref(&main);
    let branch = default.trim_start_matches("refs/heads/").to_string();
    let refuse = |error: anyhow::Error| anyhow::anyhow!("{error:#}; nothing was pushed");
    // ^ One approved push of a project at a time, and origin's tip is read under the lock: two
    // approved pushes ran together once and the older won the ref. A request waits for nothing.
    let asks = acting_as_manager() && owner_said.is_none();
    let _one_at_a_time = if asks {
        None
    } else {
        Some(store::lock_pushes(&project_id).map_err(refuse)?)
    };
    let local = copy::resolve_commit(&main, &default).map_err(refuse)?;
    let target = copy::resolve_commit(&main, commit.unwrap_or(&local)).map_err(refuse)?;
    let remote = copy::remote_tip(&main, PUSH_REMOTE, &branch).map_err(refuse)?;
    let contains =
        |ancestor: &str, commit: &str| copy::is_ancestor(&main, ancestor, commit).unwrap_or(false);
    // ^ A request leases what it saw; when origin moved since, the owner approved another state.
    if let Some(seen) = lease.filter(|seen| *seen != remote.as_deref().unwrap_or_default()) {
        bail!(
            "{PUSH_REMOTE} {branch} moved from {} to {} since the push was asked; ask again. Nothing was pushed",
            if seen.is_empty() { "none" } else { short_commit(seen) },
            remote.as_deref().map_or("none", short_commit)
        );
    }
    let remote_ff = remote.as_deref().is_none_or(|tip| contains(tip, &target));
    if let Some(tip) = remote
        .as_deref()
        .filter(|tip| !force && *tip != target && contains(&target, tip))
    {
        bail!(
            "{} is already on {PUSH_REMOTE} {branch}, which is at {}. Nothing was pushed",
            short_commit(&target),
            short_commit(tip)
        );
    }
    if !force && !remote_ff {
        bail!(
            "{} is not a fast-forward of {PUSH_REMOTE} {branch} ({}); a rewrite needs --force. Nothing was pushed",
            short_commit(&target),
            remote.as_deref().map_or("", short_commit)
        );
    }
    // ^ Local main moves to the pushed commit unless it has it already (lands since the ask).
    let local_moves = !contains(&target, &local);
    let local_ff = contains(&local, &target);
    let rewrite = local_moves && !local_ff;
    if rewrite && !force {
        bail!(
            "local {branch} ({}) is not an ancestor of {}; a rewrite needs --force. Nothing was pushed",
            short_commit(&local),
            short_commit(&target)
        );
    }
    if local_moves {
        copy::check_move(&main, &branch, &target, "push").map_err(refuse)?;
    }
    // ^ A lease that is not origin's tip was refused above, so the push leases what it saw now.
    let lease = force.then(|| remote.clone().unwrap_or_default());
    if acting_as_manager() {
        // ^ The request says what it carries: the landed tasks and the commits origin lacks.
        let mut request = request_of(
            ApprovalAction::Push,
            None,
            Some(&target),
            None,
            lease.as_deref(),
        );
        if let (ApprovalLine::Request { tasks, commits, .. }, Ok((found, count))) = (
            &mut request,
            owner::carried(&project_id, &main, remote.as_deref(), &target),
        ) {
            *tasks = found;
            *commits = Some(count);
        }
        if ask_covering(&main, &project_id, request, owner_said, json)? {
            return Ok(());
        }
    }
    // ^ A rewrite leaves open tasks on commits the new branch lacks: name them.
    let mut old_base = Vec::new();
    if rewrite {
        for id in store::task_numbers(&project_id)? {
            let Ok(record) = TaskDir::open(&project_id, id).and_then(|dir| dir.read_record())
            else {
                continue;
            };
            if record.state.is_open()
                && record.default_branch == branch
                && !contains(&record.base, &target)
            {
                old_base.push(id);
            }
        }
    }
    copy::push(&main, PUSH_REMOTE, &branch, &target, lease.as_deref()).map_err(refuse)?;
    if local_moves {
        copy::move_branch(&main, &branch, &local, &target, rewrite).map_err(|error| {
            anyhow::anyhow!(
                "pushed {} to {PUSH_REMOTE} {branch}, but local {branch} stayed at {}: {error:#}",
                short_commit(&target),
                short_commit(&local)
            )
        })?;
    }
    let mut message = format!(
        "pushed {branch} to {PUSH_REMOTE}: {}",
        short_commit(&target)
    );
    if let Some(was) = remote.as_deref().filter(|was| !contains(was, &target)) {
        message += &format!(" (forced over {})", short_commit(was));
    }
    if rewrite {
        message += &format!("; local {branch} was {}", short_commit(&local));
    }
    if !old_base.is_empty() {
        let tasks: Vec<String> = old_base.iter().map(|id| format!("#{id}")).collect();
        message += &format!("; open tasks on the old {branch}: {}", tasks.join(", "));
    }
    if json {
        return print_json(&json!({
            "pushed": true,
            "remote": PUSH_REMOTE,
            "branch": branch,
            "commit": target,
            "remote_was": remote,
            "local_was": local,
            "forced": force,
            "old_base_tasks": old_base,
            "message": message,
        }));
    }
    println!("{message}");
    Ok(())
}

/// Run the project's deploy or release command in a temporary copy of one commit.
/// ^ Never from a task's working copy: what runs is always a default-branch commit.
/// `if_new`: skip a commit that is already deployed, so the deploys that follow quick lands
/// run once for the newest one (docs/tasks.md "Deploy and release").
/// One by hand checks, takes the lock and answers at once: a detached `wsx task` holding the
/// lock (`run_locked`) runs and records it, so no caller's time limit (the manager's broker)
/// cuts a long release.
#[allow(clippy::too_many_arguments)]
fn deploy(
    kind: DeployKind,
    project: Option<&str>,
    commit: Option<&str>,
    if_new: bool,
    trial: Option<u32>,
    run_locked: Option<i32>,
    owner_said: Option<u64>,
    json: bool,
) -> Result<()> {
    let by_hand = !if_new && trial.is_none() && run_locked.is_none();
    let name = kind_name(kind);
    not_for_roles(name)?;
    let (project_name, main) = project_entry(project)?;
    let settings =
        load_task_config(&main).map_err(|error| anyhow::anyhow!("{error}; nothing was run"))?;
    let (command, public, approve, action) = match kind {
        DeployKind::Deploy => (
            settings.deploy_command.clone(),
            settings.deploy_public,
            settings.deploy_approve,
            ApprovalAction::Deploy,
        ),
        DeployKind::Release => (
            settings.release_command.clone(),
            true,
            settings.release_approve,
            ApprovalAction::Release,
        ),
    };
    let command = command.with_context(|| {
        format!("tasks.{name}.command is not set in wsx.config.yml; nothing was run")
    })?;
    let project_id = store::project_id(&project_name, &main);
    // ^ Only public output waits for what the owner has to check, and a release the owner's
    // message asked for runs: some items can be checked only on the released build.
    // ref: docs/tasks.md "Deploy and release"
    let open = checks::open_items(&project_id)?;
    let to_check = open.len();
    if public && to_check > 0 && run_locked.is_none() {
        match (kind, owner_said) {
            (DeployKind::Release, Some(said)) => owner::check_owner_said(&project_id, said)?,
            _ => bail!(
                "{to_check} item(s) to check are open; the owner checks them first (`wsx task check`). Nothing was run"
            ),
        }
    }
    if approve
        && run_locked.is_none()
        && acting_as_manager()
        && ask_covering(
            &main,
            &project_id,
            request_of(action, None, commit, None, None),
            owner_said,
            json,
        )?
    {
        return Ok(());
    }
    // ^ One deploy or release at a time per project: one by hand refuses while another runs,
    // and one that a land or a trial started waits here.
    let running = match run_locked {
        Some(fd) => store::adopt_deploy_lock(&project_id, fd)?,
        None if by_hand => store::try_lock_deploys(&project_id)?.with_context(|| {
            format!("a deploy or release of {project_name} runs; its result arrives in `wsx task deployed`. Nothing was run")
        })?,
        None => store::lock_deploys(&project_id)?,
    };
    // ^ While a trial holds the live site, only its own deploy runs; the default branch
    // deploys when it ends. ref: docs/tasks.md "Hold and trial"
    let live_trial = store::read_trial(&project_id)?;
    // ^ Each answer is JSON with `--json`, else its message.
    let answer = |value: serde_json::Value| -> Result<()> {
        if json {
            return print_json(&value);
        }
        println!("{}", value["message"].as_str().unwrap_or_default());
        Ok(())
    };
    let skip =
        |message: String| answer(json!({ "kind": kind, "skipped": true, "message": message }));
    let revision = match (trial, &live_trial) {
        (Some(task), Some(live)) if live.task == task && live.commit.is_some() => {
            live.commit.clone().unwrap_or_default()
        }
        (Some(task), _) => return skip(format!("the trial of #{task} ended; nothing was run")),
        (None, Some(live)) if if_new => {
            return skip(format!(
                "the live site runs the trial of #{}; the default branch deploys when it ends",
                live.task
            ))
        }
        (None, Some(live)) => bail!(
            "the live site runs the trial of #{task}; end it first (`wsx task trial {task} --end`). Nothing was run",
            task = live.task
        ),
        (None, None) => match commit {
            Some(commit) => commit.to_string(),
            None => default_ref(&main),
        },
    };
    if if_new {
        let tip = copy::resolve_commit(&main, &revision)?;
        let deployed = store::read_deploys(&project_id)?
            .into_iter()
            .rev()
            .find(|record| record.kind == kind && record.exit == Some(0))
            .is_some_and(|record| record.commit == tip);
        if deployed {
            let message = format!("{name} of {tip:.12}: already deployed");
            return answer(
                json!({ "kind": kind, "commit": tip, "skipped": true, "message": message }),
            );
        }
    }
    let check_suffix = match kind {
        _ if to_check == 0 => String::new(),
        DeployKind::Deploy => format!("; {to_check} to check on the deployed site"),
        DeployKind::Release => format!(
            "; still to check on the release: {}",
            open.iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if by_hand {
        let commit = copy::resolve_commit(&main, &revision)?;
        let log = store::follow_up_log(&project_id)?;
        let fd = running.as_raw_fd().to_string();
        let args = [
            "task",
            name,
            "-p",
            &project_name,
            "--commit",
            &commit,
            "--run-locked",
            &fd,
            "--json",
        ];
        hosting::spawn_wsx(&args, log, Some(&running))?;
        let message = format!(
            "{name} of {commit:.12} started; its output goes to {}, its result to `wsx task deployed`{}",
            store::follow_up_log_path(&project_id)?.display(),
            check_suffix
        );
        return answer(
            json!({ "kind": kind, "commit": commit, "started": true, "message": message }),
        );
    }
    for orphan in copy::remove_orphans(&main, &["deploy", "release"]) {
        eprintln!("removed {}: the run that made it is gone", orphan.display());
    }
    let path = copy::working_copy_path_named(&main, &format!("{name}-{}", std::process::id()))?;
    let commit = copy::create_detached(&main, &revision, &path)
        .map_err(|error| error.context(format!("nothing was run or recorded for this {name}")))?;
    let started = Instant::now();
    // ^ The output's tail is kept like a check's: a failed deploy records it for the ledger.
    let ran = output_with_timeout_tail(
        std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .current_dir(&path)
            .env("WSX_DEPLOY_COMMIT", &commit)
            .stdin(std::process::Stdio::null()),
        DEPLOY_TIMEOUT,
        host::CHECK_OUTPUT_BYTES,
    );
    let duration_ms = started.elapsed().as_millis() as u64;
    ops::cargo_clean(&path);
    let removed = copy::remove(&main, &path, None);
    let (status, output) = match &ran {
        Ok(output) => (Some(output.status), host::output_text(output)),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => (
            None,
            format!("timed out after {} s", DEPLOY_TIMEOUT.as_secs()),
        ),
        Err(error) => (None, format!("cannot run tasks.{name}.command: {error}")),
    };
    // ^ JSON callers parse stdout, so the command's output goes to stderr then.
    if !output.is_empty() {
        if json {
            eprintln!("{output}");
        } else {
            println!("{output}");
        }
    }
    let exit = status.and_then(|status| status.code());
    let succeeded = status.is_some_and(|status| status.success());
    let record = DeployRecord {
        kind,
        commit: commit.clone(),
        exit,
        unix_ms: now_unix_ms(),
        duration_ms,
        trial,
        output: (!succeeded).then(|| output.clone()),
    };
    store::append_deploy(&project_id, &record)?;
    // ^ A trial whose deploy failed ends, and the default branch deploys again.
    if let (Some(task), false) = (trial, succeeded) {
        trial::end(
            &project_id,
            &project_name,
            Some(task),
            "its deploy failed",
            false,
        )?;
    }
    removed.with_context(|| {
        format!(
            "the {name} was recorded, but its copy {} remains",
            path.display()
        )
    })?;
    let Some(status) = status else {
        bail!("tasks.{name}.command: {output}; recorded with no exit code");
    };
    let mut message = format!(
        "{name} of {commit:.12}: exit {}",
        exit.map_or("none".into(), |code| code.to_string())
    );
    if status.success() {
        message.push_str(&check_suffix);
    }
    let mut value = serde_json::to_value(&record)?;
    value["message"] = json!(message);
    answer(value)?;
    if !status.success() {
        bail!("tasks.{name}.command failed ({status}); the failure is recorded and nothing else changed");
    }
    Ok(())
}

/// `wsx task trial <n>`: take the project's trial slot and have the host put the ready
/// revision on the live site; `--end` frees the slot and deploys the default branch again.
/// ref: docs/tasks.md "Hold and trial"
fn trial_command(dir: &TaskDir, end: bool, json: bool) -> Result<()> {
    let record = dir.read_record()?;
    if end {
        let ended = trial::end(
            &dir.project_id,
            &record.project,
            Some(record.id),
            "ended without a decision",
            false,
        )?;
        let message = match ended {
            Some(_) => "the trial ended; the default branch deploys again",
            None => "no trial of this task runs",
        };
        if json {
            return print_json(&json!({ "ended": ended.is_some(), "message": message }));
        }
        println!("t{} {message}", record.id);
        return Ok(());
    }
    let settings = load_task_config(&record.project_path)
        .map_err(|error| anyhow::anyhow!("{error}; nothing was tried"))?;
    if settings.deploy_command.is_none() {
        bail!("tasks.deploy.command is not set: there is no live site to try it on");
    }
    if settings.deploy_public {
        bail!("tasks.deploy.public is on: unlanded work does not go public; try it on a preview");
    }
    // ^ A trial changes the live site: a manager that may not deploy alone asks the owner
    // to start it.
    if acting_as_manager() && settings.deploy_approve {
        bail!("tasks.deploy.approve is on: ask the owner to start the trial (Try live)");
    }
    if !trial::claim(&record, actor())? {
        let trial = store::read_trial(&dir.project_id)?;
        let message = format!(
            "the trial of t{} already runs{}",
            record.id,
            trial
                .and_then(|trial| trial.commit)
                .map_or(String::new(), |commit| format!(" at {commit:.12}"))
        );
        if json {
            return print_json(&json!({ "ok": true, "state": record.state, "message": message }));
        }
        println!("{message}");
        return Ok(());
    }
    let response = ask_host(dir, &HostRequest::Trial);
    // ^ A host that refused or did not answer leaves the slot free again.
    if !response.as_ref().is_ok_and(|response| response.ok) {
        let _ = store::change_trial(&dir.project_id, |current| match current {
            Some(trial) if trial.task == record.id && trial.commit.is_none() => (None, ()),
            other => (other, ()),
        });
    }
    report_response(dir, response?, json)
}

/// When the newest digest in `log` was posted: the next one starts there. A run with nothing
/// to say posts nothing, so the next one covers its time too.
fn last_digest_ms(log: &[ManagerEntry]) -> Option<u64> {
    log.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Message {
            from: Author::Events,
            text,
            ..
        } if text == DIGEST_EVENT => Some(entry.unix_ms),
        _ => None,
    })
}

/// The routine a check-in writes; one per project, so a manager cannot pile them up.
const CHECKIN: &str = "checkin";
/// The furthest ahead a check-in may be.
const CHECKIN_MAX_SECS: i64 = 7 * 24 * 3600;
const CHECKIN_WHY_CHARS: usize = 500;

/// Write routine `checkin`: one run at the chosen minute whose argv wsx fixes (`printf` of
/// `why`), so it needs no `--owner-said`; its output reaches the manager as the run's event.
/// ref: docs/tasks.md "Routines"
fn checkin(project: Option<&str>, after: Option<&str>, at: Option<&str>, why: &str) -> Result<()> {
    use wsx_core::routine::{Action, Routine, Trigger};
    not_for_roles("a check-in")?;
    let why = why.trim();
    if why.is_empty() || why.chars().count() > CHECKIN_WHY_CHARS {
        bail!("--why must say what to look at, in at most {CHECKIN_WHY_CHARS} characters");
    }
    let now = (now_unix_ms() / 1000) as i64;
    let at_secs = match (after, at) {
        (Some(after), _) => now + duration_secs(after)?,
        (None, Some(at)) => {
            wsx_core::routine::local::local_minute(at, now).map_err(anyhow::Error::msg)? * 60
        }
        (None, None) => bail!("pass --in or --at"),
    };
    if at_secs <= now || at_secs - now > CHECKIN_MAX_SECS {
        bail!("a check-in is between 1 minute and 7 days ahead");
    }
    // ^ The tick runs a minute that has begun: round up, so it never runs early.
    let epoch_minute = (at_secs + 59) / 60;
    let path = crate::cli::routine_project(project)?;
    let (revision, routines) = crate::cli::fetch_routines(&path)?;
    let routine = Routine {
        name: CHECKIN.into(),
        trigger: Trigger::At { epoch_minute },
        command: vec!["printf".into(), "%s\\n".into(), why.into()],
        prompt: String::new(),
        enabled: true,
        // ^ The why is for the manager, as the run's event; the owner asked for none.
        post: false,
        findings_exit: Vec::new(),
    }
    .validated()?;
    let action = match routines.iter().any(|view| view.routine.name == CHECKIN) {
        true => Action::Edit {
            revision,
            old_name: CHECKIN.into(),
            routine,
        },
        false => Action::Add { revision, routine },
    };
    crate::cli::send_routine(&path, action)?;
    println!(
        "check-in {}",
        wsx_core::routine::local::trigger_text(&Trigger::At { epoch_minute })
    );
    Ok(())
}

/// `40m`, `2h`, `1d` in seconds.
fn duration_secs(text: &str) -> Result<i64> {
    let text = text.trim();
    let unit = match text.chars().last() {
        Some('m') => 60,
        Some('h') => 3600,
        Some('d') => 24 * 3600,
        _ => bail!("--in takes minutes, hours or days: 40m, 2h, 1d"),
    };
    let count: i64 = text[..text.len() - 1]
        .parse()
        .with_context(|| format!("--in takes minutes, hours or days: {text}"))?;
    Ok(count.saturating_mul(unit))
}

/// Post the project's digest. Each try reads the lines again and starts from the chat log, so
/// a run that fails leaves its time to the next one. ref: docs/tasks.md "Routines"
fn digest(project: Option<&str>, since: Option<u64>) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let after_ms = since.map_or_else(
        || {
            let log = ManagerDir::existing(&project_id)
                .ok()
                .flatten()
                .and_then(|dir| dir.read_log().ok())
                .unwrap_or_default();
            last_digest_ms(&log).unwrap_or_else(|| now_unix_ms().saturating_sub(DAY_MS))
        },
        |seconds| seconds.saturating_mul(1000),
    );
    post_when_free(&project_id, &name, DIGEST_EVENT, None, || {
        digest_lines(&project_id, after_ms)
    })
}

/// The most of stdin `post` reads.
const POST_BYTES: u64 = 16 * 1024;

/// A post's `--key` is 1-64 characters of `[A-Za-z0-9._-]`.
fn post_key(key: &str) -> Result<String, String> {
    let fits = (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    match fits {
        true => Ok(key.to_string()),
        false => Err("a key is 1-64 characters of A-Z a-z 0-9 . _ -".into()),
    }
}

/// The same key within this long of the last kept post is dropped.
const POST_KEY_WINDOW_SECS: u64 = 10 * 60;

/// `wsx task post`: stdin's lines to the chat, as the digest posts its own. It needs only the
/// state folder (`HOME` or `XDG_STATE_HOME`) and the running manager host, no `WSX_*`
/// variable. ref: docs/tasks.md "Routines"
fn post(project: Option<&str>, title: &str, level: &str, key: Option<&str>) -> Result<()> {
    use std::io::Read;
    let (name, main) = project_entry(project)?;
    let title = title.trim();
    if title.is_empty() || title.contains('\n') {
        bail!("--title must be one line");
    }
    let level = match level {
        "warn" => Some(Level::Warn),
        "bad" => Some(Level::Bad),
        _ => None,
    };
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(POST_BYTES)
        .read_to_end(&mut bytes)
        .context("cannot read stdin")?;
    // ^ The cut at POST_BYTES may split a character.
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.trim().lines().map(str::to_string).collect();
    let project_id = store::project_id(&name, &main);
    let send = || post_when_free(&project_id, &name, title, level, || lines.clone());
    match key {
        Some(key) if !lines.is_empty() => {
            let now = now_unix_ms() / 1000;
            if !store::post_once(&project_id, key, now, POST_KEY_WINDOW_SECS, send)? {
                println!("deduped: {key}");
            }
            Ok(())
        }
        _ => send(),
    }
}

/// Post `lines()` as wsx's `event` message to the chat, with no manager turn: none when it is
/// empty. A turn that runs refuses it, so this tries again for up to 30 minutes, at once
/// failing when the manager itself runs it inside that turn. ref: docs/tasks.md "Routines"
fn post_when_free(
    project_id: &str,
    name: &str,
    event: &str,
    level: Option<Level>,
    mut lines: impl FnMut() -> Vec<String>,
) -> Result<()> {
    // ^ The manager's own command runs inside the turn it would wait for.
    let wait = match acting_as_manager() {
        true => Duration::ZERO,
        false => DIGEST_TURN_WAIT,
    };
    let deadline = Instant::now() + wait;
    loop {
        let lines = lines();
        if lines.is_empty() {
            return Ok(());
        }
        let response = owner::post(project_id, event.into(), lines.join("\n"), level)
            .with_context(|| format!("the manager host of {name} is not running"))?;
        if response.ok {
            return Ok(());
        }
        if !response.busy || Instant::now() >= deadline {
            bail!("the {event} was not posted: {}", response.message);
        }
        std::thread::sleep(DIGEST_TURN_POLL);
    }
}

/// The last successful deploy and release and how many default-branch commits each lacks.
fn deployed(project: Option<&str>, json: bool) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let mut out = deploy_status(&project_name, &main, &settings)?;
    let default = out["default_branch"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    out["project"] = json!(project_name);
    if json {
        return print_json(&out);
    }
    for kind in [DeployKind::Deploy, DeployKind::Release] {
        let entry = &out[kind_name(kind)];
        match entry["commit"].as_str() {
            Some(commit) => println!(
                "{:<8} {:.12}  {} commit(s) behind {default}",
                kind_name(kind),
                commit,
                entry["behind"]
            ),
            None => println!("{:<8} none", kind_name(kind)),
        }
    }
    Ok(())
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

fn ensure_manager(project: Option<&str>) -> Result<(bool, ManagerDir)> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_manager(&name, &main, &exe, None, launch_manager(&main))
}

/// Who sends a message with `say`, besides the owner; each with whether it is a reply.
enum From {
    /// `--from-manager`: this manager to another project's.
    Manager(bool),
    /// `--from-role`: the manager or a role to a role or the manager of its own project.
    Role(bool),
}

/// Send to the manager, or to the open role `role`.
fn say(
    project: Option<&str>,
    role: Option<&str>,
    text: String,
    from: Option<From>,
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    // ^ Only a manager host sets the sender's project (broker.rs).
    let own = || {
        std::env::var(broker::FROM_ENV)
            .ok()
            .filter(|own| acting_as_manager() && !own.is_empty())
    };
    let from = match from {
        None => None,
        Some(From::Manager(reply)) => {
            let own = own()
                .filter(|_| actor() == Actor::Manager)
                .context("--from-manager is for a project's manager")?;
            if own == name {
                bail!("--from-manager messages another project's manager");
            }
            Some(wsx_core::task::manager::Author::Manager {
                project: own,
                reply,
            })
        }
        Some(From::Role(reply)) => {
            let own = own().context("--from-role is for the manager or a role")?;
            if own != name {
                bail!("--from-role messages a role or the manager of its own project");
            }
            let sender = match actor() {
                Actor::Role(sender) => sender,
                _ => "manager".to_string(),
            };
            if sender == role.unwrap_or("manager") {
                bail!("--from-role messages another role or the manager, not itself");
            }
            Some(wsx_core::task::manager::Author::Role {
                name: sender,
                reply,
            })
        }
    };
    // ^ `--role manager` is the manager, as a role names it in its reply.
    let role = role.filter(|role| *role != "manager");
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let sent = from.is_some().then(|| text.clone());
    let response = owner::say(
        &name,
        &main,
        &exe,
        role,
        launch_manager(&main),
        text,
        from,
        Vec::new(),
    )?;
    // ^ A manager's or a role's message always answers in JSON: its host logs `sent` from
    // the `text` (broker.rs). ref: docs/tasks.md "Managers talk"
    if json || sent.is_some() {
        let mut value = serde_json::to_value(&response)?;
        if let Some(text) = sent {
            value["text"] = json!(text);
        }
        print_json(&value)?;
    } else {
        println!(
            "message {}: {}",
            response.id.map_or("-".into(), |id| id.to_string()),
            response.message
        );
    }
    if !response.ok {
        bail!("the manager refused the message: {}", response.message);
    }
    Ok(())
}

/// The conversation after message `after`, around message `around`, or from the oldest entry that
/// shows `find`: messages, replies, rotations and stops; `asks`, the open questions (docs/tasks.md "Log window").
fn chat(
    project: Option<&str>,
    role: Option<&str>,
    (after, find, around): (Option<u64>, Option<&str>, Option<u64>),
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let dir = match role {
        None => ManagerDir::open(&project_id)?,
        Some(role) => ManagerDir::existing_as(&project_id, Some(role))?
            .with_context(|| format!("the project has no role {role}"))?,
    };
    let window = Window::new(find, after, around);
    let now = wsx_core::task::now_unix_ms();
    let shown = load_task_config(&main)
        .unwrap_or_default()
        .manager_shown_name(&name)
        .to_string();
    let own = role.unwrap_or(&shown);
    let names = Names {
        own,
        manager: &shown,
        peer: &|other: &str| other.to_string(),
    };
    let page = chat::page(&dir, &window, &names, &[], now, |_| {})?;
    let asks: Vec<u64> = page.asks.iter().map(|ask| ask.id).collect();
    if json {
        return print_json(
            &json!({ "entries": page.entries, "skipped": page.skipped, "asks": asks }),
        );
    }
    for entry in &page.entries {
        match &entry.event {
            ManagerEvent::Message { id, from, text, .. } => println!("[{id}] {from:?}: {text}"),
            ManagerEvent::Reply { id, text, .. } if asks.contains(id) => {
                println!("[{id}] manager (asks): {text}")
            }
            ManagerEvent::Reply { id, text, .. } => println!("[{id}] manager: {text}"),
            // ^ A pause, a new conversation and a stop read as the other chats say them.
            ManagerEvent::Limit { .. }
            | ManagerEvent::Rotated { .. }
            | ManagerEvent::Stopped { .. } => {
                for row in chat_rows(&[entry], &[], &[], &names) {
                    println!("({})", row.text);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `wsx task search <words>`: ranked hits in one project or every registered one, each with
/// the reference to quote it by. ref: docs/tasks.md "Search"
fn search(
    project: Option<&str>,
    words: &str,
    task: Option<&str>,
    all: bool,
    limit: Option<usize>,
    reindex: bool,
    json: bool,
) -> Result<()> {
    let mut only = None;
    if let Some(task) = task {
        let dir = open(project, task)?;
        if words.trim().is_empty() {
            return history(&dir, limit.unwrap_or(HISTORY_LINES), json);
        }
        only = Some(dir.id);
    }
    let current = project_entry(project).ok().map(|(name, _)| name);
    let targets: Vec<search::Target> = match all {
        true => load_config()?
            .projects
            .iter()
            .map(|project| search::Target {
                name: project.name.clone(),
                project_id: store::project_id(&project.name, &project.path),
            })
            .collect(),
        false => {
            let (name, main) = project_entry(project)?;
            vec![search::Target {
                project_id: store::project_id(&name, &main),
                name,
            }]
        }
    };
    let started = Instant::now();
    let found = search::search(
        &targets,
        current.as_deref(),
        words,
        limit.unwrap_or(SEARCH_HITS),
        reindex,
        only,
    )?;
    if json {
        let mut value = serde_json::to_value(&found)?;
        value["ms"] = json!(started.elapsed().as_millis() as u64);
        return print_json(&value);
    }
    for hit in &found.hits {
        let reference = hit.quote.as_deref().map_or(hit.reference.clone(), |quote| {
            format!("{}  {quote}", hit.reference)
        });
        let outcome = hit
            .outcome
            .as_deref()
            .map_or(String::new(), |o| format!(" [{o}]"));
        println!(
            "{reference}{outcome}  {} {}  {}",
            text::day_of(hit.unix_ms),
            hit.by,
            hit.line
        );
    }
    for problem in &found.problems {
        eprintln!("skipped {problem}");
    }
    if found.hits.is_empty() {
        println!("nothing found in {} records", found.docs);
    }
    Ok(())
}

/// The lines of `wsx task search --task <n>` shown without `--limit`, and the hits of a search.
const HISTORY_LINES: usize = 40;
const SEARCH_HITS: usize = 10;

/// `wsx task search --task <n>`: the task's story as dated lines, newest last.
/// ref: docs/tasks.md "Search"
fn history(dir: &TaskDir, limit: usize, json: bool) -> Result<()> {
    let history = wsx_core::task::history::read(dir, limit)?;
    if json {
        return print_json(&serde_json::to_value(&history)?);
    }
    println!("t{} [{}]  {}", history.task, history.outcome, history.title);
    if history.skipped > 0 {
        println!(
            "({} earlier lines left out; --limit shows more)",
            history.skipped
        );
    }
    for entry in &history.entries {
        println!(
            "{} {:<8} {}",
            wsx_core::task::history::stamp(entry.unix_ms),
            entry.label,
            entry.text
        );
    }
    Ok(())
}

/// `wsx task dismiss <id>`: the owner's; the manager answers a question, it does not close it.
/// ref: docs/tasks.md "Manager"
fn dismiss(project: Option<&str>, id: u64, json: bool) -> Result<()> {
    owner_only("dismiss")?;
    let (name, main) = project_entry(project)?;
    let dir = ManagerDir::open(&store::project_id(&name, &main))?;
    chat::dismiss(&dir, id, wsx_core::task::now_unix_ms())?;
    let message = format!("reply {id} asks no more");
    if json {
        return print_json(&json!({ "ok": true, "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// `wsx task role new|list|close`. ref: docs/tasks.md "Roles"
fn role(action: RoleCmd) -> Result<()> {
    let open_project = |project: Option<&str>| -> Result<(String, String, PathBuf)> {
        let (name, main) = project_entry(project)?;
        let project_id = store::project_id(&name, &main);
        // ^ A role idle past tasks.roles.closeDays is closed before it is counted or listed.
        if let Ok(settings) = load_task_config(&main) {
            roles::close_idle(&project_id, settings.roles_close_secs);
        }
        Ok((name, project_id, main))
    };
    match action {
        RoleCmd::New {
            name,
            brief,
            said,
            project,
            json,
        } => {
            not_for_roles("role new")?;
            let (_, project_id, main) = open_project(project.as_deref())?;
            let settings = load_task_config(&main)
                .map_err(|error| anyhow::anyhow!("{error}; no role was opened"))?;
            roles::may_create(&project_id, &name, settings.roles_max)?;
            let brief = read_brief(&brief)?;
            // ^ The manager asks the owner once: a Yes/No card, or the owner's own word.
            if actor() == Actor::Manager
                && ask_owner(
                    &project_id,
                    ApprovalAction::Role,
                    None,
                    None,
                    Some((&name, &brief)),
                    None,
                    said.owner_said,
                    json,
                )?
            {
                return Ok(());
            }
            roles::create(&project_id, &name, &brief)?;
            let message = format!("role {name} is open; `wsx task say --role {name}` talks to it");
            if json {
                return print_json(&json!({ "role": name, "state": "open", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        RoleCmd::List { project, json } => {
            let (_, project_id, _) = open_project(project.as_deref())?;
            let lines = store::read_approvals(&project_id)?;
            let mut rows = roles::rows(&project_id, &lines);
            for row in rows.iter_mut().filter(|row| row["state"] != "asked") {
                let socket = ManagerDir::existing_as(&project_id, row["name"].as_str())?
                    .map(|dir| dir.socket_path());
                row["running"] = json!(socket.is_some_and(|socket| {
                    wsx_core::task::call_socket::<_, ManagerResponse>(
                        &socket,
                        &ManagerRequest::Status,
                        Duration::from_secs(2),
                    )
                    .is_ok()
                }));
            }
            if json {
                return print_json(&json!({ "roles": rows }));
            }
            for row in &rows {
                println!(
                    "{} {}",
                    row["name"].as_str().unwrap_or_default(),
                    row["state"].as_str().unwrap_or_default()
                );
            }
            Ok(())
        }
        RoleCmd::Close {
            name,
            project,
            json,
        } => {
            not_for_roles("role close")?;
            let (_, project_id, _) = open_project(project.as_deref())?;
            let dir = roles::open_role(&project_id, &name)?;
            roles::close(&dir, &format!("closed by the {}", String::from(actor())))?;
            let message = format!("role {name} is closed; its notes stay");
            if json {
                return print_json(&json!({ "role": name, "state": "closed", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
    }
}

/// A role's brief from a file, or `-` for standard input.
fn read_brief(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let limit = roles::MAX_BRIEF_BYTES as u64 + 1;
    match path.as_os_str() == "-" {
        true => std::io::stdin().lock().take(limit).read_to_end(&mut bytes),
        false => std::fs::File::open(path)
            .with_context(|| format!("cannot read the brief {}", path.display()))?
            .take(limit)
            .read_to_end(&mut bytes),
    }
    .context("cannot read the brief")?;
    let brief = String::from_utf8(bytes).context("the brief is not UTF-8 text")?;
    roles::checked_brief(&brief)?;
    Ok(brief)
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

/// A request for `action`; `ask_owner` and `ask_covering` record it.
fn request_of(
    action: ApprovalAction,
    task: Option<&TaskRecord>,
    commit: Option<&str>,
    role: Option<(&str, &str)>,
    lease: Option<&str>,
) -> ApprovalLine {
    ApprovalLine::Request {
        id: String::new(),
        action,
        task: task.map(|record| record.id),
        revision: task.map(|record| record.revision),
        commit: commit.map(str::to_string),
        role: role.map(|(name, _)| name.to_string()),
        brief: role.map(|(_, brief)| brief.to_string()),
        lease: lease.map(str::to_string),
        tasks: Vec::new(),
        commits: None,
        unix_ms: now_unix_ms(),
    }
}

/// The manager's action that needs the owner: approved by the owner's message `owner_said`
/// of its running turn (recorded; `false`: run it), else a request (`true`: the owner was
/// asked). ref: docs/tasks.md "Approvals"
#[allow(clippy::too_many_arguments)]
fn ask_owner(
    project_id: &str,
    action: ApprovalAction,
    task: Option<&TaskRecord>,
    commit: Option<&str>,
    role: Option<(&str, &str)>,
    lease: Option<&str>,
    owner_said: Option<u64>,
    json: bool,
) -> Result<bool> {
    let request = request_of(action, task, commit, role, lease);
    Ok(submit(project_id, request, owner_said, json)?.1)
}

/// `ask_owner` for a push, deploy, release or discard: the owner sees only the newest
/// request, so each waiting one of the same action that the new one covers is dropped
/// ("replaced by <id>"). Requests that do not cover each other stay separate.
/// ref: docs/tasks.md "Approvals"
fn ask_covering(
    main: &Path,
    project_id: &str,
    request: ApprovalLine,
    owner_said: Option<u64>,
    json: bool,
) -> Result<bool> {
    let covered = covered_requests(main, project_id, &request)?;
    let (id, asked) = submit(project_id, request, owner_said, json)?;
    for old in covered {
        if let Err(error) = owner::drop_request(project_id, &old, &format!("replaced by {id}")) {
            eprintln!("cannot drop approval {old}: {error:#}");
        }
    }
    Ok(asked)
}

/// The ids of the waiting requests that `new` covers: the same task for a discard; for a
/// push, deploy or release, a commit that `new`'s commit contains (no commit: the default
/// branch tip). A push with a lease covers a push without one, not the other way round.
fn covered_requests(main: &Path, project_id: &str, new: &ApprovalLine) -> Result<Vec<String>> {
    let ApprovalLine::Request {
        action,
        task,
        commit,
        lease,
        ..
    } = new
    else {
        return Ok(Vec::new());
    };
    let lines = store::read_approvals(project_id)?;
    let default = default_ref(main);
    let resolve = |commit: &Option<String>| {
        copy::resolve_commit(main, commit.as_deref().unwrap_or(&default)).ok()
    };
    let newest = resolve(commit);
    Ok(wsx_core::task::manager::pending_approvals(&lines)
        .into_iter()
        .filter_map(|old| {
            let ApprovalLine::Request {
                id,
                action: old_action,
                task: old_task,
                commit: old_commit,
                lease: old_lease,
                ..
            } = old
            else {
                return None;
            };
            let covers = old_action == action
                && match action {
                    ApprovalAction::Discard => task.is_some() && old_task == task,
                    ApprovalAction::Push | ApprovalAction::Deploy | ApprovalAction::Release => {
                        (old_lease.is_none() || lease.is_some())
                            && matches!((resolve(old_commit), &newest), (Some(old), Some(new))
                                if copy::is_ancestor(main, &old, new).unwrap_or(false))
                    }
                    ApprovalAction::Land | ApprovalAction::Role => false,
                };
            covers.then(|| id.clone())
        })
        .collect())
}

/// Record `request`, or, with `owner_said`, record it approved. Returns its id and whether the
/// owner was asked.
fn submit(
    project_id: &str,
    request: ApprovalLine,
    owner_said: Option<u64>,
    json: bool,
) -> Result<(String, bool)> {
    if let Some(said) = owner_said {
        let id = owner::approve_by_message(project_id, request, said)?;
        if !json {
            println!("{id}: approved by owner message {said}");
        }
        return Ok((id, false));
    }
    let ApprovalLine::Request {
        action,
        task,
        revision,
        role,
        ..
    } = &request
    else {
        bail!("only a request is asked");
    };
    let (action, target) = (
        *action,
        match (task, revision, role) {
            (Some(task), Some(revision), _) => format!(" #{task} r{revision}"),
            (_, _, Some(role)) => format!(" {role}"),
            _ => String::new(),
        },
    );
    let line = store::append_approval(project_id, request)?;
    let id = line.id();
    let message = format!(
        "{}{target} needs the owner's approval; the owner was asked ({id}). Do not retry it",
        action_name(action)
    );
    if json {
        print_json(&json!({ "ok": true, "approval": id, "message": message }))?;
    } else {
        println!("{message}");
    }
    Ok((id.to_string(), true))
}

fn owner_only(command: &str) -> Result<()> {
    if acting_as_manager() {
        bail!("wsx task {command} is for the owner; the manager cannot decide its own requests");
    }
    Ok(())
}

/// Run an approved request's action as its own `wsx task` command, without the actor mark.
fn approve(project: Option<&str>, id: &str, json: bool) -> Result<()> {
    owner_only("approve")?;
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let action = find_request(
        &store::read_approvals(&store::project_id(&name, &main))?,
        id,
    )
    .ok()
    .and_then(|line| match line {
        ApprovalLine::Request { action, .. } => Some(*action),
        ApprovalLine::Decision { .. } => None,
    });
    let approved = owner::approve(&name, &main, id, &exe)?;
    if json {
        print_json(&json!({
            "approval": id,
            "approved": true,
            "ok": approved.ok,
            "outcome": approved.outcome,
            "result": approved.result,
        }))?;
    } else {
        println!("{id} approved; {}", approved.outcome);
    }
    if !approved.ok {
        bail!(
            "the approved {} failed; the decision and its outcome are recorded",
            action.map_or("action", action_name)
        );
    }
    Ok(())
}

fn deny(project: Option<&str>, id: &str, note: Option<String>, json: bool) -> Result<()> {
    owner_only("deny")?;
    let (name, main) = project_entry(project)?;
    owner::deny(&name, &main, id, note)?;
    if json {
        print_json(&json!({ "approval": id, "approved": false }))
    } else {
        println!("{id} denied");
        Ok(())
    }
}
