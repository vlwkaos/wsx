//! A project's tasks and its manager on the web. ref: docs/web-client-tasks.md
//! ^ Reads go through `wsx_core::task`; actions go to the task host's or the manager's
//! socket. `scope` is the only code that maps a page's project id to a task store, so a
//! project outside the grant answers 404 like every other route.
use crate::locked;
use crate::{
    chat::{self, Fault, Outcome},
    site::Access,
    App,
};
use serde_json::{json, Value};
use std::{
    cell::OnceCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use wsx_core::config::project::load_task_config;
use wsx_core::runtime::Snapshot;
use wsx_core::task::{
    self, call_socket, call_task_host,
    chains::Chains,
    checks, copy, ledger,
    manager::{
        activity, decided_request, pending_approvals, waiting_approvals, ApprovalLine, Author,
        ManagerEntry, ManagerEvent, ManagerRequest, ManagerResponse,
    },
    needs::{self, ChatCounts, Stage, YouSource, APPROVAL_RANK},
    owner, stall, status,
    store::{self, ManagerDir, TaskDir, REPORT_FILE},
    view, HostRequest, LogEvent, MessageKind, TaskRecord, TaskState, MAX_MESSAGE_BYTES,
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
/// `resolutions` reads this much of each recent task's log end; a log within it is read whole.
const RESOLUTION_LOG_BYTES: u64 = 256 * 1024;
/// A cached overview is rebuilt after this long even when no file changed, for the words that
/// count minutes (a slot wait).
const OVERVIEW_TTL: Duration = Duration::from_secs(60);
const TIMELINE_ENTRIES: usize = 300;
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// The Routines list lists at most this many.
const MAX_ROUTINES: usize = 100;
/// The newest To check items the chat shows as bubbles.
const CHAT_CHECKS: usize = 100;

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

/// The working copy of open task `task` of the project, pinned for reading; none when the task is
/// unknown, landed or closed, or its copy is gone (`![alt](task/<n>/<path>)`, docs/web-client.md
/// "Files a message shows").
pub fn open_working_copy(scope: &Scope, task: u32) -> Option<crate::sandbox::Root> {
    let record = open_task(scope, task).ok()?.read_record().ok()?;
    let path = std::fs::canonicalize(&record.working_copy).ok()?;
    record
        .state
        .is_open()
        .then(|| crate::sandbox::Root::pin(&path).ok())?
}

/// What an overview was built from: the project's files and the settings it reads. The
/// manager's `busy` is not in it: it is read on every call.
#[derive(PartialEq)]
pub struct OverviewKey {
    stamp: store::Stamp,
    /// The open chats and the main chat: a chat that opens or closes moves tasks between views.
    chats: (Vec<String>, Option<String>),
    /// `land.auto`, the manager's shown name, `preview` set.
    settings: Option<(bool, String, bool)>,
}

/// A built overview and when, by project and the chat that looks ("" is the main chat).
pub type Overviews = HashMap<(u64, String), (OverviewKey, Instant, Value)>;

/// A chat's agent status; `None` when its manager or chat is not running.
fn manager_status(dir: &ManagerDir) -> Option<ManagerResponse> {
    call_socket::<_, ManagerResponse>(&dir.socket_path(), &ManagerRequest::Status, STATUS_TIMEOUT)
        .ok()
}

fn manager_busy(dir: Option<&ManagerDir>) -> bool {
    dir.and_then(manager_status)
        .is_some_and(|status| status.busy)
}

/// The chat `chat` looks through: its folder, or the main chat's for `None`.
fn viewer_dir(scope: &Scope, chat: Option<&str>) -> Option<ManagerDir> {
    match chat {
        None => ManagerDir::main(&scope.store).ok().flatten(),
        Some(chat) => task::chats::open_chat(chat).ok(),
    }
}

/// What the strip, the inbox, the chips and the `#` picker need, in one answer. The body is
/// kept while the project's stamp and settings are the same (and for `OVERVIEW_TTL`): the
/// files it reads change only with the stamp.
pub fn overview(app: &App, scope: &Scope) -> Value {
    overview_of(app, scope, None)
}

/// `overview_of` for a chat the page asked for: an unknown or closed one is 404.
pub fn chat_overview(app: &App, scope: &Scope, chat: Option<&str>) -> Outcome<Value> {
    if let Some(chat) = chat {
        task::chats::open_chat(chat)
            .map_err(|error| Fault::new(404, "not_found", format!("{error:#}")))?;
    }
    Ok(overview_of(app, scope, chat))
}

/// The overview as chat `chat` shows it (`None`: the main chat): the tasks, needs, To check
/// items and counts of what that chat owns (`chats::Viewer`), the rest of the body the same.
pub fn overview_of(app: &App, scope: &Scope, chat: Option<&str>) -> Value {
    overview_in(app, scope, chat, &OnceCell::new())
}

/// `overview_of` with the project's For you items (`needs::for_you`) read at most once for a
/// call that builds several overviews (`shared`).
fn overview_in(
    app: &App,
    scope: &Scope,
    chat: Option<&str>,
    shared: &OnceCell<Vec<needs::YouItem>>,
) -> Value {
    let settings = load_task_config(&scope.path).ok();
    // ^ The stamp comes before the build: a file that changes during it makes the next call build again.
    let viewer = task::chats::Viewer::new(&scope.store, chat);
    let key = OverviewKey {
        stamp: store::stamp(&scope.store),
        chats: (viewer.open().to_vec(), store::main_chat(&scope.store)),
        settings: settings.as_ref().map(|settings| {
            (
                settings.land_auto,
                settings.manager_shown_name(&scope.name).to_string(),
                settings.preview.is_some(),
            )
        }),
    };
    let kept_as = (scope.project, chat.unwrap_or_default().to_string());
    let cached = locked(&app.overviews)
        .get(&kept_as)
        .filter(|(kept, at, _)| *kept == key && at.elapsed() < OVERVIEW_TTL)
        .map(|(_, _, body)| body.clone());
    let mut body = cached.unwrap_or_else(|| {
        let land_auto = settings.as_ref().is_some_and(|settings| settings.land_auto);
        let you = shared.get_or_init(|| needs::for_you(&scope.store, land_auto));
        let mut body = build_overview(scope, settings.as_ref(), &viewer, you);
        // ^ The main chat's overview (`chat` none) carries the rows of Home: one per open chat (`chat_contacts`).
        if chat.is_none() {
            body["chats"] = json!(chat_contacts(app, scope, settings.as_ref(), &body, shared));
        }
        locked(&app.overviews).insert(kept_as, (key, Instant::now(), body.clone()));
        body
    });
    body["busy"] = json!(manager_busy(viewer_dir(scope, chat).as_ref()));
    // ^ Each row's `busy` is read on each call too, as the overview's.
    for row in body["chats"].as_array_mut().into_iter().flatten() {
        let name = row["name"].as_str().unwrap_or_default().to_string();
        let dir = viewer_dir(scope, (row["main"] != true).then_some(name.as_str()));
        row["busy"] = json!(manager_busy(dir.as_ref()));
    }
    // ^ The routine pill's counts (docs/web-client-tasks.md "Home"): the scheduler's, not a store
    // file's, so they are read on each call and not kept with the body.
    body["routines"] = json!(ledger::routine_counts(&scope.path));
    body
}

fn build_overview(
    scope: &Scope,
    settings: Option<&wsx_core::model::workspace::TaskConfig>,
    viewer: &task::chats::Viewer,
    you: &[needs::YouItem],
) -> Value {
    // ^ What waits for the owner in this chat is its For you items; the rows below add the web's fields.
    let mine: Vec<&needs::YouItem> = you
        .iter()
        .filter(|item| item.chat == viewer.name())
        .collect();
    let tasks = view::project_tasks(&scope.store).unwrap_or_default();
    let records: Vec<&TaskRecord> = tasks.iter().filter_map(|(_, r)| r.as_ref().ok()).collect();
    let now = task::now_unix_ms();
    let mut needs: Vec<(u8, u64, Value)> = Vec::new();
    let mut listed: Vec<(usize, u64, Value)> = Vec::new();
    let mut working = 0;
    let mut open_tasks = 0;
    let mut titles: HashMap<u32, String> = HashMap::new();
    let land_auto = settings.is_some_and(|settings| settings.land_auto);
    let chains = Chains::of(&tasks, land_auto);
    // ^ Task status words weigh every pending approval; the lists below are the viewing chat's own
    // (`chats::Viewer`): a task, an approval or a To check item shows in the chat that owns it only.
    let all_approvals = store::read_approvals(&scope.store).unwrap_or_default();
    let all_pending = pending_approvals(&all_approvals);
    let approvals = task::chats::approvals_of(all_approvals.clone(), viewer);
    let pending = pending_approvals(&approvals);
    for (dir, record) in &tasks {
        let Ok(record) = record else { continue };
        if !viewer.owns(record.owner_chat()) {
            continue;
        }
        let title = view::readable_title(dir, record).unwrap_or_default();
        // ^ The words every view shows (docs/tasks.md "Status"); a row shows the label.
        let waiters = status::waiters(records.iter().copied(), record.id);
        let facts = status::Facts {
            approvals: &all_pending,
            waiters: &waiters,
            ..Default::default()
        };
        let status = status::status(dir, record, &facts, now);
        let flow = view::flow(record, &records, &status, &chains);
        let stage = needs::stage(dir, record, land_auto);
        titles.insert(record.id, title.clone());
        // ^ Chain order, then the closed tasks newest first (`chains::Chains`).
        let order = chains
            .order()
            .iter()
            .position(|id| *id == record.id)
            .unwrap_or(usize::MAX);
        open_tasks += usize::from(record.state.is_open());
        listed.push((
            order,
            u64::MAX - record.updated_unix_ms,
            json!({
                "id": record.id, "title": title, "state": record.state, "stage": stage.name(),
                "revision": record.revision, "updated_unix_ms": record.updated_unix_ms,
                "waiting": record.waiting,
                "status": status,
                "flow": flow,
                "agent": record.state.is_open().then(|| view::task_agent(record)),
            }),
        ));
        let item = |kind: &str| {
            json!({
                "kind": kind, "task": record.id, "title": title, "state": record.state, "status": status,
                "flow": flow, "revision": record.revision, "unix_ms": record.updated_unix_ms,
            })
        };
        if stage == Stage::Working {
            working += 1;
            continue;
        }
        let Some(need) = mine
            .iter()
            .find(|item| item.source == YouSource::Task(record.id))
        else {
            continue;
        };
        let (rank, mut row) = (need.rank, item(need.kind));
        if let Some(waiting) = &record.waiting {
            row["waiting"] = json!(waiting);
        }
        needs.push((rank, record.updated_unix_ms, row));
    }
    for line in pending.into_iter().filter(|line| {
        mine.iter()
            .any(|item| item.source == YouSource::Approval(line.id().to_string()))
    }) {
        if let Some(item) = approval_item(line, |task| titles.get(&task).cloned()) {
            let unix_ms = item["unix_ms"].as_u64().unwrap_or_default();
            needs.push((APPROVAL_RANK, unix_ms, item));
        }
    }
    needs.sort_by_key(|(rank, order, _)| (*rank, *order));
    listed.sort_by_key(|(order, recent, _)| (*order, *recent));
    listed.truncate(TASKS_LISTED);
    // ^ The latest manager entry and reply, for the home row (docs/web-client-tasks.md "Home").
    let manager_dir = viewer_dir(
        scope,
        Some(viewer.name()).filter(|name| Some(*name) != viewer.main()),
    );
    let tail = manager_dir
        .as_ref()
        .and_then(|dir| dir.read_tail(MANAGER_TAIL_BYTES).ok())
        .unwrap_or_default();
    let last_reply = newest_news(&tail);
    // ^ The open questions: the home row shows the chat's "Asks you" while one is open, the
    // reservoir lists them (docs/web-client-tasks.md "Home").
    let asks: Vec<&task::chat::OpenAsk> = mine
        .iter()
        .filter_map(|item| match &item.source {
            YouSource::Ask(ask) => Some(ask),
            _ => None,
        })
        .collect();
    let check_items = task::chats::checks_of(
        &scope.store,
        checks::read(&scope.store).unwrap_or_default(),
        viewer,
    );
    // ^ The open items as the Project screen's To check list shows them: a row each, which
    // opens the sheet at the item (docs/web-client-tasks.md "Project screen").
    let open_checks: Vec<Value> = check_items
        .iter()
        .filter(|item| mine.iter().any(|you| you.source == YouSource::Check(item.id.clone())))
        .map(|item| json!({ "id": item.id, "task": item.task, "check": item.check, "pass_if": item.pass_if }))
        .collect();
    // ^ The newest message the chat counts as new: a reply that is not quiet or a To check
    // item. The home row marks it until this device read the chat that far
    // (docs/web-client-tasks.md "Home").
    let news = check_items
        .last()
        .map(|item| (item.added_unix_ms, format!("To check: {}", item.check)))
        .into_iter()
        .chain(last_reply.clone())
        .max_by_key(|(unix_ms, _)| *unix_ms)
        .map(news_value);
    // ^ The owner-facing activity the home row orders by: the newest message, the owner's own
    // message, a task entering its state and, in the page, an agent session's state change.
    // Record saves and the manager's `agent`, event and quiet entries do not count.
    let last_unix_ms = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .map(|record| record.state_unix_ms.max(record.created_unix_ms))
        .chain(tail.iter().filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                from: Author::Owner,
                ..
            } => Some(entry.unix_ms),
            _ => None,
        }))
        .chain(news.as_ref().and_then(|news| news["unix_ms"].as_u64()))
        .max();
    // ^ The Running, Needs you and To check trays, as a chat row counts its own (`ChatCounts`).
    let counts = ChatCounts {
        running: working + waiting_approvals(&approvals).len(),
        open: open_tasks + waiting_approvals(&approvals).len(),
        needs: mine.iter().filter(|item| item.kind != "check").count(),
        check: mine.iter().filter(|item| item.kind == "check").count(),
        asks: asks.len(),
    };
    json!({
        "project": scope.project,
        "manager": settings.map_or(scope.name.as_str(), |settings| settings.manager_shown_name(&scope.name)),
        // ^ Most urgent first, then oldest: a need's `rank` is its kind's (`NeedKind::rank`).
        "needs": needs.into_iter().map(|(rank, _, mut row)| {
            row["rank"] = json!(rank);
            row
        }).collect::<Vec<_>>(),
        "working": working,
        "counts": counts,
        // ^ What the owner approved in the chat that runs when a task lands (docs/tasks.md "Approvals").
        "waiting": waiting_approvals(&approvals)
            .into_iter()
            .filter_map(|line| match line {
                ApprovalLine::Request { id, action, task, after, .. } => Some(json!({
                    "approval": id, "action": owner::action_name(*action), "task": task,
                    "after": after, "label": owner::waiting_label(line),
                })),
                _ => None,
            })
            .collect::<Vec<_>>(),
        // ^ What the owner checks of landed tasks (docs/tasks.md "To check").
        "to_check": open_checks.len(),
        "check_items": open_checks,
        "tasks": listed.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
        "decisions": decisions(&approvals),
        "resolutions": resolutions(&check_items, &tasks, &approvals, now),
        "last_reply": last_reply.map(|(_, text)| text),
        "news": news,
        // ^ The project's main chat (null when the owner set none), whose route is the project's
        // manager: the rows of Home mark it (docs/web-client-tasks.md "Home").
        "main_chat": store::main_chat(&scope.store),
        // ^ The agent the New chat sheet's Default runs on.
        "default_agent": settings.map(view::default_agent),
        "asks": asks,
        "last_unix_ms": last_unix_ms,
        // ^ Tasks can be tried on a preview link (docs/tasks.md "Preview").
        "preview": settings.is_some_and(|settings| settings.preview.is_some()),
    })
}

