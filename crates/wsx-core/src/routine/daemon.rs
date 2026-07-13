use super::execution::execute_supervised;
use super::ipc::{Action, Request, Response, RoutineView};
use super::store::{ProjectRoutines, RoutineStore};
use super::{Capabilities, CronSchedule, LocalTime, RoutineError, RunStatus, PROTOCOL_VERSION};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn serve(root: PathBuf) -> Result<(), RoutineError> {
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let lock_path = root.join("daemon-v1.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        return Err(RoutineError::Unavailable(
            "routine daemon is already running".into(),
        ));
    }
    let socket = root.join("daemon-v1.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    reconcile_running(&root);
    let result = event_loop(&root, &listener);
    reconcile_running(&root);
    let _ = fs::remove_file(socket);
    result
}

fn event_loop(root: &Path, listener: &UnixListener) -> Result<(), RoutineError> {
    let mut last_minute = i64::MIN;
    let stopping = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DaemonState::default());
    while !stopping.load(Ordering::SeqCst) {
        reap_workers(&state);
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.to_path_buf();
                let stopping = stopping.clone();
                let state = state.clone();
                std::thread::spawn(move || {
                    let _ = handle_stream(&root, stream, &stopping, &state);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        let minute = now_epoch() / 60;
        if minute != last_minute {
            last_minute = minute;
            tick(root, minute, &state);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    stop_active(root, &state);
    drain_workers(&state);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunKey {
    project: PathBuf,
    routine: String,
}

#[derive(Default)]
struct DaemonState {
    active: Mutex<HashMap<RunKey, bool>>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    config: Mutex<()>,
}

fn handle_stream(
    root: &Path,
    mut stream: UnixStream,
    stopping: &AtomicBool,
    state: &Arc<DaemonState>,
) -> Result<(), RoutineError> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request = serde_json::from_str::<Request>(&line)
        .map_err(|e| RoutineError::Validation(format!("invalid request: {e}")));
    let shutdown = matches!(
        &request,
        Ok(Request {
            action: Action::Shutdown,
            ..
        })
    );
    let response = match request {
        Ok(request) => process(root, request, state).unwrap_or_else(Response::error),
        Err(error) => Response::error(error),
    };
    let mut bytes =
        serde_json::to_vec(&response).map_err(|e| RoutineError::Corrupt(e.to_string()))?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    if shutdown && !matches!(response, Response::Error { .. }) {
        stopping.store(true, Ordering::SeqCst);
    }
    Ok(())
}

fn process(
    root: &Path,
    request: Request,
    state: &Arc<DaemonState>,
) -> Result<Response, RoutineError> {
    if request.protocol != PROTOCOL_VERSION {
        return Err(RoutineError::ProtocolMismatch {
            client: request.protocol,
            daemon: PROTOCOL_VERSION,
        });
    }
    if matches!(request.action, Action::Status) {
        return Ok(Response::Daemon {
            protocol: PROTOCOL_VERSION,
            pid: std::process::id(),
        });
    }
    if matches!(request.action, Action::Shutdown) {
        return Ok(Response::Ok { revision: None });
    }
    let store = RoutineStore::new(root.to_path_buf(), &request.project)?;
    match request.action {
        Action::List => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let config = store.load()?;
            Ok(Response::Routines {
                revision: config.revision,
                routines: views(&store, &config, state),
            })
        }
        Action::Show { name } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let config = store.load()?;
            let routine = config
                .routines
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| RoutineError::NotFound(name.clone()))?;
            Ok(Response::Routine {
                revision: config.revision,
                routine: view(&store, routine, state),
            })
        }
        Action::Add { revision, routine } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let routine = routine.validated()?;
            let mut config = store.load()?;
            if config.routines.iter().any(|r| r.name == routine.name) {
                return Err(RoutineError::Duplicate(routine.name));
            }
            config.routines.push(routine);
            let saved = store.save(config, revision)?;
            Ok(Response::Ok {
                revision: Some(saved.revision),
            })
        }
        Action::Edit {
            revision,
            old_name,
            routine,
        } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let routine = routine.validated()?;
            let mut config = store.load()?;
            let index = config
                .routines
                .iter()
                .position(|r| r.name == old_name)
                .ok_or_else(|| RoutineError::NotFound(old_name.clone()))?;
            if routine.name != old_name && is_active(state, store.project(), &old_name) {
                return Err(RoutineError::AlreadyRunning(old_name));
            }
            if routine.name != old_name && config.routines.iter().any(|r| r.name == routine.name) {
                return Err(RoutineError::Duplicate(routine.name));
            }
            if routine.name != old_name && store.logs_dir().join(hex_name(&routine.name)).exists() {
                return Err(RoutineError::Duplicate(routine.name));
            }
            config.routines[index] = routine.clone();
            let saved = store.save(config, revision)?;
            if routine.name != old_name {
                migrate_runtime_name(&store, &old_name, &routine.name)?;
            }
            Ok(Response::Ok {
                revision: Some(saved.revision),
            })
        }
        Action::Delete { revision, name } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            request_cancel(state, store.project(), &name);
            cancel_running(&store, &name)?;
            let mut config = store.load()?;
            let before = config.routines.len();
            config.routines.retain(|r| r.name != name);
            if config.routines.len() == before {
                return Err(RoutineError::NotFound(name));
            }
            let saved = store.save(config, revision)?;
            Ok(Response::Ok {
                revision: Some(saved.revision),
            })
        }
        Action::Run { name } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let config = store.load()?;
            let routine = config
                .routines
                .iter()
                .find(|r| r.name == name)
                .ok_or(RoutineError::NotFound(name))?;
            spawn_run(store, routine.clone(), None, state)?;
            Ok(Response::Ok { revision: None })
        }
        Action::Cancel { name } => {
            if !is_active(state, store.project(), &name) {
                return Err(RoutineError::NotFound(format!("active run for {name}")));
            }
            request_cancel(state, store.project(), &name);
            cancel_running(&store, &name)?;
            Ok(Response::Ok { revision: None })
        }
        Action::Logs { name } => {
            let state = store.load_runtime()?;
            Ok(Response::Runs {
                runs: state.runs.get(&name).cloned().unwrap_or_default(),
            })
        }
        Action::Status | Action::Shutdown => unreachable!(),
    }
}

