use super::execution::{execute_supervised, process_start};
use super::ipc::{Action, Request, Response, RoutineView};
use super::store::{atomic_toml, ProjectRoutines, RoutineStore};
use super::{
    Capabilities, CronSchedule, LocalTime, RoutineError, RunRecord, RunStatus, MAX_RUNS,
    PROTOCOL_VERSION, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
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
    serve_with_startup(root, |_| {})
}

/// Serve while reporting the one-shot startup result after the socket is bound.
pub fn serve_with_startup(
    root: PathBuf,
    notify: impl FnOnce(Result<(), String>),
) -> Result<(), RoutineError> {
    let setup = setup(&root);
    let (lock, socket, listener) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            notify(Err(error.to_string()));
            return Err(error);
        }
    };
    notify(Ok(()));
    let result = event_loop(&root, &listener);
    drop(lock);
    reconcile_running(&root);
    let _ = fs::remove_file(socket);
    result
}

fn setup(root: &Path) -> Result<(std::fs::File, PathBuf, UnixListener), RoutineError> {
    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
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
    recover_rename_transactions(root)?;
    let socket = root.join("daemon-v1.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    reconcile_running(root);
    Ok((lock, socket, listener))
}

fn event_loop(root: &Path, listener: &UnixListener) -> Result<(), RoutineError> {
    let mut last_minute = i64::MIN;
    let stopping = Arc::new(AtomicBool::new(false));
    let state = Arc::new(DaemonState::default());
    let mut handlers = Vec::new();
    while !stopping.load(Ordering::SeqCst) {
        reap_workers(&state);
        reap_handles(&mut handlers);
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.to_path_buf();
                let stopping = stopping.clone();
                let state = state.clone();
                handlers.push(std::thread::spawn(move || {
                    let _ = handle_stream(&root, stream, &stopping, &state);
                }));
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
    drain_handles(handlers);
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
    admission: Mutex<()>,
}

fn handle_stream(
    root: &Path,
    mut stream: UnixStream,
    stopping: &AtomicBool,
    state: &Arc<DaemonState>,
) -> Result<(), RoutineError> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request = serde_json::from_str::<Request>(&line)
        .map_err(|e| RoutineError::Validation(format!("invalid request: {e}")));
    let response = match request {
        Ok(request) => dispatch(root, request, stopping, state).unwrap_or_else(Response::error),
        Err(error) => Response::error(error),
    };
    let mut bytes =
        serde_json::to_vec(&response).map_err(|e| RoutineError::Corrupt(e.to_string()))?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}

fn dispatch(
    root: &Path,
    request: Request,
    stopping: &AtomicBool,
    state: &Arc<DaemonState>,
) -> Result<Response, RoutineError> {
    let _admission = state
        .admission
        .lock()
        .map_err(|_| RoutineError::Io("request admission lock poisoned".into()))?;
    if stopping.load(Ordering::SeqCst) {
        return Err(RoutineError::Unavailable(
            "routine daemon is shutting down".into(),
        ));
    }
    if request.protocol != PROTOCOL_VERSION {
        return Err(RoutineError::ProtocolMismatch {
            client: request.protocol,
            daemon: PROTOCOL_VERSION,
        });
    }
    if matches!(request.action, Action::Shutdown) {
        stopping.store(true, Ordering::SeqCst);
        return Ok(Response::Ok { revision: None });
    }
    process(root, request, state)
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
    recover_rename_transactions(root)?;
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
            validate_revision(config.revision, revision)?;
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
            let saved = if routine.name == old_name {
                store.save(config, revision)?
            } else {
                rename_routine(root, &store, &old_name, &routine.name, config, revision)?
            };
            Ok(Response::Ok {
                revision: Some(saved.revision),
            })
        }
        Action::Delete { revision, name } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let mut config = store.load()?;
            validate_revision(config.revision, revision)?;
            let before = config.routines.len();
            config.routines.retain(|r| r.name != name);
            if config.routines.len() == before {
                return Err(RoutineError::NotFound(name));
            }
            let saved = store.save(config, revision)?;
            // ^ Persist the requested mutation before it can affect a running process.
            request_cancel(state, store.project(), &name);
            // ^ Configuration is already committed. Cancellation remains
            // best-effort and the worker observes the in-memory request too.
            let _ = cancel_running(&store, &name);
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
    let next_run_epoch = CronSchedule::parse(&routine.cron)
        .ok()
        .and_then(|schedule| schedule.next_run_after(now_epoch()));
    RoutineView {
        routine: routine.clone(),
        capabilities: Capabilities::for_running(running),
        next_run_epoch,
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

fn validate_revision(actual: u64, expected: u64) -> Result<(), RoutineError> {
    if actual != expected {
        return Err(RoutineError::Conflict { expected, actual });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenameTransaction {
    version: u32,
    project_path: PathBuf,
    old_name: String,
    new_name: String,
    expected_revision: u64,
    config: ProjectRoutines,
}

fn rename_routine(
    root: &Path,
    store: &RoutineStore,
    old: &str,
    new: &str,
    config: ProjectRoutines,
    expected_revision: u64,
) -> Result<ProjectRoutines, RoutineError> {
    let transaction = RenameTransaction {
        version: SCHEMA_VERSION,
        project_path: store.project().to_path_buf(),
        old_name: old.to_string(),
        new_name: new.to_string(),
        expected_revision,
        config,
    };
    let path = rename_transaction_path(root, store.key());
    atomic_toml(&path, &transaction)?;
    apply_rename_transaction(root, store, &transaction)
}

fn apply_rename_transaction(
    root: &Path,
    store: &RoutineStore,
    transaction: &RenameTransaction,
) -> Result<ProjectRoutines, RoutineError> {
    let old = &transaction.old_name;
    let new = &transaction.new_name;
    if old == new
        || transaction.config.revision != transaction.expected_revision
        || transaction
            .config
            .routines
            .iter()
            .filter(|routine| routine.name == *new)
            .count()
            != 1
        || transaction
            .config
            .routines
            .iter()
            .any(|routine| routine.name == *old)
    {
        return Err(RoutineError::Corrupt(
            "invalid rename transaction boundaries".into(),
        ));
    }
    store.with_runtime_lock(|| {
        let old_dir = store.logs_dir().join(hex_name(old));
        let new_dir = store.logs_dir().join(hex_name(new));
        let mut after = store.load_runtime()?;
        if let Some(mut runs) = after.runs.remove(old) {
            if after.runs.contains_key(new) {
                return Err(RoutineError::Corrupt(format!(
                    "rename state contains both '{old}' and '{new}'"
                )));
            }
            for run in &mut runs {
                run.routine = new.to_string();
                run.stdout_path = replace_prefix(&run.stdout_path, &old_dir, &new_dir);
                run.stderr_path = replace_prefix(&run.stderr_path, &old_dir, &new_dir);
            }
            after.runs.insert(new.to_string(), runs);
        }
        if let Some(claim) = after.claims.remove(old) {
            if after.claims.contains_key(new) {
                return Err(RoutineError::Corrupt(format!(
                    "rename claims contain both '{old}' and '{new}'"
                )));
            }
            after.claims.insert(new.to_string(), claim);
        }
        if old_dir.exists() {
            if new_dir.exists() {
                return Err(RoutineError::Duplicate(new.to_string()));
            }
            fs::rename(&old_dir, &new_dir)?;
            std::fs::File::open(store.logs_dir())?.sync_all()?;
        }
        store.save_runtime(&after)
    })?;

    let current = store.load()?;
    let saved = if current.revision == transaction.expected_revision {
        store.save(transaction.config.clone(), transaction.expected_revision)?
    } else if current.revision
        == transaction
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| RoutineError::Corrupt("revision overflow".into()))?
    {
        let mut expected = transaction.config.clone();
        expected.version = SCHEMA_VERSION;
        expected.project_path = store.project().to_path_buf();
        expected.revision = current.revision;
        if current != expected {
            return Err(RoutineError::Corrupt(
                "rename transaction conflicts with committed config".into(),
            ));
        }
        current
    } else {
        return Err(RoutineError::Conflict {
            expected: transaction.expected_revision,
            actual: current.revision,
        });
    };
    remove_durable(&rename_transaction_path(root, store.key()))?;
    Ok(saved)
}

fn rename_transaction_path(root: &Path, key: &str) -> PathBuf {
    root.join("transactions").join(format!("{key}.toml"))
}

fn remove_durable(path: &Path) -> Result<(), RoutineError> {
    if path.exists() {
        fs::remove_file(path)?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

fn recover_rename_transactions(root: &Path) -> Result<(), RoutineError> {
    let dir = root.join("transactions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let text = fs::read_to_string(entry.path())?;
        let transaction: RenameTransaction = toml::from_str(&text).map_err(|error| {
            RoutineError::Corrupt(format!("{}: {error}", entry.path().display()))
        })?;
        if transaction.version != SCHEMA_VERSION {
            return Err(RoutineError::Corrupt(
                "unsupported rename transaction schema".into(),
            ));
        }
        let store = RoutineStore::new(root.to_path_buf(), &transaction.project_path)?;
        if entry.path() != rename_transaction_path(root, store.key()) {
            return Err(RoutineError::ProjectCollision {
                expected: rename_transaction_path(root, store.key()),
                stored: entry.path(),
            });
        }
        apply_rename_transaction(root, &store, &transaction)?;
    }
    Ok(())
}

fn tick(root: &Path, epoch_minute: i64, state: &Arc<DaemonState>) {
    // ^ CRUD and scheduling share this boundary so a loaded definition cannot
    // be deleted or renamed before its claim and active registration complete.
    let Ok(_guard) = state.config.lock() else {
        return;
    };
    if recover_rename_transactions(root).is_err() {
        return;
    }
    let projects = root.join("projects");
    let Ok(entries) = fs::read_dir(projects) else {
        return;
    };
    let local = local_time(epoch_minute * 60);
    for entry in entries.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(discovered) = toml::from_str::<ProjectRoutines>(&text) else {
            continue;
        };
        let Ok(store) = RoutineStore::new(root.to_path_buf(), &discovered.project_path) else {
            continue;
        };
        if entry.path() != store.project_file() {
            continue;
        }
        let Ok(config) = store.load() else {
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

fn reap_handles(handles: &mut Vec<JoinHandle<()>>) {
    let mut index = handles.len();
    while index > 0 {
        index -= 1;
        if handles[index].is_finished() {
            let handle = handles.swap_remove(index);
            let _ = handle.join();
        }
    }
}

fn drain_handles(handles: Vec<JoinHandle<()>>) {
    for handle in handles {
        let _ = handle.join();
    }
}

fn cancel_running(store: &RoutineStore, name: &str) -> Result<(), RoutineError> {
    let process = store.modify_runtime(|state| {
        let latest = state.runs.get_mut(name).and_then(|runs| runs.last_mut());
        if let Some(run) = latest.filter(|run| run.status == RunStatus::Running) {
            run.status = RunStatus::Cancelled;
            Ok(run.pid.zip(run.process_start.clone()))
        } else {
            Ok(None)
        }
    })?;
    let Some((pid, start)) = process else {
        return Ok(());
    };
    if !original_process_group_exists(pid, &start) {
        return Ok(());
    }
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !process_group_exists(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    for _ in 0..20 {
        if !process_group_exists(pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn process_group_exists(pgid: i32) -> bool {
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn original_process_group_exists(pgid: i32, leader_start: &str) -> bool {
    match process_start(pgid) {
        Some(current) => current == leader_start,
        None => process_group_exists(pgid),
    }
}

fn reconcile_running(root: &Path) {
    let Ok(entries) = fs::read_dir(root.join("projects")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(discovered) = toml::from_str::<ProjectRoutines>(&text) else {
            continue;
        };
        let Ok(store) = RoutineStore::new(root.to_path_buf(), &discovered.project_path) else {
            continue;
        };
        if entry.path() != store.project_file() || store.load().is_err() {
            continue;
        }
        let removed = store.modify_runtime(|state| {
            let mut removed = Vec::<RunRecord>::new();
            for runs in state.runs.values_mut() {
                for run in runs
                    .iter_mut()
                    .filter(|run| run.status == RunStatus::Running)
                {
                    if let Some((pid, start)) = run.pid.zip(run.process_start.as_deref()) {
                        if original_process_group_exists(pid, start) {
                            unsafe {
                                libc::kill(-pid, libc::SIGKILL);
                            }
                        }
                    }
                    run.status = RunStatus::Interrupted;
                    run.finished_epoch = Some(now_epoch());
                    run.pid = None;
                    run.process_start = None;
                }
                if runs.len() > MAX_RUNS {
                    removed.extend(runs.drain(0..runs.len() - MAX_RUNS));
                }
            }
            Ok(removed)
        });
        if let Ok(removed) = removed {
            for run in removed {
                if let Some(dir) = run.stdout_path.parent() {
                    let logs = store.logs_dir();
                    if dir.starts_with(&logs) && dir != logs {
                        let _ = fs::remove_dir_all(dir);
                    }
                }
            }
        }
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
    use crate::routine::{Routine, RunRecord, SCHEMA_VERSION};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
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

    #[test]
    fn stale_delete_preserves_active_run_and_definition() {
        let (root, project, state) = fixture();
        state.active.lock().unwrap().insert(
            RunKey {
                project: project.clone(),
                routine: "one".into(),
            },
            false,
        );
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        store
            .modify_runtime(|runtime| {
                runtime.runs.insert(
                    "one".into(),
                    vec![RunRecord {
                        id: "active".into(),
                        routine: "one".into(),
                        started_epoch: 1,
                        finished_epoch: None,
                        scheduled_epoch_minute: None,
                        status: RunStatus::Running,
                        exit_code: None,
                        pid: None,
                        process_start: None,
                        final_output: String::new(),
                        stdout_path: root.join("stdout"),
                        stderr_path: root.join("stderr"),
                    }],
                );
                Ok(())
            })
            .unwrap();
        let result = process(
            &root,
            request(
                &project,
                Action::Delete {
                    revision: 0,
                    name: "one".into(),
                },
            ),
            &state,
        );
        assert!(matches!(result, Err(RoutineError::Conflict { .. })));
        assert!(is_active(&state, &project, "one"));
        assert!(store
            .load()
            .unwrap()
            .routines
            .iter()
            .any(|r| r.name == "one"));
        assert_eq!(
            store.load_runtime().unwrap().runs["one"]
                .last()
                .unwrap()
                .status,
            RunStatus::Running
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn config_save_failure_does_not_request_cancel() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut config = store.load().unwrap();
        let max_toml_integer = i64::MAX as u64;
        config.revision = max_toml_integer;
        fs::write(store.project_file(), toml::to_string(&config).unwrap()).unwrap();
        state.active.lock().unwrap().insert(
            RunKey {
                project: store.project().to_path_buf(),
                routine: "one".into(),
            },
            false,
        );

        let result = process(
            &root,
            request(
                &project,
                Action::Delete {
                    revision: max_toml_integer,
                    name: "one".into(),
                },
            ),
            &state,
        );

        assert!(matches!(result, Err(RoutineError::Corrupt(_))));
        let key = RunKey {
            project: store.project().to_path_buf(),
            routine: "one".into(),
        };
        assert_eq!(state.active.lock().unwrap().get(&key), Some(&false));
        assert!(store
            .load()
            .unwrap()
            .routines
            .iter()
            .any(|r| r.name == "one"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_reports_committed_revision_when_cancellation_cleanup_fails() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        fs::create_dir_all(store.runtime_file()).unwrap();
        state.active.lock().unwrap().insert(
            RunKey {
                project: store.project().to_path_buf(),
                routine: "one".into(),
            },
            false,
        );

        let response = process(
            &root,
            request(
                &project,
                Action::Delete {
                    revision: 1,
                    name: "one".into(),
                },
            ),
            &state,
        )
        .unwrap();

        assert!(matches!(response, Response::Ok { revision: Some(2) }));
        assert!(!store
            .load()
            .unwrap()
            .routines
            .iter()
            .any(|routine| routine.name == "one"));
        let key = RunKey {
            project: store.project().to_path_buf(),
            routine: "one".into(),
        };
        assert_eq!(state.active.lock().unwrap().get(&key), Some(&true));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scheduler_reloads_config_after_waiting_for_crud_lock() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut config = store.load().unwrap();
        config.routines[0].cron = "* * * * *".into();
        config.routines.truncate(1);
        let config = store.save(config, 1).unwrap();
        let guard = state.config.lock().unwrap();
        let tick_root = root.clone();
        let tick_state = state.clone();
        let worker = std::thread::spawn(move || tick(&tick_root, now_epoch() / 60, &tick_state));
        std::thread::sleep(Duration::from_millis(100));
        let mut deleted = config;
        deleted.routines.clear();
        store.save(deleted, 2).unwrap();
        drop(guard);
        worker.join().unwrap();

        assert!(!is_active(&state, &project, "one"));
        assert!(!store.load_runtime().unwrap().claims.contains_key("one"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scheduler_rejects_unvalidated_project_file() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut config = store.load().unwrap();
        config.routines.truncate(1);
        config.routines[0].cron = "* * * * *".into();
        config.routines[0].command.clear();
        fs::write(store.project_file(), toml::to_string(&config).unwrap()).unwrap();

        tick(&root, now_epoch() / 60, &state);

        assert!(!is_active(&state, &project, "one"));
        assert!(!store.load_runtime().unwrap().claims.contains_key("one"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_recovers_rename_after_runtime_and_logs_were_migrated() {
        let (root, project, _state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let old_dir = store.logs_dir().join(hex_name("one"));
        fs::create_dir_all(&old_dir).unwrap();
        let stdout = old_dir.join("run/stdout.log");
        let stderr = old_dir.join("run/stderr.log");
        store
            .modify_runtime(|runtime| {
                runtime.claims.insert("one".into(), 123);
                runtime.runs.insert(
                    "one".into(),
                    vec![RunRecord {
                        id: "run".into(),
                        routine: "one".into(),
                        started_epoch: 1,
                        finished_epoch: Some(2),
                        scheduled_epoch_minute: Some(123),
                        status: RunStatus::Succeeded,
                        exit_code: Some(0),
                        pid: None,
                        process_start: None,
                        final_output: "done".into(),
                        stdout_path: stdout.clone(),
                        stderr_path: stderr.clone(),
                    }],
                );
                Ok(())
            })
            .unwrap();
        let mut config = store.load().unwrap();
        config.routines[0].name = "renamed".into();
        let transaction = RenameTransaction {
            version: SCHEMA_VERSION,
            project_path: project.clone(),
            old_name: "one".into(),
            new_name: "renamed".into(),
            expected_revision: config.revision,
            config,
        };
        atomic_toml(&rename_transaction_path(&root, store.key()), &transaction).unwrap();
        fs::rename(&old_dir, store.logs_dir().join(hex_name("renamed"))).unwrap();
        store
            .modify_runtime(|runtime| {
                let mut runs = runtime.runs.remove("one").unwrap();
                for run in &mut runs {
                    run.routine = "renamed".into();
                    run.stdout_path = replace_prefix(
                        &run.stdout_path,
                        &old_dir,
                        &store.logs_dir().join(hex_name("renamed")),
                    );
                    run.stderr_path = replace_prefix(
                        &run.stderr_path,
                        &old_dir,
                        &store.logs_dir().join(hex_name("renamed")),
                    );
                }
                runtime.runs.insert("renamed".into(), runs);
                let claim = runtime.claims.remove("one").unwrap();
                runtime.claims.insert("renamed".into(), claim);
                Ok(())
            })
            .unwrap();

        recover_rename_transactions(&root).unwrap();

        let runtime = store.load_runtime().unwrap();
        assert_eq!(runtime.claims.get("renamed"), Some(&123));
        assert!(!runtime.claims.contains_key("one"));
        assert_eq!(runtime.runs["renamed"][0].routine, "renamed");
        assert!(runtime.runs["renamed"][0]
            .stdout_path
            .starts_with(store.logs_dir().join(hex_name("renamed"))));
        assert_eq!(store.load().unwrap().routines[0].name, "renamed");
        assert!(!rename_transaction_path(&root, store.key()).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancel_kills_term_ignoring_descendant_after_leader_exits() {
        let (root, project, _state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let child_pid_file = root.join("descendant.pid");
        fs::create_dir_all(&root).unwrap();
        let script = "trap 'exit 0' TERM; sh -c 'trap \"\" TERM; while :; do sleep 1; done' & echo $! > \"$1\"; wait";
        let mut leader = Command::new("/bin/sh");
        leader
            .args([
                "-c",
                script,
                "cancel-test",
                child_pid_file.to_str().unwrap(),
            ])
            .process_group(0);
        let mut leader = leader.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let descendant = loop {
            if let Ok(pid) = fs::read_to_string(&child_pid_file) {
                if let Ok(pid) = pid.trim().parse::<i32>() {
                    break pid;
                }
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(-(leader.id() as i32), libc::SIGKILL);
                }
                let _ = leader.wait();
                panic!("descendant pid was not published");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        store
            .modify_runtime(|runtime| {
                runtime.runs.insert(
                    "one".into(),
                    vec![RunRecord {
                        id: "active".into(),
                        routine: "one".into(),
                        started_epoch: 1,
                        finished_epoch: None,
                        scheduled_epoch_minute: None,
                        status: RunStatus::Running,
                        exit_code: None,
                        pid: Some(leader.id() as i32),
                        process_start: process_start(leader.id() as i32),
                        final_output: String::new(),
                        stdout_path: root.join("stdout"),
                        stderr_path: root.join("stderr"),
                    }],
                );
                Ok(())
            })
            .unwrap();
        unsafe {
            libc::kill(leader.id() as i32, libc::SIGTERM);
        }
        let _ = leader.wait();
        assert_eq!(process_start(leader.id() as i32), None);
        cancel_running(&store, "one").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(unsafe { libc::kill(descendant, 0) }, 0);
        assert!(!process_group_exists(leader.id() as i32));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_reconciliation_does_not_signal_reused_process_group() {
        let (root, project, _state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut child = Command::new("/bin/sh");
        child.args(["-c", "sleep 30"]).process_group(0);
        let mut child = child.spawn().unwrap();
        let pid = child.id() as i32;
        store
            .modify_runtime(|runtime| {
                runtime.runs.insert(
                    "one".into(),
                    vec![RunRecord {
                        id: "stale".into(),
                        routine: "one".into(),
                        started_epoch: 1,
                        finished_epoch: None,
                        scheduled_epoch_minute: None,
                        status: RunStatus::Running,
                        exit_code: None,
                        pid: Some(pid),
                        process_start: Some("different-process".into()),
                        final_output: String::new(),
                        stdout_path: root.join("stdout"),
                        stderr_path: root.join("stderr"),
                    }],
                );
                Ok(())
            })
            .unwrap();

        reconcile_running(&root);

        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        let record = &store.load_runtime().unwrap().runs["one"][0];
        assert_eq!(record.status, RunStatus::Interrupted);
        assert!(record.pid.is_none());
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_reconciliation_prunes_only_after_persisting_latest_runs() {
        let (root, project, _state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let routine_dir = store.logs_dir().join(hex_name("one"));
        let mut records = Vec::new();
        for index in 0..(MAX_RUNS + 2) {
            let run_dir = routine_dir.join(format!("run-{index}"));
            fs::create_dir_all(&run_dir).unwrap();
            records.push(RunRecord {
                id: format!("run-{index}"),
                routine: "one".into(),
                started_epoch: index as i64,
                finished_epoch: Some(index as i64),
                scheduled_epoch_minute: None,
                status: RunStatus::Succeeded,
                exit_code: Some(0),
                pid: None,
                process_start: None,
                final_output: String::new(),
                stdout_path: run_dir.join("stdout.log"),
                stderr_path: run_dir.join("stderr.log"),
            });
        }
        store
            .modify_runtime(|runtime| {
                runtime.runs.insert("one".into(), records);
                Ok(())
            })
            .unwrap();

        reconcile_running(&root);

        let retained = &store.load_runtime().unwrap().runs["one"];
        assert_eq!(retained.len(), MAX_RUNS);
        assert_eq!(retained.first().unwrap().id, "run-2");
        assert!(!routine_dir.join("run-0").exists());
        assert!(!routine_dir.join("run-1").exists());
        assert!(routine_dir.join("run-2").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn socket_crud_run_logs_and_shutdown_complete_end_to_end() {
        let project =
            fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let root = PathBuf::from(".tmp").join(format!(
            "routine-daemon-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let server_root = root.clone();
        let server = std::thread::spawn(move || {
            serve_with_startup(server_root, move |result| {
                started_tx.send(result).unwrap();
            })
        });
        started_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let socket = store.socket_path();
        let revision = store.load().unwrap().revision;
        let response = super::super::ipc::send(
            &socket,
            &request(
                &project,
                Action::Add {
                    revision,
                    routine: Routine {
                        name: "quick".into(),
                        cron: "0 0 1 1 *".into(),
                        command: vec!["/bin/echo".into(), "done".into()],
                        prompt: String::new(),
                    },
                },
            ),
        )
        .unwrap();
        assert!(matches!(
            response,
            Response::Ok {
                revision: Some(saved_revision)
            } if saved_revision == revision + 1
        ));
        super::super::ipc::send(
            &socket,
            &request(
                &project,
                Action::Run {
                    name: "quick".into(),
                },
            ),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let response = super::super::ipc::send(
                &socket,
                &request(
                    &project,
                    Action::Logs {
                        name: "quick".into(),
                    },
                ),
            )
            .unwrap();
            if let Response::Runs { runs } = response {
                if let Some(run) = runs.last() {
                    if run.status == RunStatus::Succeeded {
                        assert_eq!(run.final_output, "done");
                        break;
                    }
                }
            }
            assert!(Instant::now() < deadline, "routine did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        super::super::ipc::send(&socket, &request(&project, Action::Shutdown)).unwrap();
        server.join().unwrap().unwrap();
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shutdown_is_terminal_for_later_mutation_handlers() {
        let (root, project, state) = fixture();
        let stopping = AtomicBool::new(false);
        let response = dispatch(
            &root,
            request(&project, Action::Shutdown),
            &stopping,
            &state,
        )
        .unwrap();
        assert!(matches!(response, Response::Ok { revision: None }));

        let result = dispatch(
            &root,
            request(
                &project,
                Action::Add {
                    revision: 1,
                    routine: slow("late"),
                },
            ),
            &stopping,
            &state,
        );
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        assert!(!store
            .load()
            .unwrap()
            .routines
            .iter()
            .any(|routine| routine.name == "late"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shutdown_rejects_protocol_mismatch_without_stopping() {
        let (root, project, state) = fixture();
        let stopping = AtomicBool::new(false);
        let mut shutdown = request(&project, Action::Shutdown);
        shutdown.protocol = PROTOCOL_VERSION + 1;

        let result = dispatch(&root, shutdown, &stopping, &state);

        assert!(matches!(result, Err(RoutineError::ProtocolMismatch { .. })));
        assert!(!stopping.load(Ordering::SeqCst));
        let _ = fs::remove_dir_all(root);
    }
}