/// What the owner or the manager settled, newest first, one line each ("#4 may reach
/// crates.io (owner)", "Approved land #3"): the chat shows a resolved card as this line.
fn resolutions(
    check_items: &[checks::CheckItem],
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
    // ^ `closed`: the line says the task was closed, as its event line does too (bubbles.js).
    let mut rows: Vec<(u64, Option<u32>, String, bool)> = Vec::new();
    for (dir, record) in recent {
        let id = record.id;
        let mut blocked = false;
        // ^ A log past the tail is cut at its start: an answer whose block is older is not listed.
        for entry in dir.read_log_tail(RESOLUTION_LOG_BYTES).unwrap_or_default() {
            let text = match &entry.event {
                LogEvent::Blocked { .. } => {
                    blocked = true;
                    continue;
                }
                LogEvent::Message {
                    mode: task::MessageKind::Add,
                    ..
                } if blocked => format!("#{id} got an answer"),
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
            let closed = matches!(entry.event, LogEvent::Closed { .. });
            rows.push((entry.unix_ms, Some(id), text, closed));
        }
    }
    for (at, line) in lines.iter().enumerate() {
        let ApprovalLine::Decision {
            approved,
            outcome,
            unix_ms,
            ..
        } = line
        else {
            continue;
        };
        let Some(request @ ApprovalLine::Request { task, .. }) = decided_request(lines, at) else {
            continue;
        };
        let task = *task;
        let label = owner::approval_label(request);
        rows.push((
            *unix_ms,
            task,
            owner::decision_words(&label, *approved, outcome),
            false,
        ));
    }
    // ^ A failed check says which task fixes it; passes are the owner's own taps.
    for item in check_items {
        if let (checks::CheckState::Failed, Some(fix)) = (item.state, item.fix_task) {
            rows.push((
                item.unix_ms,
                Some(item.task),
                format!("#{} failed a check; #{fix} fixes it", item.task),
                false,
            ));
        }
    }
    rows.sort_by_key(|(unix_ms, _, _, _)| std::cmp::Reverse(*unix_ms));
    rows.truncate(RESOLUTIONS_SHOWN);
    json!(rows
        .into_iter()
        .map(|(unix_ms, task, text, closed)| json!({ "unix_ms": unix_ms, "task": task, "text": text, "closed": closed }))
        .collect::<Vec<_>>())
}

/// The latest decisions, newest first, with what was asked.
fn decisions(lines: &[ApprovalLine]) -> Value {
    let rows: Vec<Value> = lines
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(at, line)| {
            let ApprovalLine::Decision {
                id,
                approved,
                note,
                outcome,
                owner_said,
                unix_ms,
            } = line
            else {
                return None;
            };
            let asked = match decided_request(lines, at) {
                Some(ApprovalLine::Request { action, task, .. }) => {
                    Some((owner::action_name(*action), *task))
                }
                _ => None,
            };
            Some(json!({
                "approval": id, "approved": approved, "note": note, "outcome": outcome,
                "unix_ms": unix_ms, "owner_said": owner_said,
                "action": asked.map(|(action, _)| action),
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
    summary["checks"] = result["checks"].clone();
    summary["report"] = json!(report);
    summary["talk"] = talk(&log);
    summary["timeline"] = json!(task::history::timeline(&log, TIMELINE_ENTRIES));
    summary["usage"] = json!(task::usage::task(&dir));
    summary["overlaps"] = if record.state.is_open() {
        view::overlaps(&record, &records, true)
    } else {
        json!([])
    };
    // ^ It asks the task host, so it is read once for the progress and the status.
    let now = task::now_unix_ms();
    let stuck = stall::stuck(&dir, &record, now);
    summary["progress"] = progress(&dir, &record, &log, stuck.as_deref());
    let settings = load_task_config(&scope.path);
    let land_auto = settings.as_ref().is_ok_and(|settings| settings.land_auto);
    summary["agent"] = view::task_agent(&record);
    summary["stage"] = json!(needs::stage(&dir, &record, land_auto).name());
    summary["conflict"] =
        json!(record.state == TaskState::Working && view::resolving_conflict(&dir));
    if record.state == TaskState::Ready {
        summary["land_blocked_by"] = json!(ledger::land_blockers(&record));
    }
    let items = checks::read(&scope.store).unwrap_or_default();
    summary["to_check"] = to_check_rows(&dir, &items, task);
    summary["fixes"] = json!(record
        .fixes
        .as_ref()
        .and_then(|id| items.iter().find(|item| &item.id == id)));
    let landed = log.iter().rev().find_map(|entry| match &entry.event {
        LogEvent::Landed { commit } => Some(commit.clone()),
        _ => None,
    });
    summary["deploy"] = deployed(scope, landed.as_deref());
    // ^ One task, so it knows whether it is stuck and deployed (docs/tasks.md "Status").
    let waiters = status::waiters(records.iter().copied(), task);
    let facts = status::Facts {
        approvals: &approvals,
        stuck: stuck.as_deref(),
        deployed: summary["deploy"]["deployed"].as_bool(),
        waiters: &waiters,
    };
    let status = status::status(&dir, &record, &facts, now);
    summary["flow"] = view::flow(&record, &records, &status, &Chains::of(&tasks, land_auto));
    summary["status"] = json!(status);
    // ^ The page shows the label and the age itself: the rest of the sentence goes in More.
    summary["status"]["rest"] = json!(status.rest());
    summary["status"]["ask"] = json!(status.ask());
    summary["hold"] = json!(record.hold);
    summary["landed_commit"] = json!(landed);
    summary["approvals"] = approval_rows(&approvals);
    summary["actions"] = if can_act {
        let on = record.waiting.as_ref().map(|waiting| waiting.on);
        json!(view::owner_actions(
            record.state,
            on,
            summary["status"]["kind"].as_str(),
            settings
                .as_ref()
                .is_ok_and(|settings| settings.preview.is_some()),
        ))
    } else {
        json!([])
    };
    Ok(summary)
}

/// This task's items of the project's "To check" list. A capture the budget removed: the row
/// says so (docs/tasks.md "To check").
fn to_check_rows(dir: &TaskDir, items: &[checks::CheckItem], task: u32) -> Value {
    json!(items
        .iter()
        .filter(|item| item.task == task)
        .map(|item| {
            let mut row = json!(item);
            if checks::capture_removed(dir, item) {
                row["capture_removed"] = json!(true);
            }
            row
        })
        .collect::<Vec<_>>())
}

/// A task's pending approvals; a request carries its label.
/// A pending request as the page lists it among the needs, with its task's title (`task_of`).
fn approval_item(line: &ApprovalLine, task_of: impl Fn(u32) -> Option<String>) -> Option<Value> {
    let ApprovalLine::Request {
        id,
        action,
        task,
        revision,
        commit,
        path,
        url,
        run,
        unix_ms,
        ..
    } = line
    else {
        return None;
    };
    let found = task.and_then(task_of);
    // ^ A run's second line: why the agent asks, and where it runs.
    let why = run.as_ref().map(|run| match run.why.is_empty() {
        true => format!("in {}", run.cwd),
        false => format!("{} · in {}", run.why, run.cwd),
    });
    Some(json!({
        "kind": "approval", "approval": id, "action": owner::action_name(*action),
        "label": owner::approval_label(line),
        "task": task, "revision": revision, "commit": commit,
        "path": path, "url": url, "why": why,
        "title": found,
        "unix_ms": unix_ms,
    }))
}

fn approval_rows(lines: &[&ApprovalLine]) -> Value {
    json!(lines
        .iter()
        .map(|line| {
            let mut row = json!(line);
            if matches!(line, ApprovalLine::Request { .. }) {
                row["label"] = json!(owner::approval_label(line));
            }
            row
        })
        .collect::<Vec<_>>())
}

/// What the top of an open task's sheet shows: its workflow steps, whether it is stuck, the
/// last write, what the worker does now and the checks that run; `null` once it is finished.
/// `now` is the log's newest entry when it is an agent line: a tool line (`said` false) or the
/// worker's own words. The sheet words the tool.
/// ref: docs/web-client-tasks.md "Task sheet"
fn progress(
    dir: &TaskDir,
    record: &TaskRecord,
    log: &[task::LogEntry],
    stuck: Option<&str>,
) -> Value {
    if !record.state.is_open() {
        return Value::Null;
    }
    let now = log.last().and_then(|entry| match &entry.event {
        LogEvent::Agent { text, said } => Some(json!({ "line": text, "said": said })),
        _ => None,
    });
    json!({
        "steps": record.steps.iter().map(|step| &step.name).collect::<Vec<_>>(),
        "step": record.step,
        "stuck": stuck,
        "last_unix_ms": stall::progress_ms(dir, record),
        "now": now,
        "running": if record.state == TaskState::Checking { json!(record.checks) } else { Value::Null },
    })
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

/// The manager conversation, or the chat `chat`'s, after message `after`, and whether it
/// runs or thinks.
/// `find`: from the oldest entry that shows these words, for a find over the whole history
/// (docs/web-client-chat.md "Find"). `around`: the part around message `around`, at any age
/// the log keeps (a tap on a quote). `asks` lists the open questions' ids.
pub fn chat(
    scope: &Scope,
    chat: Option<&str>,
    (after, find, around): (Option<u64>, Option<&str>, Option<u64>),
    media: &crate::media::Media,
    granted: &dyn Fn(&str) -> Vec<chat::Attachment>,
) -> Outcome<Value> {
    // ^ The name the owner reads (docs/tasks.md "Config"); the first chat stays `manager` inside.
    let settings = load_task_config(&scope.path).unwrap_or_default();
    let shown = settings.manager_shown_name(&scope.name);
    let mut manager = json!({ "name": chat.unwrap_or(shown) });
    let dir = match chat {
        None => ManagerDir::main(&scope.store).map_err(internal)?,
        Some(chat) => Some(
            task::chats::open_chat(chat)
                .map_err(|error| Fault::new(404, "not_found", format!("{error:#}")))?,
        ),
    };
    let Some(dir) = dir else {
        return Ok(
            json!({ "rows": [], "skipped": 0, "asks": [], "running": false, "busy": false, "manager": manager }),
        );
    };
    // ^ A chat's shown name (`chats::shown_name`) and the projects it runs in, its own first.
    if dir.named().is_some() {
        manager["name"] = task::chats::shown_name(&dir, shown).into();
        manager["projects"] = task::chats::project_names(&dir).into();
    }
    // ^ The chat's purpose (docs/tasks.md "Chats"), whole: the Purpose sheet edits it.
    manager["id"] = dir.name().into();
    manager["goal"] = task::chats::goal(&dir).into();
    // ^ The agent in use now, a fallback's while one runs; a chat's own, else the manager's.
    manager["agent"] = view::manager_agent(&dir, &settings);
    let window = task::chat::Window::new(find, after, around);
    // ^ An owner message's files show as attachments, not as their paths.
    let mut attachments: HashMap<u64, Value> = HashMap::new();
    let task::chat::Reading {
        page, approvals, ..
    } = task::chat::read_for(
        Some(&dir),
        (&scope.name, &scope.store),
        &settings,
        &window,
        |entry| {
            if let ManagerEvent::Message {
                id,
                from: Author::Owner,
                text,
                ..
            } = &mut entry.event
            {
                let (shown, files) = chat::sent_message(text, media);
                if !files.is_empty() {
                    *text = shown;
                    attachments.insert(*id, json!(files));
                }
            }
        },
    )
    .map_err(internal)?;
    let activity = activity(&page.log).map(str::to_string);
    let rows: Vec<Value> = page
        .rows
        .into_iter()
        .map(|row| {
            // ^ The files an owner message carries; for any other message, the pictures and
            // videos it links or shows (docs/web-client.md "Media").
            let files = row
                .id
                .and_then(|id| attachments.get(&id))
                .cloned()
                .or_else(|| {
                    (row.kind == task::chat::RowKind::Message)
                        .then(|| granted(&row.text))
                        .filter(|files| !files.is_empty())
                        .map(|files| json!(files))
                });
            let mut value = serde_json::to_value(row).unwrap_or_default();
            if let Some(files) = files {
                value["attachments"] = files;
            }
            value
        })
        .collect();
    let status = manager_status(&dir);
    let busy = status.as_ref().is_some_and(|status| status.busy);
    Ok(json!({
        "manager": manager,
        "rows": rows,
        "skipped": page.skipped,
        "asks": page.asks.iter().map(|ask| ask.id).collect::<Vec<_>>(),
        // ^ This chat's own pending approvals, as the overview lists the first chat's.
        "approvals": pending_approvals(&approvals)
            .into_iter()
            .filter_map(|line| {
                approval_item(line, |id| {
                    let dir = open_task(scope, id).ok()?;
                    let record = dir.read_record().ok()?;
                    view::readable_title(&dir, &record)
                })
            })
            .collect::<Vec<_>>(),
        "running": status.is_some(),
        "busy": busy,
        "activity": if busy { activity } else { None },
    }))
}

/// The newest reply of a chat that counts as news (when, text): one that is not quiet. A ping and
/// the owner's own message are none. The project's home row and a chat's contact both use it
/// (docs/web-client-tasks.md "Home", "Chats").
fn newest_news(tail: &[ManagerEntry]) -> Option<(u64, String)> {
    tail.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Reply {
            text, quiet: false, ..
        } => Some((
            entry.unix_ms,
            view::clip(task::manager::split_quote(text).1.trim(), LAST_REPLY_CHARS),
        )),
        _ => None,
    })
}

/// A news item as the page reads it: `{unix_ms, text}`.
fn news_value((unix_ms, text): (u64, String)) -> Value {
    json!({ "unix_ms": unix_ms, "text": text })
}

/// The open chats as Home rows: the row `chats::open_rows` builds (name, the first line of the
/// goal, `last_unix_ms`, `listed`, `main`, the pill `counts`) with the same web fields for every
/// chat, read from that chat's own overview (`main_body` is the main chat's):
/// `news` (`{unix_ms, text}` of the newest reply or To check item that is news, or `null`),
/// `asks` (its open questions), `first` (its most urgent need), `busy` (a turn runs now; read on
/// each call) and `last_reply` (the first line of the last reply) (docs/web-client-tasks.md "Home").
fn chat_contacts(
    app: &App,
    scope: &Scope,
    settings: Option<&wsx_core::model::workspace::TaskConfig>,
    main_body: &Value,
    you: &OnceCell<Vec<needs::YouItem>>,
) -> Vec<Value> {
    let land_auto = settings.is_some_and(|settings| settings.land_auto);
    task::chats::open_rows(&scope.store, land_auto)
        .into_iter()
        .map(|(dir, mut row)| {
            let own = match row["main"] == true {
                true => main_body.clone(),
                false => overview_in(app, scope, Some(dir.name()), you),
            };
            row["news"] = own["news"].clone();
            row["asks"] = json!(own["asks"].as_array().map_or(0, Vec::len));
            row["first"] = own["needs"].get(0).cloned().unwrap_or(Value::Null);
            row["busy"] = json!(false);
            // ^ The name that types while `busy`: the project's manager name, else the chat's own.
            row["manager"] = match row["main"] == true {
                true => main_body["manager"].clone(),
                false => row["shown"].clone(),
            };
            let tail = dir.read_tail(MANAGER_TAIL_BYTES).unwrap_or_default();
            row["last_reply"] = tail
                .into_iter()
                .rev()
                .find_map(|entry| match entry.event {
                    ManagerEvent::Reply { text, .. } => Some(view::clip(
                        task::manager::split_quote(&text)
                            .1
                            .lines()
                            .next()
                            .unwrap_or_default(),
                        LAST_REPLY_CHARS,
                    )),
                    _ => None,
                })
                .into();
            row
        })
        .collect()
}

/// A new chat of the project, opened for the owner: its name, an optional goal and an
/// optional `[[agents]]` entry (docs/tasks.md "Chats").
pub fn new_chat(
    scope: &Scope,
    name: &str,
    goal: Option<&str>,
    agent: Option<&str>,
) -> Outcome<Value> {
    let goal = goal.map(str::trim).filter(|goal| !goal.is_empty());
    let agent = agent.filter(|agent| !agent.is_empty());
    task::chats::create(&scope.store, name, goal, agent)
        .map_err(|error| Fault::new(400, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name }))
}

/// The owner closes an open chat from its header menu; its log and notes stay (docs/tasks.md
/// "Chats"). An unknown or a closed name is refused. Closing the main chat leaves the project with none.
pub fn close_chat(app: &App, scope: &Scope, name: &str) -> Outcome<Value> {
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    owner::close_chat((&scope.name, &scope.path), &wsx, name, launcher(app, scope))
        .map_err(|error| Fault::new(409, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name, "state": "closed" }))
}

/// The owner sets (or, empty, clears) the name a chat is shown by; its id stays (docs/tasks.md
/// "Chats"). A title that breaks the rule is 400 `invalid_title` with the reason; an unknown or
/// a closed name, or the first chat, is 409.
pub fn rename_chat(name: &str, title: &str) -> Outcome<Value> {
    task::chats::checked_title(title)
        .map_err(|error| Fault::new(400, "invalid_title", format!("{error:#}")))?;
    let title = task::chats::rename(name, title)
        .map_err(|error| Fault::new(409, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name, "title": title }))
}

/// The owner sets (or, empty, clears) a chat's purpose, the first chat's too; its agent reads it
/// at its next turn. A goal over the limit is 400 `invalid_goal` with the reason, an unknown or
/// a closed name 409.
pub fn set_chat_goal(name: &str, goal: &str) -> Outcome<Value> {
    task::chats::checked_goal(goal)
        .map_err(|error| Fault::new(400, "invalid_goal", format!("{error:#}")))?;
    task::chats::set_goal(name, goal)
        .map_err(|error| Fault::new(409, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name }))
}

