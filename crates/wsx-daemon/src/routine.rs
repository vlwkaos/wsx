//! The routine scheduler inside wsxd: cron and `at` minutes, fired events, runs and their
//! records. Each run is a `wsxd __wsx_routine_run` process that writes its own result, so a
//! run outlives the daemon that started it: a live handoff's successor finishes its record.
//! ref: docs/tasks.md "Routines"

use super::*;
use crate::server::try_lock_file;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Seek;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use wsx_core::fs_util::private_dir;
use wsx_core::routine::{
    cron::{CronSchedule, LocalTime},
    store::{self, ProjectRoutines},
    Action, FireOutcome, Response, Routine, RoutineError, RoutineErrorKind, RoutineFire,
    RoutineView, RunCause, RunRecord, RunStatus, Trigger,
};

/// Run records kept per routine; an older run's record and output files go.
const MAX_RUNS: usize = 20;
const MAX_EVENT_PAYLOAD_BYTES: usize = 64 * 1024;
/// The fired event's payload (compact JSON) in a run's environment.
const EVENT_PAYLOAD_ENV: &str = "WSX_EVENT_PAYLOAD";
const MAX_RUN_LOG_BYTES: usize = 8 * 1024 * 1024;
const RUN_LOG_TAIL_BYTES: usize = 128 * 1024;
const FINAL_OUTPUT_BYTES: usize = 16 * 1024;
const OWNERSHIP_WAIT: Duration = Duration::from_secs(10);
const UPGRADE_NOTE: &str = "interrupted by the upgrade from asched to wsxd's scheduler";

static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();

struct Scheduler {
    root: PathBuf,
    /// `Some` (the store's flock) once this daemon owns the store. Every store write holds it.
    owned: Mutex<Option<fs::File>>,
    owned_changed: Condvar,
    stopping: AtomicBool,
    /// Why the store could not be set up; the scheduler then waits for a restart.
    failed: Mutex<Option<String>>,
    /// Projects with a running run, the ones `reconcile` reads.
    active: Mutex<BTreeSet<PathBuf>>,
    /// Run supervisors this process started, reaped once they end.
    children: Mutex<Vec<Child>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Start the scheduler for the daemon at `socket`. It schedules once it holds the store's
/// lock, which a live handoff's source keeps until it exits.
pub(crate) fn start(socket: &Path) {
    let root = store::root_for(socket);
    let asched = asched_root(socket);
    let scheduler = SCHEDULER.get_or_init(|| Scheduler {
        root,
        owned: Mutex::new(None),
        owned_changed: Condvar::new(),
        stopping: AtomicBool::new(false),
        failed: Mutex::new(None),
        active: Mutex::new(BTreeSet::new()),
        children: Mutex::new(Vec::new()),
        thread: Mutex::new(None),
    });
    let handle = thread::spawn(move || scheduler.run(asched));
    *lock(&scheduler.thread) = Some(handle);
}

/// Stop scheduling. `cancel_runs`: also end the running runs, as on a stop; a live handoff
/// leaves them to its successor.
pub(crate) fn stop(cancel_runs: bool) {
    let Some(scheduler) = SCHEDULER.get() else {
        return;
    };
    scheduler.stopping.store(true, Ordering::Release);
    if let Some(handle) = lock(&scheduler.thread).take() {
        let _ = handle.join();
    }
    let owned = lock(&scheduler.owned);
    if cancel_runs && owned.is_some() {
        for mut data in scheduler.projects() {
            let names: Vec<String> = data.runs.keys().cloned().collect();
            let changed = names.iter().filter(|name| cancel(&mut data, name)).count();
            if changed > 0 {
                save_or_report(&scheduler.root, &data);
            }
        }
    }
}

pub(crate) fn handle(project: &Path, action: Action) -> Response {
    let result = match SCHEDULER.get() {
        Some(scheduler) => scheduler.handle(project, action),
        None => Err(unavailable("the routine scheduler has not started")),
    };
    result.unwrap_or_else(Response::Error)
}

/// asched's store to import: `ASCHED_ROOT`, else its default folder for the owner's own
/// daemon only, so a test daemon never takes the owner's routines.
fn asched_root(socket: &Path) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("ASCHED_ROOT").filter(|root| !root.is_empty()) {
        return Some(PathBuf::from(root));
    }
    (socket == wsx_core::runtime::state_dir().join("wsx.sock"))
        .then(dirs::config_dir)
        .flatten()
        .map(|dir| dir.join("asched"))
}

