//! What the owner does through a project's manager: start it, talk to it, and decide
//! its approval requests. Shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Manager", "Approvals"

use super::manager::{
    pending_approvals, waiting_approvals, ApprovalAction, ApprovalLine, ManagerRequest,
    ManagerResponse,
};
use super::store::{self, ManagerDir, TaskDir};
use super::{call_socket, now_unix_ms, MAX_MESSAGE_BYTES};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Set to `manager` for the manager's agent; its commands then ask instead of acting.
pub const ACTOR_ENV: &str = "WSX_TASK_ACTOR";
pub const HOST_TIMEOUT: Duration = Duration::from_secs(15);
/// A landing rebases the branch onto the default branch before the host answers.
pub const LAND_TIMEOUT: Duration = Duration::from_secs(300);
const MANAGER_START_WAIT: Duration = Duration::from_secs(10);
/// Most characters of an action's answer kept in its decision.
const MAX_OUTCOME_CHARS: usize = 1000;

/// Start the project's manager host, or the open chat's (`chat`), unless one answers. True
/// when it was started. `launch` creates the hidden session (its label, `task-manager` or
/// `task-<chat>`) in the main checkout with the given argv; the CLI may start wsxd for it,
/// the web client never does.
pub fn ensure_manager(
    name: &str,
    main: &Path,
    wsx: &Path,
    chat: Option<&str>,
    launch: impl FnOnce(String, Vec<String>) -> Result<()>,
) -> Result<(bool, ManagerDir)> {
    let project_id = store::project_id(name, main);
    let dir = match chat {
        None => ManagerDir::open(&project_id)?,
        Some(chat) => super::chats::open_chat(&project_id, chat)?,
    };
    let status = || {
        call_socket::<_, ManagerResponse>(&dir.socket_path(), &ManagerRequest::Status, HOST_TIMEOUT)
    };
    if status().is_ok() {
        return Ok((false, dir));
    }
    let mut argv = vec![
        wsx.to_string_lossy().into_owned(),
        "task".into(),
        "manager-host".into(),
        project_id,
    ];
    if let Some(chat) = chat {
        argv.extend(["--chat".into(), chat.into()]);
    }
    let label = format!("task-{}", dir.name());
    launch(label.clone(), argv)
        .context("the manager did not start; no session was left running")?;
    let deadline = Instant::now() + MANAGER_START_WAIT;
    while Instant::now() < deadline {
        if status().is_ok() {
            return Ok((true, dir));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "the session started but its host does not answer; look at session {label} in {}",
        main.display()
    )
}

/// Give the manager, or the open chat `chat`, a message, starting it first when needed.
/// `from` is another manager or chat; none is the owner. `short`: an events message's lines
/// as the chat shows them.
#[allow(clippy::too_many_arguments)]
pub fn say(
    name: &str,
    main: &Path,
    wsx: &Path,
    chat: Option<&str>,
    launch: impl FnOnce(String, Vec<String>) -> Result<()>,
    text: String,
    from: Option<super::manager::Author>,
    short: Vec<super::manager::Short>,
) -> Result<ManagerResponse> {
    let (_, dir) = ensure_manager(name, main, wsx, chat, launch)?;
    call_socket(
        &dir.socket_path(),
        &ManagerRequest::Say { text, from, short },
        HOST_TIMEOUT,
    )
}

/// Ask the running manager host to write to the owner with no turn. It never starts a host: an
/// error when none answers, `ok: false` with `busy` while a turn runs.
pub fn post(
    project_id: &str,
    event: String,
    text: String,
    level: Option<super::manager::Level>,
) -> Result<ManagerResponse> {
    call_socket(
        &ManagerDir::open(project_id)?.socket_path(),
        &ManagerRequest::Post { event, text, level },
        HOST_TIMEOUT,
    )
}

/// Ask the running manager host to send `why` as an event at `at_ms`, in place of the last
/// check-in. It never starts a host: an error when none answers.
pub fn checkin(project_id: &str, at_ms: u64, why: String) -> Result<ManagerResponse> {
    call_socket(
        &ManagerDir::open(project_id)?.socket_path(),
        &ManagerRequest::Checkin { at_ms, why },
        HOST_TIMEOUT,
    )
}

pub fn action_name(action: ApprovalAction) -> &'static str {
    match action {
        ApprovalAction::Land => "land",
        ApprovalAction::Deploy => "deploy",
        ApprovalAction::Release => "release",
        ApprovalAction::Discard => "close --discard",
        ApprovalAction::Push => "push",
        ApprovalAction::Project => "project add",
    }
}

