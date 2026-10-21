//! Messages a person sent while the agent was working (docs/web-client.md "Queue").
//! ^ wsxd allows one active exchange per pane. The web server keeps what cannot be
//! delivered yet, per session and in order, and the event observer delivers it.
//! The file is owner-only site state and survives a restart of the server.
use crate::devices::{now_unix_ms, write_private};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const MAX_QUEUED: usize = 5;
/// Delivered messages kept per session until the conversation shows them; older ones go.
const MAX_DELIVERED: usize = 3;
/// How much earlier than the delivery an agent may stamp the saved message.
const SAVE_SKEW_MS: u64 = 2_000;
/// Notices kept per session; older ones go.
const MAX_NOTICES: usize = 20;
/// Conversations remembered per session; older ones go.
const MAX_CONVERSATIONS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    pub id: u64,
    pub text: String,
    /// Media hashes, as the page sent them.
    pub attachments: Vec<String>,
    pub at_unix_ms: u64,
    /// Why the delivery failed, in the daemon's words. A failed message stays in its place
    /// until Retry or Remove; later messages are delivered past it.
    #[serde(default)]
    pub failed: Option<String>,
    /// When wsxd typed it into the agent. It stays until the agent's saved conversation
    /// shows it: an agent can drop a paste (seen 2026-10-21 at the end of a Claude turn),
    /// and the page must still offer the text to copy or send again.
    #[serde(default)]
    pub delivered_unix_ms: Option<u64>,
    /// The exchange round that carried it, until the agent took it: a round the agent did
    /// not take sends it again once (docs/agent-orchestration.md "Taken, not typed").
    #[serde(default)]
    pub round: Option<SentRound>,
    /// It was sent a second time because the agent did not take it the first time.
    #[serde(default)]
    pub resent: bool,
    /// Since when wsxd says "not yet" (the agent is not at a wait boundary). Not saved: a
    /// restarted server starts the wait again.
    #[serde(skip)]
    pub waiting_since_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRound {
    pub exchange: u64,
    pub round: u32,
}

/// How the round that carried a message ended.
pub enum Round {
    /// The agent took it; `remove`: its words may never show, so it leaves the queue now.
    Taken {
        remove: bool,
    },
    NotTaken {
        reason: String,
    },
}

pub enum Settled {
    Removed,
    Watched,
    SentAgain,
    Failed,
}

/// What the server did on its own for a session's chat, shown in the conversation where it
/// happened (docs/web-client.md "Queue and Stop"), such as a message sent again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub at_unix_ms: u64,
    pub text: String,
}

/// One agent conversation a session's pane held, so the chat outlives the agent and keeps the
/// earlier conversation readable (docs/web-client.md "Conversations").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub provider: String,
    pub session_ref: wsx_core::runtime::AgentSessionRef,
    pub since_unix_ms: u64,
}

/// How a conversation the agent reports relates to the remembered ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    First,
    Same,
    New,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    next_id: u64,
    sessions: BTreeMap<u64, Vec<Queued>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    notices: BTreeMap<u64, Vec<Notice>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    conversations: BTreeMap<u64, Vec<Conversation>>,
}

pub struct Queue {
    path: PathBuf,
    stored: Mutex<Stored>,
}

