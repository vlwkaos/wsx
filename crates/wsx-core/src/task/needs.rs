//! What a project needs from the owner: the one rule behind the web's Home row and the TUI's
//! Workspace tree marks. ref: docs/tasks.md "Landing", "To check", "TUI"
use super::{
    chat, chats, checks,
    manager::{pending_approvals, waiting_approvals, ApprovalLine},
    status::{self, Holder},
    store::{self, TaskDir},
    view, TaskRecord, TaskState, WaitOn,
};
use anyhow::Result;
use std::collections::HashMap;

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

/// One thing that waits for the owner in a chat: the For you list's item, in the TUI, in
/// `ChatCounts` and in the web's overview, which adds its row fields to the `source` of each item
/// and builds no list of its own. `kind` is a task's `NeedKind::name`, or `approval`, `ask` or
/// `check`; `task` the task it is about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct YouItem {
    pub chat: String,
    pub kind: &'static str,
    pub rank: u8,
    pub task: Option<u32>,
    pub label: String,
    #[serde(skip)]
    pub source: YouSource,
}

/// What an item is made of: the task, the request (its id), the open question or the To check
/// item (its id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouSource {
    Task(u32),
    Approval(String),
    Ask(chat::OpenAsk),
    Check(String),
}

/// Everything that waits for the owner in the project's open chats, by the one ownership rule
/// (`chats::owner`): the tasks the chat made that need the owner, its own pending approvals, its
/// open questions and the To check items of its tasks. Tasks first (by `rank`), then the rest.
pub fn for_you(project_id: &str, land_auto: bool) -> Vec<YouItem> {
    let open = chats::open_names();
    let main = store::main_chat(project_id);
    let mut items = Vec::new();
    for (dir, record) in view::project_tasks(project_id).unwrap_or_default() {
        let Ok(record) = record else { continue };
        let Some(kind) = stage(&dir, &record, land_auto).need() else {
            continue;
        };
        let title = view::readable_title(&dir, &record).unwrap_or_default();
        // ^ A task no chat owns (no open main chat) is nobody's item; the ledger still has it.
        let Some(chat) = chats::owner(record.owner_chat(), &open, main.as_deref()) else {
            continue;
        };
        items.push(YouItem {
            chat: chat.to_string(),
            kind: kind.name(),
            rank: kind.rank(),
            task: Some(record.id),
            label: format!("#{} {title}", record.id),
            source: YouSource::Task(record.id),
        });
    }
    let approvals = store::read_approvals(project_id).unwrap_or_default();
    let checked = checks::open_items(project_id).unwrap_or_default();
    let now = super::now_unix_ms();
    for dir in chats::open_chats(project_id) {
        let viewer = chats::Viewer::named(&dir.chat, open.clone(), main.as_deref());
        let own = chats::approvals_of(approvals.clone(), &viewer);
        let mut push = |kind, rank, task, label, source| {
            items.push(YouItem {
                chat: dir.chat.clone(),
                kind,
                rank,
                task,
                label,
                source,
            });
        };
        for line in pending_approvals(&own) {
            let label = super::owner::approval_label(line);
            let source = YouSource::Approval(line.id().to_string());
            push("approval", APPROVAL_RANK, None, label, source);
        }
        for ask in chat::asking(&dir, now).unwrap_or_default() {
            push("ask", 0, None, ask.quote.clone(), YouSource::Ask(ask));
        }
        for item in chats::checks_of(project_id, checked.clone(), &viewer) {
            let source = YouSource::Check(item.id);
            push("check", 4, Some(item.task), item.check, source);
        }
    }
    items.sort_by_key(|item| (item.rank, item.kind == "check"));
    items
}

/// The counts of every open chat of the project, by name: every item that needs the owner or
/// runs, added to the chat that owns it (`chats::owner`).
pub fn chat_counts(project_id: &str, land_auto: bool) -> HashMap<String, ChatCounts> {
    let mut counts: HashMap<String, ChatCounts> = HashMap::new();
    let dirs = chats::open_chats(project_id);
    if dirs.is_empty() {
        return counts;
    }
    let open = chats::open_names();
    let main = store::main_chat(project_id);
    for (dir, record) in view::project_tasks(project_id).unwrap_or_default() {
        let Ok(record) = record else { continue };
        let Some(owner) = chats::owner(record.owner_chat(), &open, main.as_deref()) else {
            continue;
        };
        if !dirs.iter().any(|dir| dir.chat == owner) {
            continue;
        }
        let own = counts.entry(owner.to_string()).or_default();
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
    for dir in &dirs {
        let viewer = chats::Viewer::named(&dir.chat, open.clone(), main.as_deref());
        let own = chats::approvals_of(approvals.clone(), &viewer);
        let entry = counts.entry(dir.chat.clone()).or_default();
        entry.needs += pending_approvals(&own)
            .iter()
            .filter(|line| matches!(line, ApprovalLine::Request { .. }))
            .count();
        for line in waiting_approvals(&own) {
            if matches!(line, ApprovalLine::Request { .. }) {
                entry.running += 1;
                entry.open += 1;
            }
        }
        // ^ An open question of the chat is an item for the owner too, as its own pill counts it.
        for _ in chat::asking(dir, now).unwrap_or_default() {
            entry.needs += 1;
            entry.asks += 1;
        }
        entry.check += chats::checks_of(project_id, items.clone(), &viewer).len();
    }
    counts
}
