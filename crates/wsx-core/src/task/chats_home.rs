//! Which open chats Home lists (the web's Home, the TUI's chat section). The owner dismisses a
//! chat's row with `dismissed` in its folder, the time of the dismissal in ms; the chat, its
//! repo and its tasks go on. The row is listed again when a message the owner would read comes
//! after that time, or when the chat is opened again (`clear`). ref: docs/tasks.md "Chats"

use super::chats::{self, CHAT_FILE};
use super::manager::{Author, ManagerEntry, ManagerEvent, Short};
use super::store::{self, ManagerDir};
use anyhow::Result;
use std::io::ErrorKind;

pub const DISMISSED_FILE: &str = "dismissed";
const TAIL_BYTES: u64 = 64 * 1024;

/// Whether the owner would read this entry as a message: their own, a reply that is not quiet, a
/// `say` of another chat, or events that wake the chat. A rotation, a hold, a ping, a recap and
/// the events that start no turn (`Short::wakes` false) are not.
pub fn is_message(entry: &ManagerEntry) -> bool {
    match &entry.event {
        ManagerEvent::Message { from, short, .. } => match from {
            Author::Owner | Author::Peer { .. } => true,
            Author::Events => Short::any_wakes(short),
            Author::Recap => false,
        },
        ManagerEvent::Reply { quiet, .. } => !quiet,
        _ => false,
    }
}

/// When the chat last had a message (`is_message`), else when it was opened. The one time Home
/// sorts and shows, and dismisses by.
pub fn last_message_ms(dir: &ManagerDir) -> u64 {
    dir.read_tail(TAIL_BYTES)
        .ok()
        .and_then(|entries| {
            entries
                .iter()
                .rev()
                .find(|entry| is_message(entry))
                .map(|entry| entry.unix_ms)
        })
        .unwrap_or_else(|| {
            std::fs::metadata(dir.path.join(CHAT_FILE))
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_millis() as u64)
        })
}

/// Whether the chat's row is on Home: not dismissed, or a message after the dismissal.
pub fn listed(dir: &ManagerDir) -> bool {
    listed_at(dir, || last_message_ms(dir))
}

/// `listed` for a caller that has `last_message_ms` already, or reads it only when a mark exists.
pub fn listed_at(dir: &ManagerDir, last: impl FnOnce() -> u64) -> bool {
    let at = std::fs::read_to_string(dir.path.join(DISMISSED_FILE))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    at.is_none_or(|at| last() > at)
}

/// The owner dismisses an open chat's row at `now` (ms). An unknown or a closed name is refused.
pub fn dismiss(name: &str, now: u64) -> Result<()> {
    let dir = chats::open_chat(name)?;
    store::write_atomic(&dir.path.join(DISMISSED_FILE), now.to_string().as_bytes())
}

/// The chat is opened again: its row is listed from now on.
pub fn clear(dir: &ManagerDir) -> Result<()> {
    match std::fs::remove_file(dir.path.join(DISMISSED_FILE)) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}