impl Queue {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("queue.json");
        let stored = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid queue file", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(error) => return Err(error).context("cannot read the message queue"),
        };
        Ok(Self {
            path,
            stored: Mutex::new(stored),
        })
    }

    /// Change the queue and save it. A failed save is reported: the change stays in
    /// memory, so the running server still delivers it.
    fn change<T>(&self, change: impl FnOnce(&mut Stored) -> T) -> T {
        let mut stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
        let result = change(&mut stored);
        stored.sessions.retain(|_, queue| !queue.is_empty());
        stored.notices.retain(|_, notices| !notices.is_empty());
        stored
            .conversations
            .retain(|_, conversations| !conversations.is_empty());
        let saved = serde_json::to_vec(&*stored)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| write_private(&self.path, &bytes));
        if let Err(error) = saved {
            eprintln!("wsx web: cannot save the message queue: {error:#}");
        }
        result
    }

    pub fn list(&self, session: u64) -> Vec<Queued> {
        let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
        stored.sessions.get(&session).cloned().unwrap_or_default()
    }

    /// Add a message at the end. `None` when the session's queue is full.
    pub fn push(&self, session: u64, text: &str, attachments: &[String]) -> Option<Queued> {
        self.change(|stored| {
            let queue = stored.sessions.entry(session).or_default();
            if queue
                .iter()
                .filter(|queued| queued.delivered_unix_ms.is_none())
                .count()
                >= MAX_QUEUED
            {
                return None;
            }
            stored.next_id += 1;
            let queued = Queued {
                id: stored.next_id,
                text: text.to_owned(),
                attachments: attachments.to_vec(),
                at_unix_ms: now_unix_ms(),
                failed: None,
                delivered_unix_ms: None,
                round: None,
                resent: false,
                waiting_since_unix_ms: None,
            };
            stored
                .sessions
                .entry(session)
                .or_default()
                .push(queued.clone());
            Some(queued)
        })
    }

    pub fn remove(&self, session: u64, id: u64) -> bool {
        self.edit(session, id, |queue, index| {
            queue.remove(index);
        })
    }

    /// Let a failed or delivered message be delivered again, in its old place.
    pub fn retry(&self, session: u64, id: u64) -> bool {
        self.edit(session, id, |queue, index| {
            queue[index].failed = None;
            queue[index].delivered_unix_ms = None;
            queue[index].round = None;
            queue[index].waiting_since_unix_ms = None;
        })
    }

    /// Settle the round that carried a message, once: `None` when another caller already
    /// did. Under one lock, so the page's send and the observer never both act on it.
    pub fn settle_round(
        &self,
        session: u64,
        id: u64,
        round: SentRound,
        outcome: Round,
    ) -> Option<Settled> {
        self.change(|stored| {
            let queue = stored.sessions.get_mut(&session)?;
            let index = queue
                .iter()
                .position(|queued| queued.id == id && queued.round == Some(round))?;
            let queued = &mut queue[index];
            Some(match outcome {
                Round::Taken { remove: true } => {
                    queue.remove(index);
                    Settled::Removed
                }
                Round::Taken { remove: false } => {
                    queued.round = None;
                    Settled::Watched
                }
                // ^ Sent again once, by itself, in its old place; then it stays failed.
                Round::NotTaken { reason } if queued.resent => {
                    queued.failed = Some(reason);
                    queued.delivered_unix_ms = None;
                    queued.round = None;
                    Settled::Failed
                }
                Round::NotTaken { .. } => {
                    queued.delivered_unix_ms = None;
                    queued.round = None;
                    queued.waiting_since_unix_ms = None;
                    queued.resent = true;
                    Settled::SentAgain
                }
            })
        })
    }

    /// wsxd typed the message into the agent in `round`: keep it until the conversation
    /// shows it, and watch the round until the agent takes it.
    pub fn delivered(&self, session: u64, id: u64, round: Option<SentRound>) -> bool {
        self.edit(session, id, |queue, index| {
            queue[index].delivered_unix_ms = Some(now_unix_ms());
            queue[index].round = round;
            queue[index].waiting_since_unix_ms = None;
            let delivered = queue
                .iter()
                .filter(|queued| queued.delivered_unix_ms.is_some())
                .count();
            let mut extra = delivered.saturating_sub(MAX_DELIVERED);
            queue.retain(|queued| {
                let drop = extra > 0 && queued.delivered_unix_ms.is_some();
                extra -= usize::from(drop);
                !drop
            });
        })
    }

    /// Forget the delivered messages that the conversation now shows (`shown`: the
    /// person's messages there as the agent got them, and when they were saved). Only a
    /// message saved after the delivery counts, so an older one with the same words does not.
    /// Returns whether the queue changed.
    pub fn settle(&self, session: u64, shown: &[(&str, Option<u64>)]) -> bool {
        let seen = |queued: &Queued| {
            queued.delivered_unix_ms.is_some_and(|delivered| {
                let text = crate::chat::agent_text(&queued.text);
                shown.iter().any(|(words, at)| {
                    *words == text && at.is_none_or(|at| at + SAVE_SKEW_MS >= delivered)
                })
            })
        };
        if !self.list(session).iter().any(seen) {
            return false;
        }
        self.change(|stored| {
            if let Some(queue) = stored.sessions.get_mut(&session) {
                queue.retain(|queued| !seen(queued));
            }
        });
        true
    }

    /// wsxd said "not yet" for this message. Returns since when it says so.
    pub fn not_yet(&self, session: u64, id: u64) -> u64 {
        let mut since = now_unix_ms();
        self.edit(session, id, |queue, index| {
            since = *queue[index].waiting_since_unix_ms.get_or_insert(since);
        });
        since
    }

    /// Whether a message waits for an agent that is not at a wait boundary yet: the
    /// observer then looks again soon, because the editor may turn idle with no change.
    pub fn waiting(&self) -> bool {
        let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
        stored
            .sessions
            .values()
            .flatten()
            .any(|queued| queued.waiting_since_unix_ms.is_some() && queued.failed.is_none())
    }

    pub fn fail(&self, session: u64, id: u64, reason: &str) {
        self.edit(session, id, |queue, index| {
            queue[index].failed = Some(reason.to_owned());
            queue[index].delivered_unix_ms = None;
            queue[index].round = None;
        });
    }

    fn edit(&self, session: u64, id: u64, edit: impl FnOnce(&mut Vec<Queued>, usize)) -> bool {
        self.change(|stored| {
            let Some(queue) = stored.sessions.get_mut(&session) else {
                return false;
            };
            let Some(index) = queue.iter().position(|queued| queued.id == id) else {
                return false;
            };
            edit(queue, index);
            true
        })
    }

    pub fn notice(&self, session: u64, text: String) {
        self.change(|stored| {
            let notices = stored.notices.entry(session).or_default();
            notices.push(Notice {
                at_unix_ms: now_unix_ms(),
                text,
            });
            let extra = notices.len().saturating_sub(MAX_NOTICES);
            notices.drain(..extra);
        });
    }

    /// Remember the conversation an attached agent reports. Saves only on a change.
    pub fn see_conversation(
        &self,
        session: u64,
        provider: &str,
        session_ref: &wsx_core::runtime::AgentSessionRef,
    ) -> Seen {
        let same = |known: &Conversation| {
            known.provider == provider && known.session_ref.value == session_ref.value
        };
        let seen = match self.conversations(session).last() {
            Some(last) if same(last) => return Seen::Same,
            Some(_) => Seen::New,
            None => Seen::First,
        };
        self.change(|stored| {
            let conversations = stored.conversations.entry(session).or_default();
            conversations.push(Conversation {
                provider: provider.to_owned(),
                session_ref: session_ref.clone(),
                since_unix_ms: now_unix_ms(),
            });
            let extra = conversations.len().saturating_sub(MAX_CONVERSATIONS);
            conversations.drain(..extra);
        });
        seen
    }

    pub fn conversations(&self, session: u64) -> Vec<Conversation> {
        let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
        stored
            .conversations
            .get(&session)
            .cloned()
            .unwrap_or_default()
    }

    pub fn notices(&self, session: u64) -> Vec<Notice> {
        let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
        stored.notices.get(&session).cloned().unwrap_or_default()
    }

    /// The next message to deliver: the oldest one that did not fail and was not
    /// delivered. A failed message does not hold back the ones after it
    /// (docs/web-client.md "Queue and Stop").
    pub fn next(&self, session: u64) -> Option<Queued> {
        self.list(session)
            .into_iter()
            .find(|queued| queued.failed.is_none() && queued.delivered_unix_ms.is_none())
    }

    /// Forget the queues of sessions that no longer exist in the grant.
    pub fn retain(&self, alive: &[u64]) {
        let stale = {
            let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
            stored
                .sessions
                .keys()
                .chain(stored.notices.keys())
                .chain(stored.conversations.keys())
                .any(|session| !alive.contains(session))
        };
        if stale {
            self.change(|stored| {
                stored.sessions.retain(|session, _| alive.contains(session));
                stored.notices.retain(|session, _| alive.contains(session));
                stored
                    .conversations
                    .retain(|session, _| alive.contains(session));
            });
        }
    }
}