fn views(store: &RoutineStore, config: &ProjectRoutines, state: &DaemonState) -> Vec<RoutineView> {
    config
        .routines
        .iter()
        .map(|routine| view(store, routine, state))
        .collect()
}

fn view(store: &RoutineStore, routine: &super::Routine, state: &DaemonState) -> RoutineView {
    let runs = store
        .load_runtime()
        .ok()
        .and_then(|state| state.runs.get(&routine.name).cloned())
        .unwrap_or_default();
    let running = is_active(state, store.project(), &routine.name);
    RoutineView {
        routine: routine.clone(),
        capabilities: Capabilities::for_running(running),
        latest_run: runs.last().cloned(),
        recent_runs: runs,
    }
}

fn is_active(state: &DaemonState, project: &Path, name: &str) -> bool {
    state
        .active
        .lock()
        .map(|active| {
            active.contains_key(&RunKey {
                project: project.to_path_buf(),
                routine: name.to_string(),
            })
        })
        .unwrap_or(true)
}

fn request_cancel(state: &DaemonState, project: &Path, name: &str) {
    if let Ok(mut active) = state.active.lock() {
        if let Some(requested) = active.get_mut(&RunKey {
            project: project.to_path_buf(),
            routine: name.to_string(),
        }) {
            *requested = true;
        }
    }
}

fn migrate_runtime_name(store: &RoutineStore, old: &str, new: &str) -> Result<(), RoutineError> {
    let old_dir = store.logs_dir().join(hex_name(old));
    let new_dir = store.logs_dir().join(hex_name(new));
    if old_dir.exists() {
        if new_dir.exists() {
            return Err(RoutineError::Duplicate(new.to_string()));
        }
        fs::rename(&old_dir, &new_dir)?;
    }
    store.modify_runtime(|state| {
        if let Some(mut runs) = state.runs.remove(old) {
            for run in &mut runs {
                run.routine = new.to_string();
                run.stdout_path = replace_prefix(&run.stdout_path, &old_dir, &new_dir);
                run.stderr_path = replace_prefix(&run.stderr_path, &old_dir, &new_dir);
            }
            state.runs.insert(new.to_string(), runs);
        }
        Ok(())
    })
}

