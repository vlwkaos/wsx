//! What the owner does through a project's manager: start it, talk to it, and decide
//! its approval requests. Shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Manager", "Approvals"

use super::manager::{
    pending_approvals, ApprovalAction, ApprovalLine, GrantAsk, ManagerRequest, ManagerResponse,
};
use super::store::{self, ManagerDir, TaskDir};
use super::{call_socket, now_unix_ms, Access, HostRequest, MAX_MESSAGE_BYTES};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Set to `manager` for the manager's agent; its commands then ask instead of acting.
pub const ACTOR_ENV: &str = "WSX_TASK_ACTOR";
pub const HOST_TIMEOUT: Duration = Duration::from_secs(15);
/// A landing under the `pr` policy pushes and calls gh before the host answers.
pub const LAND_TIMEOUT: Duration = Duration::from_secs(300);
const MANAGER_START_WAIT: Duration = Duration::from_secs(10);
/// Most characters of an action's answer kept in its decision.
const MAX_OUTCOME_CHARS: usize = 1000;

/// Start the project's manager host unless one answers. True when it was started.
/// `launch` creates the hidden session `task-manager` in the main checkout with the given
/// argv; the CLI may start wsxd for it, the web client never does.
pub fn ensure_manager(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl FnOnce(Vec<String>) -> Result<()>,
) -> Result<(bool, ManagerDir)> {
    let project_id = store::project_id(name, main);
    let dir = ManagerDir::open(&project_id)?;
    let status = || {
        call_socket::<_, ManagerResponse>(&dir.socket_path(), &ManagerRequest::Status, HOST_TIMEOUT)
    };
    if status().is_ok() {
        return Ok((false, dir));
    }
    let argv = vec![
        wsx.to_string_lossy().into_owned(),
        "task".into(),
        "manager-host".into(),
        project_id,
    ];
    launch(argv).context("the manager did not start; no session was left running")?;
    let deadline = Instant::now() + MANAGER_START_WAIT;
    while Instant::now() < deadline {
        if status().is_ok() {
            return Ok((true, dir));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "the manager session started but its host does not answer; look at session task-manager in {}",
        main.display()
    )
}

/// Give the manager a message, starting it first when needed. `from` is another project's
/// manager; none is the owner.
pub fn say(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl FnOnce(Vec<String>) -> Result<()>,
    text: String,
    from: Option<super::manager::Author>,
) -> Result<ManagerResponse> {
    let (_, dir) = ensure_manager(name, main, wsx, launch)?;
    call_socket(
        &dir.socket_path(),
        &ManagerRequest::Say { text, from },
        HOST_TIMEOUT,
    )
}

pub fn action_name(action: ApprovalAction) -> &'static str {
    match action {
        ApprovalAction::Land => "land",
        ApprovalAction::Deploy => "deploy",
        ApprovalAction::Release => "release",
        ApprovalAction::Discard => "close --discard",
        ApprovalAction::Grant => "allow",
    }
}

/// An approval request as the action it asks for, never its id: "Approve land #3",
/// "Approve discard #4", "Approve deploy", "Approve network crates.io for #3".
/// ref: docs/web-client.md "Tasks"
pub fn approval_label(request: &ApprovalLine) -> String {
    let ApprovalLine::Request {
        action,
        task,
        grant,
        ..
    } = request
    else {
        return String::new();
    };
    if let Some(GrantAsk {
        access,
        project_wide,
    }) = grant
    {
        let what = match access {
            Access::Network { host } => format!("network {host}"),
            Access::Write { path } => format!("write {path}"),
            Access::Tool { rule } => format!("tool {rule}"),
        };
        return match (project_wide, task) {
            (false, Some(task)) => format!("Approve {what} for #{task}"),
            _ => format!("Approve {what} for every task"),
        };
    }
    let verb = match action {
        ApprovalAction::Discard => "discard",
        other => action_name(*other),
    };
    match task {
        Some(task) => format!("Approve {verb} #{task}"),
        None => format!("Approve {verb}"),
    }
}

/// A grant for the whole project changes `grants.json` before the task's host hears of
/// it, so the next agent start of every task has it. Other requests pass.
/// ref: docs/tasks.md "Permissions"
pub fn prepare_request(project_id: &str, request: &HostRequest) -> Result<()> {
    let HostRequest::Grant {
        access, project, ..
    } = request
    else {
        return Ok(());
    };
    access.check().map_err(anyhow::Error::msg)?;
    if *project {
        store::change_grants(project_id, |grants| {
            let present = grants.contains(access);
            if !present {
                grants.push(access.clone());
            }
            !present
        })?;
    }
    Ok(())
}

