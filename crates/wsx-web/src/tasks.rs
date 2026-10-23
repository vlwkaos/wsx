//! A project's tasks and its manager on the web. ref: docs/web-client-tasks.md
//! ^ Reads go through `wsx_core::task`; actions go to the task host's or the manager's
//! socket. `scope` is the only code that maps a page's project id to a task store, so a
//! project outside the grant answers 404 like every other route.
use crate::{
    chat::{self, Fault, Outcome},
    site::Access,
    App,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use wsx_core::config::project::load_task_config;
use wsx_core::runtime::Snapshot;
use wsx_core::task::{
    self, call_host_waiting, call_socket, checks, copy, ledger,
    manager::{pending_approvals, ApprovalLine, ManagerEvent, ManagerRequest, ManagerResponse},
    owner,
    store::{self, ManagerDir, TaskDir, REPORT_FILE},
    view, HostRequest, LogEvent, MessageKind, TaskRecord, TaskState, WaitOn, MAX_MESSAGE_BYTES,
};

const SUMMARY_CHARS: usize = 240;
const TASKS_LISTED: usize = 200;
const DECISIONS_SHOWN: usize = 5;
const RESOLUTIONS_SHOWN: usize = 30;
/// Resolutions come from the logs of the tasks changed most recently, within a week.
const RESOLUTION_TASKS: usize = 30;
const RESOLUTION_WITHIN_MS: u64 = 7 * 24 * 3600 * 1000;
const LAST_REPLY_CHARS: usize = 120;
/// The home row's last reply and activity come from this much of the manager log's end.
const MANAGER_TAIL_BYTES: u64 = 64 * 1024;
const CHAT_ENTRIES: usize = 200;
const TIMELINE_ENTRIES: usize = 300;
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// A granted project and its task store.
pub struct Scope {
    pub project: u64,
    pub name: String,
    pub path: PathBuf,
    pub store: String,
}

pub fn scopes(snapshot: &Snapshot, access: &Access) -> Vec<Scope> {
    chat::projects(snapshot, access)
        .into_iter()
        .map(|project| Scope {
            project: project.id.0,
            name: project.name.clone(),
            path: project.path.clone(),
            store: store::project_id(&project.name, &project.path),
        })
        .collect()
}

pub fn scope(app: &App, project: u64) -> Outcome<Scope> {
    scopes(&chat::snapshot(&app.client)?, &app.site.access)
        .into_iter()
        .find(|scope| scope.project == project)
        .ok_or_else(Fault::not_found)
}

fn internal(error: anyhow::Error) -> Fault {
    Fault::new(500, "task_store", format!("{error:#}"))
}

/// The three states the owner sees (docs/web-client-tasks.md); the store keeps the rest.
/// A ready task waits for a land by hand: auto land is off, there are no checks, or it stopped.
/// One the manager created waits for the manager's review, not the owner.
fn stage(record: &TaskRecord) -> &'static str {
    if record.manager_reviews() {
        return "working";
    }
    match record.state {
        // ^ The owner works on it: not a need of the owner, and not done.
        TaskState::Waiting if record.waits_on(WaitOn::Hand) => "working",
        TaskState::Working | TaskState::Checking => "working",
        TaskState::Waiting | TaskState::Failed | TaskState::Ready => "needs_you",
        TaskState::Landed => "done",
        TaskState::Closed => "closed",
    }
}