/// The owner dismisses an open chat's row from Home; the chat, its repo and its tasks go on
/// (`chats_home`, docs/tasks.md "Chats"). An unknown or a closed name is refused.
pub fn dismiss_chat(name: &str) -> Outcome<Value> {
    task::chats_home::dismiss(name, task::now_unix_ms())
        .map_err(|error| Fault::new(409, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name, "listed": false }))
}

/// The owner puts an open chat in a Home folder, or in none (`folder` null). An unknown or a
/// closed name, or a bad folder name, is refused (`chats_home::set_folder`).
pub fn set_chat_folder(name: &str, folder: Option<&str>) -> Outcome<Value> {
    task::chats_home::set_folder(name, folder)
        .map_err(|error| Fault::new(409, "invalid_chat", format!("{error:#}")))?;
    Ok(json!({ "ok": true, "chat": name, "folder": folder.map(str::trim) }))
}

/// The `[[agents]]` entries a chat may run on, for the New chat sheet.
pub fn chat_agents() -> Value {
    json!(task::chats::agent_entries()
        .into_iter()
        .map(
            |(name, agent)| json!({ "name": name, "adapter": agent.adapter, "model": agent.model })
        )
        .collect::<Vec<_>>())
}

/// A project's manager name as the owner reads it (`TaskConfig::manager_shown_name`).
fn manager_name(scope: &Scope) -> String {
    load_task_config(&scope.path)
        .unwrap_or_default()
        .manager_shown_name(&scope.name)
        .to_string()
}

