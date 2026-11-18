//! What a project needs from the owner: the one rule behind the web's Home row and the TUI's
//! Workspace tree marks. ref: docs/tasks.md "Landing", "To check", "TUI"
use super::{
    chat, chats, checks,
    manager::{pending_approvals, waiting_approvals, ApprovalLine},
    status::{self, Holder},
    store::{self, ManagerDir, TaskDir},
    thread, view, TaskRecord, TaskState, WaitOn,
};
use anyhow::Result;
use std::collections::HashMap;
use std::hash::Hash;

/// The three states the owner sees (docs/web-client-tasks.md); the store keeps the rest. A task
/// that needs the owner says what kind of need it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Working,
    NeedsYou(NeedKind),
    Done,
    Closed,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsYou(_) => "needs_you",
            Self::Done => "done",
            Self::Closed => "closed",
        }
    }

    /// The kind of need, for a task that needs the owner.
    pub fn need(self) -> Option<NeedKind> {
        match self {
            Self::NeedsYou(kind) => Some(kind),
            _ => None,
        }
    }
}

/// The one rule for "the owner is needed": an open task needs the owner when it waits on the
/// owner (`status::waits_on`), or on disk or the usage limit, which nobody else can act on. Every
/// count, list, band and push of a task reads this. ref: docs/tasks.md "Status"
pub fn stage(dir: &TaskDir, record: &TaskRecord, land_auto: bool) -> Stage {
    match record.state {
        TaskState::Landed => return Stage::Done,
        TaskState::Closed => return Stage::Closed,
        // ^ The owner works on it: not a need of the owner, and not done.
        _ if record.waits_on(WaitOn::Hand) => return Stage::Working,
        _ => {}
    }
    match status::waits_on(dir, record, land_auto) {
        Some(on) if matches!(on.kind, Holder::Owner | Holder::Disk | Holder::Limit) => {
            NeedKind::of(record.state).map_or(Stage::Working, Stage::NeedsYou)
        }
        _ => Stage::Working,
    }
}

/// Whether a task that needs the owner is news for a push: an answer is asked of the owner.
/// Not the limit (the host continues by itself), a ready task (the page shows it) or a failed one.
pub fn pushes(stage: Stage, record: &TaskRecord) -> bool {
    stage.need() == Some(NeedKind::Waiting) && !record.waits_on(WaitOn::Limit)
}

/// The kinds of need of an owner task, most urgent first (`rank`); an approval is the fourth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedKind {
    Waiting,
    Failed,
    Land,
}

impl NeedKind {
    /// The kind of a task in a state that needs the owner.
    fn of(state: TaskState) -> Option<Self> {
        match state {
            TaskState::Waiting => Some(Self::Waiting),
            TaskState::Failed => Some(Self::Failed),
            TaskState::Ready => Some(Self::Land),
            _ => None,
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Failed => 1,
            Self::Land => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Land => "land",
        }
    }
}

/// The rank of a pending approval among the kinds.
pub const APPROVAL_RANK: u8 = 2;

/// A project's need counts, as the web's overview has them (`needs`, `working`, `to_check`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// Tasks that wait on, failed for or are ready for the owner, and pending approvals.
    pub owner: usize,
    /// Tasks at work (or with the manager, or with the owner by hand).
    pub working: usize,
    /// Approved actions that run when their task lands.
    pub waiting: usize,
    /// Open To check items.
    pub to_check: usize,
}

impl Needs {
    /// The Running pill's count: tasks at work and the approved actions that wait for a landing.
    pub fn running(&self) -> usize {
        self.working + self.waiting
    }

    /// Whether anything waits for the owner.
    pub fn any(&self) -> bool {
        self.owner + self.to_check > 0
    }

    /// Read the counts from the project's task store: every record, so callers keep the
    /// answer while `store::stamp` is the same.
    pub fn read(project_id: &str, land_auto: bool) -> Result<Self> {
        let mut needs = Self::default();
        for (dir, record) in view::project_tasks(project_id)? {
            let Ok(record) = record else { continue };
            match stage(&dir, &record, land_auto) {
                Stage::Working => needs.working += 1,
                Stage::NeedsYou(_) => needs.owner += 1,
                Stage::Done | Stage::Closed => {}
            }
        }
        let approvals = store::read_approvals(project_id).unwrap_or_default();
        needs.owner += pending_approvals(&approvals).len();
        needs.waiting = waiting_approvals(&approvals).len();
        needs.to_check = checks::open_items(project_id).unwrap_or_default().len();
        Ok(needs)
    }
}

/// What an open chat has for the owner, as the Running, Needs you and To check pills count it,
/// by the one ownership rule (`chats::owner`): the tasks the chat made, its own approvals and open
/// questions, and the To check items of its tasks. ref: docs/web-client-tasks.md "Home"
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ChatCounts {
    pub running: usize,
    /// The Tasks tray: every open task, also one that waits, and the approved actions that run
    /// when a task lands.
    pub open: usize,
    pub needs: usize,
    pub check: usize,
    /// Of `needs`, the open questions.
    pub asks: usize,
}