/// A working task whose turn resolves a landing conflict: its last conflict came after its
/// last revision.
fn resolving_conflict(dir: &TaskDir) -> bool {
    let log = dir.read_log().unwrap_or_default();
    log.iter()
        .rev()
        .find_map(|entry| match entry.event {
            LogEvent::Conflict { .. } => Some(true),
            LogEvent::Revision { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// Why a ready task waits for the owner: its landing stopped, it has no checks, or the
/// project lands by hand.
fn land_reason(dir: &TaskDir, record: &TaskRecord) -> String {
    let log = dir.read_log().unwrap_or_default();
    let stopped = log.iter().rev().find_map(|entry| match &entry.event {
        LogEvent::LandStopped { reason } => Some(Some(reason.clone())),
        LogEvent::Revision { .. } => Some(None),
        _ => None,
    });
    if let Some(Some(reason)) = stopped {
        return format!("could not land: {}", view::clip(&reason, SUMMARY_CHARS));
    }
    if let Some(hold) = &record.hold {
        return format!("try it first: {hold}");
    }
    if record.manager_reviews() {
        return "the manager reviews it".into();
    }
    if record.checks.is_empty() {
        return "no checks; land by hand".into();
    }
    "ready to land".into()
}

fn open_task(scope: &Scope, task: u32) -> Outcome<TaskDir> {
    TaskDir::open(&scope.store, task).map_err(|_| Fault::not_found())
}

/// What the strip, the inbox, the chips and the `#` picker need, in one answer.
pub fn overview(scope: &Scope) -> Value {
    let tasks = view::project_tasks(&scope.store).unwrap_or_default();
    let now = task::now_unix_ms();
    let mut needs: Vec<(u8, u64, Value)> = Vec::new();
    let mut listed: Vec<(u8, u64, Value)> = Vec::new();
    let mut working = 0;
    let mut titles: HashMap<u32, String> = HashMap::new();
    for (dir, record) in &tasks {
        let Ok(record) = record else { continue };
        let title = view::readable_title(dir, record).unwrap_or_default();
        titles.insert(record.id, title.clone());
        let order = view::attention(record.state);
        listed.push((
            order,
            u64::MAX - record.updated_unix_ms,
            json!({
                "id": record.id, "title": title, "state": record.state, "stage": stage(record),
                "revision": record.revision, "updated_unix_ms": record.updated_unix_ms,
                "waiting": record.waiting,
                "hold": record.hold,
                "conflict": record.state == TaskState::Working && resolving_conflict(dir),
            }),
        ));
        let result = matches!(
            record.state,
            TaskState::Waiting | TaskState::Failed | TaskState::Ready
        )
        .then(|| view::read_result(dir))
        .flatten()
        .unwrap_or(Value::Null);
        let item = |kind: &str| {
            json!({
                "kind": kind, "task": record.id, "title": title, "state": record.state,
                "revision": record.revision, "unix_ms": record.updated_unix_ms,
                "summary": result["result"]["summary"].as_str().map(|text| view::clip(text, SUMMARY_CHARS)),
            })
        };
        if stage(record) == "working" {
            working += 1;
            continue;
        }
        let (rank, mut row) = match record.state {
            TaskState::Waiting => (0, item("waiting")),
            TaskState::Failed => (1, item("failed")),
            TaskState::Ready => {
                let mut row = item("land");
                row["reason"] = json!(land_reason(dir, record));
                (3, row)
            }
            _ => continue,
        };
        if let Some(waiting) = &record.waiting {
            row["waiting"] = json!(waiting);
            row["denials"] = result["denials"].clone();
            if waiting.on == WaitOn::Permission {
                row["asks"] = result["asks"].clone();
            }
        }
        needs.push((rank, record.updated_unix_ms, row));
    }
    let lines = store::read_approvals(&scope.store).unwrap_or_default();
    for line in pending_approvals(&lines) {
        let ApprovalLine::Request {
            id,
            action,
            task,
            revision,
            commit,
            unix_ms,
            ..
        } = line
        else {
            continue;
        };
        needs.push((
            2,
            *unix_ms,
            json!({
                "kind": "approval", "approval": id, "action": owner::action_name(*action),
                "label": owner::approval_label(line),
                "task": task, "revision": revision, "commit": commit,
                "title": task.and_then(|task| titles.get(&task)), "unix_ms": unix_ms,
            }),
        ));
    }
    needs.sort_by_key(|(rank, order, _)| (*rank, *order));
    listed.sort_by_key(|(order, recent, _)| (*order, *recent));
    listed.truncate(TASKS_LISTED);
    // ^ The latest manager entry and reply, for the home row (docs/web-client-tasks.md "Home").
    let tail = ManagerDir::existing(&scope.store)
        .ok()
        .flatten()
        .and_then(|dir| dir.read_tail(MANAGER_TAIL_BYTES).ok())
        .unwrap_or_default();
    let last_reply = tail.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Reply { text, .. } => Some(view::clip(text.trim(), LAST_REPLY_CHARS)),
        _ => None,
    });
    let last_unix_ms = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .map(|record| record.updated_unix_ms)
        .chain(tail.last().map(|entry| entry.unix_ms))
        .max();
    // ^ The manager's turn runs now: the home row shows typing dots (docs/web-client-tasks.md "Home").
    let busy = ManagerDir::existing(&scope.store)
        .ok()
        .flatten()
        .and_then(|dir| {
            call_socket::<_, ManagerResponse>(
                &dir.socket_path(),
                &ManagerRequest::Status,
                STATUS_TIMEOUT,
            )
            .ok()
        })
        .is_some_and(|status| status.busy);
    let settings = load_task_config(&scope.path).ok();
    json!({
        "project": scope.project,
        "busy": busy,
        "needs": needs.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "working": working,
        // ^ What the owner checks of landed tasks (docs/tasks.md "To check").
        "to_check": checks::open_items(&scope.store).map_or(0, |items| items.len()),
        "tasks": listed.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "decisions": decisions(&lines),
        "resolutions": resolutions(&scope.store, &tasks, &lines, now),
        "last_reply": last_reply,
        "last_unix_ms": last_unix_ms,
        // ^ Tasks can be tried on a preview link (docs/tasks.md "Preview").
        "preview": settings.as_ref().is_some_and(|settings| settings.preview.is_some()),
        // ^ A ready task can be tried on the live site, and which one is (docs/tasks.md "Hold and trial").
        "try_live": settings.as_ref().is_some_and(|settings| {
            settings.deploy_command.is_some() && !settings.deploy_public
        }),
        "trial": ledger::trial_status(&scope.store, &store::read_deploys(&scope.store).unwrap_or_default()),
    })
}

/// What the owner or the manager settled, newest first, one line each ("#4 may reach
/// crates.io (owner)", "Approved land #3"): the chat shows a resolved card as this line.
fn resolutions(
    store_id: &str,
    tasks: &[(TaskDir, anyhow::Result<TaskRecord>)],
    lines: &[ApprovalLine],
    now: u64,
) -> Value {
    let mut recent: Vec<(&TaskDir, &TaskRecord)> = tasks
        .iter()
        .filter_map(|(dir, record)| Some((dir, record.as_ref().ok()?)))
        .filter(|(_, record)| now.saturating_sub(record.updated_unix_ms) < RESOLUTION_WITHIN_MS)
        .collect();
    recent.sort_by_key(|(_, record)| std::cmp::Reverse(record.updated_unix_ms));
    recent.truncate(RESOLUTION_TASKS);
    let mut rows: Vec<(u64, Option<u32>, String)> = Vec::new();
    for (dir, record) in recent {
        let id = record.id;
        let mut blocked = false;
        for entry in dir.read_log().unwrap_or_default() {
            let text = match &entry.event {
                LogEvent::Blocked { .. } => {
                    blocked = true;
                    continue;
                }
                LogEvent::Message {
                    mode: task::MessageKind::Add,
                    ..
                } if blocked => format!("#{id} got an answer"),
                LogEvent::Granted {
                    access,
                    project,
                    by,
                } => format!(
                    "#{id} may {}{} ({})",
                    access.describe(),
                    if *project { ", every task" } else { "" },
                    match by {
                        task::Actor::Owner => "owner",
                        task::Actor::Manager => "manager",
                    }
                ),
                // ^ A landing is the worker's "done" bubble in the chat, not a line.
                LogEvent::Closed { discarded: true } => format!("#{id} was closed and discarded"),
                LogEvent::Closed { discarded: false } => format!("#{id} was closed"),
                LogEvent::Agent { .. } => continue,
                _ => {
                    blocked = false;
                    continue;
                }
            };
            blocked = false;
            rows.push((entry.unix_ms, Some(id), text));
        }
    }
    for line in lines {
        let ApprovalLine::Decision {
            id,
            approved,
            unix_ms,
            ..
        } = line
        else {
            continue;
        };
        let asked = lines.iter().find(
            |line| matches!(line, ApprovalLine::Request { id: requested, .. } if requested == id),
        );
        let Some(request @ ApprovalLine::Request { task, .. }) = asked else {
            continue;
        };
        let task = *task;
        let label = owner::approval_label(request);
        let word = if *approved { "Approved" } else { "Denied" };
        rows.push((*unix_ms, task, label.replacen("Approve", word, 1)));
    }
    // ^ A failed check says which task fixes it; passes are the owner's own taps.
    for item in checks::read(store_id).unwrap_or_default() {
        if let (checks::CheckState::Failed, Some(fix)) = (item.state, item.fix_task) {
            rows.push((
                item.unix_ms,
                Some(item.task),
                format!("#{} failed a check; #{fix} fixes it", item.task),
            ));
        }
    }
    rows.sort_by_key(|(unix_ms, _, _)| std::cmp::Reverse(*unix_ms));
    rows.truncate(RESOLUTIONS_SHOWN);
    json!(rows
        .into_iter()
        .map(|(unix_ms, task, text)| json!({ "unix_ms": unix_ms, "task": task, "text": text }))
        .collect::<Vec<_>>())
}

/// The latest decisions, newest first, with what was asked.
fn decisions(lines: &[ApprovalLine]) -> Value {
    let rows: Vec<Value> = lines
        .iter()
        .rev()
        .filter_map(|line| {
            let ApprovalLine::Decision {
                id,
                approved,
                note,
                outcome,
                unix_ms,
            } = line
            else {
                return None;
            };
            let asked = lines.iter().find_map(|line| match line {
                ApprovalLine::Request {
                    id: requested,
                    action,
                    task,
                    ..
                } if requested == id => Some((owner::action_name(*action), *task)),
                _ => None,
            });
            Some(json!({
                "approval": id, "approved": approved, "note": note, "outcome": outcome,
                "unix_ms": unix_ms, "action": asked.map(|(action, _)| action),
                "task": asked.and_then(|(_, task)| task),
            }))
        })
        .take(DECISIONS_SHOWN)
        .collect();
    json!(rows)
}

/// The project brief: the ledger the manager reads (open and recent tasks, deploys and
/// how far behind, the main checkout). The page writes it in words.
pub fn brief(scope: &Scope) -> Outcome<Value> {
    ledger::build(&scope.name, &scope.path).map_err(internal)
}

/// Whether the project deploys, and for a landed task whether the last deploy has its
/// commit, so the task story can end at Deployed. `null` when the config does not parse.
fn deployed(scope: &Scope, landed: Option<&str>) -> Value {
    let Ok(settings) = load_task_config(&scope.path) else {
        return Value::Null;
    };
    if settings.deploy_command.is_none() {
        return json!({ "configured": false });
    }
    let Ok(status) = ledger::deploy_status(&scope.name, &scope.path, &settings) else {
        return json!({ "configured": true });
    };
    let deploy = &status["deploy"];
    let has_landed = match (landed, deploy["commit"].as_str()) {
        (Some(landed), Some(commit)) => copy::unlanded_commits(&scope.path, landed, commit)
            .ok()
            .map(|count| count == 0),
        _ => Some(false),
    };
    json!({ "configured": true, "deployed": has_landed, "behind": deploy["behind"] })
}

/// One task: its report, talk, timeline (the History fold), overlaps and the actions
/// its state allows.
pub fn detail(scope: &Scope, task: u32, can_act: bool) -> Outcome<Value> {
    let dir = open_task(scope, task)?;
    let record = dir.read_record().map_err(internal)?;
    let log = dir.read_log().map_err(internal)?;
    let result = view::read_result(&dir).unwrap_or(Value::Null);
    let report = dir
        .read_file(REPORT_FILE, MAX_REPORT_BYTES)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let tasks = view::project_tasks(&scope.store).unwrap_or_default();
    let records: Vec<&TaskRecord> = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .collect();
    let lines = store::read_approvals(&scope.store).unwrap_or_default();
    let approvals: Vec<&ApprovalLine> = pending_approvals(&lines)
        .into_iter()
        .filter(|line| matches!(line, ApprovalLine::Request { task: Some(id), .. } if *id == task))
        .collect();
    let mut summary = view::task_summary(&dir, &record);
    summary["title"] = json!(view::readable_title(&dir, &record));
    summary["project"] = json!(scope.project);
    summary["created_unix_ms"] = json!(record.created_unix_ms);
    summary["default_branch"] = json!(record.default_branch);
    summary["result"] = result["result"].clone();
    summary["denials"] = result["denials"].clone();
    summary["waiting"] = json!(record.waiting);
    summary["asks"] = result["asks"].clone();
    summary["checks"] = result["checks"].clone();
    summary["report"] = json!(report);
    summary["talk"] = talk(&log);
    summary["timeline"] = timeline(&log);
    summary["overlaps"] = if record.state.is_open() {
        view::overlaps(&record, &records, true)
    } else {
        json!([])
    };
    summary["stage"] = json!(stage(&record));
    summary["conflict"] = json!(record.state == TaskState::Working && resolving_conflict(&dir));
    if record.state == TaskState::Ready {
        summary["land_reason"] = json!(land_reason(&dir, &record));
    }
    // ^ This task's items of the project's "To check" list, and the task a fail started.
    let items = checks::read(&scope.store).unwrap_or_default();
    summary["to_check"] = json!(items
        .iter()
        .filter(|item| item.task == task)
        .collect::<Vec<_>>());
    summary["fixes"] = json!(record
        .fixes
        .as_ref()
        .and_then(|id| items.iter().find(|item| &item.id == id)));
    let landed = log.iter().rev().find_map(|entry| match &entry.event {
        LogEvent::Landed { commit } => Some(commit.clone()),
        _ => None,
    });
    summary["deploy"] = deployed(scope, landed.as_deref());
    summary["hold"] = json!(record.hold);
    let trial = ledger::trial_status(
        &scope.store,
        &store::read_deploys(&scope.store).unwrap_or_default(),
    );
    // ^ The project's trial when it is this task's; `try_live` says whether one can start.
    summary["trial"] = if trial["task"] == json!(task) {
        trial
    } else {
        Value::Null
    };
    summary["landed_commit"] = json!(landed);
    summary["approvals"] = json!(approvals
        .iter()
        .map(|line| {
            let mut row = json!(line);
            if matches!(line, ApprovalLine::Request { .. }) {
                row["label"] = json!(owner::approval_label(line));
            }
            row
        })
        .collect::<Vec<_>>());
    summary["actions"] = if can_act {
        json!(actions(&record))
    } else {
        json!([])
    };
    Ok(summary)
}

/// The actions a task's state allows. The host still decides; this only picks buttons.
/// `hand` takes a build over in its working copy (docs/tasks.md "By hand").
fn actions(record: &TaskRecord) -> Vec<&'static str> {
    let on = record.waiting.as_ref().map(|waiting| waiting.on);
    let mut actions = match (record.state, on) {
        (TaskState::Waiting, Some(WaitOn::Hand)) => return vec!["handback"],
        (TaskState::Waiting, Some(WaitOn::Permission)) => vec!["add", "allow"],
        (TaskState::Working | TaskState::Checking, _) => vec!["steer", "add"],
        (TaskState::Ready, _) => vec!["land", "add", "trial"],
        (TaskState::Waiting | TaskState::Failed, _) => vec!["add"],
        (TaskState::Landed | TaskState::Closed, _) => return vec![],
    };
    if record.state != TaskState::Checking {
        actions.push("hand");
    }
    actions.push(if record.hold.is_some() {
        "unhold"
    } else {
        "hold"
    });
    actions
}

/// The messages the owner or the manager added or steered after the prompt, in order.
fn talk(log: &[task::LogEntry]) -> Value {
    let rows: Vec<Value> = log
        .iter()
        .filter_map(|entry| match &entry.event {
            LogEvent::Message {
                mode: MessageKind::Add | MessageKind::Steer,
                text,
            } => Some(json!({ "from": "owner", "text": text, "unix_ms": entry.unix_ms })),
            _ => None,
        })
        .collect();
    json!(rows)
}

/// The log without agent progress: each run of progress lines becomes one `steps` entry
/// with its count and last line.
fn timeline(log: &[task::LogEntry]) -> Value {
    let mut rows: Vec<Value> = Vec::new();
    for entry in log {
        if let LogEvent::Agent { text } = &entry.event {
            match rows.last_mut() {
                Some(last) if last["kind"] == "steps" => {
                    last["count"] = json!(last["count"].as_u64().unwrap_or(0) + 1);
                    last["last"] = json!(text);
                    last["unix_ms"] = json!(entry.unix_ms);
                }
                _ => rows.push(
                    json!({ "kind": "steps", "count": 1, "last": text, "unix_ms": entry.unix_ms }),
                ),
            }
            continue;
        }
        rows.push(serde_json::to_value(entry).unwrap_or_default());
    }
    let skipped = rows.len().saturating_sub(TIMELINE_ENTRIES);
    json!({ "entries": rows.split_off(skipped), "skipped": skipped })
}

/// The manager conversation after message `after`, and whether it runs or thinks.
/// `scopes`: the granted projects, whose managers' names a peer bubble may show.
pub fn chat(
    scope: &Scope,
    scopes: &[Scope],
    after: Option<u64>,
    media: &crate::media::Media,
) -> Outcome<Value> {
    // ^ The name the owner reads (docs/tasks.md "Config"); the role stays `manager` inside.
    let manager = json!({ "name": manager_name(&scope.path) });
    let Some(dir) = ManagerDir::existing(&scope.store).map_err(internal)? else {
        return Ok(
            json!({ "entries": [], "skipped": 0, "running": false, "busy": false, "manager": manager, "peers": {} }),
        );
    };
    let log = dir.read_log().map_err(internal)?;
    let start = after
        .and_then(|after| log.iter().position(|entry| entry.id() == Some(after)))
        .map_or(0, |index| index + 1);
    // ^ What the manager does now: its last step after its last reply.
    let activity = log
        .iter()
        .rev()
        .take_while(|entry| !matches!(entry.event, ManagerEvent::Reply { .. }))
        .find_map(|entry| match &entry.event {
            ManagerEvent::Agent { text } => Some(text.clone()),
            _ => None,
        });
    let mut entries: Vec<_> = log[start..]
        .iter()
        .filter(|entry| {
            !matches!(
                entry.event,
                ManagerEvent::Delivered { .. } | ManagerEvent::Agent { .. }
            )
        })
        .collect();
    let skipped = entries.len().saturating_sub(CHAT_ENTRIES);
    entries.drain(..skipped);
    let mut logs: HashMap<u32, Vec<task::LogEntry>> = HashMap::new();
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            let mut value = serde_json::to_value(entry).unwrap_or_default();
            // ^ An owner message's files show as attachments, not as their paths.
            if let ManagerEvent::Message {
                from: task::manager::Author::Owner,
                text,
                ..
            } = &entry.event
            {
                let (text, attachments) = chat::sent_message(text, media);
                if !attachments.is_empty() {
                    value["text"] = json!(text);
                    value["attachments"] = json!(attachments);
                }
            }
            if let ManagerEvent::Message {
                from: task::manager::Author::Events,
                text,
                ..
            } = &entry.event
            {
                let reports = worker_reports(scope, text, entry.unix_ms, &mut logs);
                if !reports.is_empty() {
                    value["reports"] = json!(reports);
                }
            }
            value
        })
        .collect();
    let status = call_socket::<_, ManagerResponse>(
        &dir.socket_path(),
        &ManagerRequest::Status,
        STATUS_TIMEOUT,
    )
    .ok();
    let busy = status.as_ref().is_some_and(|status| status.busy);
    // ^ Another project's manager by its set name, else "Manager <project>".
    let mut peers = serde_json::Map::new();
    for entry in &log {
        let peer = match &entry.event {
            ManagerEvent::Message {
                from: task::manager::Author::Manager { project, .. },
                ..
            } => project,
            ManagerEvent::Sent { to, .. } => to,
            _ => continue,
        };
        if !peers.contains_key(peer) {
            let name = scopes
                .iter()
                .find(|other| &other.name == peer)
                .and_then(|other| load_task_config(&other.path).ok()?.manager_name)
                .unwrap_or_else(|| format!("Manager {peer}"));
            peers.insert(peer.clone(), json!(name));
        }
    }
    Ok(json!({
        "manager": manager,
        "peers": peers,
        "entries": entries,
        "skipped": skipped,
        "running": status.is_some(),
        "busy": busy,
        "activity": if busy { activity } else { None },
    }))
}

