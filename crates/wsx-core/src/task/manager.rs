//! The project manager's conversation and state: `tasks/<project-id>/manager/`.
//! ref: docs/tasks.md "Manager"
//!
//! The manager host is the only writer of these files; `wsx task chat` reads the log.

use super::{Access, TaskState, TurnUsage};
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
}

impl From<Author> for String {
    fn from(author: Author) -> Self {
        match author {
            Author::Owner => "owner".into(),
            Author::Events => "events".into(),
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

impl ManagerEntry {
    /// The id of a message or reply, which `chat --after` counts from.
    pub fn id(&self) -> Option<u64> {
        match &self.event {
            ManagerEvent::Message { id, .. } | ManagerEvent::Reply { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// A task as the manager last reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub state: TaskState,
    pub revision: u32,
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
    /// The digest of the instructions the conversation in `agent_session` began with.
    /// Claude keeps a conversation's first system prompt also on resume, so other
    /// instructions start a new conversation; settings, tools and model apply at every start.
    pub instructions: Option<String>,
    /// A rotation happened: the next fresh conversation opens with the last exchanges.
    pub handover: bool,
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
    /// `start`: build a briefed task. Only requests from before `start` stopped asking
    /// (docs/tasks.md "Approvals"); they still decode and run.
    Start,
    /// `allow`: a project-wide grant or a tool rule the manager asked for.
    Grant,
}

/// What a `Grant` request would give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantAsk {
    pub access: Access,
    /// For every task of the project, not only the named one.
    #[serde(default)]
    pub project_wide: bool,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grant: Option<GrantAsk>,
        unix_ms: u64,
    },
    Decision {
        id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// What running the approved action said, or why it was not run.
        outcome: String,
        unix_ms: u64,
    },
}

/// Requests with no decision yet, oldest first.
pub fn pending_approvals(lines: &[ApprovalLine]) -> Vec<&ApprovalLine> {
    lines
        .iter()
        .filter(|line| {
            let ApprovalLine::Request { id, .. } = line else {
                return false;
            };
            !lines.iter().any(|other| {
                matches!(other, ApprovalLine::Decision { id: decided, .. } if decided == id)
            })
        })
        .collect()
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
