//! The check slots: per lane, flock files and a queue of tickets for every task host of this wsx
//! state folder. `wsx_core::task::schedule` decides who may start (the load, memory and disk of the
//! Mac, urgency, CI); this file holds the runs. The `checks` lane takes plain checks, one at a
//! time; the `scenarios` lane the slow end-to-end ones, up to `schedule::MAX_SCENARIO_RUNS` at once.
//! The disk and limit queues (`tasks/disk-slot/`) use the same tickets with one slot and no rule:
//! one task resumes at a time, and holds the slot for a short settle time.
//! ref: docs/tasks.md "Checks, revisions and report", docs/design/scheduler.md
//!
//! A slot is an `flock` on `tasks/<queue>/slot.lock`, which the kernel drops with the process,
//! so a killed host never blocks the queue. A waiter queues as a ticket file that it keeps
//! locked; a ticket whose lock is free belongs to a dead waiter and is removed. Ticket names
//! sort the queue: by `Priority`, then first come, first served.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};
pub use wsx_core::task::schedule::Priority;
use wsx_core::task::schedule::{self, Run, Verdict};
use wsx_core::task::{now_unix_ms, store};

const POLL: Duration = Duration::from_millis(250);
const SLOT_FILE: &str = "slot.lock";

/// Which machine-wide queue a check run waits in. ref: docs/tasks.md "Check slot"
#[derive(Clone, Copy)]
pub enum Lane {
    /// `tasks.checks` runs. The directory keeps its old name, so older hosts still serialize.
    Checks,
    /// A worker's `check-run` that names a scenario, and `wsx task hold`.
    Scenarios,
    /// The start of a worker turn, once `tasks.gateTurns` is on: one at a time, `schedule::SETTLE`
    /// apart, while the load allows.
    Turns,
}

impl Lane {
    fn queue(self) -> &'static str {
        match self {
            Lane::Checks => "check-slot",
            Lane::Scenarios => "scenario-slot",
            Lane::Turns => "turn-slot",
        }
    }

    /// The most runs of the lane at once; `WSX_SCENARIO_SLOTS` lowers it (a fixture sets 1).
    fn max(self) -> usize {
        match self {
            Lane::Checks | Lane::Turns => 1,
            Lane::Scenarios => std::env::var("WSX_SCENARIO_SLOTS")
                .ok()
                .and_then(|n| n.parse().ok())
                .filter(|n| *n > 0)
                .map_or(schedule::MAX_SCENARIO_RUNS, |n: usize| {
                    n.min(schedule::MAX_SCENARIO_RUNS)
                }),
        }
    }

    fn what(self) -> &'static str {
        match self {
            Lane::Checks => "the check slot",
            Lane::Scenarios => "the scenario test slot",
            Lane::Turns => "the turn slot",
        }
    }

    /// The machine rules (load, memory, disk, PTYs) apply unless the owner fixed the width
    /// (`WSX_SCENARIO_SLOTS`, which every fixture sets): then each lane is first come, first served.
    fn ruled() -> bool {
        std::env::var_os("WSX_SCENARIO_SLOTS").is_none()
    }

    /// PTYs a run of the lane needs free (`schedule::RUN_PTYS`).
    fn ptys(self) -> u64 {
        match self {
            Lane::Checks | Lane::Scenarios if Lane::ruled() => schedule::RUN_PTYS,
            _ => 0,
        }
    }
}

/// A run's place in a lane.
pub struct Want {
    pub lane: Lane,
    pub priority: Priority,
    /// Who waits for the run, when urgent (`schedule::urgency`).
    pub urgent: Option<String>,
    /// How long the run's command takes (`schedule::typical_ms`).
    pub typical_ms: u64,
    /// `tasks.minFreeGb` in MiB, which a second run leaves free.
    pub min_free_mb: u64,
}

impl Want {
    pub fn new(lane: Lane, priority: Priority) -> Want {
        Want {
            lane,
            priority,
            urgent: None,
            typical_ms: 0,
            min_free_mb: 0,
        }
    }
}