/// A project's manager name as the owner reads it (`TaskConfig::manager_shown_name`).
fn manager_name(path: &Path) -> String {
    load_task_config(path)
        .unwrap_or_default()
        .manager_shown_name()
        .to_string()
}

/// A worker's report for each events line that carries a worker result (`#3 ready r2: …`):
/// the full summary of the result logged before the events message.
/// ref: docs/web-client-tasks.md "Project screen"
fn worker_reports(
    scope: &Scope,
    text: &str,
    at: u64,
    logs: &mut HashMap<u32, Vec<task::LogEntry>>,
) -> Vec<Value> {
    let mut reports = Vec::new();
    for line in text.lines() {
        let mut words = line.splitn(3, ' ');
        let (Some(id), Some(state), Some(revision)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let Some(id) = id.strip_prefix('#').and_then(|id| id.parse::<u32>().ok()) else {
            continue;
        };
        let Some(revision) = revision
            .strip_prefix('r')
            .and_then(|rest| rest.split(':').next()?.parse::<u32>().ok())
        else {
            continue;
        };
        if !matches!(state, "ready" | "failed" | "waiting" | "landed") {
            continue;
        }
        let Ok(dir) = open_task(scope, id) else {
            continue;
        };
        let log = logs
            .entry(id)
            .or_insert_with(|| dir.read_log().unwrap_or_default());
        let before = log.iter().rev().filter(|entry| entry.unix_ms <= at);
        let report = before
            .clone()
            .find_map(|entry| match &entry.event {
                LogEvent::Result {
                    status, summary, ..
                } => Some((status.clone(), summary.clone())),
                _ => None,
            })
            .map(|(status, summary)| match state {
                "failed" => ("failed".to_string(), summary),
                // ^ Ready waits for a land by hand; landed is the task done.
                "ready" => ("ready".to_string(), summary),
                "landed" => ("done".to_string(), summary),
                _ => (status, summary),
            });
        if let Some((head, summary)) = report.filter(|(_, summary)| !summary.trim().is_empty()) {
            reports.push(json!({
                "task": id, "state": state, "revision": revision, "head": head, "summary": summary,
            }));
        }
    }
    reports
}

/// The project page (docs/tasks.md "Project page"): its text and last change, or `null`s.
pub fn page(scope: &Scope) -> Outcome<Value> {
    let page = store::read_page(&scope.store).map_err(internal)?;
    Ok(match page {
        Some((text, unix_ms)) => json!({ "text": text, "unix_ms": unix_ms }),
        None => json!({ "text": null, "unix_ms": null }),
    })
}

/// What happened since `after` (ms): the tasks that started, landed, blocked, conflicted
/// or got a new revision, and the manager's replies.
pub fn since(scope: &Scope, after: u64) -> Value {
    let mut found: HashMap<&'static str, Vec<u32>> = HashMap::new();
    for (dir, record) in view::project_tasks(&scope.store).unwrap_or_default() {
        let Ok(record) = record else { continue };
        // ^ Every event below changes the record, so an older record has none of them.
        if record.updated_unix_ms <= after {
            continue;
        }
        let mut kinds: HashSet<&'static str> = HashSet::new();
        for entry in dir.read_log().unwrap_or_default() {
            if entry.unix_ms <= after {
                continue;
            }
            match entry.event {
                LogEvent::Created { .. } => kinds.insert("started"),
                LogEvent::Landed { .. } => kinds.insert("landed"),
                LogEvent::Blocked { .. } => kinds.insert("blocked"),
                LogEvent::Conflict { .. } => kinds.insert("conflict"),
                LogEvent::Revision { .. } => kinds.insert("revised"),
                _ => false,
            };
        }
        for kind in kinds {
            found.entry(kind).or_default().push(record.id);
        }
    }
    let replies = ManagerDir::existing(&scope.store)
        .ok()
        .flatten()
        .and_then(|dir| dir.read_log().ok())
        .map_or(0, |log| {
            log.iter()
                .filter(|entry| {
                    entry.unix_ms > after && matches!(entry.event, ManagerEvent::Reply { .. })
                })
                .count()
        });
    for ids in found.values_mut() {
        ids.sort_unstable();
    }
    json!({ "after": after, "tasks": found, "replies": replies })
}

fn message(text: &str, what: &str) -> Outcome<String> {
    if text.trim().is_empty() {
        return Err(Fault::new(400, "empty", format!("{what} is empty")));
    }
    if text.len() > MAX_MESSAGE_BYTES {
        return Err(Fault::new(
            413,
            "too_large",
            format!("{what} is larger than {MAX_MESSAGE_BYTES} bytes"),
        ));
    }
    Ok(text.to_string())
}

/// One request to a task's host. The host's refusal is a 409 with its reason.
pub fn act(scope: &Scope, task: u32, request: HostRequest) -> Outcome<Value> {
    match &request {
        HostRequest::Steer { text } | HostRequest::Add { text, .. } => {
            message(text, "the message")?;
        }
        HostRequest::Grant { access, .. } => {
            access
                .check()
                .map_err(|reason| Fault::new(400, "invalid_grant", reason))?;
        }
        HostRequest::Land { .. } => {}
        HostRequest::Hold { reason, .. } => {
            if let Some(reason) = reason {
                view::checked_hold(reason)
                    .map_err(|error| Fault::new(400, "invalid_hold", error.to_string()))?;
            }
        }
        // ^ They open and close the hand session or the trial slot: `run_task`, not this,
        // runs them.
        HostRequest::Hand
        | HostRequest::Handback
        | HostRequest::Trial
        | HostRequest::Status
        | HostRequest::Stop
        | HostRequest::Close { .. } => {
            return Err(Fault::new(400, "unsupported", "not a web action"));
        }
    }
    let dir = open_task(scope, task)?;
    owner::prepare_request(&dir.project_id, &request).map_err(internal)?;
    let timeout = if matches!(request, HostRequest::Land { .. }) {
        owner::LAND_TIMEOUT
    } else {
        owner::HOST_TIMEOUT
    };
    let response = call_host_waiting(&dir.socket_path(), &request, timeout).map_err(|_| {
        let state = dir
            .read_record()
            .map_or("unknown".into(), |record| record.state.as_str().to_string());
        Fault::new(
            409,
            "host_not_running",
            format!("#{task} is {state} and its host does not answer; nothing was changed"),
        )
    })?;
    if !response.ok {
        return Err(Fault::new(409, "refused", response.message));
    }
    Ok(json!({ "ok": true, "state": response.state, "message": response.message }))
}

/// The open "To check" items, oldest first, each with its task's title and what the task
/// did (the first sentence of its worker's summary, when it is not the title).
pub fn checks(scope: &Scope) -> Outcome<Value> {
    let items = checks::open_items(&scope.store).map_err(internal)?;
    let rows: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut row = json!(item);
            let dir = open_task(scope, item.task).ok();
            let title = dir
                .as_ref()
                .and_then(|dir| view::readable_title(dir, &dir.read_record().ok()?));
            let did = dir
                .as_ref()
                .and_then(view::summary_sentence)
                .filter(|sentence| Some(sentence) != title.as_ref());
            row["title"] = json!(title);
            row["did"] = json!(did);
            row
        })
        .collect();
    Ok(json!({ "open": rows }))
}

