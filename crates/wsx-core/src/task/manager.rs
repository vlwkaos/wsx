//! The project manager's conversation and state: `tasks/<project-id>/manager/`.
//! ref: docs/tasks.md "Manager"
//!
//! The manager host is the only writer of these files; `wsx task chat` reads the log.

use super::{TaskRecord, TaskState, TurnUsage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Who wrote a message the manager receives: `owner`, `events`, `manager:<project>` or, for
/// an answer to a message this manager sent, `manager-reply:<project>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Author {
    Owner,
    /// Written by wsx from task state changes and approval decisions.
    Events,
    /// The manager of another registered project (docs/tasks.md "Managers talk").
    Manager {
        project: String,
        reply: bool,
    },
    /// wsx asks for a recap after a quiet period (docs/tasks.md "Recap").
    Recap,
}

impl From<Author> for String {
    fn from(author: Author) -> Self {
        match author {
            Author::Owner => "owner".into(),
            Author::Events => "events".into(),
            Author::Recap => "recap".into(),
            Author::Manager {
                project,
                reply: false,
            } => format!("manager:{project}"),
            Author::Manager {
                project,
                reply: true,
            } => format!("manager-reply:{project}"),
        }
    }
}

impl TryFrom<String> for Author {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let manager = |project: &str, reply| match project.is_empty() {
            true => Err(format!("{text} names no project")),
            false => Ok(Author::Manager {
                project: project.to_string(),
                reply,
            }),
        };
        match text.as_str() {
            "owner" => Ok(Self::Owner),
            "events" => Ok(Self::Events),
            "recap" => Ok(Self::Recap),
            _ => match (
                text.strip_prefix("manager:"),
                text.strip_prefix("manager-reply:"),
            ) {
                (Some(project), _) => manager(project, false),
                (_, Some(project)) => manager(project, true),
                _ => Err(format!("unknown author {text}")),
            },
        }
    }
}

/// One line of `manager/log.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManagerEvent {
    Message {
        id: u64,
        from: Author,
        text: String,
    },
    /// These messages went to the agent in one turn.
    Delivered {
        ids: Vec<u64>,
    },
    Reply {
        id: u64,
        to: Vec<u64>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        /// A short reply to a turn of only events: the manager's own work, which the chat
        /// shows as a muted line, not a message (`quiet_reply`, docs/tasks.md "Manager").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        quiet: bool,
    },
    /// This manager's message to another project's manager (`say --from-manager`).
    Sent {
        to: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply: bool,
    },
    /// A command or other step of the agent, in short form.
    Agent {
        text: String,
    },
    /// The context passed `rotateTokens`, or the launch changed (`reason`): the next turn
    /// starts a fresh conversation.
    Rotated {
        tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The agent process ended outside a reply.
    Stopped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerEntry {
    pub unix_ms: u64,
    #[serde(flatten)]
    pub event: ManagerEvent,
}

/// The longest quiet reply, as long as the web's first sentence (`firstSentence` in lib.js).
const QUIET_REPLY_CHARS: usize = 200;

/// Whether a reply to a turn of only events is quiet: one line of at most 200 characters. A
/// longer reply (a table, a list, paragraphs) is a message even after events.
pub fn quiet_reply(text: &str) -> bool {
    let text = text.trim();
    !text.contains('\n') && text.chars().count() <= QUIET_REPLY_CHARS
}

impl ManagerEntry {
    /// An entry as read from the log: a reply stored as quiet before the length rule keeps its
    /// flag only when it is short.
    pub(super) fn read(line: &[u8]) -> Option<Self> {
        let mut entry: Self = serde_json::from_slice(line).ok()?;
        if let ManagerEvent::Reply { text, quiet, .. } = &mut entry.event {
            *quiet = *quiet && quiet_reply(text);
        }
        Some(entry)
    }

    /// The id of a message or reply, which `chat --after` counts from.
    pub fn id(&self) -> Option<u64> {
        match &self.event {
            ManagerEvent::Message { id, .. } | ManagerEvent::Reply { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// Most entries the chat shows at once.
pub const CHAT_ENTRIES: usize = 200;

/// The chat after message `after` (all of it when `after` is none or unknown): messages,
/// replies, rotations and stops, the newest `CHAT_ENTRIES` of them, and how many older ones
/// were left out. `Delivered` and `Agent` entries are not conversation.
pub fn chat_window(log: &[ManagerEntry], after: Option<u64>) -> (Vec<&ManagerEntry>, usize) {
    let start = after
        .and_then(|after| log.iter().position(|entry| entry.id() == Some(after)))
        .map_or(0, |index| index + 1);
    let mut entries: Vec<&ManagerEntry> = log[start..]
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
    (entries, skipped)
}

/// What the manager does now: its last step after its last reply. Shown while it is busy.
pub fn activity(log: &[ManagerEntry]) -> Option<&str> {
    log.iter()
        .rev()
        .take_while(|entry| !matches!(entry.event, ManagerEvent::Reply { .. }))
        .find_map(|entry| match &entry.event {
            ManagerEvent::Agent { text } => Some(text.as_str()),
            _ => None,
        })
}

/// A task as the manager last reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub state: TaskState,
    pub revision: u32,
}

impl Seen {
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            state: record.state,
            revision: record.revision,
        }
    }
}

/// `manager/state.json`: what the manager host must remember across a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagerState {
    /// The conversation the next turn resumes; none after a rotation.
    pub agent_session: Option<String>,
    pub next_id: u64,
    /// Task states already turned into events. Absent until the host's first start.
    pub seen: Option<BTreeMap<u32, Seen>>,
    /// Approval decisions already turned into events.
    pub decisions_seen: u64,
    /// The `unix_ms` of the newest deploy record already turned into events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploys_seen_ms: Option<u64>,
    /// The `finished_epoch` of the newest routine run already turned into events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routines_seen: Option<i64>,
    /// The digest of the instructions the conversation in `agent_session` began with.
    /// Claude keeps a conversation's first system prompt also on resume, so other
    /// instructions start a new conversation; settings, tools and model apply at every start.
    pub instructions: Option<String>,
    /// A rotation happened: the next fresh conversation opens with the last exchanges.
    pub handover: bool,
    /// The tasks as the last turn's ledger summary showed them; none in a fresh conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_seen: Option<BTreeMap<u32, Seen>>,
    /// The conversation's last `session_cost_micro_usd`, which a resumed process counts on from.
    pub session_cost_micro_usd: u64,
    /// The last task the host started with `tasks.workflow.idle`, and when it started it:
    /// the next one waits for session notes newer than that. ref: docs/tasks.md "Workflow"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_task: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// When the last recap was asked for: the next one needs a change after it, and names
    /// the token cost since. ref: docs/tasks.md "Recap"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recap_ms: Option<u64>,
    /// The agent's usage limit: the messages of the turn it ended, answered once it resumes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitHold>,
}

/// A manager turn the usage limit ended. ref: docs/tasks.md "Manager"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitHold {
    pub until_ms: u64,
    /// Limits in a row, for the backoff when no reset time is known.
    pub attempt: u32,
    pub ids: Vec<u64>,
}

