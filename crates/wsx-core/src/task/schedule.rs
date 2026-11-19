//! The machine's scheduler: when a heavy run (a scenario round, a task's checks, a deploy's smoke
//! test) may start on this Mac, in which order the waiting runs go, and when a run goes to GitHub
//! CI instead. Plain rules over plain facts; the lock files and tickets that hold the runs are
//! `crates/wsx/src/task/slot.rs`. ref: docs/design/scheduler.md, docs/tasks.md "Check slot"

use super::status::Waiter;
use super::view::project_tasks;
use super::{Actor, LogEvent, TaskRecord};
use std::path::Path;
use std::time::Duration;

/// A scenario run's reason line starts with this when it runs on GitHub CI; `task::status` words it.
pub const ON_CI: &str = "went to CI: ";
/// A scenario command exits with this when this Mac cannot run it (a tool is missing): the host
/// runs it on CI at once and says why.
pub const CANNOT_RUN_HERE: i32 = 69;
/// The most scenario rounds at once; the load, memory and disk of the moment decide how many.
pub const MAX_SCENARIO_RUNS: usize = 3;
/// A CI round, p50 of 40 runs (docs/design/scenario-feedback-research.md), and the longest a CI run is waited for.
const CI_ROUND_MS: u64 = 25 * 60_000;
pub const CI_TIMEOUT: Duration = Duration::from_secs(75 * 60);
/// A local round when no run of the command is recorded (the same document, 11-19).
const LOCAL_ROUND_MS: u64 = 15 * 60_000;

// ^ Measured limits (docs/design/scheduler.md "Measured"). Load is the 1-minute average per core.
/// A first run starts under this load; a run that waited `LOAD_WAIT` starts anyway.
const START_LOAD: f64 = 2.5;
const LOAD_WAIT: Duration = Duration::from_secs(15 * 60);
/// A run beside another starts under this load, with this much memory and disk free, and when the
/// newest run is older than `SETTLE`: the load average shows a new run only after a minute.
const WIDEN_LOAD: f64 = 1.5;
const SETTLE: Duration = Duration::from_secs(90);
const RUN_RAM_MB: u64 = 3 * 1024;
const RUN_DISK_MB: u64 = 3 * 1024;

/// The order of the runs that wait: a land's checks, a task others wait for, a task the owner
/// asked for, the rest. The queue sorts tickets by it, then by arrival.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// The checks of a rebased revision that lands when they pass: closest to main.
    Land = 0,
    /// Another open task waits for this one to land (`after`).
    Urgent = 1,
    /// The owner opened the task.
    Asked = 2,
    Normal = 3,
}

/// The priority of a task's run, and for an urgent one who waits for it ("#518 waits for it").
/// Read from the record and `status::waiters`; nothing else stores urgency.
pub fn urgency(record: &TaskRecord, waiters: &[Waiter]) -> (Priority, Option<String>) {
    match waiters.first() {
        Some(waiter) => (
            Priority::Urgent,
            Some(format!("#{} waits for it", waiter.task)),
        ),
        None if record.created_by == Actor::Owner => (Priority::Asked, None),
        None => (Priority::Normal, None),
    }
}

/// What the Mac has now.
#[derive(Clone, Copy, Debug)]
pub struct Machine {
    pub load1: f64,
    pub cores: usize,
    pub free_ram_mb: Option<u64>,
    pub free_disk_mb: Option<u64>,
}

/// The Mac's load, free memory and free disk (on the volume of `disk`) now.
pub fn sample(disk: &Path) -> Machine {
    extern "C" {
        // ^ POSIX on macOS and Linux; libc declares it for Linux only.
        fn getloadavg(loadavg: *mut f64, nelem: std::ffi::c_int) -> std::ffi::c_int;
    }
    let mut load = [0f64; 3];
    // SAFETY: room for the 3 values asked for.
    let load1 = match unsafe { getloadavg(load.as_mut_ptr(), 3) } {
        n if n >= 1 => load[0],
        _ => 0.0,
    };
    Machine {
        load1,
        cores: std::thread::available_parallelism().map_or(1, usize::from),
        free_ram_mb: free_ram_mb(),
        free_disk_mb: super::store::free_mb(disk),
    }
}

