//! The commands on one task: list, show, hand, close (docs/tasks.md).

use super::approve::{ask_covering, owner_only, request_of};
use crate::cli::PromptSource;
use crate::cli::{existing_daemon_snapshot, load_config};
use crate::task::args::OwnerSaid;
use crate::task::{acting_as_manager, need, open, open_owned, owner_said, send};
use crate::task::{ask_host_ok, print_json, project_entry};
use crate::task::{preview, trial};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use wsx_core::task::checks::{self};
use wsx_core::task::manager::ApprovalAction;
use wsx_core::task::owner::HOST_TIMEOUT;
use wsx_core::task::policy::Capability;
use wsx_core::task::view::checked_files;
use wsx_core::task::view::{
    overlaps, project_tasks, refuse_open_children, task_summary, MAX_RESULT_READ_BYTES,
};
use wsx_core::task::MAX_MESSAGE_BYTES;
use wsx_core::task::{
    call_host, copy, now_unix_ms,
    store::{self, TaskDir, REPORT_FILE, RESULT_FILE},
    HostRequest, LogEvent, TaskRecord, TaskState,
};
use wsx_core::{ops, runtime};

pub(crate) const CLOSE_WAIT: Duration = Duration::from_secs(15);

pub(crate) fn list(project: Option<&str>, all: bool, json: bool) -> Result<()> {
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

pub(crate) fn show(dir: &TaskDir, json: bool) -> Result<()> {
    let record = dir.read_record()?;
    let result = dir
        .read_file(RESULT_FILE, MAX_RESULT_READ_BYTES)?
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let report = dir.file(REPORT_FILE);
    let report = report.exists().then_some(report);
    let host = call_host(&dir.socket_path(), &HostRequest::Status, HOST_TIMEOUT).is_ok();
    let mut summary = task_summary(dir, &record);
    // ^ The words every view shows; one task, so it asks whether it is stuck too.
    let approvals = store::read_approvals(&dir.project_id).unwrap_or_default();
    let now = now_unix_ms();
    let stuck = wsx_core::task::stall::stuck(dir, &record, now);
    let tasks = project_tasks(&dir.project_id)?;
    let records: Vec<&TaskRecord> = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .collect();
    let waiters = wsx_core::task::status::waiters(records.iter().copied(), record.id);
    let facts = wsx_core::task::status::Facts {
        approvals: &wsx_core::task::manager::pending_approvals(&approvals),
        stuck: stuck.as_deref(),
        deployed: None,
        waiters: &waiters,
    };
    let status = wsx_core::task::status::status(dir, &record, &facts, now);
    if json {
        summary["status"] = json!(status);
        summary["record"] = serde_json::to_value(&record)?;
        summary["result"] = result.unwrap_or(serde_json::Value::Null);
        summary["report"] = json!(report);
        summary["log"] = json!(dir.file(store::LOG_FILE));
        summary["usage"] = json!(wsx_core::task::usage::task(dir));
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
    println!("status:       {}", status.sentence);
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
    let usage = wsx_core::task::usage::task(dir);
    if usage.turns > 0 {
        println!("usage:        {}", usage.line());
    }
    Ok(())
}

/// The session the owner works in while a task waits on `hand`.
pub(crate) fn hand_label(id: u32) -> String {
    format!("task-{id}-hand")
}

/// `wsx task hand`: the host stops the worker and holds every turn, check and landing; a
/// session `task-<n>-hand` in the task's own working copy resumes a fork of the worker's
/// conversation (docs/tasks.md "By hand").
pub(crate) fn hand(dir: &TaskDir, json: bool) -> Result<()> {
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
            ops::create_session_launch(&runtime::Client::local(), &load_config()?, &record.working_copy, label.clone(), argv, None)
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
pub(crate) fn handback(dir: &TaskDir, json: bool) -> Result<()> {
    owner_only("handback")?;
    let response = ask_host_ok(dir, &HostRequest::Handback)?;
    let record = dir.read_record()?;
    let mut closed = Vec::new();
    for session in sessions_in(&record.working_copy, Some(&hand_label(record.id)))? {
        ops::kill_session(&runtime::Client::local(), session).with_context(|| {
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
pub(crate) fn hand_context(dir: &TaskDir, record: &TaskRecord) -> serde_json::Value {
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

pub(crate) fn close(dir: &TaskDir, discard: bool, json: bool) -> Result<()> {
    let mut record = dir.read_record()?;
    // ^ Also with --discard: the children's work would lose its branch.
    refuse_open_children(&dir.project_id, record.id, "close")?;
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
            record.state_unix_ms = now_unix_ms();
        }
        record.updated_unix_ms = now_unix_ms();
        dir.write_record(&record)?;
    }
    let mut removed = Vec::new();
    for session in sessions_in(&record.working_copy, None)? {
        ops::kill_session(&runtime::Client::local(), session).with_context(|| {
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
pub(crate) fn sessions_in(
    working_copy: &Path,
    label: Option<&str>,
) -> Result<Vec<runtime::SessionId>> {
    let snapshot = existing_daemon_snapshot()?;
    let worktrees: Vec<_> = snapshot
        .worktrees
        .iter()
        .filter(|worktree| wsx_core::fs_util::same_folder(&worktree.path, working_copy))
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

pub(crate) fn delete_revision_refs(main: &Path, id: u32) {
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

/// `wsx task steer`: a message into the running turn.
pub(crate) fn steer_command(
    task: &str,
    text: PromptSource,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = open_owned(project, task)?;
    let text = text.read(MAX_MESSAGE_BYTES)?;
    send(&dir, HostRequest::Steer { text }, json)
}

/// `wsx task add`: a message after the current turn, with more paths it expects to change.
pub(crate) fn add_command(
    task: &str,
    text: PromptSource,
    files: &[String],
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = open_owned(project, task)?;
    let text = text.read(MAX_MESSAGE_BYTES)?;
    let files = checked_files(&dir.read_record()?.files, files)?;
    send(&dir, HostRequest::Add { text, files }, json)
}

/// `wsx task stop`: interrupt the worker.
pub(crate) fn stop_command(task: &str, project: Option<&str>, json: bool) -> Result<()> {
    send(&open_owned(project, task)?, HostRequest::Stop, json)
}

/// `wsx task close`: remove the task; `--discard` of unlanded commits is the owner's word.
pub(crate) fn close_command(
    task: &str,
    discard: bool,
    said: OwnerSaid,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = open_owned(project, task)?;
    if discard {
        need(Capability::Discard, "close --discard")?;
    }
    // ^ Discarding work cannot be undone: the manager always asks first, or names
    // the owner's message that asked for it.
    let record = dir.read_record()?;
    if discard
        && acting_as_manager()
        && ask_covering(
            &record.project_path,
            &dir.project_id,
            request_of(ApprovalAction::Discard, Some(&record), None, None, None),
            owner_said(said.owner_said),
            json,
        )?
    {
        return Ok(());
    }
    close(&dir, discard, json)
}

/// `wsx task preview`: serve the task for a person to try, or stop the preview.
pub(crate) fn preview_command(
    task: &str,
    stop: bool,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let dir = open(project, task)?;
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