/// The project overview (docs/tasks.md "Project overview"): its text cut to the cap, whether
/// it was cut and its last change, or `null`s.
pub fn page(scope: &Scope) -> Outcome<Value> {
    let overview = store::read_overview(&scope.store).map_err(internal)?;
    Ok(match overview {
        Some((text, unix_ms)) => {
            let (lines, chars) = store::overview_size(&text);
            let (text, cut) = store::cut_overview(&text);
            json!({ "text": text, "unix_ms": unix_ms, "cut": cut.then_some(json!({ "lines": lines, "chars": chars })) })
        }
        None => json!({ "text": null, "unix_ms": null }),
    })
}

/// The project's routines as wsxd's scheduler lists them: `{routines}`, at most 100, each last
/// run with the id of its events message, or with `error` when the scheduler does not answer.
/// ref: docs/web-client-tasks.md "Routines"
pub fn routines(scope: &Scope) -> Value {
    let dir = ManagerDir::main(&scope.store).ok().flatten();
    match ledger::routine_rows(&scope.path, MAX_ROUTINES, true, dir.as_ref()) {
        (rows @ Value::Array(_), counts) => json!({ "routines": rows, "counts": counts }),
        (failed, _) => json!({ "routines": [], "error": failed["error"] }),
    }
}

/// `run`, `enable` or `disable` for the routine `name` through wsxd's scheduler, then the list
/// as `routines` gives it. The scheduler refuses a routine that does not exist or is already
/// running (409). ref: docs/web-client-tasks.md "Routines"
pub fn routine_act(scope: &Scope, name: &str, action: &str) -> Outcome<Value> {
    use wsx_core::routine::{self, Action};
    let client = wsx_core::runtime::Client::local();
    let sent = match action {
        "run" => routine::request(
            &client,
            &scope.path,
            Action::Run {
                name: name.to_string(),
            },
        ),
        "enable" | "disable" => {
            routine::set_enabled(&client, &scope.path, name, None, action == "enable")
        }
        _ => return Err(Fault::new(400, "unsupported", "not a routine action")),
    };
    sent.map_err(|error| Fault::new(409, "refused", error.to_string()))?;
    Ok(routines(scope))
}

