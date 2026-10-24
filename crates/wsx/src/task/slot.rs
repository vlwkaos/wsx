//! The check slot: one check run at a time for every task host of this wsx state folder.
//! ref: docs/tasks.md "Checks, revisions and report"
//!
//! The slot is an `flock` on `tasks/check-slot/slot.lock`, which the kernel drops with the
//! process, so a killed host never blocks the queue. A waiter queues as a ticket file that
//! it keeps locked; a ticket whose lock is free belongs to a dead waiter and is removed.
//! Ticket names sort the queue: a land's checks first, then first come, first served.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
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
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let dir = store::state_dir().join("tasks/check-slot");
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
    let queued_path = dir.join(&name);
    let renamed = match try_lock(&file) {
        true => fs::rename(&staged, &queued_path),
        false => Err(io::Error::other("cannot lock a new check queue ticket")),
    };
    if let Err(error) = renamed {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    let mut said = String::new();
    loop {
        let ahead = live_ahead(&dir, &name);
        if ahead == 0 && try_lock(&slot) {
            let _ = fs::remove_file(&queued_path);
            drop(file);
            let mut holder = &slot;
            let _ = holder
                .set_len(0)
                .and_then(|()| holder.write_all(label.as_bytes()));
            return Ok(Slot(slot));
        }
        let holder = fs::read_to_string(dir.join(SLOT_FILE)).unwrap_or_default();
        let reason = match ahead {
            0 => format!("queued for the check slot behind {holder}"),
            n => format!("queued for the check slot behind {holder} and {n} more"),
        };
        if reason != said {
            queued(reason.clone());
            said = reason;
        }
        std::thread::sleep(POLL);
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
