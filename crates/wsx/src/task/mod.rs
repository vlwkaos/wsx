//! `wsx task`: start a headless worker and steer, read and close it.
//! ref: docs/tasks.md

pub mod broker;
mod claude;
pub mod host;
mod hosting;
mod ledger;
pub mod manager;

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
use wsx_core::task::ledger::{default_ref, deploy_status, kind_name};
use wsx_core::task::manager::{ApprovalAction, ApprovalLine, GrantAsk, ManagerEvent};
use wsx_core::task::owner::{self, action_name, find_request, HOST_TIMEOUT, LAND_TIMEOUT};
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{
    overlaps, project_tasks, task_summary, verification_value, MAX_RESULT_READ_BYTES,
};
use wsx_core::task::{
    brief, call_host, copy, now_unix_ms,
    store::{self, TaskDir, REPORT_FILE, RESULT_FILE},
    Access, Actor, DeployKind, DeployRecord, HostRequest, HostResponse, LogEvent, MessageKind,
    Phase, TaskRecord, TaskState, Verdict, MAX_MESSAGE_BYTES,
};
use wsx_core::{ops, runtime};

const HOST_START_WAIT: Duration = Duration::from_secs(10);
const CLOSE_WAIT: Duration = Duration::from_secs(15);
const CARGO_CLEAN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Start a worker on a new branch in its own working copy
    New {
        #[command(flatten)]
        prompt: PromptSource,
        /// Start from this branch, tag or commit instead of the default branch
        #[arg(long, value_name = "REF")]
        from: Option<String>,
        /// Build at once, without a brief first (logged); only when the owner asks for it
        #[arg(long)]
        no_brief: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Build a briefed task against its rewritten prompt
    Start {
        task: String,
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
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Let a task reach a host, write a folder or use a tool (never Bash), and resume it when blocked
    Allow {
        task: String,
        #[command(flatten)]
        access: AccessArgs,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Take back a grant from the task's next turn on
    Revoke {
        task: String,
        #[command(flatten)]
        access: AccessArgs,
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
    /// Record a human verdict on the current revision; a fail goes to the worker
    Verify {
        task: String,
        #[arg(value_enum)]
        result: VerdictArg,
        /// What was checked, or for a fail what is wrong (required for a fail)
        #[arg(long)]
        note: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bring a ready task onto the default branch by the project's landing policy
    Land {
        task: String,
        /// Land although Human Verify items have no pass for this revision (logged)
        #[arg(long)]
        skip_verify: bool,
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
    /// Run tasks.deploy.command in a temporary copy of one default-branch commit and record it
    Deploy {
        /// Commit, tag or branch to deploy; default: the default branch tip
        #[arg(long, value_name = "REF")]
        commit: Option<String>,
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
            | Self::Revoke { json, .. }
            | Self::Stop { json, .. }
            | Self::Start { json, .. }
            | Self::Land { json, .. }
            | Self::Verify { json, .. }
            | Self::Close { json, .. }
            | Self::Deploy { json, .. }
            | Self::Release { json, .. }
            | Self::Deployed { json, .. } => *json,
            Self::Manager { json, .. }
            | Self::Say { json, .. }
            | Self::Chat { json, .. }
            | Self::Approve { json, .. }
            | Self::Deny { json, .. } => *json,
            Self::Ledger { .. } => true,
            Self::Host { .. } | Self::ManagerHost { .. } => false,
        }
    }

    /// Commands that read files or a host socket and never need wsxd.
    pub fn without_daemon(&self) -> bool {
        !matches!(
            self,
            Self::New { .. }
                | Self::Close { .. }
                | Self::Manager { .. }
                | Self::Say { .. }
                | Self::Approve { .. }
        )
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum VerdictArg {
    Pass,
    Fail,
}

pub fn run(command: TaskCmd) -> Result<()> {
    match command {
        TaskCmd::New {
            prompt,
            from,
            no_brief,
            project,
            json,
        } => new(prompt, from.as_deref(), no_brief, project.as_deref(), json),
        TaskCmd::Start {
            task,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            // ^ The build is the owner's decision: the manager asks (docs/tasks.md "Brief").
            if acting_as_manager() {
                let record = dir.read_record()?;
                return ask_owner(
                    &dir.project_id,
                    ApprovalAction::Start,
                    Some(&record),
                    false,
                    None,
                    None,
                    json,
                );
            }
            send(&dir, HostRequest::Start, json)
        }
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
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            let text = text.read(MAX_MESSAGE_BYTES)?;
            send(&dir, HostRequest::Add { text }, json)
        }
        TaskCmd::Allow {
            task,
            access,
            project,
            json,
        } => {
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
                    false,
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
        TaskCmd::Revoke {
            task,
            access,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            let (access, project_wide) = access.into_access();
            if acting_as_manager() && project_wide {
                bail!("a project-wide grant is the owner's to revoke; ask the owner");
            }
            let request = HostRequest::Revoke {
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
        TaskCmd::Verify {
            task,
            result,
            note,
            project,
            json,
        } => send(
            &open(project.as_deref(), &task)?,
            HostRequest::Verify {
                result: match result {
                    VerdictArg::Pass => Verdict::Pass,
                    VerdictArg::Fail => Verdict::Fail,
                },
                note,
            },
            json,
        ),
        TaskCmd::Land {
            task,
            skip_verify,
            project,
            json,
        } => {
            let dir = open(project.as_deref(), &task)?;
            let record = dir.read_record()?;
            let settings = load_task_config(&record.project_path)
                .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
            if acting_as_manager() && settings.land_approve {
                return ask_owner(
                    &dir.project_id,
                    ApprovalAction::Land,
                    Some(&record),
                    skip_verify,
                    None,
                    None,
                    json,
                );
            }
            send(&dir, HostRequest::Land { skip_verify }, json)
        }
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
                    false,
                    None,
                    None,
                    json,
                );
            }
            close(&dir, discard, json)
        }
        TaskCmd::Approve { id, project, json } => approve(project.as_deref(), &id, json),
        TaskCmd::Deny {
            id,
            note,
            project,
            json,
        } => deny(project.as_deref(), &id, note, json),
        TaskCmd::Deploy {
            commit,
            project,
            json,
        } => deploy(
            DeployKind::Deploy,
            project.as_deref(),
            commit.as_deref(),
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
            json,
        ),
        TaskCmd::Deployed { project, json } => deployed(project.as_deref(), json),
        TaskCmd::Ledger { project, .. } => ledger::ledger(project.as_deref()),
        TaskCmd::Manager { project, json } => {
            let (started, dir) = ensure_manager(project.as_deref())?;
            let state = if started { "started" } else { "running" };
            if json {
                print_json(&json!({ "state": state, "socket": dir.socket_path() }))
            } else {
                println!("manager {state}");
                Ok(())
            }
        }
        TaskCmd::Say {
            text,
            project,
            json,
        } => say(project.as_deref(), text.read(MAX_MESSAGE_BYTES)?, json),
        TaskCmd::Chat {
            after,
            project,
            json,
        } => chat(project.as_deref(), after, json),
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

fn new(
    prompt: PromptSource,
    from: Option<&str>,
    no_brief: bool,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let prompt = prompt.read(MAX_MESSAGE_BYTES)?;
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
    let skipped = if no_brief {
        Some("flag")
    } else if !settings.brief {
        Some("config")
    } else {
        None
    };
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
        model: settings.model,
        tools: settings.tools,
        phase: if skipped.is_some() {
            Phase::Build
        } else {
            Phase::Brief
        },
        sandbox: settings.sandbox,
        grants: Vec::new(),
        checks: settings.checks,
        check_timeout_secs: settings.check_timeout_secs,
        agent_session: None,
        state: TaskState::Starting,
        revision: 0,
        paths: Vec::new(),
        paths_total: 0,
        pr_url: None,
        pr_revision: None,
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
        if let Some(by) = skipped {
            dir.append(LogEvent::BriefSkipped {
                by: by.into(),
                actor: if acting_as_manager() {
                    "manager"
                } else {
                    "owner"
                }
                .into(),
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
    let state = wait_for_host(&dir).map_or(TaskState::Starting, |response| response.state);
    if json {
        return print_json(&json!({
            "id": id,
            "project": record.project,
            "state": state,
            "branch": record.branch,
            "base": record.base,
            "working_copy": record.working_copy,
            "task_dir": dir.path,
        }));
    }
    println!("task t{id} {}", state.as_str());
    println!("branch:       {}", record.branch);
    println!("working copy: {}", record.working_copy.display());
    if state == TaskState::Starting {
        println!("the host has not answered yet; check it with `wsx task show {id}`");
    }
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
    let brief_text = dir.file(store::BRIEF_TEXT_FILE);
    let brief_text = brief_text.exists().then_some(brief_text);
    let host = call_host(&dir.socket_path(), &HostRequest::Status, HOST_TIMEOUT).is_ok();
    let mut summary = task_summary(dir, &record);
    if json {
        summary["record"] = serde_json::to_value(&record)?;
        summary["result"] = result.unwrap_or(serde_json::Value::Null);
        summary["report"] = json!(report);
        summary["brief"] = json!(brief::read(dir));
        summary["brief_text"] = json!(brief_text);
        summary["log"] = json!(dir.file(store::LOG_FILE));
        summary["host_running"] = json!(host);
        summary["socket"] = json!(dir.socket_path());
        let log = dir.read_log()?;
        summary["verification"] = verification_value(&log, record.revision);
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
    if let Some(brief_text) = brief_text {
        println!("brief:        {}", brief_text.display());
    }
    if let Some(report) = report {
        println!("report:       {}", report.display());
    }
    Ok(())
}

fn send(dir: &TaskDir, request: HostRequest, json: bool) -> Result<()> {
    // ^ A pr landing pushes and calls gh on the host before it answers.
    let timeout = if matches!(request, HostRequest::Land { .. }) {
        LAND_TIMEOUT
    } else {
        HOST_TIMEOUT
    };
    let response = match wsx_core::task::call_host_waiting(&dir.socket_path(), &request, timeout) {
        Ok(response) => response,
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
    };
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
    } else if record.state != TaskState::Closed {
        let _ = std::fs::remove_file(&socket);
        dir.append(LogEvent::Closed { discarded: discard })?;
        record.state = TaskState::Closed;
        record.updated_unix_ms = now_unix_ms();
        dir.write_record(&record)?;
    }
    let mut removed = Vec::new();
    for session in sessions_in(&record.working_copy)? {
        ops::kill_session(session).with_context(|| {
            format!(
                "task t{} is closed but session {session} remains",
                record.id
            )
        })?;
        removed.push(session.0);
    }
    if working_copy_exists {
        if record.working_copy.join("Cargo.toml").exists() {
            cargo_clean(&record.working_copy);
        }
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

fn sessions_in(working_copy: &Path) -> Result<Vec<runtime::SessionId>> {
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
        .filter(|session| worktrees.contains(&session.worktree_id))
        .map(|session| session.id)
        .collect())
}

fn cargo_clean(working_copy: &Path) {
    let mut command = std::process::Command::new("cargo");
    command
        .arg("clean")
        .current_dir(working_copy)
        .stdin(std::process::Stdio::null());
    let _ = wsx_core::git::output_with_timeout_limit(&mut command, CARGO_CLEAN_TIMEOUT, 4096);
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
fn deploy(kind: DeployKind, project: Option<&str>, commit: Option<&str>, json: bool) -> Result<()> {
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
    let approve = match kind {
        DeployKind::Deploy => settings.deploy_approve,
        DeployKind::Release => settings.release_approve,
    };
    if approve && acting_as_manager() {
        let action = match kind {
            DeployKind::Deploy => ApprovalAction::Deploy,
            DeployKind::Release => ApprovalAction::Release,
        };
        return ask_owner(
            &store::project_id(&project_name, &main),
            action,
            None,
            false,
            commit,
            None,
            json,
        );
    }
    let revision = match commit {
        Some(commit) => commit.to_string(),
        None => default_ref(&main, &settings, true).map_err(|error| {
            error.context(format!("nothing was run or recorded for this {name}"))
        })?,
    };
    let project_id = store::project_id(&project_name, &main);
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
    if path.join("Cargo.toml").exists() {
        cargo_clean(&path);
    }
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
    };
    store::append_deploy(&project_id, &record)?;
    removed.with_context(|| {
        format!(
            "the {name} was recorded, but its copy {} remains",
            path.display()
        )
    })?;
    let status = status
        .with_context(|| format!("cannot run tasks.{name}.command; recorded with no exit code"))?;
    if json {
        print_json(&serde_json::to_value(&record)?)?;
    } else {
        println!(
            "{name} of {commit:.12}: exit {}",
            exit.map_or("none".into(), |code| code.to_string())
        );
    }
    if !status.success() {
        bail!("tasks.{name}.command failed ({status}); the failure is recorded and nothing else changed");
    }
    Ok(())
}

/// The last successful deploy and release and how many default-branch commits each lacks.
fn deployed(project: Option<&str>, json: bool) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let mut out = deploy_status(&project_name, &main, &settings, true)?;
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

fn say(project: Option<&str>, text: String, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let response = owner::say(&name, &main, &exe, launch_manager(&main), text)?;
    if json {
        print_json(&serde_json::to_value(&response)?)?;
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
            ManagerEvent::Rotated { tokens } => {
                println!("(new conversation after {tokens} tokens)")
            }
            ManagerEvent::Stopped { reason } => println!("(manager stopped: {reason})"),
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

/// What `allow` and `revoke` name: one tool rule, host or folder.
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

/// Record an approval request instead of acting, and say that the owner was asked.
fn ask_owner(
    project_id: &str,
    action: ApprovalAction,
    task: Option<&TaskRecord>,
    skip_verify: bool,
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
            skip_verify,
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