/// What happened since `after` (ms): the tasks that started, blocked, conflicted or got a new
/// revision, those that entered a reported state (`view::REPORTED_STATES`: ready, failed,
/// landed), and the manager's replies.
pub fn since(scope: &Scope, after: u64) -> Value {
    let mut found: HashMap<&'static str, Vec<u32>> = HashMap::new();
    for (dir, record) in view::project_tasks(&scope.store).unwrap_or_default() {
        let Ok(record) = record else { continue };
        // ^ Every event below changes the record, so an older record has none of them.
        if record.updated_unix_ms <= after {
            continue;
        }
        let mut kinds: HashSet<&'static str> = HashSet::new();
        if view::REPORTED_STATES.contains(&record.state) && record.state_unix_ms > after {
            kinds.insert(record.state.as_str());
        }
        for entry in dir.read_log().unwrap_or_default() {
            if entry.unix_ms <= after {
                continue;
            }
            match entry.event {
                LogEvent::Created { .. } => kinds.insert("started"),
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
    let replies = ManagerDir::main(&scope.store)
        .ok()
        .flatten()
        .and_then(|dir| dir.read_log().ok())
        .map_or(0, |log| {
            log.iter()
                .filter(|entry| {
                    entry.unix_ms > after
                        && matches!(entry.event, ManagerEvent::Reply { quiet: false, .. })
                })
                .count()
        });
    for ids in found.values_mut() {
        ids.sort_unstable();
    }
    json!({ "after": after, "tasks": found, "replies": replies })
}

/// What the manager composer lists: the owner's skills (`/`) and the open chats of every granted
/// project (`@`) but the one the owner writes in (`chat`, else this project's main chat), each as
/// `{id, name, project}`: `id` is what `wsx task say --to` takes, `name` the shown name. The
/// rows are the Home list's (`chat_contacts`). ref: docs/web-client-tasks.md "Project screen"
pub fn mentions(app: &App, scope: &Scope, chat: Option<&str>) -> Outcome<Value> {
    let skills = crate::commands::owner_skills(&scope.path);
    let current = chat
        .map(str::to_string)
        .or_else(|| store::main_chat(&scope.store))
        .unwrap_or_default();
    let chats: Vec<Value> = scopes(&chat::snapshot(&app.client)?, &app.site.access)
        .iter()
        .flat_map(|other| {
            // ^ Only the names: not `chat_contacts`, which reads every chat's overview.
            task::chats::open_rows(&other.store, false)
                .into_iter()
                .filter(|(_, row)| row["listed"] == true)
                .map(|(_, row)| (row["name"].clone(), row["shown"].clone()))
                .collect::<Vec<_>>()
                .into_iter()
                .map(
                move |(name, shown)| json!({ "id": name, "name": shown, "project": other.name }),
            )
        })
        .filter(|row| row["id"] != json!(current))
        .collect();
    Ok(json!({
        "skills": skills.commands,
        "truncated": skills.truncated,
        "chats": chats,
    }))
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
        HostRequest::Land => {}
        HostRequest::Hold { reason, .. } => {
            if let Some(reason) = reason {
                view::checked_hold(reason)
                    .map_err(|error| Fault::new(400, "invalid_hold", error.to_string()))?;
            }
        }
        // ^ `run_task` runs the hand session; a child's host alone sends
        // `ChildLanded`; add and steer are the manager's, through `wsx task`.
        _ => return Err(Fault::new(400, "unsupported", "not a web action")),
    }
    let dir = open_task(scope, task)?;
    let timeout = if matches!(request, HostRequest::Land) {
        owner::LAND_TIMEOUT
    } else {
        owner::HOST_TIMEOUT
    };
    let response = call_task_host(&dir, &request, timeout).map_err(|error| {
        let state = dir
            .read_record()
            .map_or("unknown".into(), |record| record.state.as_str().to_string());
        Fault::new(
            409,
            "host_not_running",
            format!(
                "#{task} is {state} and its host does not answer: {error:#}; nothing was changed"
            ),
        )
    })?;
    if !response.ok {
        return Err(Fault::new(409, "refused", response.message));
    }
    Ok(json!({ "ok": true, "state": response.state, "message": response.message }))
}

/// The open "To check" items, oldest first, each with its task's title and what the task
/// did (the first sentence of its worker's summary, when it is not the title).
pub fn checks(scope: &Scope, chat: Option<&str>) -> Outcome<Value> {
    // ^ The items of the tasks that chat owns (`chats::owner`); the main chat's without one.
    let items = task::chats::checks_of(
        &scope.store,
        checks::read(&scope.store).map_err(internal)?,
        &task::chats::Viewer::new(&scope.store, chat),
    );
    // ^ The chat shows each item once, as a bubble at the time it was added, decided or not:
    // every open item and the newest decided ones.
    let recent = items.len().saturating_sub(CHAT_CHECKS);
    let rows: Vec<Value> = items
        .iter()
        .enumerate()
        .filter(|(index, item)| *index >= recent || item.state == checks::CheckState::Open)
        .map(|(_, item)| {
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
            // ^ The page sizes each capture's box before the image arrives, so the sheet
            // does not grow when it comes (docs/web-client-tasks.md "To check"). A video
            // shows its poster, so its box is the poster's.
            row["capture_sizes"] = json!(item
                .captures
                .iter()
                .map(|name| {
                    let name = match name.ends_with(".mp4") {
                        true => item.poster.as_deref().unwrap_or(name),
                        false => name,
                    };
                    dir.as_ref().and_then(|dir| checks::capture_size(dir, name))
                })
                .collect::<Vec<_>>());
            // ^ A capture the budget removed: its chat bubble says so.
            if dir.is_some_and(|dir| checks::capture_removed(&dir, item)) {
                row["capture_removed"] = json!(true);
            }
            row
        })
        .collect();
    let open: Vec<&Value> = rows.iter().filter(|row| row["state"] == "open").collect();
    Ok(json!({ "open": open, "items": rows }))
}

/// The `slot`-th capture kept with an item, an image or a video, or with `poster` the poster of
/// its first video. ref: docs/tasks.md "To check"
pub fn check_capture(
    scope: &Scope,
    id: &str,
    poster: bool,
    slot: usize,
) -> Outcome<(Vec<u8>, &'static str, String)> {
    let items = checks::read(&scope.store).map_err(internal)?;
    let item = items
        .iter()
        .find(|item| item.id == id)
        .ok_or_else(Fault::not_found)?;
    let name = match poster {
        true => item.poster.as_deref(),
        false => item.captures.get(slot).map(String::as_str),
    }
    .ok_or_else(Fault::not_found)?;
    let dir = open_task(scope, item.task).map_err(|_| Fault::not_found())?;
    let (bytes, media) = checks::read_capture(&dir, name).map_err(|_| Fault::not_found())?;
    // ^ The name a saved copy gets: task, item and kind, such as `task-7-c12.png`.
    let kind = if poster { "-poster" } else { "" };
    let number = if slot == 0 {
        String::new()
    } else {
        format!("-{}", slot + 1)
    };
    let ext = media.rsplit('/').next().unwrap_or("bin");
    Ok((
        bytes,
        media,
        format!("task-{}-{id}{number}{kind}.{ext}", item.task),
    ))
}

/// Pass, skip or fail an item as the owner, through `wsx task check`; a fail with a note starts
/// its task the way the CLI does. `all` with `pass` or `skip` closes every open item.
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
        ("pass" | "skip", "all") => argv.extend([decision.into(), "--all".into()]),
        ("pass" | "skip", id) => argv.extend([decision.into(), id.into()]),
        ("fail", "all") => return Err(Fault::not_found()),
        ("fail", id) => {
            argv.extend(["fail".into(), id.into()]);
            // ^ A note starts the task that fixes it; none closes the item as failed, and the
            // owner's reply to the item is the manager's cue.
            if let Some(note) = note.as_deref().filter(|note| !note.trim().is_empty()) {
                let note = message(note, "the note")
                    .map_err(|_| Fault::new(400, "note_invalid", "the note is too large"))?;
                argv.extend(["--note".into(), note]);
            }
        }
        _ => return Err(Fault::not_found()),
    }
    run_wsx(scope, argv)
}

