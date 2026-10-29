//! The check slot: one check run at a time for every task host of this wsx state folder.
//! The disk slot (`tasks/disk-slot/`) queues the same way: one task resumes from a disk wait
//! at a time, and holds the slot for a short settle time. ref: docs/tasks.md "Checks, revisions and report"
//!
//! The slot is an `flock` on `tasks/<queue>/slot.lock`, which the kernel drops with the
//! process, so a killed host never blocks the queue. A waiter queues as a ticket file that
//! it keeps locked; a ticket whose lock is free belongs to a dead waiter and is removed.
//! Ticket names sort the queue: a land's checks first, then first come, first served.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use wsx_core::task::{now_unix_ms, store};

const POLL: Duration = Duration::from_millis(250);
const SLOT_FILE: &str = "slot.lock";

#[derive(Clone, Copy)]
pub enum Priority {
    /// The checks of a rebased revision that lands when they pass: closest to main.
    Land = 0,
    Normal = 1,
}

/// The held slot; dropping it frees the slot.
pub struct Slot(#[allow(dead_code)] File);

fn try_lock(file: &File) -> bool {
    // SAFETY: the descriptor is open for the life of `file`.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Wait for the slot. `label` names the holder to the waiters (`demo t3`); `queued` hears
/// each new reason this run waits, never when the slot is free at once.
pub fn take(priority: Priority, label: &str, mut queued: impl FnMut(String)) -> io::Result<Slot> {
    let ticket = Ticket::join("check-slot", "the check slot", priority)?;
    let mut said = String::new();
    loop {
        match ticket.try_take(label) {
            Ok(slot) => return Ok(slot),
            Err(reason) if reason != said => {
                queued(reason.clone());
                said = reason;
            }
            Err(_) => {}
        }
        std::thread::sleep(POLL);
    }
}

/// A place in the queue of `tasks/<queue>/`, kept while this value lives.
pub struct Ticket {
    dir: PathBuf,
    name: String,
    /// Locked while it lives, so the waiters behind it know it is not a dead one.
    #[allow(dead_code)]
    file: File,
    slot: File,
    what: &'static str,
}

impl Ticket {
    /// Join the queue of `tasks/<queue>/`; `what` names its slot in the reasons.
    pub fn join(queue: &str, what: &'static str, priority: Priority) -> io::Result<Ticket> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = store::state_dir().join("tasks").join(queue);
        DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
        let slot = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(SLOT_FILE))?;
        let name = format!(
            "q-{}-{:016}-{:010}-{:06}",
            priority as u8,
            now_unix_ms(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        // ^ Locked before it gets its name, so no other waiter takes a new ticket for a dead one.
        let staged = dir.join(format!(".{name}"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        let renamed = match try_lock(&file) {
            true => fs::rename(&staged, dir.join(&name)),
            false => Err(io::Error::other("cannot lock a new queue ticket")),
        };
        if let Err(error) = renamed {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
        Ok(Ticket {
            dir,
            name,
            file,
            slot,
            what,
        })
    }

    /// The slot when no live ticket is ahead and nobody holds it, else the reason it waits.
    pub fn try_take(&self, label: &str) -> Result<Slot, String> {
        let ahead = live_ahead(&self.dir, &self.name);
        if ahead == 0 && try_lock(&self.slot) {
            // ^ A clone shares the lock, which lasts until its last descriptor closes.
            let mut holder = self
                .slot
                .try_clone()
                .map_err(|error| format!("cannot hold {}: {error}", self.what))?;
            let _ = holder
                .set_len(0)
                .and_then(|()| holder.write_all(label.as_bytes()));
            return Ok(Slot(holder));
        }
        let holder = fs::read_to_string(self.dir.join(SLOT_FILE)).unwrap_or_default();
        Err(match ahead {
            0 => format!("queued for {} behind {holder}", self.what),
            n => format!("queued for {} behind {holder} and {n} more", self.what),
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(&self.name));
    }
}

/// Live tickets before `name`; dead ones (their lock free) are removed.
fn live_ahead(dir: &Path, name: &str) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let other = entry.file_name();
            let other = other.to_string_lossy();
            if !other.starts_with("q-") || *other >= *name {
                return false;
            }
            match File::open(entry.path()) {
                Ok(file) if try_lock(&file) => {
                    let _ = fs::remove_file(entry.path());
                    false
                }
                Ok(_) => true,
                Err(_) => false,
            }
        })
        .count()
}