/// The capture kept with an item, as its image. ref: docs/tasks.md "To check"
pub fn check_capture(scope: &Scope, id: &str) -> Outcome<(Vec<u8>, &'static str)> {
    let items = checks::read(&scope.store).map_err(internal)?;
    let item = items
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(Fault::not_found)?;
    let name = item.capture.as_deref().ok_or_else(Fault::not_found)?;
    let dir = open_task(scope, item.task).map_err(|_| Fault::not_found())?;
    checks::read_capture(&dir, name).map_err(|_| Fault::not_found())
}

/// Pass or fail an item as the owner, through `wsx task check` so a fail starts its task
/// the way the CLI does. `all` with `pass` passes every open item.
pub fn check(scope: &Scope, id: &str, decision: &str, note: Option<String>) -> Outcome<Value> {
    let valid_id = id == "all"
        || id
            .strip_prefix('c')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !valid_id {
        return Err(Fault::not_found());
    }
    let mut argv: Vec<String> = vec!["task".into(), "check".into()];
    match (decision, id) {
        ("pass", "all") => argv.extend(["pass".into(), "--all".into()]),
        ("pass", id) => argv.extend(["pass".into(), id.into()]),
        ("fail", "all") => return Err(Fault::not_found()),
        ("fail", id) => {
            let note = message(note.as_deref().unwrap_or_default(), "the note").map_err(|_| {
                Fault::new(
                    400,
                    "note_required",
                    "a fail needs a note that says what is wrong",
                )
            })?;
            argv.extend(["fail".into(), id.into(), "--note".into(), note]);
        }
        _ => return Err(Fault::not_found()),
    }
    run_wsx(scope, argv)
}