/// The owner dismisses an open question: `wsx task dismiss <id>` without `WSX_TASK_ACTOR`
/// (docs/tasks.md "Manager"); a reply that is not an open question is a 409.
pub fn dismiss(scope: &Scope, id: u64) -> Outcome<Value> {
    run_wsx(scope, vec!["task".into(), "dismiss".into(), id.to_string()])
}

/// `wsx task <command> <task>`: `preview` starts a preview or answers the running one
/// (`{url, port, until_unix_ms}`); `hand` and `handback` (`hand --back`) open and close the hand session the
/// way the CLI does (docs/tasks.md "By hand").
pub fn run_task(scope: &Scope, command: &str, task: u32) -> Outcome<Value> {
    let mut argv = vec!["task".into(), command.into(), task.to_string()];
    if command == "handback" {
        argv[1] = "hand".into();
        argv.push("--back".into());
    }
    run_wsx(scope, argv)
}

/// Run `wsx <argv> -p <project> --json` as the owner and return its JSON answer.
fn run_wsx(scope: &Scope, argv: Vec<String>) -> Outcome<Value> {
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    owner::run_as_owner(&wsx, &scope.name, argv)
        .map_err(|said| Fault::new(409, "refused", view::clip(&said, SUMMARY_CHARS)))
}

/// Start a session in the main checkout for a manager or chat host.
/// ^ The session is created with this server's client, which never starts wsxd.
fn launcher<'a>(
    app: &'a App,
    scope: &'a Scope,
) -> impl Fn(String, Vec<String>) -> anyhow::Result<()> + 'a {
    move |label: String, argv: Vec<String>| -> anyhow::Result<()> {
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
                label,
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
    }
}