/// An approval request as the action it asks for, never its id: "Approve land #3",
/// "Approve discard #4", "Approve deploy". A push names what it carries: "Approve push #319
/// #320 (9 commits)", else the commit. ref: docs/web-client-tasks.md
pub fn approval_label(request: &ApprovalLine) -> String {
    let ApprovalLine::Request {
        action,
        task,
        commit,
        lease,
        tasks,
        commits,
        path,
        ..
    } = request
    else {
        return String::new();
    };
    let verb = match action {
        ApprovalAction::Discard => "discard",
        ApprovalAction::Push if lease.is_some() => "force push",
        other => action_name(*other),
    };
    let mut label = match (task, action, commit) {
        (Some(task), ..) => format!("Approve {verb} #{task}"),
        // ^ A project add names the folder; the URL is the card's second line.
        _ if path.is_some() => format!("Approve {verb} {}", path.as_deref().unwrap_or_default()),
        // ^ A push with no landed task names the commit: "Approve force push 1a2b3c4d5e6f".
        (None, ApprovalAction::Push, Some(commit)) if tasks.is_empty() => {
            format!("Approve {verb} {}", super::view::short_commit(commit))
        }
        _ => format!("Approve {verb}"),
    };
    for task in tasks.iter().take(MAX_LABEL_TASKS) {
        label += &format!(" #{task}");
    }
    if tasks.len() > MAX_LABEL_TASKS {
        label += &format!(" +{} more", tasks.len() - MAX_LABEL_TASKS);
    }
    match commits {
        Some(1) => label += " (1 commit)",
        Some(count) => label += &format!(" ({count} commits)"),
        None => {}
    }
    label
}

/// A request the owner approved in the chat that waits for task `after` to land, in words:
/// "release after #382, approved by the owner". ref: docs/tasks.md "Approvals"
pub fn waiting_label(request: &ApprovalLine) -> String {
    let after = match request {
        ApprovalLine::Request {
            after: Some(after), ..
        } => *after,
        _ => return String::new(),
    };
    let label = approval_label(request);
    format!(
        "{} after #{after}, approved by the owner",
        label.strip_prefix("Approve ").unwrap_or(&label)
    )
}

/// Most task numbers a push label lists before "+n more".
const MAX_LABEL_TASKS: usize = 8;

/// What a push of `to` brings to origin, whose tip is `from` (none: a branch origin lacks):
/// the landed tasks (by number) whose landed commit lies in `from..to`, and the commits.
/// Commits carry no task ids, so a task is matched by the `Landed` entry of its log.
pub fn carried(
    project_id: &str,
    main: &Path,
    from: Option<&str>,
    to: &str,
) -> Result<(Vec<u32>, u32)> {
    let commits = super::copy::commits_after(main, from, to)?;
    let on = |commit: &str, tip: &str| super::copy::is_ancestor(main, commit, tip).unwrap_or(false);
    let mut tasks = Vec::new();
    for id in store::task_numbers(project_id)? {
        let Ok(dir) = TaskDir::open(project_id, id) else {
            continue;
        };
        if dir
            .read_record()
            .map_or(true, |record| record.state != super::TaskState::Landed)
        {
            continue;
        }
        let landed = dir.last_entry(|entry| match &entry.event {
            super::LogEvent::Landed { commit } => Some(commit.clone()),
            _ => None,
        });
        if let Ok(Some(commit)) = landed {
            if on(&commit, to) && !from.is_some_and(|from| on(&commit, from)) {
                tasks.push(id);
            }
        }
    }
    Ok((tasks, commits))
}

/// A decided request in words, from its label: "Approved land #3", "Denied discard #4",
/// "Dropped discard #4: #4 is closed".
pub fn decision_words(label: &str, approved: bool, outcome: &str) -> String {
    match (approved, outcome.strip_prefix(DROPPED)) {
        (true, _) => label.replacen("Approve", "Approved", 1),
        (false, Some(why)) => format!("{}: {why}", label.replacen("Approve", "Dropped", 1)),
        (false, None) => label.replacen("Approve", "Denied", 1),
    }
}

/// The outcome of a request wsx settled itself because its task moved on: "dropped; <why>".
pub const DROPPED: &str = "dropped; ";