fn tick(root: &Path, epoch_minute: i64, state: &Arc<DaemonState>) {
    let projects = root.join("projects");
    let Ok(entries) = fs::read_dir(projects) else {
        return;
    };
    let local = local_time(epoch_minute * 60);
    for entry in entries.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(config) = toml::from_str::<ProjectRoutines>(&text) else {
            continue;
        };
        let Ok(store) = RoutineStore::new(root.to_path_buf(), &config.project_path) else {
            continue;
        };
        for routine in &config.routines {
            let Ok(schedule) = CronSchedule::parse(&routine.cron) else {
                continue;
            };
            if schedule.matches(local)
                && !is_active(state, store.project(), &routine.name)
                && store.claim(&routine.name, epoch_minute).unwrap_or(false)
            {
                let _ = spawn_run(store.clone(), routine.clone(), Some(epoch_minute), state);
            }
        }
    }
}

fn spawn_run(
    store: RoutineStore,
    routine: super::Routine,
    scheduled: Option<i64>,
    state: &Arc<DaemonState>,
) -> Result<(), RoutineError> {
    let key = RunKey {
        project: store.project().to_path_buf(),
        routine: routine.name.clone(),
    };
    {
        let mut active = state
            .active
            .lock()
            .map_err(|_| RoutineError::Io("active-run lock poisoned".into()))?;
        if active.insert(key.clone(), false).is_some() {
            return Err(RoutineError::AlreadyRunning(routine.name));
        }
    }
    let shared = state.clone();
    let handle = std::thread::spawn(move || {
        let callback_state = shared.clone();
        let callback_key = key.clone();
        let callback_store = store.clone();
        let callback_name = routine.name.clone();
        let result = execute_supervised(&store, &routine, scheduled, move |_| {
            let cancellation_requested = callback_state
                .active
                .lock()
                .ok()
                .and_then(|active| active.get(&callback_key).copied())
                .unwrap_or(true);
            if cancellation_requested {
                let _ = cancel_running(&callback_store, &callback_name);
            }
        });
        if result.is_err() {
            let _ = interrupt_latest_running(&store, &routine.name);
        }
        if let Ok(mut active) = shared.active.lock() {
            active.remove(&key);
        }
    });
    state
        .workers
        .lock()
        .map_err(|_| RoutineError::Io("worker lock poisoned".into()))?
        .push(handle);
    Ok(())
}

fn interrupt_latest_running(store: &RoutineStore, name: &str) -> Result<(), RoutineError> {
    store.modify_runtime(|state| {
        if let Some(run) = state.runs.get_mut(name).and_then(|runs| runs.last_mut()) {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Interrupted;
                run.finished_epoch = Some(now_epoch());
                run.pid = None;
            }
        }
        Ok(())
    })
}

fn stop_active(root: &Path, state: &DaemonState) {
    let keys = state
        .active
        .lock()
        .map(|mut active| {
            for requested in active.values_mut() {
                *requested = true;
            }
            active.keys().cloned().collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for key in keys {
        if let Ok(store) = RoutineStore::new(root.to_path_buf(), &key.project) {
            let _ = cancel_running(&store, &key.routine);
        }
    }
}

fn drain_workers(state: &DaemonState) {
    let workers = state
        .workers
        .lock()
        .map(|mut workers| std::mem::take(&mut *workers))
        .unwrap_or_default();
    for worker in workers {
        let _ = worker.join();
    }
}

fn reap_workers(state: &DaemonState) {
    let Ok(mut workers) = state.workers.lock() else {
        return;
    };
    let mut index = workers.len();
    while index > 0 {
        index -= 1;
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            let _ = worker.join();
        }
    }
}

fn cancel_running(store: &RoutineStore, name: &str) -> Result<(), RoutineError> {
    let pid = store.modify_runtime(|state| {
        let latest = state.runs.get_mut(name).and_then(|runs| runs.last_mut());
        if let Some(run) = latest.filter(|run| run.status == RunStatus::Running) {
            run.status = RunStatus::Cancelled;
            Ok(run.pid)
        } else {
            Ok(None)
        }
    })?;
    let Some(pid) = pid else { return Ok(()) };
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    Ok(())
}

fn reconcile_running(root: &Path) {
    let Ok(entries) = fs::read_dir(root.join("projects")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(config) = toml::from_str::<ProjectRoutines>(&text) else {
            continue;
        };
        let Ok(store) = RoutineStore::new(root.to_path_buf(), &config.project_path) else {
            continue;
        };
        let _ = store.modify_runtime(|state| {
            for runs in state.runs.values_mut() {
                for run in runs
                    .iter_mut()
                    .filter(|run| run.status == RunStatus::Running)
                {
                    if let Some(pid) = run.pid {
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        }
                    }
                    run.status = RunStatus::Interrupted;
                    run.finished_epoch = Some(now_epoch());
                    run.pid = None;
                }
            }
            Ok(())
        });
    }
}

