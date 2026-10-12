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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    pub id: u64,
    pub text: String,
    /// Media hashes, as the page sent them.
    pub attachments: Vec<String>,
    pub at_unix_ms: u64,
    /// Why the delivery failed, in the daemon's words. The queue waits while this is set.
    #[serde(default)]
    pub failed: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    next_id: u64,
    sessions: BTreeMap<u64, Vec<Queued>>,
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
            if queue.len() >= MAX_QUEUED {
                return None;
            }
            stored.next_id += 1;
            let queued = Queued {
                id: stored.next_id,
                text: text.to_owned(),
                attachments: attachments.to_vec(),
                at_unix_ms: now_unix_ms(),
                failed: None,
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

    /// Let a failed message be delivered again.
    pub fn retry(&self, session: u64, id: u64) -> bool {
        self.edit(session, id, |queue, index| queue[index].failed = None)
    }

    pub fn fail(&self, session: u64, id: u64, reason: &str) {
        self.edit(session, id, |queue, index| {
            queue[index].failed = Some(reason.to_owned())
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

    /// The next message to deliver: the oldest one, unless it failed.
    pub fn next(&self, session: u64) -> Option<Queued> {
        self.list(session)
            .into_iter()
            .next()
            .filter(|queued| queued.failed.is_none())
    }

    /// Forget the queues of sessions that no longer exist in the grant.
    pub fn retain(&self, alive: &[u64]) {
        let stale = {
            let stored = self.stored.lock().unwrap_or_else(|lock| lock.into_inner());
            stored
                .sessions
                .keys()
                .any(|session| !alive.contains(session))
        };
        if stale {
            self.change(|stored| stored.sessions.retain(|session, _| alive.contains(session)));
        }
    }
}
