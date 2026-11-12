//! `wsx task land`, `after` and `hold` (docs/tasks.md "Landing").

use super::approve::{after_landing, ask_owner, request_of};
use crate::task::args::OwnerSaidAfter;
use crate::task::{
    acting_as_manager, actor, asks_owner, batch, may, open, open_owned, outward_said, send,
};
use anyhow::{bail, Context, Result};
use wsx_core::config::project::load_task_config;
use wsx_core::task::manager::ApprovalAction;
use wsx_core::task::policy::Capability;
use wsx_core::task::{AfterKind, HostRequest, TaskState};

/// `land <a> <b>...`: refuse what one land would refuse, then ask the first task's host to
/// start the batch land. ref: docs/tasks.md "Batch land"
pub(crate) fn land_batch(
    project: Option<&str>,
    tasks: &[String],
    tried: bool,
    json: bool,
) -> Result<()> {
    if tried {
        bail!("--tried passes one trial: land that task alone");
    }
    if !may(Capability::Land) {
        bail!("a chat lands only a child into its parent task, one at a time");
    }
    let mut ids = Vec::new();
    let mut first = None;
    for task in tasks {
        let dir = open(project, task)?;
        let record = dir.read_record()?;
        if ids.contains(&record.id) {
            bail!("#{} is named twice; nothing was landed", record.id);
        }
        if record.state != TaskState::Ready || record.parent.is_some() || record.hold.is_some() {
            bail!(
                "#{} is {}: a batch lands only ready tasks with no parent and no hold; nothing was landed",
                record.id,
                record.state.as_str()
            );
        }
        ids.push(record.id);
        first.get_or_insert((dir, record));
    }
    let (dir, first) = first.context("no task to land")?;
    let settings = load_task_config(&first.project_path)
        .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
    if asks_owner(settings.land_approve) {
        bail!("each land needs the owner's approval: land the tasks one at a time");
    }
    send(&dir, HostRequest::LandBatch { tasks: ids }, json)
}

/// `wsx task after <n> <m> "<then>"`, or `<n> --off`: task `n` waits for task `m` to land,
/// then gets `then` as an add. ref: docs/tasks.md "After another task"
pub(crate) fn after_command(
    project: Option<&str>,
    task: &str,
    on: Option<String>,
    then: Option<String>,
    because: Option<AfterKind>,
    json: bool,
) -> Result<()> {
    let dir = open_owned(project, task)?;
    let target = on
        .map(|on| {
            let other = open(project, &on)?.read_record()?;
            if other.id == dir.id || !other.state.is_open() {
                bail!(
                    "#{} is {}: a task waits for another open task",
                    other.id,
                    if other.id == dir.id {
                        "this task"
                    } else {
                        other.state.as_str()
                    }
                );
            }
            Ok(other.id)
        })
        .transpose()?;
    let request = HostRequest::After {
        task: target,
        text: then.unwrap_or_default(),
        because,
        by: actor(),
    };
    send(&dir, request, json)
}

/// `wsx task land`: several tasks land as one batch; one asks the owner first when the manager
/// lands onto a project that approves lands.
pub(crate) fn land_command(
    task: Vec<String>,
    tried: bool,
    said: OwnerSaidAfter,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let (said, after) = outward_said(&said)?;
    if task.len() > 1 {
        if after.is_some() {
            bail!("--after records one task's land: land the tasks one at a time");
        }
        return land_batch(project, &task, tried, json);
    }
    let dir = open_owned(project, &task[0])?;
    // ^ Only the owner tries a trial, so only the owner passes one.
    if tried && acting_as_manager() {
        bail!("only the owner passes a trial; ask the owner to try it");
    }
    let record = dir.read_record()?;
    // ^ A chat lands only into a parent task; the manager lands onto the default branch.
    if !may(Capability::Land) && record.parent.is_none() {
        bail!(
            "a chat lands only a child into its parent task; #{} lands onto {}, which is the manager's",
            record.id,
            record.default_branch
        );
    }
    let settings = load_task_config(&record.project_path)
        .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
    if (after.is_some() || asks_owner(settings.land_approve))
        && ask_owner(
            &dir.project_id,
            after_landing(
                request_of(ApprovalAction::Land, Some(&record), None, None),
                after,
            ),
            said,
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

/// `wsx task hold`: hold a task for the owner to try, or release it with `--off`.
pub(crate) fn hold_command(
    task: &str,
    reason: Option<String>,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    send(
        &open_owned(project, task)?,
        HostRequest::Hold {
            reason,
            by: actor(),
        },
        json,
    )
}

/// The hidden `wsx task land-batch`: the detached run of a batch land.
pub(crate) fn run_batch(project: Option<&str>, tasks: &[String]) -> Result<()> {
    batch::run(
        tasks
            .iter()
            .map(|task| open(project, task))
            .collect::<Result<_>>()?,
    )
}