impl ChatCounts {
    /// Whether anything waits for the owner.
    pub fn waits(&self) -> bool {
        self.needs + self.check > 0
    }

    /// What waits, in the owner's words: "1 question, 1 check", the other needs (a task, an
    /// approval) as "to decide".
    pub fn waits_words(&self) -> String {
        let plural = |n: usize, one: &str, many: &str| match n {
            1 => format!("1 {one}"),
            n => format!("{n} {many}"),
        };
        let mut words = Vec::new();
        if self.asks > 0 {
            words.push(plural(self.asks, "question", "questions"));
        }
        if self.needs > self.asks {
            words.push(format!("{} to decide", self.needs - self.asks));
        }
        if self.check > 0 {
            words.push(plural(self.check, "check", "checks"));
        }
        words.join(", ")
    }
}

/// The counts of every open chat of the project, by name.
pub fn chat_counts(project_id: &str, land_auto: bool) -> HashMap<String, ChatCounts> {
    tally(
        project_id,
        land_auto,
        &chats::open_chats(project_id),
        |chat, _| chat.to_string(),
    )
}

/// The counts of the chat `dir`, by the slug of the thread each item is in; the trunk is none.
/// A thread merged into another counts as that one (`resolve`).
pub fn thread_counts(
    dir: &ManagerDir,
    land_auto: bool,
    resolve: impl Fn(&str) -> String,
) -> HashMap<Option<String>, ChatCounts> {
    tally(
        &dir.project_id,
        land_auto,
        std::slice::from_ref(dir),
        |_, thread| thread.map(&resolve),
    )
}

/// The one count walk, for the open chats `dirs`: every item that needs the owner or runs, added
/// to the entry `key(chat, thread)` names, `thread` being the slug of its thread in that chat.
fn tally<K: Eq + Hash>(
    project_id: &str,
    land_auto: bool,
    dirs: &[ManagerDir],
    key: impl Fn(&str, Option<&str>) -> K,
) -> HashMap<K, ChatCounts> {
    let mut counts: HashMap<K, ChatCounts> = HashMap::new();
    if dirs.is_empty() {
        return counts;
    }
    let open = chats::open_names();
    let home = store::home_chat(project_id).unwrap_or_default();
    let mut thread_of: HashMap<u32, String> = HashMap::new();
    for (dir, record) in view::project_tasks(project_id).unwrap_or_default() {
        let Ok(record) = record else { continue };
        if let Some(thread) = &record.thread {
            thread_of.insert(record.id, thread.clone());
        }
        let owner = chats::owner(record.created_by.chat(), &open, &home);
        if !dirs.iter().any(|dir| dir.chat == owner) {
            continue;
        }
        let slug = record
            .thread
            .as_deref()
            .and_then(|thread| thread::slug_in(owner, thread));
        let own = counts.entry(key(owner, slug)).or_default();
        match stage(&dir, &record, land_auto) {
            Stage::Working => own.running += 1,
            Stage::NeedsYou(_) => own.needs += 1,
            Stage::Done | Stage::Closed => continue,
        }
        own.open += 1;
    }
    let approvals = store::read_approvals(project_id).unwrap_or_default();
    let items = checks::open_items(project_id).unwrap_or_default();
    let now = super::now_unix_ms();
    for dir in dirs {
        let slug_of_task = |task: Option<u32>| {
            task.and_then(|task| thread_of.get(&task))
                .and_then(|thread| thread::slug_in(&dir.chat, thread))
        };
        let viewer = chats::Viewer::named(&dir.chat, open.clone(), &home);
        let own = chats::approvals_of(approvals.clone(), &viewer);
        for line in pending_approvals(&own) {
            if let ApprovalLine::Request { task, .. } = line {
                counts
                    .entry(key(&dir.chat, slug_of_task(*task)))
                    .or_default()
                    .needs += 1;
            }
        }
        for line in waiting_approvals(&own) {
            if let ApprovalLine::Request { task, .. } = line {
                let entry = counts
                    .entry(key(&dir.chat, slug_of_task(*task)))
                    .or_default();
                entry.running += 1;
                entry.open += 1;
            }
        }
        // ^ An open question of the chat is an item for the owner too, as its own pill counts it.
        for ask in chat::asking(dir, now).unwrap_or_default() {
            let entry = counts
                .entry(key(&dir.chat, ask.threads.first().map(String::as_str)))
                .or_default();
            entry.needs += 1;
            entry.asks += 1;
        }
        for item in chats::checks_of(project_id, items.clone(), &viewer) {
            counts
                .entry(key(&dir.chat, slug_of_task(Some(item.task))))
                .or_default()
                .check += 1;
        }
    }
    counts
}
