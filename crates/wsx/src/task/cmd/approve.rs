//! The owner's approvals: a request, `approve` and `deny` (docs/tasks.md "Approvals").

use crate::task::{acting_as_manager, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;
use wsx_core::task::ledger::default_ref;
use wsx_core::task::manager::{ApprovalAction, ApprovalLine};
use wsx_core::task::owner::{self, action_name, find_request};
use wsx_core::task::{
    copy, now_unix_ms,
    store::{self},
    TaskRecord,
};

/// A request for `action`; `ask_owner` and `ask_covering` record it.
pub(crate) fn request_of(
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
        path: None,
        url: None,
        unix_ms: now_unix_ms(),
    }
}

/// The manager's action that needs the owner: approved by the owner's message `owner_said`
/// of its running turn (recorded; `false`: run it), else a request (`true`: the owner was
/// asked). ref: docs/tasks.md "Approvals"
pub(crate) fn ask_owner(
    project_id: &str,
    request: ApprovalLine,
    owner_said: Option<u64>,
    json: bool,
) -> Result<bool> {
    Ok(submit(project_id, request, owner_said, json)?.1)
}

/// `ask_owner` for a push, deploy, release or discard: the owner sees only the newest
/// request, so each waiting one of the same action that the new one covers is dropped
/// ("replaced by <id>"). Requests that do not cover each other stay separate.
/// ref: docs/tasks.md "Approvals"
pub(crate) fn ask_covering(
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

/// The ids of the waiting requests that `new` covers: the same task for a discard, the same
/// path for a project add; for a push, deploy or release, a commit that `new`'s commit
/// contains (no commit: the default branch tip). A push with a lease covers a push without one, not the other way round.
pub(crate) fn covered_requests(
    main: &Path,
    project_id: &str,
    new: &ApprovalLine,
) -> Result<Vec<String>> {
    let ApprovalLine::Request {
        action,
        task,
        commit,
        lease,
        path,
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
                path: old_path,
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
                    ApprovalAction::Project => path.is_some() && old_path == path,
                    ApprovalAction::Land | ApprovalAction::Role => false,
                };
            covers.then(|| id.clone())
        })
        .collect())
}

/// Record `request`, or, with `owner_said`, record it approved. Returns its id and whether the
/// owner was asked.
pub(crate) fn submit(
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
        path,
        ..
    } = &request
    else {
        bail!("only a request is asked");
    };
    let (action, target) = (
        *action,
        match (task, revision, role, path) {
            (Some(task), Some(revision), ..) => format!(" #{task} r{revision}"),
            (_, _, Some(role), _) => format!(" {role}"),
            (.., Some(path)) => format!(" {path}"),
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

pub(crate) fn owner_only(command: &str) -> Result<()> {
    if acting_as_manager() {
        bail!("wsx task {command} is for the owner; the manager cannot decide its own requests");
    }
    Ok(())
}

/// Run an approved request's action as its own `wsx task` command, without the actor mark.
pub(crate) fn approve(project: Option<&str>, id: &str, json: bool) -> Result<()> {
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

pub(crate) fn deny(
    project: Option<&str>,
    id: &str,
    note: Option<String>,
    json: bool,
) -> Result<()> {
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