#[cfg(target_os = "macos")]
fn free_ram_mb() -> Option<u64> {
    use std::sync::OnceLock;
    // ^ Each mach_host_self() adds a reference to the port: ask once in the process's life.
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    #[allow(deprecated)]
    // SAFETY: no arguments; the port stays valid for the process.
    let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
    // SAFETY: the out-parameter is HOST_VM_INFO64's struct and the count its size.
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let status = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    // SAFETY: no arguments.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as u64;
    // ^ What the kernel gives back without swapping: free, inactive and purgeable pages.
    let pages = u64::from(stats.free_count)
        + u64::from(stats.inactive_count)
        + u64::from(stats.purgeable_count);
    (status == libc::KERN_SUCCESS).then_some(pages * page / (1024 * 1024))
}

#[cfg(not(target_os = "macos"))]
fn free_ram_mb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = text
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(kb / 1024)
}

/// A lane of runs as it stands when a waiter asks.
pub struct Lane {
    pub machine: Machine,
    /// Runs that hold a slot, and how long the newest has held it.
    pub holders: usize,
    pub newest: Option<Duration>,
    /// Waiters in front of this one.
    pub ahead: usize,
    pub waited: Duration,
    pub scenarios: bool,
    /// The load, memory and disk rule sets the width. Off, the lane is first come, first served with
    /// its `max` slots: the checks lane, and a scenarios lane whose width the owner fixed
    /// (`WSX_SCENARIO_SLOTS`, which every fixture sets).
    pub paced: bool,
    /// The slots the lane has.
    pub max: usize,
    /// `tasks.minFreeGb` in MiB: a run beside another leaves it free.
    pub min_free_mb: u64,
}

/// A waiting run: who waits for it (urgent), and how long its command runs (recorded, else the
/// default).
pub struct Run {
    pub urgent: Option<String>,
    pub typical_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Take a slot now.
    Go,
    /// Wait here; the note says why, for the reason line.
    Wait(Option<String>),
    /// Run on GitHub CI: the reason.
    Ci(String),
}

/// Whether the run starts now, waits for the lane, or goes to CI. A lane starts a first run when
/// the load allows (a loaded Mac does not starve it: past `LOAD_WAIT` it starts), and a run beside
/// another only when load, memory, disk and the settle time allow. Only an urgent scenario run
/// goes to CI, and only when its estimated wait here is longer than a CI round.
pub fn decide(run: &Run, lane: &Lane) -> Verdict {
    let Machine {
        load1,
        cores,
        free_ram_mb,
        free_disk_mb,
    } = lane.machine;
    let cores_f = cores as f64;
    let busy = lane.holders + lane.ahead;
    if !lane.paced && lane.ahead == 0 {
        return Verdict::Go;
    }
    if lane.paced && lane.ahead == 0 && lane.holders == 0 {
        return match load1 < START_LOAD * cores_f || lane.waited >= LOAD_WAIT {
            true => Verdict::Go,
            false => Verdict::Wait(Some(format!(
                "load {load1:.0} on {cores} cores, it starts under {:.0}",
                START_LOAD * cores_f
            ))),
        };
    }
    if lane.paced && lane.ahead == 0 && lane.holders < lane.max {
        let settled = lane.newest.is_none_or(|age| age >= SETTLE);
        let roomy = free_ram_mb.is_none_or(|free| free >= RUN_RAM_MB)
            && free_disk_mb.is_none_or(|free| free >= lane.min_free_mb + RUN_DISK_MB);
        if load1 < WIDEN_LOAD * cores_f && roomy && settled {
            return Verdict::Go;
        }
    }
    // ^ The lane is full or the Mac has no room for one more: rounds ahead of this run, upper bound.
    let rounds = (busy + 1).div_ceil(lane.holders.max(1)) - 1;
    let eta_ms = rounds as u64 * run.typical_ms;
    let (Some(who), true) = (&run.urgent, lane.scenarios) else {
        return Verdict::Wait(None);
    };
    let local = format!(
        "the local lane is full for ~{} min",
        eta_ms.div_ceil(60_000)
    );
    match eta_ms > CI_ROUND_MS {
        true => Verdict::Ci(format!(
            "urgent: {who}; {local}, a CI round takes ~{} min",
            CI_ROUND_MS / 60_000
        )),
        false => Verdict::Wait(Some(format!("urgent: {who}; {local}"))),
    }
}

