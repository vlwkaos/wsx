//! What a project needs from the owner: the one rule behind the web's Home row and the TUI's
//! Workspace tree marks. ref: docs/tasks.md "Landing", "To check", "TUI"
use super::{
    chats, checks,
    manager::{pending_approvals, waiting_approvals},
    status,
    store::{self, TaskDir},
    view, TaskRecord, TaskState, WaitOn,
};
use anyhow::Result;
use std::collections::HashMap;

/// The three states the owner sees (docs/web-client-tasks.md); the store keeps the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Working,
    NeedsYou,
    Done,
    Closed,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsYou => "needs_you",
            Self::Done => "done",
            Self::Closed => "closed",
        }
    }
}

/// A ready task waits for a land by hand: auto land is off, there are no checks, or it stopped.
/// One the manager created waits for the manager's review, not the owner. An idle worker whose
/// task waits for another to land asks nobody either.
pub fn stage(dir: &TaskDir, record: &TaskRecord, land_auto: bool) -> Stage {
    if record.manager_reviews(land_auto) || status::owner_waits_after(dir, record) {
        return Stage::Working;
    }
    match record.state {
        // ^ The owner works on it: not a need of the owner, and not done.
        TaskState::Waiting if record.waits_on(WaitOn::Hand) => Stage::Working,
        TaskState::Working | TaskState::Checking => Stage::Working,
        TaskState::Waiting | TaskState::Failed | TaskState::Ready => Stage::NeedsYou,
        TaskState::Landed => Stage::Done,
        TaskState::Closed => Stage::Closed,
    }
}

/// The kinds of need of an owner task, most urgent first (`rank`); an approval is the fourth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedKind {
    Waiting,
    Failed,
    Land,
}

impl NeedKind {
    /// The kind of a task in a state that needs the owner (`Stage::NeedsYou`).
    pub fn of(state: TaskState) -> Option<Self> {
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
                Stage::NeedsYou => needs.owner += 1,
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
/// by the one ownership rule (`chats::owner`): the tasks the chat made, its own approvals and the
/// To check items of its tasks. ref: docs/web-client-tasks.md "Home"
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatCounts {
    pub running: usize,
    pub needs: usize,
    pub check: usize,
}

/// The counts of every open chat of the project, by name.
pub fn chat_counts(project_id: &str, land_auto: bool) -> HashMap<String, ChatCounts> {
    let open = chats::open_names(project_id);
    let mut counts: HashMap<String, ChatCounts> = open
        .iter()
        .map(|name| (name.clone(), ChatCounts::default()))
        .collect();
    if open.is_empty() {
        return counts;
    }
    for (dir, record) in view::project_tasks(project_id).unwrap_or_default() {
        let Ok(record) = record else { continue };
        let Some(own) =
            chats::owner(record.created_by.chat(), &open).and_then(|n| counts.get_mut(n))
        else {
            continue;
        };
        match stage(&dir, &record, land_auto) {
            Stage::Working => own.running += 1,
            Stage::NeedsYou => own.needs += 1,
            Stage::Done | Stage::Closed => {}
        }
    }
    let approvals = store::read_approvals(project_id).unwrap_or_default();
    let items = checks::open_items(project_id).unwrap_or_default();
    for name in &open {
        let own = chats::approvals_of(approvals.clone(), Some(name), &open);
        let Some(entry) = counts.get_mut(name) else {
            continue;
        };
        entry.needs += pending_approvals(&own).len();
        entry.running += waiting_approvals(&own).len();
        entry.check = chats::checks_of(project_id, items.clone(), Some(name), &open).len();
    }
    counts
}