impl Scheduler {
    fn run(&self, asched: Option<PathBuf>) {
        let mut last_minute = None;
        while !self.stopping.load(Ordering::Acquire) {
            if lock(&self.owned).is_none() && lock(&self.failed).is_none() {
                match self.take_ownership(asched.as_deref()) {
                    Ok(true) => self.owned_changed.notify_all(),
                    Ok(false) => {}
                    // ^ No retry each second: requests name the error until a restart.
                    Err(error) => {
                        eprintln!("wsxd routines: {error}");
                        *lock(&self.failed) = Some(error.to_string());
                        self.owned_changed.notify_all();
                    }
                }
            }
            if lock(&self.owned).is_some() {
                self.reconcile();
                let minute = now_epoch() / 60;
                if last_minute != Some(minute) {
                    last_minute = Some(minute);
                    self.tick(minute);
                }
            }
            for _ in 0..5 {
                if self.stopping.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    fn take_ownership(&self, asched: Option<&Path>) -> io::Result<bool> {
        private_dir(self.root.parent().unwrap_or(Path::new("/")))?;
        let lock_path = self.root.with_file_name("routines.lock");
        let Some(file) = try_lock_file(&lock_path)? else {
            return Ok(false);
        };
        if !self.root.exists() {
            match asched.filter(|root| root.join("projects").is_dir()) {
                Some(asched) => import_asched(asched, &self.root, &self.stopping)?,
                None => private_dir(&self.root)?,
            }
        }
        let running = |data: &ProjectRoutines| has_running(data).then(|| data.project_path.clone());
        *lock(&self.active) = self.projects().iter().filter_map(running).collect();
        *lock(&self.owned) = Some(file);
        Ok(true)
    }

    fn handle(&self, project: &Path, action: Action) -> Result<Response, RoutineError> {
        let deadline = Instant::now() + OWNERSHIP_WAIT;
        let mut owned = lock(&self.owned);
        while owned.is_none() {
            if let Some(error) = lock(&self.failed).as_deref() {
                return Err(unavailable(format!(
                    "the routine store could not be set up: {error}; fix it and run `wsx daemon restart`"
                )));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.stopping.load(Ordering::Acquire) {
                return Err(unavailable(
                    "the routine scheduler waits for the previous wsxd to stop",
                ));
            }
            owned = self
                .owned_changed
                .wait_timeout(owned, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let project = store::canonical_project(project)?;
        let mut data = store::load(&self.root, &project)?;
        self.process(&mut data, action)
    }

    fn process(
        &self,
        data: &mut ProjectRoutines,
        action: Action,
    ) -> Result<Response, RoutineError> {
        let root = &self.root;
        let check_revision = |data: &ProjectRoutines, expected: u64| match data.revision == expected
        {
            true => Ok(()),
            false => Err(RoutineError::new(
                RoutineErrorKind::Conflict,
                format!(
                    "stale routine revision: expected {expected}, actual {}",
                    data.revision
                ),
            )),
        };
        let find = |data: &ProjectRoutines, name: &str| {
            data.routines
                .iter()
                .find(|routine| routine.name == name)
                .cloned()
                .ok_or_else(|| not_found(format!("routine '{name}' not found")))
        };
        let changed = |data: &mut ProjectRoutines| {
            data.revision += 1;
            store::save(root, data).map(|()| Response::Ok {
                revision: Some(data.revision),
            })
        };
        match action {
            Action::List => Ok(Response::Routines {
                revision: data.revision,
                routines: views(data),
            }),
            Action::Logs { name } => Ok(Response::Runs {
                runs: data.runs.get(&name).cloned().unwrap_or_default(),
            }),
            Action::Add { revision, routine } => {
                let routine = routine.validated()?;
                check_revision(data, revision)?;
                if data.routines.iter().any(|r| r.name == routine.name) {
                    return Err(duplicate(&routine.name));
                }
                data.routines.push(routine);
                changed(data)
            }
            Action::Edit {
                revision,
                old_name,
                routine,
            } => {
                let routine = routine.validated()?;
                check_revision(data, revision)?;
                let index = data
                    .routines
                    .iter()
                    .position(|r| r.name == old_name)
                    .ok_or_else(|| not_found(format!("routine '{old_name}' not found")))?;
                let renamed = routine.name != old_name;
                if renamed && data.running(&old_name).is_some() {
                    return Err(already_running(&old_name));
                }
                if renamed && data.routines.iter().any(|r| r.name == routine.name) {
                    return Err(duplicate(&routine.name));
                }
                if renamed {
                    if let Some(mut runs) = data.runs.remove(&old_name) {
                        runs.iter_mut()
                            .for_each(|run| run.routine.clone_from(&routine.name));
                        data.runs.insert(routine.name.clone(), runs);
                    }
                    if let Some(claim) = data.claims.remove(&old_name) {
                        data.claims.insert(routine.name.clone(), claim);
                    }
                }
                data.routines[index] = routine;
                changed(data)
            }
            Action::Delete { revision, name } => {
                check_revision(data, revision)?;
                find(data, &name)?;
                cancel(data, &name);
                data.routines.retain(|routine| routine.name != name);
                data.claims.remove(&name);
                for run in data.runs.remove(&name).unwrap_or_default() {
                    remove_run_dir(root, &data.project_path, &run);
                }
                changed(data)
            }
            Action::Run { name } => {
                let routine = find(data, &name)?;
                if data.running(&name).is_some() {
                    return Err(already_running(&name));
                }
                self.start_run(data, &routine, RunCause::Manual, None)?;
                Ok(Response::Ok { revision: None })
            }
            Action::Cancel { name } => {
                if !cancel(data, &name) {
                    return Err(not_found(format!("no running run of '{name}'")));
                }
                store::save(root, data)?;
                Ok(Response::Ok { revision: None })
            }
            Action::Fire {
                kind,
                payload,
                event_id,
            } => {
                let Trigger::Event { kind } = (Trigger::Event { kind }).validated()? else {
                    unreachable!("an event trigger stays one")
                };
                if event_id.is_empty()
                    || event_id.len() > 1_024
                    || event_id.chars().any(char::is_control)
                {
                    return Err(RoutineError::validation(
                        "event id must be 1-1024 bytes without control characters",
                    ));
                }
                let payload = payload.to_string();
                if payload.len() > MAX_EVENT_PAYLOAD_BYTES {
                    return Err(RoutineError::validation(format!(
                        "event payload exceeds {MAX_EVENT_PAYLOAD_BYTES} bytes"
                    )));
                }
                let matching: Vec<Routine> = data
                    .routines
                    .iter()
                    .filter(|r| r.enabled && r.trigger == Trigger::Event { kind: kind.clone() })
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    return Ok(Response::Fire {
                        outcome: FireOutcome::NoMatch,
                    });
                }
                if data.has_receipt(&kind, &event_id) {
                    return Ok(Response::Fire {
                        outcome: FireOutcome::Deduplicated,
                    });
                }
                data.add_receipt(&kind, &event_id);
                let mut routines = Vec::new();
                for routine in matching {
                    let name = routine.name.clone();
                    if data.running(&name).is_some() {
                        routines.push(RoutineFire::AlreadyRunning { name });
                        continue;
                    }
                    let cause = RunCause::Event {
                        kind: kind.clone(),
                        event_id: event_id.clone(),
                    };
                    self.start_run(data, &routine, cause, Some(payload.clone()))?;
                    routines.push(RoutineFire::Started { name });
                }
                // ^ The receipt is kept also when every routine was already running.
                store::save(root, data)?;
                Ok(Response::Fire {
                    outcome: FireOutcome::Handled { routines },
                })
            }
        }
    }

    /// Every project's stored routines whose folder still exists.
    fn projects(&self) -> Vec<ProjectRoutines> {
        let Ok(entries) = fs::read_dir(self.root.join("projects")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| store::read(&entry.path()).ok().flatten())
            .filter(|data| data.project_path.is_dir())
            .collect()
    }

    /// Start the routines whose cron minute or `at` minute is due, each minute once.
    fn tick(&self, minute: i64) {
        let _owned = lock(&self.owned);
        let local = LocalTime::at(minute * 60);
        for mut data in self.projects() {
            for routine in data.routines.clone() {
                let due = match &routine.trigger {
                    _ if !routine.enabled => None,
                    Trigger::Cron(expression) => CronSchedule::parse(expression)
                        .is_ok_and(|schedule| schedule.matches(local))
                        .then_some(minute),
                    Trigger::At { epoch_minute } => {
                        (*epoch_minute <= minute).then_some(*epoch_minute)
                    }
                    Trigger::Event { .. } => None,
                };
                let Some(due) = due else { continue };
                if data.running(&routine.name).is_some()
                    || data
                        .claims
                        .get(&routine.name)
                        .is_some_and(|claimed| *claimed >= due)
                {
                    continue;
                }
                data.claims.insert(routine.name.clone(), due);
                let cause = RunCause::Cron {
                    scheduled_epoch_minute: due,
                };
                if let Err(error) = self.start_run(&mut data, &routine, cause, None) {
                    eprintln!("wsxd routines: {}: {error}", data.project_path.display());
                    break;
                }
            }
        }
    }

    /// Finish the record of each run whose supervisor wrote its result, and mark a run whose
    /// supervisor is gone without one as interrupted.
    fn reconcile(&self) {
        lock(&self.children).retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
        let _owned = lock(&self.owned);
        let active: Vec<PathBuf> = lock(&self.active).iter().cloned().collect();
        for project in active {
            let mut data = match store::load(&self.root, &project) {
                Ok(data) => data,
                Err(error) => {
                    eprintln!("wsxd routines: {error}");
                    continue;
                }
            };
            let mut changed = false;
            let findings: BTreeMap<&str, &[u8]> = data
                .routines
                .iter()
                .map(|routine| (routine.name.as_str(), routine.findings_exit.as_slice()))
                .collect();
            for run in data.runs.values_mut().flatten() {
                if run.status != RunStatus::Running {
                    continue;
                }
                let dir = run.stdout_path.parent().unwrap_or(Path::new("/"));
                if let Some(result) = read_result(dir) {
                    // ^ The routine's codes as they stand now: an edit during a run counts.
                    let codes = findings.get(run.routine.as_str()).copied().unwrap_or(&[]);
                    run.status = match (result.spawn_failed, result.exit_code) {
                        (true, _) => RunStatus::SpawnFailed,
                        (false, Some(0)) => RunStatus::Succeeded,
                        (false, Some(code))
                            if u8::try_from(code).is_ok_and(|c| codes.contains(&c)) =>
                        {
                            RunStatus::Findings
                        }
                        (false, _) => RunStatus::Failed,
                    };
                    run.exit_code = result.exit_code;
                    run.final_output = result.final_output;
                    run.finished_epoch = Some(result.finished_epoch);
                } else if supervisor_alive(run) {
                    continue;
                } else {
                    kill_group(&dir.join("pid"));
                    run.status = RunStatus::Interrupted;
                    run.finished_epoch = Some(now_epoch());
                }
                run.pid = None;
                run.process_start = None;
                changed = true;
            }
            if changed {
                save_or_report(&self.root, &data);
            }
            if !has_running(&data) {
                lock(&self.active).remove(&project);
            }
        }
    }

    /// Store `routine`'s running record with `data` (its claim or receipt too), then start its
    /// supervisor: a wsxd that ends between them leaves an interrupted record, never a run
    /// with none.
    fn start_run(
        &self,
        data: &mut ProjectRoutines,
        routine: &Routine,
        cause: RunCause,
        payload: Option<String>,
    ) -> Result<(), RoutineError> {
        let started = now_epoch();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("{started}-{nanos}");
        let dir = store::run_dir(&self.root, &data.project_path, &id);
        let record = RunRecord {
            id,
            routine: routine.name.clone(),
            started_epoch: started,
            finished_epoch: None,
            cause,
            status: RunStatus::Running,
            exit_code: None,
            pid: None,
            process_start: None,
            final_output: String::new(),
            stdout_path: dir.join("stdout.log"),
            stderr_path: dir.join("stderr.log"),
        };
        let runs = data.runs.entry(routine.name.clone()).or_default();
        runs.push(record);
        let excess = runs.len().saturating_sub(MAX_RUNS);
        let old: Vec<RunRecord> = runs.drain(..excess).collect();
        store::save(&self.root, data)?;
        for old in old {
            remove_run_dir(&self.root, &data.project_path, &old);
        }
        let (argv, input) = expanded_argv(routine);
        let spec = RunSpec {
            argv,
            input,
            payload,
            dir,
        };
        let spawned = spawn_supervisor(&data.project_path, &spec);
        let record = data
            .runs
            .get_mut(&routine.name)
            .and_then(|runs| runs.last_mut())
            .expect("the record was added above");
        match spawned {
            Ok(child) => {
                record.pid = Some(child.id() as i32);
                record.process_start = process_start(child.id() as i32);
                lock(&self.children).push(child);
                lock(&self.active).insert(data.project_path.clone());
            }
            Err(error) => {
                record.status = RunStatus::SpawnFailed;
                record.finished_epoch = Some(now_epoch());
                record.final_output = format!("could not start the run: {error}");
            }
        }
        store::save(&self.root, data)
    }
}

/// The argv with `{prompt}` replaced, and the prompt as stdin when no argument names it.
fn expanded_argv(routine: &Routine) -> (Vec<String>, Option<String>) {
    if routine.command.iter().any(|arg| arg == "{prompt}") {
        let argv = routine.command.iter().map(|arg| match arg.as_str() {
            "{prompt}" => routine.prompt.clone(),
            _ => arg.clone(),
        });
        (argv.collect(), None)
    } else {
        (routine.command.clone(), Some(routine.prompt.clone()))
    }
}

fn views(data: &ProjectRoutines) -> Vec<RoutineView> {
    let now = now_epoch();
    data.routines
        .iter()
        .map(|routine| {
            let runs = data.runs.get(&routine.name).cloned().unwrap_or_default();
            let next_run_epoch = match &routine.trigger {
                _ if !routine.enabled => None,
                Trigger::Cron(expression) => CronSchedule::parse(expression)
                    .ok()
                    .and_then(|schedule| schedule.next_run_after(now)),
                Trigger::At { epoch_minute } => data
                    .claims
                    .get(&routine.name)
                    .is_none_or(|claimed| claimed < epoch_minute)
                    .then_some(epoch_minute * 60),
                Trigger::Event { .. } => None,
            };
            RoutineView {
                routine: routine.clone(),
                running: data.running(&routine.name).is_some(),
                next_run_epoch,
                latest_run: runs.last().cloned(),
                recent_runs: runs,
            }
        })
        .collect()
}

/// End `name`'s running run: mark it cancelled and stop its processes. False when none runs.
fn cancel(data: &mut ProjectRoutines, name: &str) -> bool {
    let Some(run) = data
        .runs
        .get_mut(name)
        .and_then(|runs| runs.last_mut())
        .filter(|run| run.status == RunStatus::Running)
    else {
        return false;
    };
    run.status = RunStatus::Cancelled;
    run.finished_epoch = Some(now_epoch());
    let dir = run.stdout_path.parent().unwrap_or(Path::new("/"));
    // ^ The supervisor's group first: its command stays in that group until it has written
    // `pid`, and only then moves to its own group, which `pid` names.
    if supervisor_alive(run) {
        terminate_group(run.pid.unwrap_or_default());
    }
    kill_group(&dir.join("pid"));
    run.pid = None;
    run.process_start = None;
    true
}

fn remove_run_dir(root: &Path, project: &Path, run: &RunRecord) {
    if is_run_id(&run.id) {
        let _ = fs::remove_dir_all(store::run_dir(root, project, &run.id));
    }
}

/// ^ Only a run id wsx or asched made (`<secs>-<nanos>`) names a folder, never a stored path.
fn is_run_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-')
}

fn has_running(data: &ProjectRoutines) -> bool {
    data.runs
        .values()
        .flatten()
        .any(|run| run.status == RunStatus::Running)
}

fn save_or_report(root: &Path, data: &ProjectRoutines) {
    if let Err(error) = store::save(root, data) {
        eprintln!("wsxd routines: {}: {error}", data.project_path.display());
    }
}

fn supervisor_alive(run: &RunRecord) -> bool {
    run.pid
        .zip(run.process_start.as_deref())
        .is_some_and(|(pid, start)| process_start(pid).as_deref() == Some(start))
}

/// Stop the process group named by a `pid` file (`<pid> <start>`) whose leader is still that
/// process. False when there is none.
fn kill_group(pid_file: &Path) -> bool {
    let Ok(text) = fs::read_to_string(pid_file) else {
        return false;
    };
    let Some((pid, start)) = text
        .trim()
        .split_once(' ')
        .and_then(|(pid, start)| Some((pid.parse::<i32>().ok()?, start)))
    else {
        return false;
    };
    if pid <= 1 || process_start(pid).as_deref() != Some(start) {
        return false;
    }
    terminate_group(pid);
    true
}

fn group_exists(pgid: i32) -> bool {
    let alive = unsafe { libc::kill(-pgid, 0) } == 0;
    alive || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// TERM the group, then KILL what is left after 2 s.
fn terminate_group(pgid: i32) {
    if pgid <= 1 {
        return;
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        unsafe { libc::kill(-pgid, signal) };
        for _ in 0..20 {
            // ^ A supervisor is wsxd's child: reap it, else its zombie keeps the group.
            unsafe { libc::waitpid(-pgid, std::ptr::null_mut(), libc::WNOHANG) };
            if !group_exists(pgid) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The process's start time, which tells it from a later process with its pid.
fn process_start(pid: i32) -> Option<String> {
    wsx_core::runtime::process_identity(pid)
        .ok()
        .map(|(_, started)| started.to_string())
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unavailable(message: impl Into<String>) -> RoutineError {
    RoutineError::new(RoutineErrorKind::Unavailable, message)
}

fn not_found(message: String) -> RoutineError {
    RoutineError::new(RoutineErrorKind::NotFound, message)
}

fn duplicate(name: &str) -> RoutineError {
    RoutineError::new(
        RoutineErrorKind::Duplicate,
        format!("routine '{name}' already exists"),
    )
}

fn already_running(name: &str) -> RoutineError {
    RoutineError::new(
        RoutineErrorKind::AlreadyRunning,
        format!("routine '{name}' is already running"),
    )
}

// ---- One run: `wsxd __wsx_routine_run` reads a RunSpec on stdin. ----

#[derive(Serialize, Deserialize)]
struct RunSpec {
    argv: Vec<String>,
    /// The prompt as stdin, when no argument takes it.
    input: Option<String>,
    payload: Option<String>,
    /// Gets `stdout.log`, `stderr.log`, `pid` and at the end `result.json`.
    dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct RunResult {
    exit_code: Option<i32>,
    spawn_failed: bool,
    final_output: String,
    finished_epoch: i64,
}

fn read_result(dir: &Path) -> Option<RunResult> {
    serde_json::from_slice(&fs::read(dir.join("result.json")).ok()?).ok()
}

fn spawn_supervisor(project: &Path, spec: &RunSpec) -> io::Result<Child> {
    private_dir(&spec.dir)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg(wsx_core::runtime::ROUTINE_RUN_ARG)
        .current_dir(project)
        // ^ Its routines run as the owner, also when a manager's command started wsxd.
        .env_remove(wsx_core::task::owner::ACTOR_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // ^ Its own group: a signal to wsxd's group, or wsxd's exit, leaves the run alone.
        .process_group(0)
        .spawn()?;
    let written = serde_json::to_writer(child.stdin.take().expect("stdin is piped"), spec);
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other(error));
    }
    Ok(child)
}

/// The supervisor: run the argv in its own process group, keep bounded output files, and
/// write `result.json` when it ends.
pub fn run_routine_run(mut arguments: impl Iterator<Item = OsString>) -> io::Result<()> {
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected routine run argument",
        ));
    }
    let mut bytes = Vec::new();
    io::stdin().take(4 * 1024 * 1024).read_to_end(&mut bytes)?;
    let spec: RunSpec = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let stdout_path = spec.dir.join("stdout.log");
    let stderr_path = spec.dir.join("stderr.log");
    let create = |path: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    };
    let Some((program, args)) = spec.argv.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a routine run needs a command",
        ));
    };
    let (stdout_file, mut stderr_file) = (create(&stdout_path)?, create(&stderr_path)?);
    let pid_path = spec.dir.join("pid");
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // ^ The command names itself in `pid` while still in the supervisor's group, then takes its
    // own group, so a cancel at any moment reaches it (see `cancel`). The supervisor has no
    // other thread yet, so this child may allocate.
    unsafe {
        command.pre_exec(move || {
            let pid = libc::getpid();
            let token = process_start(pid).unwrap_or_default();
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&pid_path)?
                .write_all(format!("{pid} {token}").as_bytes())?;
            match libc::setpgid(0, 0) {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        });
    }
    if let Some(payload) = &spec.payload {
        command.env(EVENT_PAYLOAD_ENV, payload);
    }
    let finish = |exit_code, spawn_failed, final_output| {
        let result = RunResult {
            exit_code,
            spawn_failed,
            final_output,
            finished_epoch: now_epoch(),
        };
        let bytes = serde_json::to_vec(&result).map_err(io::Error::other)?;
        wsx_core::fs_util::write_atomic(
            &spec.dir.join("result.json"),
            &bytes,
            wsx_core::fs_util::AtomicWrite::replace().mode(0o600),
        )
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("{program}: {error}");
            stderr_file.write_all(message.as_bytes())?;
            return finish(None, true, message);
        }
    };
    let pid = child.id() as i32;
    let drain = |source: Box<dyn Read + Send>, target: fs::File| {
        thread::spawn(move || copy_bounded(source, target))
    };
    let out = drain(Box::new(child.stdout.take().expect("piped")), stdout_file);
    let err = drain(Box::new(child.stderr.take().expect("piped")), stderr_file);
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), spec.input.as_deref()) {
        // ^ A command that does not read its prompt closes the pipe; that is not a failure.
        let _ = stdin.write_all(input.as_bytes());
    }
    let status = child.wait()?;
    // ^ What the command left in its group would hold the output pipes open.
    terminate_group(pid);
    let _ = out.join();
    let _ = err.join();
    let output = match file_tail(&stdout_path) {
        tail if tail.trim().is_empty() => file_tail(&stderr_path),
        tail => tail,
    };
    finish(status.code(), false, output)
}