/// Give the manager, or the open chat `chat`, an owner message, starting it when it does not
/// run.
pub fn say(app: &App, scope: &Scope, chat: Option<&str>, text: &str) -> Outcome<Value> {
    let text = message(text, "the message")?;
    let wsx = std::env::current_exe().map_err(|error| internal(error.into()))?;
    let launch = launcher(app, scope);
    let response = owner::say(
        &scope.name,
        &scope.path,
        &wsx,
        chat,
        launch,
        text,
        None,
        Vec::new(),
    )
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

/// What the event observer remembers about each project's tasks between polls.
#[derive(Default)]
pub struct Watch {
    stamps: HashMap<String, store::Stamp>,
    overviews: HashMap<u64, String>,
    /// Each task's (state, revision) when last seen, by project; a project seen for the
    /// first time, as at a server start, notifies nothing.
    seen: HashMap<u64, HashMap<u32, (TaskState, u32)>>,
    /// Approval requests already seen, by project.
    requests: HashMap<u64, HashSet<String>>,
    /// The last reply id seen, by project and chat (`manager` for the manager).
    replies: HashMap<(u64, String), u64>,
    /// New items for "Manager needs me" that wait for the merge window.
    pending: crate::events::Pending<(u64, Key)>,
    /// New posts with a level, by project; each is a push of its own.
    alerts: crate::events::Pending<(u64, Alert)>,
    /// What waits on the owner now, by project, with the project's name.
    open: HashMap<u64, (String, Vec<Open>)>,
    /// How many items need the owner, by project: the overview's `needs`.
    needs: HashMap<u64, usize>,
}

impl Watch {
    /// What needs the owner in every granted project, for the app badge.
    pub fn needs(&self) -> usize {
        self.needs.values().sum()
    }
}

/// One item of "Manager needs me".
#[derive(Clone, PartialEq, Eq)]
enum Key {
    Task(u32),
    Approval(String),
    Checks,
    /// A reply the manager or a chat started on its own: its line `<name> wrote to you`, or the
    /// lines of one wsx wrote, and the chat it opens.
    Wrote(String, String),
}

/// A post with a level (`wsx task post --level warn|bad`): a push of its own. The text as
/// `wrote` cut it, the reply's id and the chat it opens.
#[derive(Clone, PartialEq, Eq)]
struct Alert {
    reply: u64,
    title: String,
    body: String,
    url: String,
}

/// An item that waits on the owner: a task that waits on them, an approval request, or the
/// project's open To check items (`count` of them).
struct Open {
    key: Key,
    line: String,
    url: String,
    count: usize,
}

/// The items of "Manager needs me" in an overview, in its order (docs/web-client.md
/// "Notifications"): `#4 Fix the login: Waiting for your answer`, `Approve land #5`, `2 to check`.
fn attention(project: u64, overview: &Value) -> Vec<Open> {
    let mut open = Vec::new();
    for need in overview["needs"].as_array().into_iter().flatten() {
        let task = need["task"].as_u64().map(|task| task as u32);
        let url = match task {
            Some(task) => format!("#/p/{project}/t/{task}"),
            None => format!("#/p/{project}"),
        };
        let (key, line) = match (need["kind"].as_str(), task) {
            (Some("waiting"), Some(task)) => (
                Key::Task(task),
                view::task_line(
                    task,
                    need["title"].as_str().unwrap_or(""),
                    // ^ The label: the sentence counts minutes, and a push is sent once.
                    need["status"]["label"].as_str().unwrap_or(""),
                ),
            ),
            (Some("approval"), _) => (
                Key::Approval(need["approval"].as_str().unwrap_or("").to_owned()),
                need["label"].as_str().unwrap_or("").to_owned(),
            ),
            _ => continue,
        };
        open.push(Open {
            key,
            line,
            url,
            count: 1,
        });
    }
    let checks = overview["to_check"].as_u64().unwrap_or(0) as usize;
    if checks > 0 {
        open.push(Open {
            key: Key::Checks,
            line: format!("{checks} to check"),
            url: format!("#/p/{project}/checks"),
            count: checks,
        });
    }
    open
}

/// Note new tasks that wait on the owner and new approval requests for "Manager needs me".
fn notice(scope: &Scope, watch: &mut Watch) {
    let land_auto = load_task_config(&scope.path).is_ok_and(|settings| settings.land_auto);
    let first = !watch.seen.contains_key(&scope.project);
    let seen = watch.seen.entry(scope.project).or_default();
    for (dir, record) in view::project_tasks(&scope.store).unwrap_or_default() {
        let Ok(record) = record else { continue };
        let now = (record.state, record.revision);
        let before = seen.insert(record.id, now);
        // ^ Only a task that waits for an answer pushes (`needs::pushes`).
        if !first
            && before != Some(now)
            && needs::pushes(needs::stage(&dir, &record, land_auto), &record)
        {
            watch.pending.add((scope.project, Key::Task(record.id)));
        }
    }
    let lines = store::read_approvals(&scope.store).unwrap_or_default();
    let known = watch.requests.entry(scope.project).or_default();
    for line in pending_approvals(&lines) {
        let ApprovalLine::Request { id, .. } = line else {
            continue;
        };
        if known.insert(id.clone()) && !first {
            watch
                .pending
                .add((scope.project, Key::Approval(id.clone())));
        }
    }
}

/// Note a reply the manager or a chat started on its own for "Manager needs me": one that is
/// a message (not quiet) to a turn of only wsx's events or a recap request, as after a
/// routine's result. A reply to the owner or another manager is no news (docs/web-client.md
/// "Notifications"). Another chat's names it and opens it. A `post` came from wsx, not
/// the agent, so the push shows its text.
fn wrote(scope: &Scope, watch: &mut Watch) {
    for dir in task::chats::open_chats(&scope.store) {
        let log = dir.read_tail(MANAGER_TAIL_BYTES).unwrap_or_default();
        let own = |id: &u64| {
            log.iter().any(|entry| match &entry.event {
                ManagerEvent::Message { id: of, from, .. } => {
                    of == id && matches!(from, Author::Events | Author::Recap)
                }
                _ => false,
            })
        };
        let event_of = |id: &u64| {
            log.iter().find_map(|entry| match &entry.event {
                ManagerEvent::Message { id: of, text, .. } if of == id => Some(text.as_str()),
                _ => None,
            })
        };
        let key = (scope.project, dir.name().to_string());
        // ^ A chat seen for the first time, as at a server start, notifies nothing.
        let last = watch.replies.get(&key).copied();
        let url = format!("#/c/{}", dir.chat);
        let name = dir
            .named()
            .map_or_else(|| manager_name(scope), str::to_string);
        let mut news = false;
        for entry in &log {
            let ManagerEvent::Reply {
                id,
                to,
                quiet,
                text,
                post,
                level,
                ..
            } = &entry.event
            else {
                continue;
            };
            if last.is_some_and(|last| *id > last) && !quiet && !to.is_empty() && to.iter().all(own)
            {
                match (post, level) {
                    (false, _) => news = true,
                    // ^ Push text is written by wsx: `<project>: <title>` over the first line.
                    (true, Some(_)) => {
                        let title = to.first().and_then(event_of).unwrap_or_default();
                        watch.alerts.add((
                            scope.project,
                            Alert {
                                reply: *id,
                                title: view::clip(&format!("{}: {title}", scope.name), 60),
                                body: view::clip(text.trim(), 160),
                                url: url.clone(),
                            },
                        ));
                    }
                    (true, None) => watch
                        .pending
                        .add((scope.project, Key::Wrote(text.clone(), url.clone()))),
                }
            }
            watch.replies.insert(key.clone(), *id);
        }
        watch.replies.entry(key).or_default();
        if news {
            watch.pending.add((
                scope.project,
                Key::Wrote(format!("{name} wrote to you"), url),
            ));
        }
    }
}

/// Once the merge window of new items passed: one push of its own for each post with a level
/// (`wsx task post --level warn|bad`), and one "Manager needs me" push that names every
/// open item, replacing the one before (its tag). A device whose page shows the project of
/// every new item is skipped. `badge` is what needs the owner now, for the app badge.
pub fn flush(app: &Arc<App>, watch: &mut Watch, badge: usize) {
    let merge = Duration::from_secs(app.site.notify.manager.merge_seconds);
    if let Some(alerts) = watch.alerts.due(merge) {
        flush_alerts(app, alerts, badge);
    }
    if let Some(new) = watch.pending.due(merge) {
        flush_needs(app, watch, new, badge);
    }
}

/// One push for each post with a level, to the devices whose page does not show its project.
fn flush_alerts(app: &Arc<App>, alerts: Vec<(u64, Alert)>, badge: usize) {
    for (project, alert) in alerts {
        let showing = app.showing(project);
        let payload = json!({
            "kind": "manager",
            "title": alert.title,
            "body": alert.body,
            "url": alert.url,
            "tag": format!("wsx-{}-alert-{project}-{}", app.site.id, alert.reply),
            "badge": badge,
        });
        crate::events::deliver(app, crate::push::Switch::Manager, |device| {
            (!showing.contains(&device.device_id)).then(|| payload.clone())
        });
    }
}

/// The "Manager needs me" push for `new`, once per merge window.
fn flush_needs(app: &Arc<App>, watch: &Watch, new: Vec<(u64, Key)>, badge: usize) {
    // ^ An item answered within the window is no news.
    let new: Vec<(u64, Key)> = new
        .into_iter()
        .filter(|(project, key)| {
            matches!(key, Key::Wrote(..))
                || watch
                    .open
                    .get(project)
                    .is_some_and(|(_, items)| items.iter().any(|item| item.key == *key))
        })
        .collect();
    let Some((first, _)) = new.first() else {
        return;
    };
    let mut skip = app.showing(*first);
    for (project, _) in &new {
        let showing = app.showing(*project);
        skip.retain(|device| showing.contains(device));
    }
    // ^ A reply the manager wrote is an item of this push only: it waits on nothing.
    let wrote: Vec<(u64, Open)> = new
        .iter()
        .filter_map(|(project, key)| match key {
            Key::Wrote(line, url) => Some((
                *project,
                Open {
                    key: key.clone(),
                    line: line.clone(),
                    url: url.clone(),
                    count: 1,
                },
            )),
            _ => None,
        })
        .collect();
    let mut projects: Vec<(u64, &str, Vec<&Open>)> = watch
        .open
        .iter()
        .map(|(project, (name, items))| {
            let wrote = wrote.iter().filter(|(of, _)| of == project);
            let items: Vec<&Open> = items.iter().chain(wrote.map(|(_, item)| item)).collect();
            (*project, name.as_str(), items)
        })
        .filter(|(_, _, items)| !items.is_empty())
        .collect();
    projects.sort_by_key(|(project, _, _)| *project);
    let count: usize = projects
        .iter()
        .flat_map(|(_, _, items)| items)
        .map(|item| item.count)
        .sum();
    let names: Vec<&str> = projects.iter().map(|(_, name, _)| *name).collect();
    let several = names.len() > 1;
    let lines: Vec<String> = projects
        .iter()
        .flat_map(|(_, name, items)| {
            items
                .iter()
                .flat_map(|item| item.line.lines())
                .map(move |line| match several {
                    false => line.to_string(),
                    true => format!("{name}: {line}"),
                })
        })
        .collect();
    let items: Vec<&Open> = projects
        .iter()
        .flat_map(|(_, _, items)| items.iter().copied())
        .collect();
    let url = match (items.as_slice(), projects.as_slice()) {
        ([item], _) => item.url.clone(),
        (_, [(project, _, _)]) => format!("#/p/{project}"),
        _ => "#/".into(),
    };
    let payload = json!({
        "kind": "manager",
        "title": format!(
            "{count} {}: {}",
            if count == 1 { "item needs you" } else { "items need you" },
            names.join(", ")
        ),
        "body": crate::events::notice_body(&lines),
        "url": url,
        "tag": format!("wsx-{}-manager", app.site.id),
        "badge": badge,
    });
    crate::events::deliver(app, crate::push::Switch::Manager, |device| {
        (!skip.contains(&device.device_id)).then(|| payload.clone())
    });
}

/// Build the project's overview: note what needs the owner, and publish `tasks` when it
/// differs from the last one published.
fn watch_overview(app: &Arc<App>, scope: &Scope, watch: &mut Watch) {
    let overview = overview(app, scope);
    let mut text = overview.to_string();
    let mut needs = overview["needs"].as_array().map_or(0, Vec::len);
    let mut open = attention(scope.project, &overview);
    // ^ What the open chats own needs the owner too: the push and the badge count it (a chat's
    // To check items fold into the project's one line).
    // ^ The main chat is the project's own overview above: its row is not counted again.
    for chat in overview["chats"].as_array().into_iter().flatten() {
        let Some(name) = chat["name"].as_str().filter(|_| chat["main"] != true) else {
            continue;
        };
        let own = overview_of(app, scope, Some(name));
        // ^ A change of what a chat owns is published as a `tasks` event too: its page reads its own overview again.
        text.push_str(&own.to_string());
        needs += own["needs"].as_array().map_or(0, Vec::len);
        for item in attention(scope.project, &own) {
            match open
                .iter_mut()
                .find(|kept| kept.key == item.key && item.key == Key::Checks)
            {
                Some(kept) => {
                    kept.count += item.count;
                    kept.line = format!("{} to check", kept.count);
                }
                None => open.push(item),
            }
        }
    }
    watch.needs.insert(scope.project, needs);
    let checks = |items: &[Open]| {
        items
            .iter()
            .find(|item| item.key == Key::Checks)
            .map_or(0, |item| item.count)
    };
    // ^ New To check items are news; a project first seen is not.
    if let Some((_, before)) = watch.open.get(&scope.project) {
        if checks(&open) > checks(before) {
            watch.pending.add((scope.project, Key::Checks));
        }
    }
    watch.open.insert(scope.project, (scope.name.clone(), open));
    if watch.overviews.get(&scope.project) != Some(&text) {
        watch.overviews.insert(scope.project, text);
        crate::events::publish_json(app, "tasks", overview);
    }
}

/// Publish `page`, `manager` or `task` for each file of the project's store that changed.
fn publish_changes(app: &Arc<App>, scope: &Scope, changed: Vec<&PathBuf>) {
    let mut tasks: HashSet<u32> = HashSet::new();
    for path in changed {
        let Some(parent) = path.parent() else {
            continue;
        };
        if path
            .file_name()
            .is_some_and(|name| name == store::OVERVIEW_FILE)
        {
            crate::events::publish_json(app, "page", json!({ "project": scope.project }));
        } else if let Some(chat) = store::chat_of(path) {
            // ^ Another chat's page reads its folder, the manager's its contacts.
            let event = match store::main_chat(&scope.store).as_deref() == Some(chat) {
                false => json!({ "project": scope.project, "chat": chat }),
                true => json!({ "project": scope.project }),
            };
            crate::events::publish_json(app, "manager", event);
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

/// One observer step: publish `tasks` when a project's overview changed, `task` for each
/// task whose record or log changed, and `manager` when the conversation changed.
pub fn watch(app: &Arc<App>, snapshot: &Snapshot, watch: &mut Watch) {
    if !app.site.allows(crate::site::Capability::TaskView) {
        return;
    }
    let scopes = scopes(snapshot, &app.site.access);
    for scope in &scopes {
        let now = store::stamp(&scope.store);
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
        if before.is_none() || changed.iter().any(|path| store::chat_of(path).is_some()) {
            wrote(scope, watch);
        }
        // ^ A working agent appends a progress line about every second. Only a ready task's
        // log can change the overview (a landing that stopped), and the overview reads those
        // logs whole. The manager's log is `log.jsonl` too, outside a task's folder: its turn
        // changes `busy` and `last_reply`, so it is never progress only.
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
                        .is_some_and(|id| !ready(id))
            });
        if !progress_only {
            watch_overview(app, scope, watch);
        }
        if before.is_none() {
            continue;
        }
        publish_changes(app, scope, changed);
    }
    let alive: HashSet<u64> = scopes.iter().map(|scope| scope.project).collect();
    watch.overviews.retain(|project, _| alive.contains(project));
    locked(&app.overviews).retain(|(project, _), _| alive.contains(project));
    watch.seen.retain(|project, _| alive.contains(project));
    watch.needs.retain(|project, _| alive.contains(project));
    watch.open.retain(|project, _| alive.contains(project));
    watch.requests.retain(|project, _| alive.contains(project));
    let stores: HashSet<&String> = scopes.iter().map(|scope| &scope.store).collect();
    watch.stamps.retain(|store, _| stores.contains(store));
}

/// The granted projects' spend of `filter` summed (docs/web-client-tasks.md "All projects").
pub fn usage_all(app: &App, filter: &task::usage::Filter) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    let projects: Vec<(String, String)> = scopes(&snapshot, &app.site.access)
        .into_iter()
        .map(|scope| (scope.name, scope.store))
        .collect();
    Ok(task::usage::all(&projects, task::now_unix_ms(), filter))
}

/// One page of the project's media archive, newest first (docs/tasks.md "Media"). A capture
/// is served by its item's route; a file a message links goes through the chat's grant (it needs
/// `media_download`, else the item has no `url`); an upload is in the media store or `gone`
/// (expired).
pub fn media(app: &App, scope: &Scope, filter: &task::media::Filter) -> Outcome<Value> {
    let page = task::media::list(&scope.store, filter, &scope.path).map_err(internal)?;
    let root = app
        .site
        .allows(crate::site::Capability::MediaDownload)
        .then(|| app.root(&scope.path).ok())
        .flatten();
    let rows: Vec<Value> = page
        .items
        .iter()
        .map(|item| {
            let mut row = json!(item);
            let shown = match item.source {
                task::media::Source::Capture => {
                    item.check.as_deref().filter(|_| !item.gone).map(|id| {
                        let base = format!("api/tasks/{}/checks/{id}", scope.project);
                        let poster = item.poster.then(|| format!("{base}/poster"));
                        (format!("{base}/capture/{}", item.slot), poster, None)
                    })
                }
                task::media::Source::File => root
                    .as_ref()
                    .filter(|_| !item.gone)
                    .and_then(|root| {
                        app.chat
                            .granted_file(item.path.as_deref()?, root, &app.media)
                    })
                    .map(|file| (file.url, file.poster, Some((file.mime, file.size)))),
                task::media::Source::Upload => item
                    .path
                    .as_deref()
                    .and_then(|file| app.media.get(file))
                    .map(|(entry, _)| {
                        (
                            format!("media/{}", entry.file_name()),
                            None,
                            Some((entry.mime, entry.size)),
                        )
                    }),
            };
            if let Some((url, poster, file)) = shown {
                row["url"] = json!(url);
                row["poster_url"] = json!(poster);
                if let Some((mime, size)) = file {
                    row["mime"] = json!(mime);
                    row["size"] = json!(size);
                }
            } else if item.source == task::media::Source::Upload {
                row["gone"] = json!(true);
            }
            row
        })
        .collect();
    Ok(json!({ "items": rows, "next": page.next }))
}

/// One project's spend of `filter` (docs/web-client-tasks.md "Usage").
pub fn usage(scope: &Scope, filter: &task::usage::Filter) -> Value {
    let project = [(scope.name.clone(), scope.store.clone())];
    task::usage::all(&project, task::now_unix_ms(), filter)
}

/// Every granted project's overview, for a page that just opened.
pub fn overviews(app: &App) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    Ok(json!(scopes(&snapshot, &app.site.access)
        .iter()
        .map(|scope| overview(app, scope))
        .collect::<Vec<_>>()))
}