/// How long `command` runs: the median of its passed runs that the project's tasks recorded
/// (`check_run` events), else `LOCAL_ROUND_MS`.
pub fn typical_ms(project_id: &str, command: &str) -> u64 {
    let mut times: Vec<u64> = project_tasks(project_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(dir, _)| {
            dir.last_event(|event| match event {
                LogEvent::CheckRun { results } => results
                    .iter()
                    .find(|r| r.command == command && !r.stored && r.exit == Some(0))
                    .map(|r| r.ms),
                _ => None,
            })
            .ok()
            .flatten()
        })
        .collect();
    times.sort_unstable();
    times
        .get(times.len() / 2)
        .copied()
        .unwrap_or(LOCAL_ROUND_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(holders: usize, ahead: usize, load1: f64) -> Lane {
        Lane {
            machine: Machine {
                load1,
                cores: 10,
                free_ram_mb: Some(16 * 1024),
                free_disk_mb: Some(40 * 1024),
            },
            holders,
            newest: Some(Duration::from_secs(600)),
            ahead,
            waited: Duration::ZERO,
            scenarios: true,
            paced: true,
            max: 3,
            min_free_mb: 3 * 1024,
        }
    }

    fn run(urgent: bool, typical_ms: u64) -> Run {
        Run {
            urgent: urgent.then(|| "#518 waits for it".to_string()),
            typical_ms,
        }
    }

    #[test]
    fn width_follows_load_ram_and_disk() {
        let normal = run(false, 15 * 60_000);
        assert_eq!(decide(&normal, &lane(0, 0, 18.0)), Verdict::Go);
        assert!(matches!(
            decide(&normal, &lane(0, 0, 30.0)),
            Verdict::Wait(Some(_))
        ));
        // A run that waited past the load wait starts on a loaded Mac.
        let mut late = lane(0, 0, 30.0);
        late.waited = LOAD_WAIT;
        assert_eq!(decide(&normal, &late), Verdict::Go);
        // A second run needs headroom; at load 18 the lane stays at one.
        assert_eq!(decide(&normal, &lane(1, 0, 8.0)), Verdict::Go);
        assert_eq!(decide(&normal, &lane(1, 0, 18.0)), Verdict::Wait(None));
        let mut few = lane(1, 0, 8.0);
        few.machine.free_ram_mb = Some(1024);
        assert_eq!(decide(&normal, &few), Verdict::Wait(None));
        let mut fresh = lane(1, 0, 8.0);
        fresh.newest = Some(Duration::from_secs(10));
        assert_eq!(decide(&normal, &fresh), Verdict::Wait(None));
        // The lane's slots are a ceiling.
        assert_eq!(decide(&normal, &lane(3, 0, 1.0)), Verdict::Wait(None));
        // A lane that is not paced starts at once, whatever the load.
        let mut fixed = lane(0, 0, 90.0);
        fixed.paced = false;
        assert_eq!(decide(&normal, &fixed), Verdict::Go);
    }

    #[test]
    fn only_urgent_work_goes_to_ci_and_only_when_it_pays() {
        // Two rounds ahead of ~15 min: ~30 min here against a 25 min CI round.
        let long = decide(&run(true, 15 * 60_000), &lane(2, 2, 18.0));
        assert!(matches!(&long, Verdict::Ci(why) if why.starts_with("urgent: #518 waits for it")));
        // One round ahead: ~15 min, shorter than CI, so it waits and says so.
        let short = decide(&run(true, 15 * 60_000), &lane(2, 0, 18.0));
        assert_eq!(
            short,
            Verdict::Wait(Some(
                "urgent: #518 waits for it; the local lane is full for ~15 min".into()
            ))
        );
        // The same wait for a task nobody waits for stays here.
        assert_eq!(
            decide(&run(false, 15 * 60_000), &lane(2, 2, 18.0)),
            Verdict::Wait(None)
        );
        // A plain checks lane never goes to CI.
        let mut checks = lane(1, 3, 18.0);
        checks.scenarios = false;
        checks.paced = false;
        assert_eq!(
            decide(&run(true, 15 * 60_000), &checks),
            Verdict::Wait(None)
        );
    }
}
