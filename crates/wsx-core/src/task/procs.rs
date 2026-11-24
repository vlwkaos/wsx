//! What loads this Mac: the CPU the processes use, grouped by whose work they are (a task's working
//! copy and the runs it started, wsxd, the live web site, a chat's agent, the rest). The agents'
//! boundary refuses `ps` and `top`; the host and wsxd read the process table with libproc, so a load
//! wait and the `this-mac` panel name the cause. One reading serves both: at most one per `FRESH`.
//! ref: docs/design/scheduler.md "Sample"

use super::store::{self, TaskDir};
use crate::process::root_forms;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A reading is used again for this long.
const FRESH: Duration = Duration::from_secs(10);
/// A sample older than this is no rate of now: a reading takes a new one, and waits `COLD_GAP`.
const WARM: Duration = Duration::from_secs(60);
const COLD_GAP: Duration = Duration::from_millis(250);
/// A group below this many cores is not worth a word.
const SHOWN_CORES: f64 = 0.5;
/// The processes named in a group.
const TOPS: usize = 2;
/// The live site's folder: its processes run a file below it (`scripts/deploy-web-live.sh`).
const WEB_LIVE: &str = "/.work/web-live/";
const OUTSIDE: &str = "outside";

/// The CPU one owner's processes use, in cores, and its busiest process names.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub cores: f64,
    pub tops: Vec<(String, f64)>,
}

impl Group {
    /// The busiest processes: "nextest 5.0, rustc 0.9".
    pub fn tops_words(&self) -> String {
        let words: Vec<String> = self
            .tops
            .iter()
            .map(|(name, cores)| format!("{name} {cores:.1}"))
            .collect();
        words.join(", ")
    }
}

/// One process of the table, its CPU as the cores it used over the last interval.
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub cores: f64,
}

/// The folders that tell whose work a process is.
#[derive(Default)]
pub struct Owners {
    /// Each task's working copy (as given and as the system reports it) and its name.
    pub tasks: Vec<(Vec<PathBuf>, String)>,
    /// The task store: a chat's or a chat's agent runs below it.
    pub store: Vec<PathBuf>,
}