/// A stable digest of an agent's instructions, to tell whether a conversation began with them.
pub fn instructions_digest(instructions: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(instructions.as_bytes())[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the manager asked the owner to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    Land,
    Deploy,
    Release,
    /// `close --discard`
    Discard,
}

/// One line of `approvals.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalLine {
    Request {
        id: String,
        action: ApprovalAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        unix_ms: u64,
    },
    Decision {
        id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// What running the approved action said, or why it was not run.
        outcome: String,
        /// The owner message of the manager's turn that approved it: the owner asked for
        /// the action in the chat, so no button was needed. ref: docs/tasks.md "Approvals"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_said: Option<u64>,
        unix_ms: u64,
    },
}

impl ApprovalLine {
    pub fn id(&self) -> &str {
        match self {
            Self::Request { id, .. } | Self::Decision { id, .. } => id,
        }
    }
}

/// Requests with no decision yet, oldest first. A decision settles the requests of its id
/// before it: an id reused by an older wsx (which miscounted lines it could not read) is a
/// new request.
pub fn pending_approvals(lines: &[ApprovalLine]) -> Vec<&ApprovalLine> {
    lines
        .iter()
        .enumerate()
        .filter(|(at, line)| {
            matches!(line, ApprovalLine::Request { .. })
                && !lines[at + 1..].iter().any(|later| {
                    matches!(later, ApprovalLine::Decision { .. }) && later.id() == line.id()
                })
        })
        .map(|(_, line)| line)
        .collect()
}

/// The request a decision at `at` settles: the last one of its id before it.
pub fn decided_request(lines: &[ApprovalLine], at: usize) -> Option<&ApprovalLine> {
    let id = lines.get(at)?.id();
    lines[..at]
        .iter()
        .rev()
        .find(|line| matches!(line, ApprovalLine::Request { .. }) && line.id() == id)
}

/// Whether the owner's message `id` is in the manager's running turn: the last delivered
/// turn has no reply yet and holds that message from the owner. ref: docs/tasks.md "Approvals"
pub fn owner_said_in_turn(log: &[ManagerEntry], id: u64) -> Result<(), String> {
    let turn = log.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Reply { .. } => Some(None),
        ManagerEvent::Delivered { ids } => Some(Some(ids)),
        _ => None,
    });
    let Some(Some(ids)) = turn else {
        return Err("no manager turn runs".into());
    };
    if !ids.contains(&id) {
        return Err(format!("message {id} is not in the running turn"));
    }
    let from = log.iter().find_map(|entry| match &entry.event {
        ManagerEvent::Message { id: said, from, .. } if *said == id => Some(from),
        _ => None,
    });
    match from {
        Some(Author::Owner) => Ok(()),
        Some(_) => Err(format!("message {id} is not from the owner")),
        None => Err(format!("there is no message {id}")),
    }
}

/// One request on the manager host's control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ManagerRequest {
    Status,
    Say {
        text: String,
        /// Another project's manager; none is the owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Author>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub busy: bool,
    pub message: String,
}