/// Copy all of `source`, keeping the first part and the last `RUN_LOG_TAIL_BYTES` when it is
/// longer than `MAX_RUN_LOG_BYTES`.
fn copy_bounded(mut source: impl Read, mut target: fs::File) -> io::Result<()> {
    const MARKER: &[u8] = b"\n[wsx: middle output cut]\n";
    let head_limit = MAX_RUN_LOG_BYTES - RUN_LOG_TAIL_BYTES - MARKER.len();
    let mut head = 0;
    let mut tail = std::collections::VecDeque::with_capacity(RUN_LOG_TAIL_BYTES);
    let mut cut = false;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let mut chunk = &buffer[..read];
        let keep = chunk.len().min(head_limit - head);
        target.write_all(&chunk[..keep])?;
        head += keep;
        chunk = &chunk[keep..];
        tail.extend(chunk);
        if tail.len() > RUN_LOG_TAIL_BYTES {
            cut = true;
            tail.drain(..tail.len() - RUN_LOG_TAIL_BYTES);
        }
    }
    if cut {
        target.write_all(MARKER)?;
    }
    target.write_all(tail.make_contiguous())?;
    target.sync_all()
}

fn file_tail(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let _ = file.seek(io::SeekFrom::Start(
        length.saturating_sub(FINAL_OUTPUT_BYTES as u64),
    ));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).trim_end().to_string()
}