fn below(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// The owner a process names itself: the live site's file, `wsxd`, a working copy as its cwd, an
/// agent's folder in the store. A process that names none belongs to its parent's owner.
fn own(proc: &Proc, owners: &Owners) -> Option<String> {
    if let Some(exe) = &proc.exe {
        if exe.to_string_lossy().contains(WEB_LIVE) {
            return Some("web live".into());
        }
        if exe.file_name().is_some_and(|name| name == "wsxd") {
            return Some("wsxd".into());
        }
    }
    let cwd = proc.cwd.as_deref()?;
    if let Some((_, name)) = owners.tasks.iter().find(|(roots, _)| below(cwd, roots)) {
        return Some(name.clone());
    }
    let root = owners.store.iter().find(|root| cwd.starts_with(root))?;
    let first = cwd.strip_prefix(root).ok()?.components().next()?;
    Some(format!("manager {}", first.as_os_str().to_string_lossy()))
}

/// The owner of `pid`: its own, else the nearest ancestor's, else `outside`. A run's children (a
/// browser in a scratch folder) count for the task that started them.
fn owner_of(
    pid: i32,
    table: &HashMap<i32, &Proc>,
    memo: &mut HashMap<i32, String>,
    owners: &Owners,
) -> String {
    let mut chain = Vec::new();
    let mut at = pid;
    let found = loop {
        if let Some(name) = memo.get(&at) {
            break name.clone();
        }
        let Some(proc) = table.get(&at) else {
            break OUTSIDE.into();
        };
        chain.push(at);
        if let Some(name) = own(proc, owners) {
            break name;
        }
        if proc.ppid <= 1 || proc.ppid == at || chain.len() > 64 {
            break OUTSIDE.into();
        }
        at = proc.ppid;
    };
    for pid in chain {
        memo.insert(pid, found.clone());
    }
    found
}

/// The table's CPU by owner, the busiest first.
pub fn group(table: &[Proc], owners: &Owners) -> Vec<Group> {
    let by_pid: HashMap<i32, &Proc> = table.iter().map(|proc| (proc.pid, proc)).collect();
    let mut memo = HashMap::new();
    let mut sums: HashMap<String, (f64, HashMap<String, f64>)> = HashMap::new();
    for proc in table.iter().filter(|proc| proc.cores > 0.0) {
        let (total, names) = sums
            .entry(owner_of(proc.pid, &by_pid, &mut memo, owners))
            .or_default();
        *total += proc.cores;
        let name = proc
            .exe
            .as_deref()
            .and_then(Path::file_name)
            .map_or_else(|| "?".into(), |name| name.to_string_lossy().into_owned());
        *names.entry(name).or_default() += proc.cores;
    }
    let busiest = |a: &f64, b: &f64| b.total_cmp(a);
    let mut groups: Vec<Group> = sums
        .into_iter()
        .map(|(name, (cores, names))| {
            let mut tops: Vec<(String, f64)> = names.into_iter().collect();
            tops.sort_by(|a, b| busiest(&a.1, &b.1).then_with(|| a.0.cmp(&b.0)));
            tops.truncate(TOPS);
            Group { name, cores, tops }
        })
        .collect();
    groups.sort_by(|a, b| busiest(&a.cores, &b.cores).then_with(|| a.name.cmp(&b.name)));
    groups
}

#[derive(Default)]
struct State {
    /// The CPU time (ns) of each process at the last reading, by pid and start time.
    cpu: HashMap<(i32, u64, u64), u64>,
    at: Option<Instant>,
    shown: Vec<Group>,
    shown_at: Option<Instant>,
    /// Each task's folders by project and number: a record is read once.
    known: HashMap<(String, u32), (Vec<PathBuf>, String)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

/// The busiest groups now, at most `n` of them over `SHOWN_CORES`. A reading takes a new sample
/// only when the last is older than `FRESH`; the first one waits `COLD_GAP` for the difference.
/// Empty where the system gives no process table (not macOS).
pub fn top_groups(n: usize) -> Vec<Group> {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    if state.shown_at.is_none_or(|at| at.elapsed() >= FRESH) {
        state.refresh();
    }
    let shown = state.shown.iter().filter(|g| g.cores >= SHOWN_CORES);
    shown.take(n).cloned().collect()
}

/// The groups as a load wait says them: "t551 wsx 6.1 cores (nextest 5.0, rustc 0.9), outside 2.1
/// cores (Chrome 1.4)"; None when no group is over `SHOWN_CORES`.
pub fn blame(n: usize) -> Option<String> {
    let groups = top_groups(n);
    let words: Vec<String> = groups
        .iter()
        .map(|g| format!("{} {:.1} cores ({})", g.name, g.cores, g.tops_words()))
        .collect();
    (!words.is_empty()).then(|| words.join(", "))
}

impl State {
    fn refresh(&mut self) {
        if self.at.is_none_or(|at| at.elapsed() > WARM) {
            self.cpu = read(false)
                .into_iter()
                .map(|row| (row.key, row.ns))
                .collect();
            self.at = Some(Instant::now());
            std::thread::sleep(COLD_GAP);
        }
        let now = Instant::now();
        let secs = self.at.map_or(1.0, |at| (now - at).as_secs_f64()).max(0.1);
        let rows = read(true);
        // ^ A process that began after the last sample used all its time since: it counts whole.
        let table: Vec<Proc> = rows
            .iter()
            .map(|row| Proc {
                pid: row.key.0,
                ppid: row.ppid,
                exe: row.exe.clone(),
                cwd: row.cwd.clone(),
                cores: row.ns.saturating_sub(*self.cpu.get(&row.key).unwrap_or(&0)) as f64
                    / 1e9
                    / secs,
            })
            .collect();
        self.cpu = rows.iter().map(|row| (row.key, row.ns)).collect();
        self.at = Some(now);
        let owners = self.owners();
        self.shown = group(&table, &owners);
        self.shown_at = Some(now);
    }

    /// The folders of every task of every project; a record is read once, a removed task is dropped.
    fn owners(&mut self) -> Owners {
        let mut known = std::mem::take(&mut self.known);
        let mut next = HashMap::new();
        for project in store::project_ids() {
            for id in store::task_numbers(&project).unwrap_or_default() {
                let key = (project.clone(), id);
                let value = known.remove(&key).or_else(|| {
                    let record = TaskDir::open(&project, id).ok()?.read_record().ok()?;
                    Some((
                        root_forms(&record.working_copy),
                        format!("{} t{}", record.project, record.id),
                    ))
                });
                next.extend(value.map(|value| (key, value)));
            }
        }
        self.known = next;
        Owners {
            tasks: self.known.values().cloned().collect(),
            store: root_forms(&store::state_dir().join("tasks")),
        }
    }
}

struct Row {
    /// The pid and the start time, which tell a process from a newer one with its pid.
    key: (i32, u64, u64),
    ppid: i32,
    /// CPU time, user and system, in ns.
    ns: u64,
    exe: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

/// Every process this user may read; with `detail` its executable and cwd too.
#[cfg(target_os = "macos")]
fn read(detail: bool) -> Vec<Row> {
    use crate::process::{born, pids, ProcArgs};
    use std::ffi::{c_void, CStr};
    use std::sync::OnceLock;

    // ^ `pti_total_*` count mach absolute time: ticks on Apple silicon, not ns.
    #[allow(deprecated)]
    fn timebase() -> (u64, u64) {
        static TICKS: OnceLock<(u64, u64)> = OnceLock::new();
        *TICKS.get_or_init(|| {
            let mut info = libc::mach_timebase_info { numer: 1, denom: 1 };
            // SAFETY: the out-parameter is the struct the call fills.
            unsafe { libc::mach_timebase_info(&mut info) };
            (u64::from(info.numer), u64::from(info.denom.max(1)))
        })
    }
    let (numer, denom) = timebase();
    let info = |pid: i32, flavor: i32, out: *mut c_void, size: usize| {
        // SAFETY: `out` holds `size` bytes of plain data, which proc_pidinfo fills at most.
        let read = unsafe { libc::proc_pidinfo(pid, flavor, 0, out, size as i32) };
        read == size as i32
    };
    let mut args = ProcArgs::default();
    let mut rows = Vec::new();
    for pid in pids().into_iter().filter(|pid| *pid > 0) {
        // SAFETY: a zeroed proc_taskinfo is plain data.
        let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>();
        let task_ptr = (&mut task as *mut libc::proc_taskinfo).cast();
        let Some((ppid, (sec, usec))) = born(pid) else {
            continue;
        };
        if !info(pid, libc::PROC_PIDTASKINFO, task_ptr, size) {
            continue;
        }
        let ticks = task.pti_total_user.saturating_add(task.pti_total_system);
        let (exe, cwd) = match detail {
            false => (None, None),
            true => {
                // SAFETY: a zeroed proc_vnodepathinfo is plain data.
                let mut vnodes: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
                let size = std::mem::size_of::<libc::proc_vnodepathinfo>();
                let ptr = (&mut vnodes as *mut libc::proc_vnodepathinfo).cast();
                let cwd = info(pid, libc::PROC_PIDVNODEPATHINFO, ptr, size).then(|| {
                    // SAFETY: the kernel ends the path with a NUL within the 1024 bytes.
                    let path = unsafe { CStr::from_ptr(vnodes.pvi_cdir.vip_path.as_ptr().cast()) };
                    PathBuf::from(path.to_string_lossy().into_owned())
                });
                (args.executable(pid), cwd)
            }
        };
        rows.push(Row {
            key: (pid, sec, usec),
            ppid,
            ns: (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64,
            exe,
            cwd,
        });
    }
    rows
}

#[cfg(not(target_os = "macos"))]
fn read(_detail: bool) -> Vec<Row> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: i32, ppid: i32, exe: &str, cwd: Option<&str>, cores: f64) -> Proc {
        Proc {
            pid,
            ppid,
            exe: Some(PathBuf::from(exe)),
            cwd: cwd.map(PathBuf::from),
            cores,
        }
    }

    /// A fake process table: a task's nextest and the Chrome a run started in a scratch folder, a
    /// second task, wsxd and a pane under it, the live site, a chat's agent, and what is outside.
    #[test]
    fn the_cpu_goes_to_the_owner_of_each_process() {
        let owners = Owners {
            tasks: vec![
                (vec!["/ws/wsx-t551".into()], "wsx t551".into()),
                (vec!["/ws/wsx-t561".into()], "wsx t561".into()),
            ],
            store: vec!["/state/wsx/tasks".into()],
        };
        let table = [
            proc(1, 0, "/sbin/launchd", Some("/"), 0.0),
            proc(10, 1, "/bin/zsh", Some("/ws/wsx-t551"), 0.0),
            proc(11, 10, "/bin/nextest", Some("/ws/wsx-t551/target"), 5.0),
            proc(12, 11, "/bin/rustc", Some("/tmp/scratch"), 0.9),
            proc(13, 10, "/Applications/Chrome", Some("/tmp/web-1"), 1.4),
            proc(20, 1, "/bin/cargo", Some("/ws/wsx-t561"), 0.6),
            proc(30, 1, "/usr/local/bin/wsxd", Some("/"), 0.2),
            proc(31, 30, "/bin/zsh", Some("/Users/me"), 0.7),
            proc(40, 1, "/x/.work/web-live/wsx", Some("/x"), 0.3),
            proc(
                50,
                1,
                "/bin/claude",
                Some("/state/wsx/tasks/wsx-8cbe/manager"),
                1.1,
            ),
            proc(60, 1, "/usr/sbin/mds_stores", Some("/"), 2.0),
            proc(61, 999, "/Applications/Slack", None, 0.4),
        ];
        let groups = group(&table, &owners);
        let got: Vec<(&str, f64)> = groups.iter().map(|g| (g.name.as_str(), g.cores)).collect();
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            [
                "wsx t551",
                "outside",
                "manager wsx-8cbe",
                "wsxd",
                "wsx t561",
                "web live"
            ]
        );
        assert!(
            approx(got[0].1, 7.3),
            "nextest, rustc and Chrome count for t551: {got:?}"
        );
        assert!(
            approx(got[1].1, 2.4),
            "mds_stores and Slack, no owner: {got:?}"
        );
        assert!(approx(got[3].1, 0.9), "wsxd and its pane: {got:?}");
        assert_eq!(
            groups[0].tops,
            [("nextest".to_string(), 5.0), ("Chrome".to_string(), 1.4)]
        );
        assert_eq!(groups[1].tops_words(), "mds_stores 2.0, Slack 0.4");
    }
}
