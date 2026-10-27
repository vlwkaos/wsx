//! `wsx task trial`: one ready task's revision on the live site for the owner to try
//! before it lands. The project has one trial slot, `trial.json`; while it is taken the
//! default branch does not deploy, and hosts do not follow a replaced wsx binary.
//! ref: docs/tasks.md "Hold and trial"

use super::hosting;
use anyhow::{bail, Result};
use wsx_core::task::{now_unix_ms, store, store::TaskDir, Actor, LogEvent, TaskRecord, Trial};

/// Take the slot for `record`. True when it was free; false when this task holds it.
pub fn claim(record: &TaskRecord, by: Actor) -> Result<bool> {
    let claimed = Trial {
        task: record.id,
        revision: None,
        commit: None,
        by,
        unix_ms: now_unix_ms(),
    };
    let taken = store::change_trial(&record.project_id, |current| match current {
        Some(trial) => {
            let other = trial.task;
            (Some(trial), Some(other))
        }
        None => (Some(claimed), None),
    })?;
    match taken {
        None => Ok(true),
        Some(task) if task == record.id => Ok(false),
        Some(task) => bail!(
            "the live site runs the trial of #{task}; end it first (`wsx task trial {task} --end`)"
        ),
    }
}

/// Name the revision to try. False when the slot no longer belongs to `task`.
pub fn set_commit(project_id: &str, task: u32, revision: u32, commit: &str) -> Result<bool> {
    store::change_trial(project_id, |current| match current {
        Some(trial) if trial.task == task => (
            Some(Trial {
                revision: Some(revision),
                commit: Some(commit.to_string()),
                ..trial
            }),
            true,
        ),
        other => (other, false),
    })
}

/// End `task`'s trial, or any trial with `None`, and log why in the task. A trial that had
/// a commit may be live, so the default branch tip deploys again (`deploy --if-new`).
/// `logged`: the caller (the task's host) logs `trial_ended` itself.
pub fn end(
    project_id: &str,
    project: &str,
    task: Option<u32>,
    reason: &str,
    logged: bool,
) -> Result<Option<Trial>> {
    let ended = store::change_trial(project_id, |current| match current {
        Some(trial) if task.is_none_or(|id| id == trial.task) => (None, Some(trial)),
        other => (other, None),
    })?;
    let Some(trial) = ended else {
        return Ok(None);
    };
    if !logged {
        TaskDir::open(project_id, trial.task)?.append(LogEvent::TrialEnded {
            reason: reason.to_string(),
        })?;
    }
    if trial.commit.is_some() {
        let log = store::follow_up_log(project_id)?;
        hosting::spawn_wsx(
            &["task", "deploy", "-p", project, "--if-new", "--json"],
            log,
            None,
        )?;
    }
    Ok(Some(trial))
}