// ---- The one-time import of asched's store. ----

#[derive(Deserialize)]
struct AschedProject {
    project_path: PathBuf,
    revision: u64,
    #[serde(default)]
    routines: Vec<Routine>,
}

#[derive(Deserialize, Default)]
struct AschedRuntime {
    #[serde(default)]
    claims: std::collections::BTreeMap<String, i64>,
    #[serde(default)]
    runs: std::collections::BTreeMap<String, Vec<RunRecord>>,
    #[serde(default)]
    event_receipts: Vec<store::EventReceipt>,
}

/// Copy asched's routines, claims, runs and their output into `root`, after asking a still
/// running asched daemon to stop. A run asched had running is recorded as interrupted by the
/// upgrade. An entry that cannot be read is left out with a warning; asched's files stay as
/// they are. `root` appears whole or not at all.
fn import_asched(asched: &Path, root: &Path, stopping: &AtomicBool) -> io::Result<()> {
    let warn = |what: &dyn std::fmt::Display| eprintln!("wsxd routines: import skips {what}");
    let read_toml = |path: &Path| -> Result<Option<String>, String> {
        let bytes =
            store::read_bounded(path).map_err(|error| format!("{}: {error}", path.display()))?;
        bytes
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|error| format!("{}: {error}", path.display()))
            })
            .transpose()
    };
    let runtime_of = |key: &str| -> Result<AschedRuntime, String> {
        let path = asched.join("runtime").join(format!("{key}.toml"));
        match read_toml(&path)? {
            Some(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
            None => Ok(AschedRuntime::default()),
        }
    };
    let mut keys = Vec::new();
    for entry in fs::read_dir(asched.join("projects"))?.flatten() {
        let path = entry.path();
        if let (Some(key), Some("toml")) = (
            path.file_stem().and_then(|stem| stem.to_str()),
            path.extension().and_then(|ext| ext.to_str()),
        ) {
            keys.push(key.to_string());
        }
    }
    // ^ asched's stop cancels its running runs: note them first, to say why they ended.
    let mut in_flight = HashSet::new();
    for key in &keys {
        for run in runtime_of(key)
            .unwrap_or_default()
            .runs
            .into_values()
            .flatten()
        {
            if run.status == RunStatus::Running {
                in_flight.insert(run.id);
            }
        }
    }
    retire_asched(asched, stopping)?;
    let staging = root.with_file_name(format!("routines.import-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let imported = (|| {
        private_dir(&staging.join("projects"))?;
        for key in &keys {
            let path = asched.join("projects").join(format!("{key}.toml"));
            let project: AschedProject = match read_toml(&path).and_then(|text| {
                toml::from_str(&text.unwrap_or_default())
                    .map_err(|error| format!("{}: {error}", path.display()))
            }) {
                Ok(project) => project,
                Err(error) => {
                    warn(&error);
                    continue;
                }
            };
            let runtime = runtime_of(key).unwrap_or_else(|error| {
                warn(&format!("the run history in {error}"));
                AschedRuntime::default()
            });
            let mut runs = runtime.runs;
            runs.values_mut()
                .for_each(|runs| runs.retain(|run| is_run_id(&run.id)));
            for run in runs.values_mut().flatten() {
                if run.status == RunStatus::Running || in_flight.contains(&run.id) {
                    run.status = RunStatus::Interrupted;
                    run.finished_epoch = Some(now_epoch());
                    run.final_output = match run.final_output.trim() {
                        "" => UPGRADE_NOTE.to_string(),
                        output => format!("{output}\n{UPGRADE_NOTE}"),
                    };
                }
                run.pid = None;
                run.process_start = None;
                let dir = store::run_dir(root, &project.project_path, &run.id);
                let staged = store::run_dir(&staging, &project.project_path, &run.id);
                private_dir(&staged)?;
                for (old, name) in [
                    (&mut run.stdout_path, "stdout.log"),
                    (&mut run.stderr_path, "stderr.log"),
                ] {
                    // ^ Only a regular file: a stored path never makes wsxd read a link or a device.
                    let regular = fs::symlink_metadata(&*old).is_ok_and(|meta| meta.is_file());
                    if regular {
                        if let Err(error) = fs::copy(&*old, staged.join(name)) {
                            warn(&format!("{}: {error}", old.display()));
                        }
                    }
                    *old = dir.join(name);
                }
            }
            let data = ProjectRoutines {
                project_path: project.project_path,
                revision: project.revision,
                routines: project.routines,
                claims: runtime.claims,
                runs,
                event_receipts: runtime.event_receipts,
            };
            store::save(&staging, &data).map_err(io::Error::other)?;
        }
        fs::rename(&staging, root)
    })();
    if imported.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    imported
}

