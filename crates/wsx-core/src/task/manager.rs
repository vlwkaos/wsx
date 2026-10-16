//! The project manager's conversation and state: `tasks/<project-id>/manager/`.
//! ref: docs/tasks.md "Manager"
//!
//! The manager host is the only writer of these files; `wsx task chat` reads the log.

use super::{TaskState, TurnUsage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Who wrote a message the manager receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    Owner,
    /// Written by wsx from task state changes and approval decisions.
    Events,
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
    /// A command or other step of the agent, in short form.
    Agent {
        text: String,
    },
    /// The context passed `rotateTokens`: the next turn starts a fresh conversation.
    Rotated {
        tokens: u64,
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
}

/// One request on `manager/manager.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ManagerRequest {
    Status,
    Say { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub busy: bool,
    pub message: String,
}