/// What a run got: a slot of this Mac, or the order to run on GitHub CI (the reason).
pub enum Taken {
    Slot(Slot),
    Ci(String),
}

/// The held slot; dropping it frees the slot.
pub struct Slot {
    #[allow(dead_code)]
    file: File,
    /// The runs that held the lane when it was taken, itself included.
    pub width: u32,
}

fn try_lock(file: &File) -> bool {
    // SAFETY: the descriptor is open for the life of `file`.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Wait for a slot, or for the order to run on CI. `label` names the holder to the waiters
/// (`demo t3`); `queued` hears each new reason this run waits, never when the slot is free at once.
pub fn take(want: &Want, label: &str, mut queued: impl FnMut(String)) -> io::Result<Taken> {
    let lane = want.lane;
    let ticket = Ticket::join(lane.queue(), lane.what(), lane.max(), want.priority)?;
    let joined = Instant::now();
    let mut said = String::new();
    loop {
        let decided = admit(&ticket, want, label, joined.elapsed());
        match decided {
            Ok(taken) => return Ok(taken),
            Err(reason) if reason != said => {
                queued(reason.clone());
                said = reason;
            }
            Err(_) => {}
        }
        std::thread::sleep(POLL);
    }
}

/// Ask the lane's rule (`schedule::decide`) whether `ticket`'s run starts now: the slot or the order
/// for CI, else the reason it waits. `waited` is how long the run has waited.
pub fn admit(ticket: &Ticket, want: &Want, label: &str, waited: Duration) -> Result<Taken, String> {
    let lane = want.lane;
    let run = Run {
        urgent: want.urgent.clone(),
        typical_ms: want.typical_ms,
    };
    ticket.try_take_by(label, |held| {
        schedule::decide(
            &run,
            &schedule::Lane {
                machine: schedule::sample(&store::state_dir()),
                holders: held.holders,
                newest: held.newest,
                ahead: held.ahead,
                waited,
                scenarios: matches!(lane, Lane::Scenarios),
                ptys: lane.ptys(),
                paced: matches!(lane, Lane::Scenarios | Lane::Turns) && Lane::ruled(),
                max: lane.max(),
                min_free_mb: want.min_free_mb,
            },
        )
    })
}

/// `wsx task slot <command>`: run `command` once the scenarios lane lets it start, and give back its
/// exit code. Its label names the command, as a task's names the task (`status` words it).
pub fn run(argv: &[String]) -> anyhow::Result<i32> {
    use anyhow::Context;
    let label = format!(
        "{} (pid {})",
        argv.join(" ").chars().take(120).collect::<String>(),
        std::process::id()
    );
    let want = Want::new(Lane::Scenarios, Priority::Normal);
    let _held = take(&want, &label, |reason| println!("{reason}"))?;
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("cannot run {}", argv[0]))?;
    Ok(status.code().unwrap_or(1))
}

/// The slots of a lane as a waiter sees them.
pub struct Held {
    pub holders: usize,
    /// How long the newest holder has held its slot.
    pub newest: Option<Duration>,
    pub ahead: usize,
}

/// A place in the queue of `tasks/<queue>/`, kept while this value lives.
pub struct Ticket {
    dir: PathBuf,
    name: String,
    /// Locked while it lives, so the waiters behind it know it is not a dead one.
    #[allow(dead_code)]
    file: File,
    slots: Vec<File>,
    what: &'static str,
}

impl Ticket {
    /// Join the queue of `tasks/<queue>/`; `what` names its slot in the reasons.
    pub fn join(
        queue: &str,
        what: &'static str,
        slots: usize,
        priority: Priority,
    ) -> io::Result<Ticket> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let dir = store::state_dir().join("tasks").join(queue);
        DirBuilder::new().mode(0o700).recursive(true).create(&dir)?;
        let slots = (0..slots)
            .map(|n| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(dir.join(slot_file(n)))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let name = ticket_name(
            priority,
            now_unix_ms(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
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
            slots,
            what,
        })
    }