/// Ask asched's daemon (`wsxd __wsx_routine_daemon` before revision 41) to stop, and wait
/// until its lock is free. One that does not stop in 30 s gets a warning: its routines are
/// imported anyway and may run twice until it ends.
fn retire_asched(asched: &Path, stopping: &AtomicBool) -> io::Result<()> {
    let lock_path = asched.join("daemon-v1.lock");
    if !lock_path.exists() || try_lock_file(&lock_path)?.is_some() {
        return Ok(());
    }
    let mut protocol = 5;
    for _ in 0..2 {
        let answer = (|| {
            let mut stream = UnixStream::connect(asched.join("daemon-v1.sock"))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let request = format!(
                "{{\"protocol\":{protocol},\"project\":\"\",\"action\":{{\"action\":\"shutdown\"}}}}\n"
            );
            stream.write_all(request.as_bytes())?;
            let mut answer = String::new();
            BufReader::new(stream).read_line(&mut answer)?;
            Ok::<_, io::Error>(answer)
        })()
        .unwrap_or_default();
        // ^ An older asched answers its own protocol: "client 5, daemon 4".
        match answer.split("daemon ").nth(1).and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        }) {
            Some(theirs) if answer.contains("protocol_mismatch") => protocol = theirs,
            _ => break,
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if stopping.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "wsxd stops"));
        }
        if try_lock_file(&lock_path)?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    eprintln!(
        "wsxd routines: asched's daemon at {} did not stop; stop it, or its routines run twice",
        asched.display()
    );
    Ok(())
}