/// `wsx task <command> <task>`: `preview` starts a preview or answers the running one
/// (`{url, port, until_unix_ms}`); `hand` and `handback` open and close the hand session the
/// way the CLI does (docs/tasks.md "By hand"); `trial` and `trial_end` take and free the
/// trial slot (docs/tasks.md "Hold and trial").
pub fn run_task(scope: &Scope, command: &str, task: u32) -> Outcome<Value> {
    let mut argv = vec!["task".into(), command.into(), task.to_string()];
    if command == "trial_end" {
        argv[1] = "trial".into();
        argv.push("--end".into());
    }
    run_wsx(scope, argv)
}

/// Run `wsx <argv> -p <project> --json` as the owner and return its JSON answer.
fn run_wsx(scope: &Scope, mut argv: Vec<String>) -> Outcome<Value> {
    argv.extend(["-p".into(), scope.name.clone(), "--json".into()]);
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    let output = std::process::Command::new(wsx)
        .args(&argv)
        .env_remove(owner::ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| internal(error.into()))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim().trim_start_matches("Error: ");
        return Err(Fault::new(409, "refused", view::clip(said, SUMMARY_CHARS)));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| internal(anyhow::anyhow!("wsx {} did not print JSON", argv[1])))
}

/// Give the manager an owner message, starting it when it does not run.
pub fn say(app: &App, scope: &Scope, text: &str) -> Outcome<Value> {
    let text = message(text, "the message")?;
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    // ^ The session is created with this server's client, which never starts wsxd.
    let launch = |argv: Vec<String>| -> anyhow::Result<()> {
        let snapshot = chat::snapshot(&app.client).map_err(|fault| anyhow::anyhow!(fault.2))?;
        let worktree = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.path == scope.path)
            .ok_or_else(|| anyhow::anyhow!("wsxd does not list the main checkout"))?;
        match chat::call(
            &app.client,
            &wsx_core::runtime::Request::SessionCreate {
                worktree_id: worktree.id,
                label: "task-manager".into(),
                command: argv,
                initial_input: None,
                rows: 24,
                cols: 80,
            },
        )
        .map_err(|fault| anyhow::anyhow!(fault.2))?
        {
            wsx_core::runtime::Response::Created { .. } => Ok(()),
            _ => anyhow::bail!("unexpected create response"),
        }
    };
    let response = owner::say(&scope.name, &scope.path, &wsx, launch, text, None)
        .map_err(|error| Fault::new(502, "manager_unavailable", format!("{error:#}")))?;
    if !response.ok {
        return Err(Fault::new(409, "refused", response.message));
    }
    Ok(json!({ "ok": true, "id": response.id, "message": response.message }))
}

