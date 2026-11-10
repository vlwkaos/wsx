//! `wsx task new`: a task and its worker (docs/tasks.md).

use crate::cli::{load_full_workspace, resolve_project};
use crate::task::agent;
use crate::task::args::NewArgs;
use crate::task::{actor, ask_host, ask_host_ok, print_json, wait_for_host};
use anyhow::{bail, Context, Result};
use serde_json::json;
use wsx_core::config::global::Operation;
use wsx_core::config::project::load_task_config;
use wsx_core::task::view::checked_files;
use wsx_core::task::{
    copy, now_unix_ms,
    store::{self, TaskDir},
    After, AfterKind, HostRequest, LogEvent, MessageKind, TaskRecord, TaskState, MAX_MESSAGE_BYTES,
};
use wsx_core::{ops, runtime};

pub(crate) fn new(args: NewArgs) -> Result<()> {
    let NewArgs {
        prompt,
        from,
        into,
        after,
        because,
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
    } = args;
    let steps = match (no_steps, size) {
        (true, _) => Steps::None,
        (false, Size::Small) => Steps::Small,
        (false, Size::Full) => Steps::Skip(skip),
    };
    let prompt = prompt.read(MAX_MESSAGE_BYTES)?;
    let title = title
        .as_deref()
        .map(wsx_core::task::view::checked_title)
        .transpose()?;
    let model = model
        .as_deref()
        .map(wsx_core::task::view::checked_model)
        .transpose()?;
    let hold = hold
        .as_deref()
        .map(wsx_core::task::view::checked_hold)
        .transpose()?;
    let files = checked_files(&[], &files)?;
    let after = match (after, because) {
        (Some(task), Some(because)) => Some((store::parse_task_number(&task)?, because)),
        _ => None,
    };
    let (dir, record, answered) = create(NewTask {
        prompt,
        from,
        into,
        after,
        title,
        model,
        fixes: None,
        hold,
        files,
        plan,
        steps,
        project,
    })?;
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

/// `steps` without the ones `skip` names. Every name must be a step, and one step must stay.
pub(crate) fn without_steps(
    steps: &[wsx_core::model::workspace::WorkflowStep],
    skip: &[String],
) -> Result<Vec<wsx_core::model::workspace::WorkflowStep>> {
    let names = || {
        steps
            .iter()
            .map(|step| step.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(unknown) = skip
        .iter()
        .find(|name| !steps.iter().any(|step| &step.name == *name))
    {
        anyhow::bail!(
            "--skip {unknown}: tasks.workflow.steps has no such step (steps: {}); no task was created",
            names()
        );
    }
    let kept: Vec<_> = steps
        .iter()
        .filter(|step| !skip.contains(&step.name))
        .cloned()
        .collect();
    if kept.is_empty() && !steps.is_empty() {
        anyhow::bail!("--skip leaves no step of {}; no task was created", names());
    }
    Ok(kept)
}

/// The fold of a step into its neighbour: its prompt joins that step's turn, not a turn of its
/// own. ref: docs/tasks.md "Workflow"
pub(crate) const FOLDED_CONTEXT: &str = "context";

pub(crate) const FOLDED_MEMO: &str = "memo";

/// `steps` with `context` and `memo` folded: a `context` prompt joins the first other step as
/// optional text after the host's context block, a `memo` prompt joins the step before it.
/// With no other step nothing stays: the host's block and the result's memo cover them.
pub(crate) fn folded_steps(
    steps: Vec<wsx_core::model::workspace::WorkflowStep>,
) -> Vec<wsx_core::model::workspace::WorkflowStep> {
    let mut kept: Vec<wsx_core::model::workspace::WorkflowStep> = Vec::new();
    let mut context: Vec<String> = Vec::new();
    for mut step in steps {
        match step.name.as_str() {
            FOLDED_CONTEXT => context.push(step.prompt),
            FOLDED_MEMO => {
                if let Some(last) = kept.last_mut() {
                    last.prompt = format!("{}\n\nFor the memo: {}", last.prompt, step.prompt);
                }
            }
            _ => {
                if !context.is_empty() {
                    step.prompt = format!(
                        "{}\n\nIf the task context above is not enough: {}",
                        step.prompt,
                        context.join(" ")
                    );
                    context.clear();
                }
                kept.push(step);
            }
        }
    }
    kept
}

/// How `task new` picks the worker's steps.
pub(crate) enum Steps {
    /// One turn, without `tasks.workflow.steps`: the idle task.
    None,
    /// `tasks.workflow.steps` without these names.
    Skip(Vec<String>),
    /// `--size small`: only the `build` step, on `tasks.agent.smallModel`.
    Small,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Size {
    Small,
    Full,
}

/// What `create` starts a task from. `new` builds it from the command line (checked); a failed
/// To check item builds it for its fix task.
pub(crate) struct NewTask {
    pub(crate) prompt: String,
    pub(crate) from: Option<String>,
    pub(crate) into: Option<u32>,
    /// It starts when this open task has landed (`new --after`).
    pub(crate) after: Option<(u32, AfterKind)>,
    pub(crate) title: Option<String>,
    pub(crate) model: Option<String>,
    /// The "To check" item it fixes.
    pub(crate) fixes: Option<String>,
    pub(crate) hold: Option<String>,
    pub(crate) files: Vec<String>,
    pub(crate) plan: bool,
    pub(crate) steps: Steps,
    pub(crate) project: Option<String>,
}

/// Create a task and start its host; `fixes` names the "To check" item it fixes; with `plan`
/// its first turn writes a plan; `steps` picks its turns from `tasks.workflow.steps`, with
/// `context` and `memo` folded (docs/tasks.md "Workflow"). The state is none when the host
/// did not answer in time.
pub(crate) fn create(task: NewTask) -> Result<(TaskDir, TaskRecord, Option<TaskState>)> {
    let NewTask {
        prompt,
        from,
        into,
        after,
        title,
        model,
        fixes,
        hold,
        files,
        plan,
        steps,
        project,
    } = task;
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project.as_deref())?;
    let settings = load_task_config(&project.path)
        .map_err(|error| anyhow::anyhow!("{error}; no task was created"))?;
    // ^ A missing profile folder would fail the host's start after the ID is taken.
    config
        .claude_config_dir(&project.path)
        .map_err(|error| anyhow::anyhow!("{error}; no task was created"))?;
    let (model, effort) = match steps {
        Steps::Small => (
            model.or_else(|| Some(settings.small_model().to_string())),
            wsx_core::model::workspace::SMALL_EFFORT.to_string(),
        ),
        _ => (model, settings.effort.clone()),
    };
    let steps = match steps {
        Steps::None => Vec::new(),
        Steps::Skip(skip) => folded_steps(without_steps(&settings.workflow.steps, &skip)?),
        // ^ Kept before the fold, so the build gets the context and memo words.
        Steps::Small => folded_steps(
            settings
                .workflow
                .steps
                .into_iter()
                .filter(|step| [FOLDED_CONTEXT, "build", FOLDED_MEMO].contains(&step.name.as_str()))
                .collect(),
        ),
    };
    let project_id = store::project_id(&project.name, &project.path);
    let parent = into
        .map(|parent| -> Result<(TaskDir, TaskRecord)> {
            let parent_dir = TaskDir::open(&project_id, parent)?;
            let record = parent_dir.read_record()?;
            if !record.state.is_open() {
                bail!("--into: task t{parent} is {}; no task was created", record.state.as_str());
            }
            if let Some(grandparent) = record.parent {
                bail!("--into: task t{parent} lands into #{grandparent}, and a child takes no children; no task was created");
            }
            Ok((parent_dir, record))
        })
        .transpose()?;
    let wait = after
        .map(|(task, because)| -> Result<After> {
            let state = TaskDir::open(&project_id, task)?.read_record()?.state;
            if !state.is_open() {
                bail!("--after: task t{task} is {}; a task waits for another open task; no task was created", state.as_str());
            }
            Ok(After {
                task,
                text: String::new(),
                since_unix_ms: now_unix_ms(),
                because,
            })
        })
        .transpose()?;
    let dir = TaskDir::allocate(&project_id)?;
    let id = dir.id;
    // ^ A child starts from its parent's tip and lands on its branch; the parent is held so
    // it never lands by itself. ref: docs/tasks.md "Parent tasks"
    let created = (|| -> Result<(String, copy::WorkingCopy)> {
        let default_branch = match &parent {
            Some((parent_dir, record)) => {
                if record.hold.is_none() {
                    let hold = HostRequest::Hold {
                        reason: Some(format!("parent of #{id}")),
                        by: actor(),
                    };
                    ask_host_ok(parent_dir, &hold)?;
                }
                record.branch.clone()
            }
            None => project.default_branch.clone(),
        };
        let working_copy = copy::create(
            &project.path,
            from.as_deref().unwrap_or(&default_branch),
            id,
        )?;
        Ok((default_branch, working_copy))
    })();
    let (default_branch, working_copy) = match created {
        Ok(created) => created,
        Err(error) => {
            let _ = dir.remove();
            return Err(error.context(format!(
                "task t{id} was not created and its number is not reused"
            )));
        }
    };
    let now = now_unix_ms();
    // ^ New work starts on an available agent: a task never begins on one whose usage limit
    // is on record. `--model` names the configured agent's model. ref: docs/tasks.md "Agents"
    let own = agent::account(&settings.adapter, &settings.model);
    let moved = agent::blocked_until(&own, now)
        .and_then(|_| agent::fallback(&settings.fallback, now, Operation::Worker));
    // ^ A task born on a fallback is on one like a switched task: it goes home when its own
    // agent is free (after a failed sign-in, at its first turn once the record ends).
    let on_fallback = moved.is_some();
    let sign_in = on_fallback && store::sign_in_failed_until(&own, now).is_some();
    let (adapter, model) = match moved {
        Some(next) => (next.adapter, next.model),
        None => (settings.adapter, model.unwrap_or(settings.model)),
    };
    let record = TaskRecord {
        id,
        project: project.name.clone(),
        project_id: project_id.clone(),
        project_path: project.path.clone(),
        default_branch,
        base: working_copy.base.clone(),
        branch: working_copy.branch.clone(),
        working_copy: working_copy.path.clone(),
        pane_id: None,
        adapter,
        model,
        effort,
        fallback: on_fallback,
        sign_in,
        checks: settings.checks,
        steps,
        step: 0,
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
        after: wait.clone(),
        parent: into,
        plan,
        waiting: None,
        created_unix_ms: now,
        updated_unix_ms: now,
        state_unix_ms: now,
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
                by: record.created_by.clone(),
            })?;
        }
        if let Some(wait) = wait {
            dir.append(LogEvent::After {
                task: Some(wait.task),
                text: wait.text,
                because: Some(wait.because),
                by: record.created_by.clone(),
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
            &runtime::Client::local(),
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
    // ^ A host that never answers is replaced once by `call_task_host` (a deploy can break the
    // start); `new` then reports the state it has, or that the host has not answered.
    let state = wait_for_host(&dir)
        .or_else(|| ask_host(&dir, &HostRequest::Status).ok())
        .map(|response| response.state);
    Ok((dir, record, state))
}