/// Why a pending request no longer applies: its task left or moved past the revision it
/// names. ref: docs/tasks.md "Approvals"
pub fn stale_reason(request: &ApprovalLine, record: &super::TaskRecord) -> Option<String> {
    let ApprovalLine::Request { task, revision, .. } = request else {
        return None;
    };
    let task = (*task)?;
    if !record.state.is_open() {
        return Some(format!("#{task} is {}", record.state.as_str()));
    }
    revision
        .filter(|revision| *revision != record.revision)
        .map(|revision| format!("#{task} moved from r{revision} to r{}", record.revision))
}

/// The pending request `id`. A request that was decided answers with its outcome ("dropped;
/// replaced by a170"), and nothing runs.
pub fn find_request<'a>(lines: &'a [ApprovalLine], id: &str) -> Result<&'a ApprovalLine> {
    pending_approvals(lines)
        .into_iter()
        .find(|line| matches!(line, ApprovalLine::Request { id: pending, .. } if pending == id))
        .with_context(|| {
            let decided = lines.iter().rev().find_map(|line| match line {
                ApprovalLine::Decision {
                    id: decided,
                    outcome,
                    ..
                } if decided == id => Some(outcome),
                _ => None,
            });
            let waiting = waiting_approvals(lines)
                .into_iter()
                .find_map(|line| match line {
                    ApprovalLine::Request {
                        id: waiting,
                        after: Some(after),
                        ..
                    } if waiting == id => Some(*after),
                    _ => None,
                });
            match (decided, waiting) {
                (_, Some(after)) => format!(
                    "approval {id} was approved by the owner's message; it runs when #{after} lands"
                ),
                (Some(outcome), _) => format!("approval {id} was decided: {outcome}; nothing ran"),
                (None, _) => {
                    format!("no pending approval {id}; `wsx task ledger` lists the pending ones")
                }
            }
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
    run_request(name, &project_id, find_request(&lines, id)?.clone(), wsx)
}

/// Run a request that waited for its `after` task, once that task has landed: the same run
/// as `approve`, its decision naming the owner's message. Refused while the task is open.
/// ref: docs/tasks.md "Approvals"
pub fn approve_waiting(name: &str, main: &Path, id: &str, wsx: &Path) -> Result<Approved> {
    let project_id = store::project_id(name, main);
    let lines = store::read_approvals(&project_id)?;
    let request = waiting_approvals(&lines)
        .into_iter()
        .find(|line| line.id() == id)
        .with_context(|| format!("no waiting approval {id}"))?
        .clone();
    if let ApprovalLine::Request {
        after: Some(after), ..
    } = &request
    {
        let state = TaskDir::open(&project_id, *after)?.read_record()?.state;
        if state != super::TaskState::Landed {
            bail!("#{after} is {}, not landed; {id} waits", state.as_str());
        }
    }
    run_request(name, &project_id, request, wsx)
}

fn run_request(
    name: &str,
    project_id: &str,
    request: ApprovalLine,
    wsx: &Path,
) -> Result<Approved> {
    let ApprovalLine::Request {
        id,
        action,
        task,
        revision,
        commit,
        lease,
        path,
        url,
        said: owner_said,
        ..
    } = request
    else {
        unreachable!("only requests run");
    };
    let id = id.as_str();
    // ^ The owner approves one revision: a task that moved on needs a new request.
    if let (Some(task), Some(revision)) = (task, revision) {
        let now = TaskDir::open(project_id, task)?.read_record()?.revision;
        if now != revision {
            let outcome = format!("not run: #{task} moved from r{revision} to r{now}; ask again");
            decide(project_id, id, true, None, &outcome, owner_said)?;
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
        ApprovalAction::Project => {
            argv.extend(["project".into(), "add".into(), path.unwrap_or_default()]);
            if let Some(url) = url {
                argv.extend(["--clone".into(), url]);
            }
        }
        ApprovalAction::Push => {
            argv.push("push".into());
            // ^ No commit: the default branch tip at run time (a push that waited for a land).
            if let Some(commit) = commit {
                argv.extend(["--commit".into(), commit]);
            }
            // ^ The owner approved a rewrite over the origin tip the request saw, no other.
            if let Some(lease) = lease {
                argv.extend(["--force".into(), format!("--lease={lease}")]);
            }
        }
        ApprovalAction::Deploy | ApprovalAction::Release => {
            argv.push(action_name(action).into());
            if let Some(commit) = commit {
                argv.extend(["--commit".into(), commit]);
            }
            // ^ The owner's message asked for it, as it would a release now: open To check
            // items stay on the release.
            if owner_said.is_some() && action == ApprovalAction::Release {
                argv.push("--checks-waived".into());
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
    decide(project_id, id, true, None, &outcome, owner_said)?;
    Ok(Approved {
        ok,
        outcome,
        result,
    })
}

/// Run `wsx <argv> -p <project> --json` as the owner (no task actor) and return its JSON
/// answer, or what it said when it failed. The web and the TUI both run an owner's task
/// action this way, the CLI's own path.
pub fn run_as_owner(
    wsx: &Path,
    project: &str,
    mut argv: Vec<String>,
) -> Result<serde_json::Value, String> {
    argv.extend(["-p".into(), project.into(), "--json".into()]);
    let output = std::process::Command::new(wsx)
        .args(&argv)
        .env_remove(ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(said.trim().trim_start_matches("Error: ").to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| format!("wsx {} did not print JSON", argv[1]))
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
    decide(&project_id, id, false, note, "denied", None)
}

/// Settle a pending request that no longer applies, with why; the manager host tells the
/// manager and the owner in the chat.
pub fn drop_request(project_id: &str, id: &str, why: &str) -> Result<()> {
    decide(
        project_id,
        id,
        false,
        None,
        &format!("{DROPPED}{why}"),
        None,
    )
}

/// Record a request the owner approved by asking for it in the chat: the manager names the
/// owner's message `said` of its running turn, and the action runs without a button.
/// The id of the recorded request is returned. ref: docs/tasks.md "Approvals"
pub fn approve_by_message(project_id: &str, request: ApprovalLine, said: u64) -> Result<String> {
    check_owner_said(project_id, request.chat(), said)?;
    let line = store::append_approval(project_id, request)?;
    let id = line.id().to_string();
    store::append_approval(
        project_id,
        ApprovalLine::Decision {
            id: id.clone(),
            approved: true,
            note: None,
            outcome: format!("approved by owner message {said}"),
            owner_said: Some(said),
            unix_ms: now_unix_ms(),
        },
    )?;
    Ok(id)
}

/// Record a request the owner approved in the chat that can run only once task `after` has
/// landed: `said` is checked now, in the owner's turn, and kept on the request; the manager
/// host runs it when `after` lands (`approve_waiting`). Nothing is recorded when a check fails.
/// ref: docs/tasks.md "Approvals"
pub fn approve_after(project_id: &str, mut request: ApprovalLine, said: u64) -> Result<String> {
    check_owner_said(project_id, request.chat(), said)?;
    let ApprovalLine::Request {
        after: Some(after),
        task,
        said: kept,
        ..
    } = &mut request
    else {
        bail!("only a request that waits is recorded");
    };
    if *task == Some(*after) {
        bail!("--after {after}: a task cannot wait for itself; nothing was recorded");
    }
    let state = TaskDir::open(project_id, *after)
        .and_then(|dir| dir.read_record())
        .with_context(|| format!("--after {after}: no such task; nothing was recorded"))?
        .state;
    if !state.is_open() {
        bail!(
            "--after {after}: #{after} is {}, not open; nothing was recorded",
            state.as_str()
        );
    }
    *kept = Some(said);
    Ok(store::append_approval(project_id, request)?
        .id()
        .to_string())
}

/// Refuse unless `said` is an owner message of the manager's running turn.
pub fn check_owner_said(project_id: &str, chat: Option<&str>, said: u64) -> Result<()> {
    let manager = ManagerDir::existing_as(project_id, chat)?
        .context("the chat has no log, so no owner message")?;
    super::manager::owner_said_in_turn(&manager.read_log()?, said)
        .map_err(|why| anyhow::anyhow!("--owner-said {said}: {why}; nothing was run"))
}

fn decide(
    project_id: &str,
    id: &str,
    approved: bool,
    note: Option<String>,
    outcome: &str,
    owner_said: Option<u64>,
) -> Result<()> {
    store::append_approval(
        project_id,
        ApprovalLine::Decision {
            id: id.into(),
            approved,
            note,
            outcome: outcome.chars().take(MAX_OUTCOME_CHARS).collect(),
            owner_said,
            unix_ms: now_unix_ms(),
        },
    )
    .map(drop)
}