/// Start an approved action. Its outcome is the decision line, which reaches the page as
/// a `tasks` event; a deploy can run for minutes, so the answer does not wait for it.
/// A second tap while it runs, or after it was decided, is not an error.
pub fn approve(app: &Arc<App>, scope: Scope, id: &str) -> Outcome<Value> {
    let lines = store::read_approvals(&scope.store).map_err(internal)?;
    if let Err(error) = owner::find_request(&lines, id) {
        let decided = lines.iter().rev().find_map(|line| match line {
            ApprovalLine::Decision {
                id: decided,
                outcome,
                ..
            } if decided == id => Some(outcome),
            _ => None,
        });
        return match decided {
            Some(outcome) => Ok(json!({ "ok": true, "decided": true, "message": outcome })),
            None => Err(Fault::new(409, "not_pending", format!("{error:#}"))),
        };
    }
    let key = format!("{}/{id}", scope.store);
    if !crate::locked(&app.approving).insert(key.clone()) {
        return Ok(json!({ "ok": true, "running": true, "message": "already running" }));
    }
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        if let Err(error) = owner::approve(&scope.name, &scope.path, &id, &wsx) {
            eprintln!("wsx web: approval {id} did not run: {error:#}");
        }
        crate::locked(&app.approving).remove(&key);
    });
    Ok(json!({ "ok": true, "started": true }))
}

