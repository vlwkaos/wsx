//! `wsx task`: start a headless worker and steer, read and close it.
//! ref: docs/tasks.md

pub mod broker;
mod claude;
pub mod host;
mod hosting;
mod ledger;
pub mod manager;
mod preview;
mod slot;
mod trial;

use crate::cli::{
    existing_daemon_snapshot, load_config, load_full_workspace, resolve_project, PromptSource,
};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde_json::json;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::checks::{self, CheckItem};
use wsx_core::task::ledger::{default_ref, deploy_status, kind_name};
use wsx_core::task::manager::{
    ApprovalAction, ApprovalLine, GrantAsk, ManagerEvent, ManagerRequest, ManagerResponse,
};
use wsx_core::task::owner::{self, action_name, find_request, HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{
    checked_files, overlaps, project_tasks, task_summary, MAX_RESULT_READ_BYTES,
};
use wsx_core::task::{
    call_host, call_socket, copy, now_unix_ms,
    store::{self, TaskDir, REPORT_FILE, RESULT_FILE},
    Access, Actor, DeployKind, DeployRecord, HostRequest, HostResponse, LogEvent, MessageKind,
    TaskRecord, TaskState, MAX_MESSAGE_BYTES,
};
use wsx_core::{ops, runtime};

const HOST_START_WAIT: Duration = Duration::from_secs(10);
const CLOSE_WAIT: Duration = Duration::from_secs(15);

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Start a worker on a new branch in its own working copy
    New {
        #[command(flatten)]
        prompt: PromptSource,
        /// Start from this branch, tag or commit instead of the default branch
        #[arg(long, value_name = "REF")]
        from: Option<String>,
        /// A short title in plain words (at most 60 characters); default: the prompt's first line
        #[arg(long)]
        title: Option<String>,
        /// The worker's model; default: tasks.agent.model (opus)
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        /// Hold the task for the owner to try before it lands, and say why
        #[arg(long, value_name = "WHY")]
        hold: Option<String>,
        /// A glob of the paths it expects to change, relative to the checkout (repeatable)
        #[arg(long, value_name = "GLOB")]
        files: Vec<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List a project's tasks (closed ones with --all)
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one task with its latest result
    Show {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message into the running turn
    Steer {
        task: String,
        #[command(flatten)]
        text: PromptSource,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message after the current turn
    Add {
        task: String,
        #[command(flatten)]
        text: PromptSource,
        /// Add a glob of the paths it expects to change (repeatable)
        #[arg(long, value_name = "GLOB")]
        files: Vec<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Let a task reach a host, write a folder or use a tool (never Bash), and resume it when blocked;
    /// `allow manager --path <dir>` lets the project's manager write a folder
    Allow {
        /// A task number, or `manager`
        task: String,
        #[command(flatten)]
        access: AccessArgs,
        /// For `allow manager`: until this duration (30m, 2h, 1d) or local time (18:00) passes;
        /// without it, until the manager's conversation ends
        #[arg(long)]
        until: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Interrupt the worker; the next add resumes the same conversation
    Stop {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bring a ready task onto the default branch
    Land {
        task: String,
        /// The owner passed its trial: its Human Verify items are not added to To check
        #[arg(long)]
        tried: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Hold a task for the owner to try before it lands, or release it with --off
    Hold {
        task: String,
        /// Why the owner should try it first
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        reason: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put a ready task's revision on the live site to try before it lands, or end the trial
    Trial {
        task: String,
        /// End the trial and deploy the default branch again
        #[arg(long)]
        end: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove the working copy, session and branch
    Close {
        task: String,
        /// Close although the branch has commits that are not on the default branch
        #[arg(long)]
        discard: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Work on the task by hand in its working copy, with the worker's conversation (owner only)
    Hand {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Give a task worked by hand back: its commits are checked as the next revision (owner only)
    Handback {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List what the owner has to check of the landed tasks, or pass or fail items (owner only)
    Check {
        #[command(subcommand)]
        action: Option<CheckAction>,
        #[arg(short, long, global = true)]
        project: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Run tasks.deploy.command in a temporary copy of one default-branch commit and record it
    Deploy {
        /// Commit, tag or branch to deploy; default: the default branch tip
        #[arg(long, value_name = "REF")]
        commit: Option<String>,
        /// Only when the default branch tip is not deployed yet, after a running deploy ends
        #[arg(long, conflicts_with = "commit")]
        if_new: bool,
        /// Deploy the revision in this task's trial slot (the task host starts it)
        #[arg(long, hide = true, value_name = "TASK", conflicts_with_all = ["commit", "if_new"])]
        trial: Option<u32>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run tasks.release.command the same way and record it
    Release {
        /// Commit, tag or branch to release; default: the default branch tip
        #[arg(long, value_name = "REF")]
        commit: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the bounded project state a manager reads first (JSON, at most 16 KiB)
    Ledger {
        #[arg(short, long)]
        project: Option<String>,
        /// Accepted like on every task command; the ledger is always JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the last deployed and released commits and how far each is behind
    Deployed {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Start the project's manager agent, or report the running one
    Manager {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message to the project's manager; returns at once with its id
    Say {
        #[command(flatten)]
        text: PromptSource,
        /// From a project's manager to the manager of the project named with -p
        #[arg(long)]
        from_manager: bool,
        /// With --from-manager: this answers a message that manager sent
        #[arg(long, requires = "from_manager")]
        reply: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read the manager conversation, after a message id
    Chat {
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Approve a request the manager made, and run its action (owner only)
    Approve {
        /// The request id, such as a3
        id: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Deny a request the manager made (owner only)
    Deny {
        id: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// From a task's worker: run the project's checks as after its result (all, or by number)
    CheckRun { numbers: Vec<u32> },
    /// Serve the task for a person to try on its own tailnet port (tasks.preview)
    Preview {
        task: String,
        /// Stop the running preview
        #[arg(long)]
        stop: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    PreviewHost { project_id: String, task: String },
    #[command(hide = true)]
    Host { project_id: String, task: String },
    #[command(hide = true)]
    ManagerHost { project_id: String },
}

impl TaskCmd {
    pub fn json_output(&self) -> bool {
        match self {
            Self::New { json, .. }
            | Self::List { json, .. }
            | Self::Show { json, .. }
            | Self::Steer { json, .. }
            | Self::Add { json, .. }
            | Self::Allow { json, .. }
            | Self::Stop { json, .. }
            | Self::Land { json, .. }
            | Self::Hold { json, .. }
            | Self::Trial { json, .. }
            | Self::Hand { json, .. }
            | Self::Handback { json, .. }
            | Self::Check { json, .. }
            | Self::Close { json, .. }
            | Self::Deploy { json, .. }
            | Self::Release { json, .. }
            | Self::Deployed { json, .. }
            | Self::Preview { json, .. } => *json,
            Self::Manager { json, .. }
            | Self::Say { json, .. }
            | Self::Chat { json, .. }
            | Self::Approve { json, .. }
            | Self::Deny { json, .. } => *json,
            Self::Ledger { .. } => true,
            Self::CheckRun { .. }
            | Self::Host { .. }
            | Self::ManagerHost { .. }
            | Self::PreviewHost { .. } => false,
        }
    }

    /// Commands that read files or a host socket and never need wsxd.
    pub fn without_daemon(&self) -> bool {
        !matches!(
            self,
            Self::New { .. }
                | Self::Check {
                    action: Some(CheckAction::Fail { .. }),
                    ..
                }
                | Self::Close { .. }
                | Self::Hand { .. }
                | Self::Handback { .. }
                | Self::Manager { .. }
                | Self::Say { .. }
                | Self::Approve { .. }
        )
    }
}

#[derive(Subcommand)]
pub enum CheckAction {
    /// The named items passed, or every open item with --all
    Pass {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// The item failed: a new task fixes it, linked to the item
    Fail {
        id: String,
        /// What is wrong; the new task gets it
        #[arg(long)]
        note: String,
    },
}

pub fn run(command: TaskCmd) -> Result<()> {
    match command {
        TaskCmd::New {
            prompt,
            from,
            title,
            model,
            hold,
            files,
            project,
            json,
        } => new(
            prompt,
            from.as_deref(),
            title.as_deref(),
            model.as_deref(),
            hold,
            files,
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
            let dir = open(project.as_deref(), &task)?;
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
            let dir = open(project.as_deref(), &task)?;
            let text = text.read(MAX_MESSAGE_BYTES)?;
            let files = checked_files(&dir.read_record()?.files, &files)?;
            send(&dir, HostRequest::Add { text, files }, json)
        }
        TaskCmd::Allow {
            task,
            access,
            until,
            project,
            json,
        } => {
            if task == "manager" {
                return allow_manager(project.as_deref(), access, until.as_deref(), json);
            }
            if until.is_some() {
                bail!("--until is for `wsx task allow manager`");
            }
            let dir = open(project.as_deref(), &task)?;
            let (access, project_wide) = access.into_access();
            // ^ The manager grants a host or a path for one task alone; the project and
            // tool rules are the owner's. ref: docs/tasks.md "Permissions"
            if acting_as_manager() && (project_wide || matches!(access, Access::Tool { .. })) {
                access.check().map_err(anyhow::Error::msg)?;
                let record = dir.read_record()?;
                return ask_owner(
                    &dir.project_id,
                    ApprovalAction::Grant,
                    Some(&record),
                    None,
                    Some(GrantAsk {
                        access,
                        project_wide,
                    }),
                    json,
                );
            }
            let request = HostRequest::Grant {
                access,
                project: project_wide,
                by: actor(),
            };
            owner::prepare_request(&dir.project_id, &request)?;
            send(&dir, request, json)
        }
        TaskCmd::Stop {
            task,
            project,
            json,
        } => send(&open(project.as_deref(), &task)?, HostRequest::Stop, json),
        TaskCmd::Check {
            action,
            project,
            json,
        } => check(action, project.as_deref(), json),
        TaskCmd::Land {
            task,
            tried,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            // ^ Only the owner tries a trial, so only the owner passes one.
            if tried && acting_as_manager() {
                bail!("only the owner passes a trial; ask the owner to try it");
            }
            let record = dir.read_record()?;
            let settings = load_task_config(&record.project_path)
                .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
            if acting_as_manager() && settings.land_approve {
                return ask_owner(
                    &dir.project_id,
                    ApprovalAction::Land,
                    Some(&record),
                    None,
                    None,
                    json,
                );
            }
            send(&dir, HostRequest::Land { tried }, json)
        }
        TaskCmd::Hold {
            task,
            reason,
            off: _,
            project,
            json,
        } => send(
            &open(project.as_deref(), &task)?,
            HostRequest::Hold {
                reason,
                by: actor(),
            },
            json,
        ),
        TaskCmd::Trial {
            task,
            end,
            project,
            json,
        } => trial_command(&open(project.as_deref(), &task)?, end, json),
        TaskCmd::Close {
            task,
            discard,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            // ^ Discarding work cannot be undone: the manager always asks first.
            if discard && acting_as_manager() {
                let record = dir.read_record()?;
                return ask_owner(
                    &dir.project_id,
                    ApprovalAction::Discard,
                    Some(&record),
                    None,
                    None,
                    json,
                );
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
            project,
            json,
        } => deploy(
            DeployKind::Deploy,
            project.as_deref(),
            commit.as_deref(),
            if_new,
            trial,
            json,
        ),
        TaskCmd::Release {
            commit,
            project,
            json,
        } => deploy(
            DeployKind::Release,
            project.as_deref(),
            commit.as_deref(),
            false,
            None,
            json,
        ),
        TaskCmd::Deployed { project, json } => deployed(project.as_deref(), json),
        TaskCmd::Ledger { project, .. } => ledger::ledger(project.as_deref()),
        TaskCmd::Manager { project, json } => {
            let (started, dir) = ensure_manager(project.as_deref())?;
            let state = if started { "started" } else { "running" };
            if json {
                let (_, main) = project_entry(project.as_deref())?;
                let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
                let name = settings.manager_shown_name();
                print_json(&json!({ "state": state, "socket": dir.socket_path(), "name": name }))
            } else {
                println!("manager {state}");
                Ok(())
            }
        }
        TaskCmd::Say {
            text,
            from_manager,
            reply,
            project,
            json,
        } => {
            let from = from_manager.then_some(reply);
            say(
                project.as_deref(),
                text.read(MAX_MESSAGE_BYTES)?,
                from,
                json,
            )
        }
        TaskCmd::Chat {
            after,
            project,
            json,
        } => chat(project.as_deref(), after, json),
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
        TaskCmd::ManagerHost { project_id } => manager::run(&project_id),
        TaskCmd::Host { project_id, task } => {
            host::run(&project_id, store::parse_task_number(&task)?)
        }
    }
}

/// The registered project by name, by the current folder, or the only one.
fn project_entry(name: Option<&str>) -> Result<(String, PathBuf)> {
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

#[allow(clippy::too_many_arguments)]
fn new(
    prompt: PromptSource,
    from: Option<&str>,
    title: Option<&str>,
    model: Option<&str>,
    hold: Option<String>,
    files: Vec<String>,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let prompt = prompt.read(MAX_MESSAGE_BYTES)?;
    let title = title.map(wsx_core::task::view::checked_title).transpose()?;
    let model = model.map(wsx_core::task::view::checked_model).transpose()?;
    let hold = hold
        .as_deref()
        .map(wsx_core::task::view::checked_hold)
        .transpose()?;
    let files = checked_files(&[], &files)?;
    let (dir, record, answered) = create(prompt, from, title, model, None, hold, files, project)?;
    let state = answered.unwrap_or(TaskState::Working);
    let id = dir.id;
    if json {
        return print_json(&json!({
            "id": id,
            "project": record.project,
            "state": state,
            "branch": record.branch,
            "base": record.base,
            "hold": record.hold,
            "working_copy": record.working_copy,
            "task_dir": dir.path,
        }));
    }
    println!("task t{id} {}", state.as_str());
    println!("branch:       {}", record.branch);
    println!("working copy: {}", record.working_copy.display());
    if answered.is_none() {
        println!("the host has not answered yet; check it with `wsx task show {id}`");
    }
    Ok(())
}

/// Create a task and start its host; `fixes` names the "To check" item it fixes. The state
/// is none when the host did not answer in time.
#[allow(clippy::too_many_arguments)]
fn create(
    prompt: String,
    from: Option<&str>,
    title: Option<String>,
    model: Option<String>,
    fixes: Option<String>,
    hold: Option<String>,
    files: Vec<String>,
    project: Option<&str>,
) -> Result<(TaskDir, TaskRecord, Option<TaskState>)> {
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project)?;
    let settings = load_task_config(&project.path)
        .map_err(|error| anyhow::anyhow!("{error}; no task was created"))?;
    let project_id = store::project_id(&project.name, &project.path);
    let dir = TaskDir::allocate(&project_id)?;
    let id = dir.id;
    let working_copy =
        match copy::create(&project.path, from.unwrap_or(&project.default_branch), id) {
            Ok(copy) => copy,
            Err(error) => {
                let _ = dir.remove();
                return Err(error.context(format!(
                    "task t{id} was not created and its number is not reused"
                )));
            }
        };
    let now = now_unix_ms();
    let record = TaskRecord {
        id,
        project: project.name.clone(),
        project_id: project_id.clone(),
        project_path: project.path.clone(),
        default_branch: project.default_branch.clone(),
        base: working_copy.base.clone(),
        branch: working_copy.branch.clone(),
        working_copy: working_copy.path.clone(),
        pane_id: None,
        adapter: settings.adapter,
        model: model.unwrap_or(settings.model),
        tools: settings.tools,
        sandbox: settings.sandbox,
        grants: Vec::new(),
        checks: settings.checks,
        env: settings.env,
        check_timeout_secs: settings.check_timeout_secs,
        agent_session: None,
        state: TaskState::Working,
        revision: 0,
        paths: Vec::new(),
        paths_total: 0,
        files,
        title,
        fixes,
        created_by: actor(),
        hold: hold.clone(),
        waiting: None,
        created_unix_ms: now,
        updated_unix_ms: now,
    };
    let started = (|| -> Result<()> {
        dir.write_record(&record)?;
        dir.append(LogEvent::Created {
            base: record.base.clone(),
            branch: record.branch.clone(),
        })?;
        dir.append(LogEvent::Message {
            mode: MessageKind::Prompt,
            text: prompt,
        })?;
        if hold.is_some() {
            dir.append(LogEvent::Hold {
                reason: hold,
                by: record.created_by,
            })?;
        }
        let exe = std::env::current_exe().context("cannot find the wsx binary")?;
        let argv = vec![
            exe.to_string_lossy().into_owned(),
            "task".into(),
            "host".into(),
            project_id.clone(),
            id.to_string(),
        ];
        ops::create_session_launch(
            &config,
            &record.working_copy,
            format!("task-{id}"),
            argv,
            None,
        )?;
        Ok(())
    })();
    if let Err(error) = started {
        let _ = copy::remove(&project.path, &record.working_copy, Some(&record.branch));
        let _ = dir.remove();
        return Err(error.context(format!(
            "task t{id} did not start; its working copy and branch were removed"
        )));
    }
    let state = wait_for_host(&dir).map(|response| response.state);
    Ok((dir, record, state))
}

/// `wsx task check`: list the open items, or pass or fail them (docs/tasks.md "To check").
fn check(action: Option<CheckAction>, project: Option<&str>, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let changed = match action {
        None => {
            let items = checks::open_items(&project_id)?;
            if json {
                return print_json(&json!({ "project": name, "open": items }));
            }
            if items.is_empty() {
                println!("nothing to check in {name}");
            }
            for item in items {
                println!(
                    "{:<5} #{:<4} {} (pass if: {})",
                    item.id, item.task, item.check, item.pass_if
                );
            }
            return Ok(());
        }
        Some(CheckAction::Pass { ids, all }) => {
            owner_only("check pass")?;
            let ids = if all { Vec::new() } else { ids };
            checks::pass(&project_id, &ids)?
        }
        Some(CheckAction::Fail { id, note }) => {
            owner_only("check fail")?;
            vec![fail_check(&name, &project_id, &id, &note)?]
        }
    };
    let open = checks::open_items(&project_id)?.len();
    let message = match changed.as_slice() {
        [item] if item.fix_task.is_some() => format!(
            "{} failed; #{} fixes it. {open} to check",
            item.id,
            item.fix_task.unwrap_or_default()
        ),
        items => format!("{} passed. {open} to check", items.len()),
    };
    if json {
        return print_json(
            &json!({ "ok": true, "changed": changed, "open": open, "message": message }),
        );
    }
    println!("{message}");
    Ok(())
}

/// A failed item starts a task that fixes it and links both.
fn fail_check(project: &str, project_id: &str, id: &str, note: &str) -> Result<CheckItem> {
    let note = note.trim();
    if note.is_empty() {
        bail!("a fail needs --note saying what is wrong; the new task gets it");
    }
    if note.len() > checks::MAX_NOTE_BYTES {
        bail!("the note is larger than {} bytes", checks::MAX_NOTE_BYTES);
    }
    let item = checks::find_open(&checks::read(project_id)?, id)?.clone();
    let report = TaskDir::open(project_id, item.task)?.file(REPORT_FILE);
    let prompt = format!(
        "Fix what the owner found when checking #{task} after it landed.\n\n\
         Check: {check}\nPass if: {pass_if}\nThe owner wrote: {note}\n\n\
         The report of #{task}: {report}\n\n\
         Find the cause, fix it, prove what a command or a capture can prove, and commit.",
        task = item.task,
        check = item.check,
        pass_if = item.pass_if,
        report = report.display(),
    );
    let (dir, ..) = create(
        prompt,
        None,
        Some(fix_title(&item.check)),
        None,
        Some(item.id.clone()),
        None,
        Vec::new(),
        Some(project),
    )?;
    checks::fail(project_id, id, note, dir.id).with_context(|| {
        format!(
            "#{} was started to fix {id}, but the item was not marked failed",
            dir.id
        )
    })
}

/// "Fix: <check>" within the title limit, on one line.
fn fix_title(check: &str) -> String {
    let words: String = check
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let title = format!("Fix: {}", words.trim());
    let limit = wsx_core::task::view::MAX_TITLE_CHARS;
    if title.chars().count() <= limit {
        return title;
    }
    let mut short: String = title.chars().take(limit - 1).collect();
    short.push('…');
    short
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

fn list(project: Option<&str>, all: bool, json: bool) -> Result<()> {
    let (name, path) = project_entry(project)?;
    let tasks = project_tasks(&store::project_id(&name, &path))?;
    let records: Vec<&TaskRecord> = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .collect();
    let mut rows = Vec::new();
    for (dir, record) in &tasks {
        match record {
            Ok(record) if all || record.state.is_open() => {
                let mut row = task_summary(dir, record);
                row["paths_total"] = json!(record.paths_total);
                if record.state.is_open() {
                    row["overlaps"] = overlaps(record, &records, false);
                }
                rows.push(row);
            }
            Ok(_) => {}
            Err(error) => rows.push(json!({ "id": dir.id, "error": format!("{error:#}") })),
        }
    }
    if json {
        return print_json(&json!({ "project": name, "tasks": rows }));
    }
    if rows.is_empty() {
        println!("no tasks in {name}");
    }
    for row in rows {
        println!(
            "t{:<4} {:<9} r{:<3} {}",
            row["id"],
            row["state"].as_str().unwrap_or("error"),
            row["revision"].as_u64().unwrap_or(0),
            row["title"]
                .as_str()
                .or(row["error"].as_str())
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn show(dir: &TaskDir, json: bool) -> Result<()> {
    let record = dir.read_record()?;
    let result = dir
        .read_file(RESULT_FILE, MAX_RESULT_READ_BYTES)?
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let report = dir.file(REPORT_FILE);
    let report = report.exists().then_some(report);
    let host = call_host(&dir.socket_path(), &HostRequest::Status, HOST_TIMEOUT).is_ok();
    let mut summary = task_summary(dir, &record);
    if json {
        summary["record"] = serde_json::to_value(&record)?;
        summary["result"] = result.unwrap_or(serde_json::Value::Null);
        summary["report"] = json!(report);
        summary["log"] = json!(dir.file(store::LOG_FILE));
        summary["host_running"] = json!(host);
        if record.waits_on(wsx_core::task::WaitOn::Hand) {
            summary["hand"] = json!({
                "session": hand_label(record.id),
                "context": hand_context(dir, &record),
            });
        }
        summary["socket"] = json!(dir.socket_path());
        summary["to_check"] = json!(checks::read(&dir.project_id)?
            .into_iter()
            .filter(|item| item.task == record.id)
            .collect::<Vec<_>>());
        let tasks = project_tasks(&dir.project_id)?;
        let records: Vec<&TaskRecord> = tasks
            .iter()
            .filter_map(|(_, record)| record.as_ref().ok())
            .collect();
        summary["overlaps"] = if record.state.is_open() {
            overlaps(&record, &records, true)
        } else {
            json!([])
        };
        return print_json(&summary);
    }
    println!(
        "t{} {}  r{}  {}",
        record.id,
        record.state.as_str(),
        record.revision,
        summary["title"].as_str().unwrap_or_default()
    );
    println!("branch:       {}", record.branch);
    println!("working copy: {}", record.working_copy.display());
    println!(
        "host:         {}",
        if host { "running" } else { "not running" }
    );
    if let Some(result) = result {
        println!(
            "result:       {}: {}",
            result["result"]["status"].as_str().unwrap_or_default(),
            result["result"]["summary"].as_str().unwrap_or_default()
        );
    }
    if let Some(report) = report {
        println!("report:       {}", report.display());
    }
    Ok(())
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

/// One request to the task's host; a host that does not answer is an error that says how
/// to restart it.
fn ask_host(dir: &TaskDir, request: &HostRequest) -> Result<HostResponse> {
    // ^ A landing or a trial rebases the branch on the host before it answers.
    let timeout = if matches!(request, HostRequest::Land { .. } | HostRequest::Trial) {
        LAND_TIMEOUT
    } else {
        HOST_TIMEOUT
    };
    match wsx_core::task::call_host_waiting(&dir.socket_path(), request, timeout) {
        Ok(response) => Ok(response),
        Err(error) => {
            let (state, working_copy) = dir.read_record().map_or_else(
                |_| ("unknown".to_string(), String::new()),
                |record| {
                    (
                        record.state.as_str().to_string(),
                        record.working_copy.display().to_string(),
                    )
                },
            );
            bail!(
                "{error:#}; task t{} is {state} and unchanged. Restart its host with `wsx session restart task-{} -w {working_copy}`",
                dir.id,
                dir.id
            );
        }
    }
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

/// The session the owner works in while a task waits on `hand`.
fn hand_label(id: u32) -> String {
    format!("task-{id}-hand")
}

/// `wsx task hand`: the host stops the worker and holds every turn, check and landing; a
/// session `task-<n>-hand` in the task's own working copy resumes a fork of the worker's
/// conversation (docs/tasks.md "By hand").
fn hand(dir: &TaskDir, json: bool) -> Result<()> {
    owner_only("hand")?;
    let record = dir.read_record()?;
    if !record.working_copy.exists() {
        bail!(
            "task t{} has no working copy at {}",
            record.id,
            record.working_copy.display()
        );
    }
    let response = ask_host_ok(dir, &HostRequest::Hand)?;
    let record = dir.read_record()?;
    let label = hand_label(record.id);
    let opened = match sessions_in(&record.working_copy, Some(&label))?.first() {
        Some(_) => false,
        None => {
            // ^ A fork leaves the worker's own conversation as it was for its next turn. A
            // resumed conversation keeps the system prompt it began with until compacted, so
            // the snapshot is off: the owner gets Claude's own prompt, not the worker rules.
            let argv = match &record.agent_session {
                Some(session) => vec![
                    "claude".into(),
                    "--resume".into(),
                    session.clone(),
                    "--fork-session".into(),
                    "--system-prompt-snapshot".into(),
                    "off".into(),
                ],
                None => Vec::new(),
            };
            ops::create_session_launch(&load_config()?, &record.working_copy, label.clone(), argv, None)
                .with_context(|| {
                    format!(
                        "task t{} is worked by hand, but session {label} did not open; work in {} or run `wsx task handback {}`",
                        record.id,
                        record.working_copy.display(),
                        record.id
                    )
                })?;
            true
        }
    };
    let context = hand_context(dir, &record);
    if json {
        return print_json(&json!({
            "ok": true,
            "state": record.state,
            "message": response.message,
            "session": label,
            "session_opened": opened,
            "context": context,
        }));
    }
    println!(
        "t{} by hand in session {label}: {}",
        record.id, response.message
    );
    for (name, value) in context.as_object().into_iter().flatten() {
        if let Some(text) = value.as_str() {
            println!("{name:<13} {text}");
        }
    }
    println!(
        "when the work is committed: wsx task handback {}",
        record.id
    );
    Ok(())
}

/// `wsx task handback`: the host refuses uncommitted tracked changes, and the hand session
/// stays; otherwise the session closes and the commits are checked as the next revision.
fn handback(dir: &TaskDir, json: bool) -> Result<()> {
    owner_only("handback")?;
    let response = ask_host_ok(dir, &HostRequest::Handback)?;
    let record = dir.read_record()?;
    let mut closed = Vec::new();
    for session in sessions_in(&record.working_copy, Some(&hand_label(record.id)))? {
        ops::kill_session(session).with_context(|| {
            format!(
                "task t{} was handed back, but its hand session remains",
                record.id
            )
        })?;
        closed.push(session.0);
    }
    if json {
        return print_json(&json!({
            "ok": true,
            "state": response.state,
            "message": response.message,
            "sessions_closed": closed,
        }));
    }
    println!(
        "t{} {}: {}",
        record.id,
        response.state.as_str(),
        response.message
    );
    Ok(())
}

/// What the owner works from: the task's files in the store, the branch and its state.
fn hand_context(dir: &TaskDir, record: &TaskRecord) -> serde_json::Value {
    let existing = |name: &str| {
        let path = dir.file(name);
        path.exists().then(|| path.display().to_string())
    };
    let git = |args: &[&str]| {
        wsx_core::git::git_cmd(&record.working_copy)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string()
            })
    };
    let status = git(&["status", "--short"]).map(|status| {
        let lines: Vec<&str> = status.lines().collect();
        match lines.len() {
            0 => "clean".to_string(),
            n if n <= 20 => lines.join("; "),
            n => format!("{}; and {} more", lines[..20].join("; "), n - 20),
        }
    });
    json!({
        "working_copy": record.working_copy.display().to_string(),
        "branch": record.branch,
        "head": git(&["rev-parse", "HEAD"]),
        "status": status,
        "report": existing(REPORT_FILE),
        "result": existing(RESULT_FILE),
    })
}

fn close(dir: &TaskDir, discard: bool, json: bool) -> Result<()> {
    let mut record = dir.read_record()?;
    let main = record.project_path.clone();
    let working_copy_exists = record.working_copy.exists();
    // ^ A landed task's commits are on the default branch, also when a remote merged them.
    if working_copy_exists && !discard && record.state != TaskState::Landed {
        let unlanded = copy::unlanded_commits(&main, &record.branch, &record.default_branch)
            .with_context(|| format!("task t{} is unchanged", record.id))?;
        if unlanded > 0 {
            bail!(
                "{} has {unlanded} commit(s) that are not on {}; task t{} is unchanged ({}). Land them, or run `wsx task close {} --discard`",
                record.branch,
                record.default_branch,
                record.id,
                record.state.as_str(),
                record.id
            );
        }
    }
    // ^ A preview runs in the working copy: it stops before the copy goes.
    preview::stop(dir).with_context(|| format!("task t{} is unchanged", record.id))?;
    // ^ The host ends its trial on close; this covers a host that no longer runs.
    trial::end(
        &dir.project_id,
        &record.project,
        Some(record.id),
        "the task closed",
        false,
    )?;
    // ^ The host is the writer while it runs: ask it to close and wait for it to exit.
    let socket = dir.socket_path();
    if call_host(&socket, &HostRequest::Close { discard }, HOST_TIMEOUT).is_ok() {
        let deadline = Instant::now() + CLOSE_WAIT;
        while socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if socket.exists() {
            bail!(
                "the host of task t{} did not exit; it is closed but its working copy remains. Run `wsx task close {}{}` again",
                record.id,
                record.id,
                if discard { " --discard" } else { "" }
            );
        }
        record = dir.read_record()?;
    } else if record.state != TaskState::Closed
        && (record.state != TaskState::Landed || working_copy_exists)
    {
        let _ = std::fs::remove_file(&socket);
        dir.append(LogEvent::Closed { discarded: discard })?;
        // ^ A landed task keeps `landed`: its work is on the default branch.
        if record.state != TaskState::Landed || discard {
            record.state = TaskState::Closed;
        }
        record.updated_unix_ms = now_unix_ms();
        dir.write_record(&record)?;
    }
    let mut removed = Vec::new();
    for session in sessions_in(&record.working_copy, None)? {
        ops::kill_session(session).with_context(|| {
            format!(
                "task t{} is closed but session {session} remains",
                record.id
            )
        })?;
        removed.push(session.0);
    }
    if working_copy_exists {
        ops::cargo_clean(&record.working_copy);
        copy::remove(&main, &record.working_copy, Some(&record.branch)).with_context(|| {
            format!(
                "task t{} is closed but its working copy remains; run `wsx task close {} --discard` again",
                record.id, record.id
            )
        })?;
    }
    if discard {
        delete_revision_refs(&main, record.id);
    }
    if json {
        return print_json(&json!({
            "id": record.id,
            "state": record.state,
            "discarded": discard,
            "sessions_removed": removed,
            "working_copy_removed": working_copy_exists,
        }));
    }
    println!(
        "task t{} closed; removed {} and {} session(s)",
        record.id,
        record.working_copy.display(),
        removed.len()
    );
    Ok(())
}

/// The sessions in the working copy; with `label`, only those named so.
fn sessions_in(working_copy: &Path, label: Option<&str>) -> Result<Vec<runtime::SessionId>> {
    let snapshot = existing_daemon_snapshot()?;
    let worktrees: Vec<_> = snapshot
        .worktrees
        .iter()
        .filter(|worktree| worktree.path == working_copy)
        .map(|worktree| worktree.id)
        .collect();
    Ok(snapshot
        .sessions
        .iter()
        .filter(|session| {
            worktrees.contains(&session.worktree_id)
                && label.is_none_or(|label| session.label == label)
        })
        .map(|session| session.id)
        .collect())
}

fn delete_revision_refs(main: &Path, id: u32) {
    let prefix = format!("refs/wsx/task/{id}/");
    let Ok(output) = wsx_core::git::git_cmd(main)
        .args(["for-each-ref", "--format=%(refname)", &prefix])
        .output()
    else {
        return;
    };
    for reference in String::from_utf8_lossy(&output.stdout).lines() {
        let _ = wsx_core::git::git_cmd(main)
            .args(["update-ref", "-d", reference])
            .status();
    }
}

/// Run the project's deploy or release command in a temporary copy of one commit.
/// ^ Never from a task's working copy: what runs is always a default-branch commit.
/// `if_new`: skip a commit that is already deployed, so the deploys that follow quick lands
/// run once for the newest one (docs/tasks.md "Deploy and release").
fn deploy(
    kind: DeployKind,
    project: Option<&str>,
    commit: Option<&str>,
    if_new: bool,
    trial: Option<u32>,
    json: bool,
) -> Result<()> {
    let name = kind_name(kind);
    let (project_name, main) = project_entry(project)?;
    let settings =
        load_task_config(&main).map_err(|error| anyhow::anyhow!("{error}; nothing was run"))?;
    let command = match kind {
        DeployKind::Deploy => settings.deploy_command.clone(),
        DeployKind::Release => settings.release_command.clone(),
    }
    .with_context(|| {
        format!("tasks.{name}.command is not set in wsx.config.yml; nothing was run")
    })?;
    let project_id = store::project_id(&project_name, &main);
    // ^ Only public output waits for what the owner has to check (docs/tasks.md "To check").
    let to_check = checks::open_items(&project_id)?.len();
    let public = match kind {
        DeployKind::Deploy => settings.deploy_public,
        DeployKind::Release => true,
    };
    if public && to_check > 0 {
        bail!(
            "{to_check} item(s) to check are open; the owner checks them first (`wsx task check`). Nothing was run"
        );
    }
    let approve = match kind {
        DeployKind::Deploy => settings.deploy_approve,
        DeployKind::Release => settings.release_approve,
    };
    if approve && acting_as_manager() {
        let action = match kind {
            DeployKind::Deploy => ApprovalAction::Deploy,
            DeployKind::Release => ApprovalAction::Release,
        };
        return ask_owner(&project_id, action, None, commit, None, json);
    }
    // ^ One deploy or release at a time per project; a later one waits here.
    let _running = store::lock_deploys(&project_id)?;
    // ^ While a trial holds the live site, only its own deploy runs; the default branch
    // deploys when it ends. ref: docs/tasks.md "Hold and trial"
    let live_trial = store::read_trial(&project_id)?;
    let skip = |message: String| -> Result<()> {
        if json {
            return print_json(&json!({ "kind": kind, "skipped": true, "message": message }));
        }
        println!("{message}");
        Ok(())
    };
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
            if json {
                return print_json(
                    &json!({ "kind": kind, "commit": tip, "skipped": true, "message": message }),
                );
            }
            println!("{message}");
            return Ok(());
        }
    }
    let path = copy::working_copy_path_named(&main, &format!("{name}-{}", std::process::id()))?;
    let commit = copy::create_detached(&main, &revision, &path)
        .map_err(|error| error.context(format!("nothing was run or recorded for this {name}")))?;
    let started = Instant::now();
    // ^ JSON callers parse stdout, so the command writes to stderr then.
    let stdout = if json {
        std::process::Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?)
    } else {
        std::process::Stdio::inherit()
    };
    let status = std::process::Command::new("/bin/sh")
        .args(["-c", &command])
        .current_dir(&path)
        .env("WSX_DEPLOY_COMMIT", &commit)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .status();
    let duration_ms = started.elapsed().as_millis() as u64;
    ops::cargo_clean(&path);
    let removed = copy::remove(&main, &path, None);
    let exit = match &status {
        Ok(status) => status.code(),
        Err(_) => None,
    };
    let record = DeployRecord {
        kind,
        commit: commit.clone(),
        exit,
        unix_ms: now_unix_ms(),
        duration_ms,
        trial,
    };
    store::append_deploy(&project_id, &record)?;
    // ^ A trial whose deploy failed ends, and the default branch deploys again.
    if let (Some(task), false) = (trial, status.as_ref().is_ok_and(|status| status.success())) {
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
    let status = status
        .with_context(|| format!("cannot run tasks.{name}.command; recorded with no exit code"))?;
    let mut message = format!(
        "{name} of {commit:.12}: exit {}",
        exit.map_or("none".into(), |code| code.to_string())
    );
    if status.success() && to_check > 0 {
        message.push_str(&format!("; {to_check} to check on the deployed site"));
    }
    if json {
        let mut value = serde_json::to_value(&record)?;
        value["message"] = json!(message);
        print_json(&value)?;
    } else {
        println!("{message}");
    }
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

/// Most entries `chat` prints at once.
const MAX_CHAT_ENTRIES: usize = 200;

/// Reuse the project's manager host, or start one in a hidden session in the main checkout.
/// Returns whether it was started.
/// Create the manager's hidden session, starting wsxd when it does not run.
fn launch_manager(main: &Path) -> impl FnOnce(Vec<String>) -> Result<()> + '_ {
    move |argv| {
        ops::create_session_launch(&load_config()?, main, "task-manager".into(), argv, None)
            .map(drop)
    }
}

fn ensure_manager(project: Option<&str>) -> Result<(bool, ManagerDir)> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_manager(&name, &main, &exe, launch_manager(&main))
}

/// `from`: `Some(reply)` when another project's manager sends it (`--from-manager`).
fn say(project: Option<&str>, text: String, from: Option<bool>, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let from = match from {
        None => None,
        Some(reply) => {
            // ^ Only a manager host sets the sender's project (broker.rs).
            let own = std::env::var(broker::FROM_ENV)
                .ok()
                .filter(|own| acting_as_manager() && !own.is_empty())
                .context("--from-manager is for a project's manager")?;
            if own == name {
                bail!("--from-manager messages another project's manager");
            }
            Some(wsx_core::task::manager::Author::Manager {
                project: own,
                reply,
            })
        }
    };
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let sent = from.is_some().then(|| text.clone());
    let response = owner::say(&name, &main, &exe, launch_manager(&main), text, from)?;
    if json {
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

/// The conversation after message `after`: messages, replies, rotations and stops.
fn chat(project: Option<&str>, after: Option<u64>, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let dir = ManagerDir::open(&store::project_id(&name, &main))?;
    let log = dir.read_log()?;
    let start = after
        .and_then(|after| log.iter().position(|entry| entry.id() == Some(after)))
        .map_or(0, |index| index + 1);
    let mut entries: Vec<_> = log[start..]
        .iter()
        .filter(|entry| {
            !matches!(
                entry.event,
                ManagerEvent::Delivered { .. } | ManagerEvent::Agent { .. }
            )
        })
        .collect();
    let skipped = entries.len().saturating_sub(MAX_CHAT_ENTRIES);
    entries.drain(..skipped);
    if json {
        return print_json(&json!({ "entries": entries, "skipped": skipped }));
    }
    for entry in entries {
        match &entry.event {
            ManagerEvent::Message { id, from, text } => println!("[{id}] {from:?}: {text}"),
            ManagerEvent::Reply { id, text, .. } => println!("[{id}] manager: {text}"),
            ManagerEvent::Rotated {
                reason: Some(reason),
                ..
            } => println!("(new conversation: {reason})"),
            ManagerEvent::Rotated { tokens, .. } => {
                println!("(new conversation after {tokens} tokens)")
            }
            ManagerEvent::Stopped { reason } => println!("(manager stopped: {reason})"),
            ManagerEvent::Granted { access, .. } => {
                println!("(the owner let the manager {})", access.describe())
            }
            _ => {}
        }
    }
    Ok(())
}

/// True when the manager's agent runs this command (`WSX_TASK_ACTOR=manager`).
/// ^ A guard against the manager's mistakes, not a security boundary: anything that runs
/// commands as the owner can unset it (docs/tasks.md "Approvals").
fn acting_as_manager() -> bool {
    std::env::var(owner::ACTOR_ENV).is_ok_and(|actor| actor == "manager")
}

fn actor() -> Actor {
    if acting_as_manager() {
        Actor::Manager
    } else {
        Actor::Owner
    }
}

/// What `allow` names: one tool rule, host or folder.
#[derive(clap::Args)]
pub struct AccessArgs {
    /// A tool rule that is not Bash, such as WebSearch
    #[arg(required_unless_present_any = ["host", "path"], conflicts_with_all = ["host", "path"])]
    tool: Option<String>,
    /// A host the task's commands may reach, such as crates.io
    #[arg(long, conflicts_with = "path")]
    host: Option<String>,
    /// A folder the task's commands may write, absolute or ~/, such as ~/.cargo
    #[arg(long)]
    path: Option<String>,
    /// For every task of the project, kept in the task store
    #[arg(long)]
    project_wide: bool,
}

impl AccessArgs {
    fn into_access(self) -> (Access, bool) {
        let access = match (self.tool, self.host, self.path) {
            (_, Some(host), _) => Access::Network { host },
            (_, _, Some(path)) => Access::Write { path },
            (rule, ..) => Access::Tool {
                rule: rule.unwrap_or_default(),
            },
        };
        (access, self.project_wide)
    }
}

/// The owner lets the manager's Bash write one more folder; it applies from the manager's
/// next turn. ref: docs/tasks.md "Permissions"
fn allow_manager(
    project: Option<&str>,
    access: AccessArgs,
    until: Option<&str>,
    json: bool,
) -> Result<()> {
    // ^ The manager never widens its own boundary, also through its broker.
    if acting_as_manager() {
        bail!("only the owner widens the manager's boundary; ask the owner");
    }
    let (Access::Write { path }, false) = access.into_access() else {
        bail!("`wsx task allow manager` takes --path <dir> alone");
    };
    let until_ms = until.map(until_unix_ms).transpose()?;
    let (_, dir) = ensure_manager(project)?;
    let response: ManagerResponse = call_socket(
        &dir.socket_path(),
        &ManagerRequest::Grant { path, until_ms },
        HOST_TIMEOUT,
    )?;
    if json {
        print_json(&serde_json::to_value(&response)?)?;
    } else {
        println!("{}", response.message);
    }
    if !response.ok {
        bail!("{}", response.message);
    }
    Ok(())
}

/// `--until`: a duration from now (`90s`, `30m`, `2h`, `1d`) or the next local `HH:MM`.
fn until_unix_ms(text: &str) -> Result<u64> {
    let now = now_unix_ms();
    if let Some((hour, minute)) = text.split_once(':') {
        let (Ok(hour), Ok(minute)) = (hour.parse::<i32>(), minute.parse::<i32>()) else {
            bail!("--until {text:?}: a time is HH:MM, such as 18:00");
        };
        if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
            bail!("--until {text:?}: a time is HH:MM, such as 18:00");
        }
        let seconds = (now / 1000) as libc::time_t;
        let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: localtime_r fills the tm it is given; mktime reads and normalizes it.
        let at = unsafe {
            libc::localtime_r(&seconds, local.as_mut_ptr());
            let mut local = local.assume_init();
            local.tm_hour = hour;
            local.tm_min = minute;
            local.tm_sec = 0;
            local.tm_isdst = -1;
            libc::mktime(&mut local)
        };
        let mut at = u64::try_from(at).context("--until: the local time is out of range")? * 1000;
        if at <= now {
            at += 24 * 3600 * 1000;
        }
        return Ok(at);
    }
    let last = text.chars().last().unwrap_or_default();
    let split = text.len().saturating_sub(last.len_utf8());
    let unit = match last {
        's' => 1000,
        'm' => 60 * 1000,
        'h' => 3600 * 1000,
        'd' => 24 * 3600 * 1000,
        _ => {
            bail!("--until {text:?}: give a duration such as 30m, 2h, 1d, or a time such as 18:00")
        }
    };
    let count: u64 = text[..split]
        .parse()
        .ok()
        .filter(|count| *count > 0)
        .with_context(|| format!("--until {text:?}: the duration needs a count above 0"))?;
    count
        .checked_mul(unit)
        .and_then(|span| now.checked_add(span))
        .context("--until: the duration is too long")
}

/// Record an approval request instead of acting, and say that the owner was asked.
fn ask_owner(
    project_id: &str,
    action: ApprovalAction,
    task: Option<&TaskRecord>,
    commit: Option<&str>,
    grant: Option<GrantAsk>,
    json: bool,
) -> Result<()> {
    // ^ A grant does not depend on the revision; the owner approves it at any time.
    let revision = task
        .filter(|_| grant.is_none())
        .map(|record| record.revision);
    let line = store::append_approval(
        project_id,
        ApprovalLine::Request {
            id: String::new(),
            action,
            task: task.map(|record| record.id),
            revision,
            commit: commit.map(str::to_string),
            grant,
            unix_ms: now_unix_ms(),
        },
    )?;
    let ApprovalLine::Request { id, .. } = &line else {
        unreachable!("a request stays a request");
    };
    let target = task.map_or(String::new(), |record| {
        format!(" #{} r{}", record.id, record.revision)
    });
    let message = format!(
        "{}{target} needs the owner's approval; the owner was asked ({id}). Do not retry it",
        action_name(action)
    );
    if json {
        print_json(&json!({ "ok": true, "approval": id, "message": message }))
    } else {
        println!("{message}");
        Ok(())
    }
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
