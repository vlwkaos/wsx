//! A project's tasks and its manager on the web. ref: docs/web-client.md "Tasks"
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
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use wsx_core::runtime::Snapshot;
use wsx_core::task::{
    self, call_host, call_socket,
    manager::{pending_approvals, ApprovalLine, ManagerEvent, ManagerRequest, ManagerResponse},
    owner,
    store::{self, ManagerDir, TaskDir, REPORT_FILE},
    view, HostRequest, LogEvent, TaskRecord, TaskState, Verdict, MAX_MESSAGE_BYTES,
};

const SUMMARY_CHARS: usize = 240;
const DONE_SHOWN: usize = 10;
const DONE_WITHIN_MS: u64 = 7 * 24 * 3600 * 1000;
const TASKS_LISTED: usize = 200;
const DECISIONS_SHOWN: usize = 5;
const CHAT_ENTRIES: usize = 200;
const TIMELINE_ENTRIES: usize = 300;
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const MAX_TOOL_BYTES: usize = 256;
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

fn open_task(scope: &Scope, task: u32) -> Outcome<TaskDir> {
    TaskDir::open(&scope.store, task).map_err(|_| Fault::not_found())
}

/// What the strip, the inbox, the chips and the `#` picker need, in one answer.
pub fn overview(scope: &Scope) -> Value {
    let tasks = view::project_tasks(&scope.store).unwrap_or_default();
    let now = task::now_unix_ms();
    let mut needs: Vec<(u8, u64, Value)> = Vec::new();
    let mut done: Vec<(u64, Value)> = Vec::new();
    let mut listed: Vec<(u8, u64, Value)> = Vec::new();
    let (mut working, mut stopped) = (0, 0);
    let mut titles: HashMap<u32, String> = HashMap::new();
    for (dir, record) in &tasks {
        let Ok(record) = record else { continue };
        let title = view::title(dir).unwrap_or_default();
        titles.insert(record.id, title.clone());
        let order = view::attention(record.state);
        listed.push((
            order,
            u64::MAX - record.updated_unix_ms,
            json!({
                "id": record.id, "title": title, "state": record.state,
                "revision": record.revision, "updated_unix_ms": record.updated_unix_ms,
            }),
        ));
        let result = matches!(
            record.state,
            TaskState::Blocked | TaskState::Failed | TaskState::Ready
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
        let (rank, mut row) = match record.state {
            TaskState::Blocked => (0, item("blocked")),
            TaskState::Conflict => (1, item("conflict")),
            TaskState::Failed => (1, item("failed")),
            TaskState::Ready => {
                let checks = result["result"]["human_verify"]
                    .as_array()
                    .map_or(0, Vec::len);
                let log = dir.read_log().unwrap_or_default();
                if checks > 0 && !task::passed(&log, record.revision) {
                    let mut row = item("verify");
                    row["human_verify"] = json!(checks);
                    (3, row)
                } else {
                    (4, item("land"))
                }
            }
            TaskState::InReview => {
                let mut row = item("review");
                row["pr_url"] = json!(record.pr_url);
                (4, row)
            }
            TaskState::Landed => {
                if now.saturating_sub(record.updated_unix_ms) < DONE_WITHIN_MS {
                    done.push((record.updated_unix_ms, item("done")));
                }
                continue;
            }
            TaskState::Starting | TaskState::Working | TaskState::Checking => {
                working += 1;
                continue;
            }
            TaskState::Stopped => {
                stopped += 1;
                continue;
            }
            TaskState::Closed => continue,
        };
        if record.state == TaskState::Blocked {
            row["denials"] = result["denials"].clone();
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
            skip_verify,
            commit,
            unix_ms,
        } = line
        else {
            continue;
        };
        needs.push((
            2,
            *unix_ms,
            json!({
                "kind": "approval", "approval": id, "action": owner::action_name(*action),
                "task": task, "revision": revision, "skip_verify": skip_verify, "commit": commit,
                "title": task.and_then(|task| titles.get(&task)), "unix_ms": unix_ms,
            }),
        ));
    }
    done.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    needs.extend(
        done.into_iter()
            .take(DONE_SHOWN)
            .map(|(updated, row)| (5, u64::MAX - updated, row)),
    );
    needs.sort_by_key(|(rank, order, _)| (*rank, *order));
    listed.sort_by_key(|(order, recent, _)| (*order, *recent));
    listed.truncate(TASKS_LISTED);
    json!({
        "project": scope.project,
        "needs": needs.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "working": working,
        "stopped": stopped,
        "tasks": listed.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "decisions": decisions(&lines),
    })
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

/// One task: its report, timeline, overlaps and the actions its state allows.
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
    let human_verify = result["result"]["human_verify"].clone();
    let mut summary = view::task_summary(&dir, &record);
    summary["project"] = json!(scope.project);
    summary["created_unix_ms"] = json!(record.created_unix_ms);
    summary["default_branch"] = json!(record.default_branch);
    summary["pr_url"] = json!(record.pr_url);
    summary["result"] = result["result"].clone();
    summary["denials"] = result["denials"].clone();
    summary["checks"] = result["checks"].clone();
    summary["report"] = json!(report);
    summary["timeline"] = timeline(&log);
    summary["overlaps"] = if record.state.is_open() {
        view::overlaps(&record, &records, true)
    } else {
        json!([])
    };
    summary["verification"] = view::verification_value(&log, record.revision);
    summary["approvals"] = json!(approvals);
    summary["actions"] = if can_act {
        // ^ A revision with a pass needs no verdict; a new revision brings the buttons back.
        let unverified = human_verify
            .as_array()
            .is_some_and(|items| !items.is_empty())
            && !task::passed(&log, record.revision);
        json!(actions(record.state, unverified))
    } else {
        json!([])
    };
    Ok(summary)
}

/// The actions a task's state allows. The host still decides; this only picks buttons.
fn actions(state: TaskState, unverified: bool) -> Vec<&'static str> {
    match state {
        TaskState::Starting | TaskState::Working | TaskState::Checking => vec!["steer", "add"],
        TaskState::Blocked => vec!["add", "allow"],
        // ^ The host refuses to land a revision whose Human Verify items have no pass.
        TaskState::Ready if unverified => vec!["verify", "add"],
        TaskState::Ready => vec!["land", "add"],
        TaskState::Failed | TaskState::Conflict | TaskState::Stopped => vec!["add"],
        TaskState::InReview => vec!["land", "add"],
        TaskState::Landed | TaskState::Closed => vec![],
    }
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
pub fn chat(scope: &Scope, after: Option<u64>) -> Outcome<Value> {
    let Some(dir) = ManagerDir::existing(&scope.store).map_err(internal)? else {
        return Ok(json!({ "entries": [], "skipped": 0, "running": false, "busy": false }));
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
    let status = call_socket::<_, ManagerResponse>(
        &dir.socket_path(),
        &ManagerRequest::Status,
        STATUS_TIMEOUT,
    )
    .ok();
    let busy = status.as_ref().is_some_and(|status| status.busy);
    Ok(json!({
        "entries": entries,
        "skipped": skipped,
        "running": status.is_some(),
        "busy": busy,
        "activity": if busy { activity } else { None },
    }))
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
        HostRequest::Steer { text } | HostRequest::Add { text } => {
            message(text, "the message")?;
        }
        HostRequest::Allow { tool } => {
            if tool.trim().is_empty()
                || tool.len() > MAX_TOOL_BYTES
                || tool.chars().any(char::is_control)
            {
                return Err(Fault::new(
                    400,
                    "invalid_tool",
                    format!("a tool is 1-{MAX_TOOL_BYTES} bytes without control characters"),
                ));
            }
        }
        HostRequest::Verify { result, note } => {
            if let Some(note) = note {
                message(note, "the note")?;
            } else if *result == Verdict::Fail {
                return Err(Fault::new(
                    400,
                    "note_required",
                    "a fail needs a note that says what is wrong",
                ));
            }
        }
        HostRequest::Land { .. } => {}
        HostRequest::Status | HostRequest::Stop | HostRequest::Close { .. } => {
            return Err(Fault::new(400, "unsupported", "not a web action"));
        }
    }
    let dir = open_task(scope, task)?;
    let timeout = if matches!(request, HostRequest::Land { .. }) {
        owner::LAND_TIMEOUT
    } else {
        owner::HOST_TIMEOUT
    };
    let response = call_host(&dir.socket_path(), &request, timeout).map_err(|_| {
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
    let response = owner::say(&scope.name, &scope.path, &wsx, launch, text)
        .map_err(|error| Fault::new(502, "manager_unavailable", format!("{error:#}")))?;
    if !response.ok {
        return Err(Fault::new(409, "refused", response.message));
    }
    Ok(json!({ "ok": true, "id": response.id, "message": response.message }))
}

/// Start an approved action. Its outcome is the decision line, which reaches the page as
/// a `tasks` event; a deploy can run for minutes, so the answer does not wait for it.
pub fn approve(app: &Arc<App>, scope: Scope, id: &str) -> Outcome<Value> {
    let lines = store::read_approvals(&scope.store).map_err(internal)?;
    owner::find_request(&lines, id)
        .map_err(|error| Fault::new(409, "not_pending", format!("{error:#}")))?;
    let key = format!("{}/{id}", scope.store);
    if !app
        .approving
        .lock()
        .unwrap_or_else(|lock| lock.into_inner())
        .insert(key.clone())
    {
        return Err(Fault::new(
            409,
            "running",
            format!("{id} is already being run"),
        ));
    }
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        if let Err(error) = owner::approve(&scope.name, &scope.path, &id, &wsx) {
            eprintln!("wsx web: approval {id} did not run: {error:#}");
        }
        app.approving
            .lock()
            .unwrap_or_else(|lock| lock.into_inner())
            .remove(&key);
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
    /// `(task, state word)`, or `(None, "a3 approve land #5")` for a request.
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
/// (docs/web-client.md "Task notifications").
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
            TaskState::Blocked | TaskState::Conflict => {
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
        let ApprovalLine::Request {
            id, action, task, ..
        } = line
        else {
            continue;
        };
        if known.insert(id.clone()) && !first {
            let what = task.map_or(String::new(), |task| format!(" #{task}"));
            Batch::add(
                &mut watch.urgent,
                scope,
                (
                    None,
                    format!("{id} approve {}{what}", owner::action_name(*action)),
                ),
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

/// One push for a batch: `#4 blocked · a3 approve land #5` at once, or as a digest
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
        // log can change the overview (its verdict), and the overview reads those logs whole.
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
            if parent.file_name().is_some_and(|name| name == "manager") {
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