pub fn deny(scope: &Scope, id: &str, note: Option<String>) -> Outcome<Value> {
    owner::deny(&scope.name, &scope.path, id, note)
        .map_err(|error| Fault::new(409, "not_pending", format!("{error:#}")))?;
    Ok(json!({ "ok": true }))
}

/// Size and modification time of each followed file of a project.
type Stamp = HashMap<PathBuf, (u64, i64, i64)>;

fn stamp(store_id: &str) -> Stamp {
    store::watched_files(store_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            Some((
                path,
                (metadata.len(), metadata.mtime(), metadata.mtime_nsec()),
            ))
        })
        .collect()
}

/// What the event observer remembers about each project's tasks between polls.
#[derive(Default)]
pub struct Watch {
    stamps: HashMap<String, Stamp>,
    overviews: HashMap<u64, String>,
    /// Each task's (state, revision) when last seen, by project; a project seen for the
    /// first time, as at a server start, notifies nothing.
    seen: HashMap<u64, HashMap<u32, (TaskState, u32)>>,
    /// Approval requests already seen, by project.
    requests: HashMap<u64, HashSet<String>>,
    /// Blocked, conflict and approval requests that wait for the merge window, by project.
    urgent: HashMap<u64, Batch>,
    /// Ready, failed and landed that wait for the digest, by project.
    digest: HashMap<u64, Batch>,
}

/// Changes that go out as one notification.
struct Batch {
    project: String,
    first: Instant,
    last: Instant,
    /// `(task, state word)`, or `(None, "Approve land #5")` for a request.
    changes: Vec<(Option<u32>, String)>,
}

impl Batch {
    fn add(batches: &mut HashMap<u64, Batch>, scope: &Scope, change: (Option<u32>, String)) {
        let now = Instant::now();
        let batch = batches.entry(scope.project).or_insert_with(|| Batch {
            project: scope.name.clone(),
            first: now,
            last: now,
            changes: Vec::new(),
        });
        batch.last = now;
        batch.changes.push(change);
    }
}

/// Turn new task states and approval requests into waiting notifications
/// (docs/web-client-tasks.md "Task notifications").
fn notice(scope: &Scope, watch: &mut Watch) {
    let first = !watch.seen.contains_key(&scope.project);
    let seen = watch.seen.entry(scope.project).or_default();
    for (_, record) in view::project_tasks(&scope.store).unwrap_or_default() {
        let Ok(record) = record else { continue };
        let now = (record.state, record.revision);
        let before = seen.insert(record.id, now);
        if first || before == Some(now) {
            continue;
        }
        let change = (Some(record.id), record.state.as_str().to_string());
        match record.state {
            // ^ Hands at work need no push; every other wait is sent at once.
            TaskState::Waiting if !record.waits_on(WaitOn::Hand) => {
                Batch::add(&mut watch.urgent, scope, change)
            }
            TaskState::Ready | TaskState::Failed | TaskState::Landed => {
                Batch::add(&mut watch.digest, scope, change)
            }
            _ => {}
        }
    }
    let lines = store::read_approvals(&scope.store).unwrap_or_default();
    let known = watch.requests.entry(scope.project).or_default();
    for line in pending_approvals(&lines) {
        let ApprovalLine::Request { id, .. } = line else {
            continue;
        };
        if known.insert(id.clone()) && !first {
            Batch::add(
                &mut watch.urgent,
                scope,
                (None, owner::approval_label(line)),
            );
        }
    }
}

