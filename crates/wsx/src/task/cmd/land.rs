//! `wsx task land`, `after` and `hold` (docs/tasks.md "Landing").

use super::approve::{after_landing, ask_owner, request_of};
use crate::task::args::OwnerSaidAfter;
use crate::task::{actor, asks_owner, open, outward_said, send};
use anyhow::{bail, Result};
use wsx_core::config::project::load_task_config;
use wsx_core::task::manager::ApprovalAction;
use wsx_core::task::{AfterKind, HostRequest};

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
    let dir = open(project, task)?;
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

/// `wsx task land`: asks the owner first when the manager lands onto a project that approves
/// lands.
pub(crate) fn land_command(
    task: String,
    said: OwnerSaidAfter,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let (said, after) = outward_said(&said)?;
    let dir = open(project, &task)?;
    let record = dir.read_record()?;
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
    send(&dir, HostRequest::Land, json)
}

/// `wsx task hold`: hold a task for the owner to try, or release it with `--off`.
pub(crate) fn hold_command(
    task: &str,
    reason: Option<String>,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    send(
        &open(project, task)?,
        HostRequest::Hold {
            reason,
            by: actor(),
        },
        json,
    )
}
