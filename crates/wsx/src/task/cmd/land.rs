//! `wsx task land`, `after` and `hold` (docs/tasks.md "Landing").

use crate::task::{acting_as_manager, actor, open, open_owned, send};
use anyhow::{bail, Context, Result};
use wsx_core::config::project::load_task_config;
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
    if actor().role().is_some() {
        bail!("a role lands only a child into its parent task, one at a time");
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
    if acting_as_manager() && settings.land_approve {
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