/// What waits for the owner in every open chat of every granted project, one entry per chat that
/// has anything: the chat's own `needs`, `asks` and `check_items` as its overview has them, so the
/// For you list of Home holds the rows of each chat's For you pill and its count is the sum of
/// the chats' `counts` (docs/web-client-tasks.md "Home").
pub fn for_you(app: &App) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    let mut chats = Vec::new();
    for scope in scopes(&snapshot, &app.site.access) {
        let project = overview(app, &scope);
        for row in project["chats"].as_array().into_iter().flatten() {
            let waits = ["needs", "check"]
                .iter()
                .any(|kind| row["counts"][kind].as_u64().unwrap_or_default() > 0);
            // ^ A chat that runs in several projects is one entry, under its first.
            let own =
                row["main"] == true || row["projects"][0].as_str() == Some(scope.name.as_str());
            let Some(name) = row["name"].as_str().filter(|_| waits && own) else {
                continue;
            };
            let body = if row["main"] == true {
                project.clone()
            } else {
                overview_of(app, &scope, Some(name))
            };
            chats.push(json!({
                "project": scope.project, "chat": name, "shown": row["shown"], "main": row["main"],
                "needs": body["needs"], "asks": body["asks"], "check_items": body["check_items"],
            }));
        }
    }
    Ok(json!(chats))
}