fn hex_name(name: &str) -> String {
    name.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn replace_prefix(path: &Path, old: &Path, new: &Path) -> PathBuf {
    path.strip_prefix(old)
        .map(|suffix| new.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn local_time(epoch: i64) -> LocalTime {
    let timestamp = epoch as libc::time_t;
    let mut out = std::mem::MaybeUninit::<libc::tm>::uninit();
    unsafe {
        libc::localtime_r(&timestamp, out.as_mut_ptr());
    }
    let out = unsafe { out.assume_init() };
    LocalTime {
        minute: out.tm_min as u8,
        hour: out.tm_hour as u8,
        day_of_month: out.tm_mday as u8,
        month: (out.tm_mon + 1) as u8,
        day_of_week: out.tm_wday as u8,
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::{Routine, SCHEMA_VERSION};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, PathBuf, Arc<DaemonState>) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/routine-daemon-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let project =
            fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        store
            .save(
                ProjectRoutines {
                    version: SCHEMA_VERSION,
                    revision: 0,
                    project_path: project.clone(),
                    routines: vec![slow("one"), slow("two")],
                },
                0,
            )
            .unwrap();
        (root, project, Arc::new(DaemonState::default()))
    }

    fn slow(name: &str) -> Routine {
        Routine {
            name: name.into(),
            cron: "0 0 1 1 *".into(),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            prompt: String::new(),
        }
    }

    fn request(project: &Path, action: Action) -> Request {
        Request::new(project.to_path_buf(), action)
    }

    #[test]
    fn slow_run_keeps_status_responsive_rejects_overlap_and_allows_other_routine() {
        let (root, project, state) = fixture();
        let started = Instant::now();
        assert!(matches!(
            process(
                &root,
                request(&project, Action::Run { name: "one".into() }),
                &state
            )
            .unwrap(),
            Response::Ok { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            process(&root, request(&project, Action::Status), &state).unwrap(),
            Response::Daemon { .. }
        ));
        assert!(matches!(
            process(
                &root,
                request(&project, Action::Run { name: "one".into() }),
                &state
            ),
            Err(RoutineError::AlreadyRunning(_))
        ));
        assert!(process(
            &root,
            request(&project, Action::Run { name: "two".into() }),
            &state
        )
        .is_ok());
        assert!(process(
            &root,
            request(&project, Action::Cancel { name: "one".into() }),
            &state
        )
        .is_ok());
        assert!(process(
            &root,
            request(&project, Action::Cancel { name: "two".into() }),
            &state
        )
        .is_ok());
        drain_workers(&state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn confirmed_delete_cancels_active_run_and_removes_definition() {
        let (root, project, state) = fixture();
        process(
            &root,
            request(&project, Action::Run { name: "one".into() }),
            &state,
        )
        .unwrap();
        assert!(process(
            &root,
            request(
                &project,
                Action::Delete {
                    revision: 1,
                    name: "one".into()
                }
            ),
            &state
        )
        .is_ok());
        drain_workers(&state);
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        assert!(!store
            .load()
            .unwrap()
            .routines
            .iter()
            .any(|routine| routine.name == "one"));
        let _ = fs::remove_dir_all(root);
    }
}