pub fn find_request<'a>(lines: &'a [ApprovalLine], id: &str) -> Result<&'a ApprovalLine> {
    pending_approvals(lines)
        .into_iter()
        .find(|line| matches!(line, ApprovalLine::Request { id: pending, .. } if pending == id))
        .with_context(|| {
            format!("no pending approval {id}; `wsx task ledger` lists the pending ones")
        })
}

/// What an approved action did.
pub struct Approved {
    pub ok: bool,
    pub outcome: String,
    /// The action's own `--json` answer, or null.
    pub result: serde_json::Value,
}

/// Run an approved request's action as its own `wsx task` command, without the actor
/// mark, and record the decision. A task that moved past the requested revision is
/// recorded as not run and is an error.
pub fn approve(name: &str, main: &Path, id: &str, wsx: &Path) -> Result<Approved> {
    let project_id = store::project_id(name, main);
    let lines = store::read_approvals(&project_id)?;
    let ApprovalLine::Request {
        action,
        task,
        revision,
        commit,
        grant,
        ..
    } = find_request(&lines, id)?.clone()
    else {
        unreachable!("find_request returns requests");
    };
    // ^ The owner approves one revision: a task that moved on needs a new request.
    if let (Some(task), Some(revision)) = (task, revision) {
        let now = TaskDir::open(&project_id, task)?.read_record()?.revision;
        if now != revision {
            let outcome = format!("not run: #{task} moved from r{revision} to r{now}; ask again");
            decide(&project_id, id, true, None, &outcome)?;
            bail!("{outcome}");
        }
    }
    let mut argv: Vec<String> = vec!["task".into()];
    match action {
        ApprovalAction::Land => {
            argv.extend(["land".into(), task.unwrap_or_default().to_string()]);
        }
        ApprovalAction::Discard => argv.extend([
            "close".into(),
            task.unwrap_or_default().to_string(),
            "--discard".into(),
        ]),
        ApprovalAction::Grant => {
            let grant = grant.context("the request names no grant")?;
            argv.extend(["allow".into(), task.unwrap_or_default().to_string()]);
            argv.extend(match grant.access {
                Access::Network { host } => vec!["--host".to_string(), host],
                Access::Write { path } => vec!["--path".to_string(), path],
                Access::Tool { rule } => vec![rule],
            });
            if grant.project_wide {
                argv.push("--project-wide".into());
            }
        }
        ApprovalAction::Deploy | ApprovalAction::Release => {
            argv.push(action_name(action).into());
            if let Some(commit) = commit {
                argv.extend(["--commit".into(), commit]);
            }
        }
    }
    argv.extend(["-p".into(), name.into(), "--json".into()]);
    let output = std::process::Command::new(wsx)
        .args(&argv)
        .env_remove(ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run the approved action")?;
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    let said = result["message"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            result["state"]
                .as_str()
                .map(|state| format!("state {state}"))
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string());
    let ok = output.status.success();
    let outcome = format!("{}: {said}", if ok { "done" } else { "failed" });
    decide(&project_id, id, true, None, &outcome)?;
    Ok(Approved {
        ok,
        outcome,
        result,
    })
}

/// Record that the owner denied a pending request.
pub fn deny(name: &str, main: &Path, id: &str, note: Option<String>) -> Result<()> {
    let project_id = store::project_id(name, main);
    find_request(&store::read_approvals(&project_id)?, id)?;
    let note = note.filter(|note| !note.trim().is_empty());
    if note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_MESSAGE_BYTES)
    {
        bail!("the note is larger than the limit of {MAX_MESSAGE_BYTES} bytes");
    }
    decide(&project_id, id, false, note, "denied")
}

fn decide(
    project_id: &str,
    id: &str,
    approved: bool,
    note: Option<String>,
    outcome: &str,
) -> Result<()> {
    store::append_approval(
        project_id,
        ApprovalLine::Decision {
            id: id.into(),
            approved,
            note,
            outcome: outcome.chars().take(MAX_OUTCOME_CHARS).collect(),
            unix_ms: now_unix_ms(),
        },
    )
    .map(drop)
}