/// Send each batch whose time has come: the urgent one after its merge window, the digest
/// after a quiet time or at its cap. A device whose page shows the project is skipped.
fn flush(app: &Arc<App>, watch: &mut Watch) {
    let times = &app.site.tasks;
    let now = Instant::now();
    let merge = Duration::from_secs(times.merge_seconds);
    let quiet = Duration::from_secs(times.digest_quiet_seconds);
    let cap = Duration::from_secs(times.digest_max_seconds);
    let urgent: Vec<u64> = watch
        .urgent
        .iter()
        .filter(|(_, batch)| now - batch.first >= merge)
        .map(|(project, _)| *project)
        .collect();
    let digest: Vec<u64> = watch
        .digest
        .iter()
        .filter(|(_, batch)| now - batch.last >= quiet || now - batch.first >= cap)
        .map(|(project, _)| *project)
        .collect();
    for project in urgent {
        if let Some(batch) = watch.urgent.remove(&project) {
            send(app, project, &batch, true);
        }
    }
    for project in digest {
        if let Some(batch) = watch.digest.remove(&project) {
            send(app, project, &batch, false);
        }
    }
}

/// One push for a batch: `#4 blocked · Approve land #5` at once, or as a digest
/// `2 ready (#5, #6) · 1 landed (#3)` with each task in its latest state only.
fn send(app: &Arc<App>, project: u64, batch: &Batch, urgent: bool) {
    let tasks: HashSet<u32> = batch.changes.iter().filter_map(|(task, _)| *task).collect();
    let url = match (tasks.iter().next(), tasks.len(), batch.changes.len()) {
        (Some(task), 1, 1) => format!("#/p/{project}/t/{task}"),
        _ => format!("#/p/{project}"),
    };
    let body = if urgent {
        batch
            .changes
            .iter()
            .map(|(task, words)| task.map_or(words.clone(), |task| format!("#{task} {words}")))
            .collect::<Vec<_>>()
            .join(" · ")
    } else {
        let mut latest: Vec<(u32, &str)> = Vec::new();
        for (task, words) in &batch.changes {
            let Some(task) = task else { continue };
            latest.retain(|(other, _)| other != task);
            latest.push((*task, words));
        }
        let mut words: Vec<&str> = latest.iter().map(|(_, words)| *words).collect();
        words.sort_unstable();
        words.dedup();
        words
            .iter()
            .map(|word| {
                let ids: Vec<String> = latest
                    .iter()
                    .filter(|(_, other)| other == word)
                    .map(|(task, _)| format!("#{task}"))
                    .collect();
                format!("{} {word} ({})", ids.len(), ids.join(", "))
            })
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let payload = json!({
        "kind": "tasks",
        "title": format!("{}: {}", app.site.name, batch.project),
        "body": view::clip(&body, 300),
        "url": url,
        "tag": format!("wsx-{}-tasks-{project}-{}", app.site.id, if urgent { "now" } else { "digest" }),
    });
    crate::events::deliver(
        app,
        crate::push::Topic::Tasks,
        payload,
        app.showing(project),
    );
}

/// One observer step: publish `tasks` when a project's overview changed, `task` for each
/// task whose record or log changed, and `manager` when the conversation changed.
pub fn watch(app: &Arc<App>, snapshot: &Snapshot, watch: &mut Watch) {
    if !app.site.allows(crate::site::Capability::TaskView) {
        return;
    }
    let scopes = scopes(snapshot, &app.site.access);
    for scope in &scopes {
        let now = stamp(&scope.store);
        let before = watch.stamps.insert(scope.store.clone(), now.clone());
        if before.as_ref() == Some(&now) {
            continue;
        }
        let changed: Vec<&PathBuf> = now
            .iter()
            .filter(|(path, stamp)| {
                before.as_ref().and_then(|before| before.get(*path)) != Some(stamp)
            })
            .map(|(path, _)| path)
            .collect();
        notice(scope, watch);
        // ^ A working agent appends a progress line about every second. Only a ready task's
        // log can change the overview (a landing that stopped), and the overview reads those
        // logs whole.
        let ready = |id: u32| {
            watch
                .seen
                .get(&scope.project)
                .and_then(|seen| seen.get(&id))
                .is_some_and(|(state, _)| *state == TaskState::Ready)
        };
        let progress_only = before
            .as_ref()
            .is_some_and(|before| before.len() == now.len())
            && changed.iter().all(|path| {
                path.file_name().is_some_and(|name| name == store::LOG_FILE)
                    && path
                        .parent()
                        .and_then(|parent| parent.file_name()?.to_str()?.parse().ok())
                        .is_none_or(|id| !ready(id))
            });
        if !progress_only {
            let overview = overview(scope);
            let text = overview.to_string();
            if watch.overviews.get(&scope.project) != Some(&text) {
                watch.overviews.insert(scope.project, text);
                crate::events::publish_json(app, "tasks", overview);
            }
        }
        if before.is_none() {
            continue;
        }
        let mut tasks: HashSet<u32> = HashSet::new();
        for path in changed {
            let Some(parent) = path.parent() else {
                continue;
            };
            if path
                .file_name()
                .is_some_and(|name| name == store::PAGE_FILE)
            {
                crate::events::publish_json(app, "page", json!({ "project": scope.project }));
            } else if parent.file_name().is_some_and(|name| name == "manager") {
                crate::events::publish_json(app, "manager", json!({ "project": scope.project }));
            } else if let Some(id) = parent
                .file_name()
                .and_then(|name| name.to_str()?.parse::<u32>().ok())
            {
                tasks.insert(id);
            }
        }
        for task in tasks {
            crate::events::publish_json(
                app,
                "task",
                json!({ "project": scope.project, "task": task }),
            );
        }
    }
    flush(app, watch);
    let alive: HashSet<u64> = scopes.iter().map(|scope| scope.project).collect();
    watch.overviews.retain(|project, _| alive.contains(project));
    watch.seen.retain(|project, _| alive.contains(project));
    watch.requests.retain(|project, _| alive.contains(project));
    let stores: HashSet<&String> = scopes.iter().map(|scope| &scope.store).collect();
    watch.stamps.retain(|store, _| stores.contains(store));
}

/// Every granted project's overview, for a page that just opened.
pub fn overviews(app: &App) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    Ok(json!(scopes(&snapshot, &app.site.access)
        .iter()
        .map(overview)
        .collect::<Vec<_>>()))
}