    /// The slot when no live ticket is ahead and nobody holds it, else the reason it waits.
    /// The disk and limit queues take it this way: one slot, no rule.
    pub fn try_take(&self, label: &str) -> Result<Slot, String> {
        match self.try_take_by(label, |held| match held.ahead + held.holders {
            0 => Verdict::Go,
            _ => Verdict::Wait(None),
        })? {
            Taken::Slot(slot) => Ok(slot),
            Taken::Ci(reason) => Err(reason),
        }
    }

    /// The slot when `decide` says go and a slot is free, the order to run on CI when it says so,
    /// else the reason the run waits: "queued for <slot> behind <holders>[ and <n> more][; <note>]".
    pub fn try_take_by(
        &self,
        label: &str,
        decide: impl FnOnce(&Held) -> Verdict,
    ) -> Result<Taken, String> {
        let ahead = live_ahead(&self.dir, &self.name);
        let held: Vec<(String, SystemTime)> = (0..self.slots.len())
            .map(|n| self.dir.join(slot_file(n)))
            .filter(|path| File::open(path).is_ok_and(|file| !try_lock(&file)))
            .filter_map(|path| {
                let at = fs::metadata(&path).and_then(|meta| meta.modified()).ok()?;
                Some((fs::read_to_string(path).ok()?, at))
            })
            .collect();
        let newest = held
            .iter()
            .map(|(_, at)| at.elapsed().unwrap_or_default())
            .min();
        let note = match decide(&Held {
            holders: held.len(),
            newest,
            ahead,
        }) {
            Verdict::Go => {
                if let Some(slot) = self.slots.iter().find(|slot| try_lock(slot)) {
                    // ^ A clone shares the lock, which lasts until its last descriptor closes.
                    let mut holder = slot
                        .try_clone()
                        .map_err(|error| format!("cannot hold {}: {error}", self.what))?;
                    let _ = holder
                        .set_len(0)
                        .and_then(|()| holder.write_all(label.as_bytes()));
                    return Ok(Taken::Slot(Slot {
                        file: holder,
                        width: held.len() as u32 + 1,
                    }));
                }
                None
            }
            Verdict::Ci(reason) => return Ok(Taken::Ci(reason)),
            Verdict::Wait(note) => note,
        };
        // ^ A slot whose lock is free names no holder: its file keeps the last one's label.
        let holders = held
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
            .join(", ");
        let mut reason = format!("queued for {}", self.what);
        if !holders.is_empty() {
            reason.push_str(&format!(" behind {holders}"));
            if ahead > 0 {
                reason.push_str(&format!(" and {ahead} more"));
            }
        }
        if holders.is_empty() && ahead > 0 {
            reason.push_str(&format!("; {ahead} more ahead"));
        }
        if let Some(note) = note {
            reason.push_str(&format!("; {note}"));
        }
        Err(reason)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(&self.name));
    }
}

/// A ticket's name; the queue sorts by it: urgency first, then arrival.
fn ticket_name(priority: Priority, unix_ms: u64, pid: u32, sequence: u64) -> String {
    format!("q-{}-{unix_ms:016}-{pid:010}-{sequence:06}", priority as u8)
}

/// The lock file of slot `n`; the first keeps the name older hosts use.
fn slot_file(n: usize) -> String {
    match n {
        0 => SLOT_FILE.to_string(),
        n => format!("slot-{n}.lock"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_sort_by_urgency_then_arrival() {
        let mut queue = [
            ticket_name(Priority::Normal, 1, 1, 0),
            ticket_name(Priority::Asked, 2, 1, 1),
            ticket_name(Priority::Urgent, 3, 1, 2),
            ticket_name(Priority::Land, 4, 1, 3),
            ticket_name(Priority::Urgent, 2, 1, 4),
        ];
        queue.sort();
        let order: Vec<u8> = queue.iter().map(|name| name.as_bytes()[2] - b'0').collect();
        assert_eq!(order, [0, 1, 1, 2, 3]);
        // Within a priority the earlier arrival goes first.
        assert!(queue[1] < queue[2] && queue[1].contains("0000000000000002"));
    }
}
