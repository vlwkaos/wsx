//! The routine scheduler inside wsxd: cron and `at` minutes, fired events, runs and their
//! records. Each run is a `wsxd __wsx_routine_run` process that writes its own result, so a
//! run outlives the daemon that started it: a live handoff's successor finishes its record.
//! ref: docs/tasks.md "Routines"

use super::*;
use std::io::Seek;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
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
            let running: Vec<String> = data.runs.keys().cloned().collect();
            let changed = running
                .iter()
                .filter(|name| cancel(&mut data, name))
                .count();
            if changed > 0 {
                let _ = store::save(&scheduler.root, &data);
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
            if lock(&self.owned).is_none() {
                match self.take_ownership(asched.as_deref()) {
                    Ok(true) => self.owned_changed.notify_all(),
                    Ok(false) => {}
                    Err(error) => eprintln!("wsxd routines: {error}"),
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
        store::private_dir(self.root.parent().unwrap_or(Path::new("/")))?;
        let lock_path = self.root.with_file_name("routines.lock");
        let Some(file) = try_flock(&lock_path)? else {
            return Ok(false);
        };
        if !self.root.exists() {
            match asched.filter(|root| root.join("projects").is_dir()) {
                Some(asched) => import_asched(asched, &self.root)?,
                None => store::private_dir(&self.root)?,
            }
        }
        *lock(&self.owned) = Some(file);
        Ok(true)
    }

    fn handle(&self, project: &Path, action: Action) -> Result<Response, RoutineError> {
        let deadline = Instant::now() + OWNERSHIP_WAIT;
        let mut owned = lock(&self.owned);
        while owned.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.stopping.load(Ordering::Acquire) {
                return Err(unavailable(
                    "the routine scheduler waits for the previous one to stop",
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
        let response = self.process(&mut data, action)?;
        Ok(response)
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
                find(data, &old_name)?;
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
                let index = data.routines.iter().position(|r| r.name == old_name);
                data.routines[index.expect("found above")] = routine;
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
                self.start_run(data, &routine, RunCause::Manual, None);
                store::save(root, data)?;
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
                let routines = matching
                    .into_iter()
                    .map(|routine| {
                        let name = routine.name.clone();
                        if data.running(&name).is_some() {
                            return RoutineFire::AlreadyRunning { name };
                        }
                        let cause = RunCause::Event {
                            kind: kind.clone(),
                            event_id: event_id.clone(),
                        };
                        self.start_run(data, &routine, cause, Some(payload.clone()));
                        RoutineFire::Started { name }
                    })
                    .collect();
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
            let mut started = false;
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
                self.start_run(&mut data, &routine, cause, None);
                started = true;
            }
            if started {
                let _ = store::save(&self.root, &data);
            }
        }
    }

    /// Finish the record of each run whose supervisor wrote its result, and mark a run whose
    /// supervisor is gone without one as interrupted.
    fn reconcile(&self) {
        lock(&self.children).retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
        let _owned = lock(&self.owned);
        for mut data in self.projects() {
            let mut changed = false;
            for run in data.runs.values_mut().flatten() {
                if run.status != RunStatus::Running {
                    continue;
                }
                let dir = run.stdout_path.parent().unwrap_or(Path::new("/"));
                if let Some(result) = read_result(dir) {
                    run.status = match (result.spawn_failed, result.exit_code) {
                        (true, _) => RunStatus::SpawnFailed,
                        (false, Some(0)) => RunStatus::Succeeded,
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
                let _ = store::save(&self.root, &data);
            }
        }
    }

    /// Start `routine`'s supervisor and add its run record; the caller saves `data`.
    fn start_run(
        &self,
        data: &mut ProjectRoutines,
        routine: &Routine,
        cause: RunCause,
        payload: Option<String>,
    ) {
        let started = now_epoch();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("{started}-{nanos}");
        let dir = store::run_dir(&self.root, &data.project_path, &id);
        let mut record = RunRecord {
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
        let (argv, input) = expanded_argv(routine);
        let spec = RunSpec {
            argv,
            input,
            payload,
            dir: dir.clone(),
        };
        match spawn_supervisor(&data.project_path, &spec) {
            Ok(child) => {
                record.pid = Some(child.id() as i32);
                record.process_start = process_start(child.id() as i32);
                lock(&self.children).push(child);
            }
            Err(error) => {
                record.status = RunStatus::SpawnFailed;
                record.finished_epoch = Some(now_epoch());
                record.final_output = format!("could not start the run: {error}");
            }
        }
        let runs = data.runs.entry(routine.name.clone()).or_default();
        runs.push(record);
        let excess = runs.len().saturating_sub(MAX_RUNS);
        for old in runs.drain(..excess).collect::<Vec<_>>() {
            remove_run_dir(&self.root, &data.project_path, &old);
        }
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
    // ^ Before its command started, the supervisor is the only process to stop.
    if !kill_group(&dir.join("pid")) && supervisor_alive(run) {
        terminate_group(run.pid.unwrap_or_default());
    }
    run.pid = None;
    run.process_start = None;
    true
}

fn remove_run_dir(root: &Path, project: &Path, run: &RunRecord) {
    // ^ Only a run id wsx made names a folder to remove, never a stored path.
    if !run.id.is_empty()
        && run
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        let _ = fs::remove_dir_all(store::run_dir(root, project, &run.id));
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
            if !group_exists(pgid) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The process's start time, which tells it from a later process with its pid.
fn process_start(pid: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        // ^ proc_pidinfo writes at most `size` bytes into this proc_bsdinfo.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(format!(
            "{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_name = stat.rsplit_once(") ")?.1;
        after_name.split_whitespace().nth(19).map(str::to_string)
    }
}

fn try_flock(path: &Path) -> io::Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    Ok(locked.then_some(file))
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unavailable(message: &str) -> RoutineError {
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
    store::private_dir(&spec.dir)?;
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
    let (stdout_file, mut stderr_file) = (create(&stdout_path)?, create(&stderr_path)?);
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
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
            let message = format!("{}: {error}", spec.argv[0]);
            stderr_file.write_all(message.as_bytes())?;
            return finish(None, true, message);
        }
    };
    let pid = child.id() as i32;
    let token = process_start(pid).unwrap_or_default();
    wsx_core::fs_util::write_atomic(
        &spec.dir.join("pid"),
        format!("{pid} {token}").as_bytes(),
        wsx_core::fs_util::AtomicWrite::replace().mode(0o600),
    )?;
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
/// upgrade. asched's files stay as they are. `root` appears whole or not at all.
fn import_asched(asched: &Path, root: &Path) -> io::Result<()> {
    let read_toml = |path: &Path| -> io::Result<Option<String>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    };
    let runtime_of = |key: &str| -> io::Result<AschedRuntime> {
        let path = asched.join("runtime").join(format!("{key}.toml"));
        match read_toml(&path)? {
            Some(text) => toml::from_str(&text)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display()))),
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
        for run in runtime_of(key)?.runs.into_values().flatten() {
            if run.status == RunStatus::Running {
                in_flight.insert(run.id);
            }
        }
    }
    retire_asched(asched)?;
    let staging = root.with_file_name(format!("routines.import-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    let imported = (|| {
        store::private_dir(&staging.join("projects"))?;
        for key in &keys {
            let path = asched.join("projects").join(format!("{key}.toml"));
            let Some(text) = read_toml(&path)? else {
                continue;
            };
            let project: AschedProject = toml::from_str(&text)
                .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))?;
            let runtime = runtime_of(key)?;
            let mut runs = runtime.runs;
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
                store::private_dir(&staged)?;
                for (old, name) in [
                    (&mut run.stdout_path, "stdout.log"),
                    (&mut run.stderr_path, "stderr.log"),
                ] {
                    match fs::copy(&*old, staged.join(name)) {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
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
/// until its lock is free.
fn retire_asched(asched: &Path) -> io::Result<()> {
    let lock_path = asched.join("daemon-v1.lock");
    if !lock_path.exists() || try_flock(&lock_path)?.is_some() {
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
        if try_flock(&lock_path)?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "asched's routine daemon at {} did not stop",
            asched.display()
        ),
    ))
}
