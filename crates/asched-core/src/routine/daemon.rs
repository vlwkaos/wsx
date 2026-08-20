use super::execution::{
    execute_prepared_supervised, execute_supervised, prepare_run_record, process_start,
    prune_run_logs,
};
use super::ipc::{Action, Request, Response, RoutineView};
use super::store::{atomic_toml, read_text_limited, ProjectRoutines, RoutineStore, RuntimeState};
use super::{
    Capabilities, CronSchedule, FireOutcome, LocalTime, RoutineError, RoutineFire, RunCause,
    RunRecord, RunStatus, Trigger, MAX_EVENT_PAYLOAD_BYTES, MAX_RUNS, PROTOCOL_VERSION,
    SCHEMA_VERSION, TRANSACTION_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_REQUEST_FRAME_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_HANDLERS: usize = 64;

#[cfg(test)]
type TickAdmissionHook = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
static TICK_ADMISSION_HOOK: OnceLock<Mutex<Option<TickAdmissionHook>>> = OnceLock::new();

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
    reconcile_running(&root);
    let _ = fs::remove_file(socket);
    drop(lock);
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
    crate::migration::ensure_no_pending_wsx_import(root)
        .map_err(|error| RoutineError::Unavailable(error.to_string()))?;
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
    event_loop_with(
        root,
        listener,
        Arc::new(DaemonState::default()),
        Arc::new(AtomicBool::new(false)),
        wait_for_request,
        handle_stream,
        |task| std::thread::Builder::new().spawn(task),
    )
}

fn event_loop_with<W, H, S>(
    root: &Path,
    listener: &UnixListener,
    state: Arc<DaemonState>,
    stopping: Arc<AtomicBool>,
    mut wait: W,
    handle_request: H,
    mut spawn_handler: S,
) -> Result<(), RoutineError>
where
    W: FnMut(&UnixListener, Duration) -> Result<(), RoutineError>,
    H: Fn(&Path, UnixStream, &AtomicBool, &Arc<DaemonState>) -> Result<(), RoutineError>
        + Clone
        + Send
        + 'static,
    S: FnMut(Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<JoinHandle<()>>,
{
    let mut last_minute = i64::MIN;
    let mut handlers = Vec::new();
    let result = loop {
        if stopping.load(Ordering::SeqCst) {
            break Ok(());
        }
        reap_workers(&state);
        reap_handles(&mut handlers);
        if handlers.len() < MAX_REQUEST_HANDLERS {
            match listener.accept() {
                Ok((stream, _)) => {
                    let root = root.to_path_buf();
                    let stopping = stopping.clone();
                    let state = state.clone();
                    let handle_request = handle_request.clone();
                    let task: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
                        let _ = handle_request(&root, stream, &stopping, &state);
                    });
                    match spawn_handler(task) {
                        Ok(handler) => handlers.push(handler),
                        Err(error) => break Err(error.into()),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => break Err(error.into()),
            }
        }
        let minute = now_epoch() / 60;
        if minute != last_minute {
            last_minute = minute;
            tick(root, minute, &state);
        }
        if handlers.len() == MAX_REQUEST_HANDLERS {
            std::thread::sleep(Duration::from_millis(100));
        } else if let Err(error) = wait(listener, Duration::from_millis(100)) {
            break Err(error);
        }
    };
    if result.is_err() {
        stopping.store(true, Ordering::SeqCst);
    }
    drain_handles(handlers);
    stop_active(root, &state);
    drain_workers(&state);
    result
}

fn wait_for_request(listener: &UnixListener, timeout: Duration) -> Result<(), RoutineError> {
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
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
    // ^ A nonblocking listener can yield nonblocking streams; framed reads need timeout blocking.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(handler_io_timeout()))?;
    stream.set_write_timeout(Some(handler_io_timeout()))?;
    let frame = read_request_frame(BufReader::new(stream.try_clone()?));
    let request = frame.and_then(|frame| {
        serde_json::from_slice::<Request>(&frame)
            .map_err(|e| RoutineError::Validation(format!("invalid request: {e}")))
    });
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

fn read_request_frame(reader: impl BufRead) -> Result<Vec<u8>, RoutineError> {
    let mut frame = Vec::new();
    reader
        .take((MAX_REQUEST_FRAME_BYTES + 2) as u64)
        .read_until(b'\n', &mut frame)?;
    if !frame.ends_with(b"\n") {
        return Err(RoutineError::Validation(
            "request frame must end with a newline".into(),
        ));
    }
    if frame.len() > MAX_REQUEST_FRAME_BYTES + 1 {
        return Err(RoutineError::Validation(format!(
            "request frame exceeds {MAX_REQUEST_FRAME_BYTES} bytes"
        )));
    }
    frame.pop();
    Ok(frame)
}

fn handler_io_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(5)
    }
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
    let registered = scheduled_project_paths(root)
        .map_err(|error| RoutineError::Unavailable(error.to_string()))?;
    if registered
        .as_ref()
        .is_some_and(|paths| !paths.contains(store.project()))
    {
        return Err(RoutineError::Validation(format!(
            "project is not registered: {}",
            store.project().display()
        )));
    }
    match request.action {
        Action::List => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let config = store.load()?;
            Ok(Response::Routines {
                revision: config.revision,
                routines: views(&store, &config, state)?,
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
                routine: Box::new(view(&store, routine, state)?),
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
        Action::SetEnabled {
            revision,
            name,
            enabled,
        } => {
            let _guard = state
                .config
                .lock()
                .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
            let mut config = store.load()?;
            validate_revision(config.revision, revision)?;
            let routine = config
                .routines
                .iter_mut()
                .find(|routine| routine.name == name)
                .ok_or(RoutineError::NotFound(name))?;
            routine.enabled = enabled;
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
        Action::Fire {
            kind,
            payload,
            event_id,
        } => Ok(Response::Fire {
            outcome: fire_event(&store, kind, payload, event_id, state)?,
        }),
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

// ^ [[Routine Scheduling and Run Lifecycle]]; store.rs commits receipts and initial history,
// ^ then execution.rs finalizes each prepared run without persisting the payload.
fn fire_event(
    store: &RoutineStore,
    kind: String,
    payload: serde_json::Value,
    event_id: String,
    state: &Arc<DaemonState>,
) -> Result<FireOutcome, RoutineError> {
    let Trigger::Event { kind } = (Trigger::Event { kind }).validated()? else {
        unreachable!("event validation returned a cron trigger")
    };
    if event_id.is_empty() || event_id.len() > 1_024 || event_id.chars().any(char::is_control) {
        return Err(RoutineError::Validation(
            "event id must be 1-1024 bytes without control characters".into(),
        ));
    }
    let payload = serde_json::to_string(&payload)
        .map_err(|error| RoutineError::Validation(format!("invalid event payload: {error}")))?;
    if payload.len() > MAX_EVENT_PAYLOAD_BYTES {
        return Err(RoutineError::Validation(format!(
            "event payload exceeds {MAX_EVENT_PAYLOAD_BYTES} bytes"
        )));
    }

    let _config_guard = state
        .config
        .lock()
        .map_err(|_| RoutineError::Io("config lock poisoned".into()))?;
    let matching = store
        .load()?
        .routines
        .into_iter()
        .filter(|routine| {
            routine.enabled
                && matches!(&routine.trigger, Trigger::Event { kind: candidate } if candidate == &kind)
        })
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(FireOutcome::NoMatch);
    }

    let mut active = state
        .active
        .lock()
        .map_err(|_| RoutineError::Io("active-run lock poisoned".into()))?;
    let mut outcomes = Vec::with_capacity(matching.len());
    let mut admitted = Vec::new();
    for routine in matching {
        let key = RunKey {
            project: store.project().to_path_buf(),
            routine: routine.name.clone(),
        };
        if active.contains_key(&key) {
            outcomes.push(RoutineFire::AlreadyRunning { name: routine.name });
            continue;
        }
        active.insert(key.clone(), false);
        let record = prepare_run_record(
            store,
            &routine,
            RunCause::Event {
                kind: kind.clone(),
                event_id: event_id.clone(),
            },
        );
        outcomes.push(RoutineFire::Started {
            name: routine.name.clone(),
        });
        admitted.push((key, routine, record));
    }
    drop(active);

    let records = admitted
        .iter()
        .map(|(_, _, record)| record.clone())
        .collect::<Vec<_>>();
    let recorded = store.admit_event(&kind, &event_id, &records);
    if !matches!(recorded, Ok(true)) {
        if let Ok(mut active) = state.active.lock() {
            for (key, _, _) in &admitted {
                active.remove(key);
            }
        }
        return match recorded {
            Ok(false) => Ok(FireOutcome::Deduplicated),
            Err(error) => Err(error),
            Ok(true) => unreachable!(),
        };
    }

    for (key, routine, record) in admitted {
        spawn_prepared_event_run(store.clone(), key, routine, record, payload.clone(), state);
    }
    Ok(FireOutcome::Handled { routines: outcomes })
}

fn views(
    store: &RoutineStore,
    config: &ProjectRoutines,
    state: &DaemonState,
) -> Result<Vec<RoutineView>, RoutineError> {
    let runtime = store.load_runtime()?;
    Ok(config
        .routines
        .iter()
        .map(|routine| view_with_runtime(store, routine, state, &runtime))
        .collect())
}

fn view(
    store: &RoutineStore,
    routine: &super::Routine,
    state: &DaemonState,
) -> Result<RoutineView, RoutineError> {
    let runtime = store.load_runtime()?;
    Ok(view_with_runtime(store, routine, state, &runtime))
}

fn view_with_runtime(
    store: &RoutineStore,
    routine: &super::Routine,
    state: &DaemonState,
    runtime: &RuntimeState,
) -> RoutineView {
    let runs = runtime.runs.get(&routine.name).cloned().unwrap_or_default();
    let running = is_active(state, store.project(), &routine.name);
    let next_run_epoch = match (&routine.trigger, routine.enabled) {
        (Trigger::Cron(expression), true) => CronSchedule::parse(expression)
            .ok()
            .and_then(|schedule| schedule.next_run_after(now_epoch())),
        _ => None,
    };
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
        version: TRANSACTION_VERSION,
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
        let text = read_text_limited(&entry.path())?;
        let transaction: RenameTransaction = toml::from_str(&text).map_err(|error| {
            RoutineError::Corrupt(format!("{}: {error}", entry.path().display()))
        })?;
        if transaction.version != TRANSACTION_VERSION {
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
    // ^ Hold registry admission through run registration. After unregister
    // returns, an older scheduler snapshot can no longer spawn a new run.
    let registry_store = crate::RegistryStore::new(root.to_path_buf());
    let _registry_guard = if registry_store.path().exists() {
        match registry_store.exclusive_lock() {
            Ok(guard) => Some(guard),
            Err(_) => return,
        }
    } else {
        None
    };
    let registered = match scheduled_project_paths(root) {
        Ok(registered) => registered,
        Err(_) => return,
    };
    #[cfg(test)]
    if let Some((entered, release)) = TICK_ADMISSION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut hook| hook.take())
    {
        let _ = entered.send(());
        let _ = release.recv_timeout(Duration::from_secs(5));
    }
    let projects = root.join("projects");
    let Ok(entries) = fs::read_dir(projects) else {
        return;
    };
    let local = local_time(epoch_minute * 60);
    for entry in entries.flatten() {
        let Ok(text) = read_text_limited(&entry.path()) else {
            continue;
        };
        let Ok(discovered) = toml::from_str::<ProjectRoutines>(&text) else {
            continue;
        };
        let Ok(store) = RoutineStore::new(root.to_path_buf(), &discovered.project_path) else {
            continue;
        };
        if registered
            .as_ref()
            .is_some_and(|paths| !paths.contains(store.project()))
        {
            continue;
        }
        if entry.path() != store.project_file() {
            continue;
        }
        let Ok(config) = store.load() else {
            continue;
        };
        for routine in &config.routines {
            if !routine.enabled {
                continue;
            }
            let Trigger::Cron(expression) = &routine.trigger else {
                continue;
            };
            let Ok(schedule) = CronSchedule::parse(expression) else {
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

fn scheduled_project_paths(root: &Path) -> Result<Option<HashSet<PathBuf>>, crate::RegistryError> {
    let registry = crate::RegistryStore::new(root.to_path_buf());
    if !registry.path().exists() {
        return Ok(None);
    }
    Ok(Some(
        registry
            .load()?
            .projects
            .into_iter()
            .map(|project| project.working_dir)
            .collect(),
    ))
}

fn spawn_prepared_event_run(
    store: RoutineStore,
    key: RunKey,
    routine: super::Routine,
    record: RunRecord,
    payload: String,
    state: &Arc<DaemonState>,
) {
    let shared = state.clone();
    let failure_store = store.clone();
    let failure_key = key.clone();
    let failure_routine = routine.name.clone();
    let failure_run = record.id.clone();
    let worker_run = failure_run.clone();
    let worker = std::thread::Builder::new().spawn(move || {
        let callback_state = shared.clone();
        let callback_key = key.clone();
        let callback_store = store.clone();
        let callback_name = routine.name.clone();
        let result =
            execute_prepared_supervised(&store, &routine, record, Some(payload), move |_| {
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
            let _ = interrupt_run(&store, &routine.name, &worker_run);
        }
        if let Ok(mut active) = shared.active.lock() {
            active.remove(&key);
        }
    });
    match worker {
        Ok(handle) => state
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handle),
        Err(_) => {
            let _ = interrupt_run(&failure_store, &failure_routine, &failure_run);
            state
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&failure_key);
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
            interrupt_record(run);
        }
        Ok(())
    })
}

fn interrupt_run(store: &RoutineStore, name: &str, id: &str) -> Result<(), RoutineError> {
    store.modify_runtime(|state| {
        if let Some(run) = state
            .runs
            .get_mut(name)
            .and_then(|runs| runs.iter_mut().find(|run| run.id == id))
        {
            interrupt_record(run);
        }
        Ok(())
    })
}

fn interrupt_record(run: &mut RunRecord) {
    if run.status == RunStatus::Running {
        run.status = RunStatus::Interrupted;
        run.finished_epoch = Some(now_epoch());
        run.pid = None;
        run.process_start = None;
    }
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
    process_start(pgid).is_some_and(|current| current == leader_start)
}

fn reconcile_running(root: &Path) {
    let Ok(entries) = fs::read_dir(root.join("projects")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(text) = read_text_limited(&entry.path()) else {
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
                let routine = run.routine.clone();
                prune_run_logs(&store, &routine, &run);
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
    use crate::routine::{Routine, RoutineErrorKind, RunCause, RunRecord, SCHEMA_VERSION};
    use std::io::Cursor;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Instant;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, PathBuf, Arc<DaemonState>) {
        // ^ Unix-domain socket paths have a small platform limit; keep this fixture root short.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/rd")
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
            trigger: Trigger::Cron("0 0 1 1 *".into()),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            prompt: String::new(),
            enabled: true,
        }
    }

    fn request(project: &Path, action: Action) -> Request {
        Request::new(project.to_path_buf(), action)
    }

    fn event_fixture(command: Vec<String>) -> (PathBuf, PathBuf, RoutineStore, Arc<DaemonState>) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/re")
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
                    routines: vec![Routine {
                        name: "event-one".into(),
                        trigger: Trigger::Event {
                            kind: "test.changed".into(),
                        },
                        command,
                        prompt: String::new(),
                        enabled: true,
                    }],
                },
                0,
            )
            .unwrap();
        (root, project, store, Arc::new(DaemonState::default()))
    }

    #[test]
    fn event_fire_delivers_payload_records_cause_and_deduplicates() {
        let (root, project, store, state) = event_fixture(vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$ASCHED_EVENT_PAYLOAD\" = '{\"value\":7}'".into(),
        ]);
        let action = Action::Fire {
            kind: "test.changed".into(),
            payload: serde_json::json!({"value": 7}),
            event_id: "delivery-1".into(),
        };

        let first = process(&root, request(&project, action.clone()), &state).unwrap();
        drain_workers(&state);
        let second = process(&root, request(&project, action), &state).unwrap();
        let runtime = store.load_runtime().unwrap();
        let runs = runtime.runs.get("event-one").unwrap();

        assert!(matches!(
            first,
            Response::Fire {
                outcome: FireOutcome::Handled { .. }
            }
        ));
        assert!(matches!(
            second,
            Response::Fire {
                outcome: FireOutcome::Deduplicated
            }
        ));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Succeeded);
        assert_eq!(
            runs[0].cause,
            RunCause::Event {
                kind: "test.changed".into(),
                event_id: "delivery-1".into(),
            }
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn event_fire_admits_all_idle_matches_in_one_runtime_update() {
        let (root, project, store, state) = event_fixture(vec!["/bin/true".into()]);
        let mut config = store.load().unwrap();
        let mut second = config.routines[0].clone();
        second.name = "event-two".into();
        config.routines.push(second);
        store.save(config, 1).unwrap();

        let response = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: " test.changed ".into(),
                    payload: serde_json::json!({}),
                    event_id: "multi-delivery".into(),
                },
            ),
            &state,
        )
        .unwrap();
        let admitted = store.load_runtime().unwrap();

        assert!(matches!(
            response,
            Response::Fire {
                outcome: FireOutcome::Handled { routines }
            } if routines == vec![
                RoutineFire::Started { name: "event-one".into() },
                RoutineFire::Started { name: "event-two".into() },
            ]
        ));
        assert!(admitted.has_event_receipt("test.changed", "multi-delivery"));
        assert_eq!(admitted.runs["event-one"].len(), 1);
        assert_eq!(admitted.runs["event-two"].len(), 1);

        drain_workers(&state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn event_admission_crash_is_reconciled_without_replay() {
        let (root, _project, store, _state) = event_fixture(vec!["/bin/true".into()]);
        let config = store.load().unwrap();
        let record = prepare_run_record(
            &store,
            &config.routines[0],
            RunCause::Event {
                kind: "test.changed".into(),
                event_id: "delivery-crash".into(),
            },
        );
        assert!(store
            .admit_event("test.changed", "delivery-crash", &[record])
            .unwrap());

        reconcile_running(&root);

        let runtime = store.load_runtime().unwrap();
        assert_eq!(runtime.runs["event-one"][0].status, RunStatus::Interrupted);
        assert!(!store
            .admit_event("test.changed", "delivery-crash", &[])
            .unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_event_routine_is_no_match_and_records_no_receipt() {
        let (root, project, store, state) = event_fixture(vec!["/bin/true".into()]);
        let mut config = store.load().unwrap();
        config.routines[0].enabled = false;
        store.save(config, 1).unwrap();

        let response = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: "test.changed".into(),
                    payload: serde_json::json!({}),
                    event_id: "disabled-delivery".into(),
                },
            ),
            &state,
        )
        .unwrap();

        assert!(matches!(
            response,
            Response::Fire {
                outcome: FireOutcome::NoMatch
            }
        ));
        assert!(store.load_runtime().unwrap().event_receipts.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn event_payload_at_compact_size_limit_is_accepted() {
        let (root, project, _store, state) = event_fixture(vec!["/bin/true".into()]);
        let payload = serde_json::Value::String("x".repeat(MAX_EVENT_PAYLOAD_BYTES - 2));

        let response = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: "test.changed".into(),
                    payload,
                    event_id: "max-payload".into(),
                },
            ),
            &state,
        )
        .unwrap();

        assert!(matches!(
            response,
            Response::Fire {
                outcome: FireOutcome::Handled { .. }
            }
        ));
        drain_workers(&state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_event_payload_is_rejected_before_receipt() {
        let (root, project, store, state) = event_fixture(vec!["/bin/true".into()]);
        let result = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: "test.changed".into(),
                    payload: serde_json::Value::String("x".repeat(MAX_EVENT_PAYLOAD_BYTES + 1)),
                    event_id: "oversized".into(),
                },
            ),
            &state,
        );

        assert!(matches!(result, Err(RoutineError::Validation(_))));
        assert!(store.load_runtime().unwrap().event_receipts.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_duplicate_event_requests_admit_once() {
        let (root, project, store, state) = event_fixture(vec!["/bin/true".into()]);
        let stopping = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let project = project.clone();
            let state = state.clone();
            let stopping = stopping.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                dispatch(
                    &root,
                    request(
                        &project,
                        Action::Fire {
                            kind: "test.changed".into(),
                            payload: serde_json::json!({}),
                            event_id: "same-delivery".into(),
                        },
                    ),
                    &stopping,
                    &state,
                )
                .unwrap()
            }));
        }
        barrier.wait();
        let responses = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        drain_workers(&state);

        assert_eq!(
            responses
                .iter()
                .filter(|response| matches!(
                    response,
                    Response::Fire {
                        outcome: FireOutcome::Handled { .. }
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| matches!(
                    response,
                    Response::Fire {
                        outcome: FireOutcome::Deduplicated
                    }
                ))
                .count(),
            1
        );
        assert_eq!(store.load_runtime().unwrap().runs["event-one"].len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn busy_event_routine_is_reported_and_not_queued() {
        let (root, project, store, state) =
            event_fixture(vec!["/bin/sh".into(), "-c".into(), "sleep 0.2".into()]);
        let first = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: "test.changed".into(),
                    payload: serde_json::json!({}),
                    event_id: "first".into(),
                },
            ),
            &state,
        )
        .unwrap();
        let second = process(
            &root,
            request(
                &project,
                Action::Fire {
                    kind: "test.changed".into(),
                    payload: serde_json::json!({}),
                    event_id: "second".into(),
                },
            ),
            &state,
        )
        .unwrap();
        drain_workers(&state);

        assert!(matches!(
            first,
            Response::Fire {
                outcome: FireOutcome::Handled { .. }
            }
        ));
        assert!(matches!(
            second,
            Response::Fire {
                outcome: FireOutcome::Handled { routines }
            } if routines == vec![RoutineFire::AlreadyRunning { name: "event-one".into() }]
        ));
        assert_eq!(store.load_runtime().unwrap().runs["event-one"].len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[derive(Clone, Copy)]
    struct TestDeadline(Instant);

    impl TestDeadline {
        fn after(duration: Duration) -> Self {
            Self(Instant::now() + duration)
        }

        fn remaining(self) -> Duration {
            self.0.saturating_duration_since(Instant::now())
        }

        fn wait_until(self, mut predicate: impl FnMut() -> bool) -> bool {
            loop {
                let satisfied = predicate();
                if satisfied || Instant::now() >= self.0 {
                    return satisfied;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    struct HandlerReleaseGate {
        released: Mutex<Vec<bool>>,
        wake: std::sync::Condvar,
    }

    impl HandlerReleaseGate {
        fn new(count: usize) -> Self {
            Self {
                released: Mutex::new(vec![false; count]),
                wake: std::sync::Condvar::new(),
            }
        }

        fn wait_until_released(&self, index: usize, deadline: TestDeadline) -> bool {
            let released = self.released.lock().unwrap();
            let (released, _) = self
                .wake
                .wait_timeout_while(released, deadline.remaining(), |released| {
                    !released.get(index).copied().unwrap_or(true)
                })
                .unwrap();
            released.get(index).copied().unwrap_or(true)
        }

        fn release(&self, index: usize) {
            self.released.lock().unwrap()[index] = true;
            self.wake.notify_all();
        }

        fn release_all(&self) {
            self.released.lock().unwrap().fill(true);
            self.wake.notify_all();
        }
    }

    #[test]
    fn given_two_accepted_handlers_and_active_worker_when_wait_fails_then_error_waits_for_teardown()
    {
        let (root, project, state) = fixture();
        let listener = UnixListener::bind(root.join("event-loop-error.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let clients = [
            UnixStream::connect(root.join("event-loop-error.sock")).unwrap(),
            UnixStream::connect(root.join("event-loop-error.sock")).unwrap(),
        ];
        let deadline = TestDeadline::after(Duration::from_secs(10));

        let key = RunKey {
            project,
            routine: "one".into(),
        };
        state.active.lock().unwrap().insert(key.clone(), false);
        let (worker_release_tx, worker_release_rx) = std::sync::mpsc::channel();
        let (worker_finished_tx, worker_finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = worker_release_rx.recv_timeout(deadline.remaining());
            let _ = worker_finished_tx.send(());
        });
        state.workers.lock().unwrap().push(worker);

        let gate = Arc::new(HandlerReleaseGate::new(2));
        let next_handler = Arc::new(AtomicUsize::new(0));
        let (handler_started_tx, handler_started_rx) = std::sync::mpsc::channel();
        let (handler_finished_tx, handler_finished_rx) = std::sync::mpsc::channel();
        let gate_in_handler = gate.clone();
        let next_handler_in_handler = next_handler.clone();
        let (error_issued_tx, error_issued_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let cleanup_stopping = stopping.clone();
        let event_state = state.clone();
        let loop_root = root.clone();
        let event_loop = std::thread::spawn(move || {
            let mut waits = 0;
            let result = event_loop_with(
                &loop_root,
                &listener,
                event_state,
                stopping,
                move |_, _| {
                    waits += 1;
                    if waits == 1 {
                        Ok(())
                    } else {
                        let _ = error_issued_tx.send(());
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "injected wait failure",
                        )
                        .into())
                    }
                },
                move |_, _, _, _| {
                    let index = next_handler_in_handler.fetch_add(1, Ordering::SeqCst);
                    let _ = handler_started_tx.send(index);
                    if !gate_in_handler.wait_until_released(index, deadline) {
                        return Err(RoutineError::Io("test handler release timed out".into()));
                    }
                    let _ = handler_finished_tx.send(index);
                    Ok(())
                },
                |task| std::thread::Builder::new().spawn(task),
            );
            let _ = result_tx.send(result);
        });

        let remaining = || deadline.remaining();
        let error_was_issued = error_issued_rx.recv_timeout(remaining()).is_ok();
        let mut started = (0..2)
            .filter_map(|_| handler_started_rx.recv_timeout(remaining()).ok())
            .collect::<Vec<_>>();
        started.sort_unstable();
        let admission_closed_before_releases =
            deadline.wait_until(|| cleanup_stopping.load(Ordering::SeqCst));
        let pending_before_releases = matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        gate.release(0);
        let first_finished = handler_finished_rx.recv_timeout(remaining()).ok();
        let pending_after_first_handler = matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        gate.release(1);
        let second_finished = handler_finished_rx.recv_timeout(remaining()).ok();
        let worker_was_stopped = deadline.wait_until(|| {
            state
                .active
                .lock()
                .ok()
                .and_then(|active| active.get(&key).copied())
                == Some(true)
        });
        let pending_before_worker = matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        let _ = worker_release_tx.send(());
        let worker_finished = worker_finished_rx.recv_timeout(remaining()).is_ok();
        let result = result_rx.recv_timeout(remaining()).ok();

        cleanup_stopping.store(true, Ordering::SeqCst);
        gate.release_all();
        let _ = worker_release_tx.send(());
        let event_loop_joined = event_loop.join().is_ok();
        drop(clients);
        let _ = fs::remove_dir_all(root);

        assert_eq!(
            (
                error_was_issued,
                started,
                admission_closed_before_releases,
                pending_before_releases,
                first_finished,
                pending_after_first_handler,
                second_finished,
                worker_was_stopped,
                pending_before_worker,
                worker_finished,
                matches!(result, Some(Err(RoutineError::Io(_)))),
                event_loop_joined,
            ),
            (
                true,
                vec![0, 1],
                true,
                true,
                Some(0),
                true,
                Some(1),
                true,
                true,
                true,
                true,
                true,
            )
        );
    }

    #[test]
    fn given_sixty_five_clients_when_one_of_sixty_four_live_handlers_finishes_then_accepting_resumes(
    ) {
        let (root, _, state) = fixture();
        let socket = root.join("event-loop-cap.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let deadline = TestDeadline::after(Duration::from_secs(10));
        let gate = Arc::new(HandlerReleaseGate::new(MAX_REQUEST_HANDLERS + 1));
        let next_handler = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let peak_live = Arc::new(AtomicUsize::new(0));
        let (handler_started_tx, handler_started_rx) = std::sync::mpsc::channel();
        let (handler_finished_tx, handler_finished_rx) = std::sync::mpsc::channel();
        let gate_in_handler = gate.clone();
        let next_handler_in_handler = next_handler.clone();
        let live_in_handler = live.clone();
        let peak_live_in_handler = peak_live.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let test_stop = stopping.clone();
        let (wait_started_tx, wait_started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let loop_root = root.clone();
        let event_loop = std::thread::spawn(move || {
            let mut wait_announced = false;
            let result = event_loop_with(
                &loop_root,
                &listener,
                state,
                stopping,
                move |listener, timeout| {
                    if !wait_announced {
                        wait_announced = true;
                        let _ = wait_started_tx.send(());
                    }
                    wait_for_request(listener, timeout)
                },
                move |_, _, _, _| {
                    let index = next_handler_in_handler.fetch_add(1, Ordering::SeqCst);
                    let current = live_in_handler.fetch_add(1, Ordering::SeqCst) + 1;
                    peak_live_in_handler.fetch_max(current, Ordering::SeqCst);
                    let _ = handler_started_tx.send(index);
                    let was_released = gate_in_handler.wait_until_released(index, deadline);
                    live_in_handler.fetch_sub(1, Ordering::SeqCst);
                    let _ = handler_finished_tx.send(index);
                    if was_released {
                        Ok(())
                    } else {
                        Err(RoutineError::Io("test handler release timed out".into()))
                    }
                },
                |task| std::thread::Builder::new().spawn(task),
            );
            let _ = result_tx.send(result);
        });

        let remaining = || deadline.remaining();
        let event_loop_started = wait_started_rx.recv_timeout(remaining()).is_ok();
        let mut clients = Vec::new();
        for _ in 0..=MAX_REQUEST_HANDLERS {
            match UnixStream::connect(&socket) {
                Ok(client) => clients.push(client),
                Err(_) => break,
            }
        }
        let mut first_wave = (0..MAX_REQUEST_HANDLERS)
            .filter_map(|_| handler_started_rx.recv_timeout(remaining()).ok())
            .collect::<Vec<_>>();
        first_wave.sort_unstable();
        let accepted_while_queued = next_handler.load(Ordering::SeqCst);
        let live_while_queued = live.load(Ordering::SeqCst);

        gate.release(0);
        let first_finished = handler_finished_rx.recv_timeout(remaining()).ok();
        let resumed_handler = handler_started_rx.recv_timeout(remaining()).ok();

        test_stop.store(true, Ordering::SeqCst);
        gate.release_all();
        let result = result_rx.recv_timeout(remaining()).ok();
        let event_loop_joined = event_loop.join().is_ok();
        let all_clients_connected = clients.len() == MAX_REQUEST_HANDLERS + 1;
        drop(clients);
        let _ = fs::remove_dir_all(root);

        assert_eq!(
            (
                event_loop_started,
                all_clients_connected,
                first_wave,
                accepted_while_queued,
                live_while_queued,
                first_finished,
                resumed_handler,
                peak_live.load(Ordering::SeqCst),
                next_handler.load(Ordering::SeqCst),
                live.load(Ordering::SeqCst),
                matches!(result, Some(Ok(()))),
                event_loop_joined,
            ),
            (
                true,
                true,
                (0..MAX_REQUEST_HANDLERS).collect::<Vec<_>>(),
                MAX_REQUEST_HANDLERS,
                MAX_REQUEST_HANDLERS,
                Some(0),
                Some(MAX_REQUEST_HANDLERS),
                MAX_REQUEST_HANDLERS,
                MAX_REQUEST_HANDLERS + 1,
                0,
                true,
                true,
            )
        );
    }

    #[test]
    fn given_live_handler_when_next_handler_spawn_fails_then_admission_closes_before_cleanup() {
        let (root, _, state) = fixture();
        // ^ Unix-domain socket paths have a small platform limit; keep this test name short.
        let socket = root.join("spawn.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let clients = [
            UnixStream::connect(&socket).unwrap(),
            UnixStream::connect(&socket).unwrap(),
        ];
        let deadline = TestDeadline::after(Duration::from_secs(10));
        let gate = Arc::new(HandlerReleaseGate::new(1));
        let gate_in_handler = gate.clone();
        let (handler_started_tx, handler_started_rx) = std::sync::mpsc::channel();
        let (handler_finished_tx, handler_finished_rx) = std::sync::mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let cleanup_stopping = stopping.clone();
        let spawn_attempt = Arc::new(AtomicUsize::new(0));
        let spawn_attempt_in_loop = spawn_attempt.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let loop_root = root.clone();
        let event_loop = std::thread::spawn(move || {
            let result = event_loop_with(
                &loop_root,
                &listener,
                state,
                stopping,
                wait_for_request,
                move |_, _, _, _| {
                    let _ = handler_started_tx.send(());
                    if !gate_in_handler.wait_until_released(0, deadline) {
                        return Err(RoutineError::Io("test handler release timed out".into()));
                    }
                    let _ = handler_finished_tx.send(());
                    Ok(())
                },
                move |task| {
                    if spawn_attempt_in_loop.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::thread::Builder::new().spawn(task)
                    } else {
                        Err(std::io::Error::other("injected handler spawn failure"))
                    }
                },
            );
            let _ = result_tx.send(result);
        });

        let remaining = || deadline.remaining();
        let handler_started = handler_started_rx.recv_timeout(remaining()).is_ok();
        let admission_closed_before_release =
            deadline.wait_until(|| cleanup_stopping.load(Ordering::SeqCst));
        let result_pending_while_handler_is_live = matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );

        gate.release(0);
        let handler_finished = handler_finished_rx.recv_timeout(remaining()).is_ok();
        let result = result_rx.recv_timeout(remaining()).ok();

        cleanup_stopping.store(true, Ordering::SeqCst);
        gate.release(0);
        let event_loop_joined = event_loop.join().is_ok();
        drop(clients);
        let _ = fs::remove_dir_all(root);

        assert_eq!(
            (
                handler_started,
                spawn_attempt.load(Ordering::SeqCst),
                admission_closed_before_release,
                result_pending_while_handler_is_live,
                handler_finished,
                matches!(
                    result,
                    Some(Err(RoutineError::Io(message)))
                        if message == "injected handler spawn failure"
                ),
                event_loop_joined,
            ),
            (true, 2, true, true, true, true, true)
        );
    }

    #[test]
    fn given_started_daemon_when_twelve_sequential_status_requests_then_completes_within_batch_latency_budget(
    ) {
        let (root, project, _) = fixture();
        let (lock, socket, listener) = setup(&root).unwrap();
        let daemon_root = root.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let daemon_stopping = stopping.clone();
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let daemon = std::thread::spawn(move || {
            let _ = startup_tx.send(());
            let result = event_loop_with(
                &daemon_root,
                &listener,
                Arc::new(DaemonState::default()),
                daemon_stopping,
                wait_for_request,
                handle_stream,
                |task| std::thread::Builder::new().spawn(task),
            );
            let _ = result_tx.send(result);
        });
        let client = super::super::RoutineClient::new(root.clone());
        let scenario = (|| -> Result<Duration, String> {
            startup_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())?;
            client
                .request(&request(&project, Action::Status))
                .map_err(|error| error.to_string())?;

            let started = Instant::now();
            for _ in 0..12 {
                client
                    .request(&request(&project, Action::Status))
                    .map_err(|error| error.to_string())?;
            }
            Ok(started.elapsed())
        })();

        let shutdown = client.request(&request(&project, Action::Shutdown));
        stopping.store(true, Ordering::SeqCst);
        let daemon_result = result_rx.recv_timeout(Duration::from_secs(5));
        let daemon_joined = if daemon_result.is_ok() {
            daemon.join().is_ok()
        } else {
            false
        };
        drop(lock);
        let socket_removed = match fs::remove_file(socket) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        };
        let root_removed = fs::remove_dir_all(&root).is_ok() || !root.exists();

        let elapsed_within_budget = matches!(
            &scenario,
            Ok(elapsed) if *elapsed < Duration::from_millis(900)
        );
        assert!(
            elapsed_within_budget
                && shutdown.is_ok()
                && matches!(&daemon_result, Ok(Ok(())))
                && daemon_joined
                && socket_removed
                && root_removed,
            "scenario={scenario:?}, shutdown={shutdown:?}, daemon={daemon_result:?}, joined={daemon_joined}, socket_removed={socket_removed}, root_removed={root_removed}"
        );
    }

    #[test]
    fn request_frames_are_bounded_and_require_a_terminator() {
        let mut exact = vec![b' '; MAX_REQUEST_FRAME_BYTES];
        exact.push(b'\n');
        assert_eq!(
            read_request_frame(Cursor::new(exact)).unwrap().len(),
            MAX_REQUEST_FRAME_BYTES
        );

        let mut oversized = vec![b' '; MAX_REQUEST_FRAME_BYTES + 1];
        oversized.push(b'\n');
        assert!(matches!(
            read_request_frame(Cursor::new(oversized)),
            Err(RoutineError::Validation(message)) if message.contains("exceeds")
        ));
        assert!(matches!(
            read_request_frame(Cursor::new(b"{}")),
            Err(RoutineError::Validation(message)) if message.contains("newline")
        ));
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
    fn disabled_routine_is_not_scheduled_has_no_next_run_and_still_runs_manually() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut config = store.load().unwrap();
        let routine = config
            .routines
            .iter_mut()
            .find(|routine| routine.name == "one")
            .unwrap();
        routine.enabled = false;
        routine.trigger = Trigger::Cron("* * * * *".into());
        routine.command = vec!["/bin/echo".into(), "manual".into()];
        store.save(config, 1).unwrap();

        let listed = process(&root, request(&project, Action::List), &state).unwrap();
        let Response::Routines { routines, .. } = listed else {
            panic!("expected routine list");
        };
        let view = routines
            .iter()
            .find(|view| view.routine.name == "one")
            .unwrap();
        assert!(!view.routine.enabled);
        assert_eq!(view.next_run_epoch, None);

        tick(&root, 1_000_000, &state);
        drain_workers(&state);
        let runtime = store.load_runtime().unwrap();
        assert!(!runtime.claims.contains_key("one"));
        assert!(!runtime.runs.contains_key("one"));

        process(
            &root,
            request(&project, Action::Run { name: "one".into() }),
            &state,
        )
        .unwrap();
        drain_workers(&state);
        assert!(store
            .load_runtime()
            .unwrap()
            .runs
            .get("one")
            .and_then(|runs| runs.last())
            .is_some_and(|run| run.status == RunStatus::Succeeded));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn given_existing_registry_excluding_project_when_tick_then_schedule_waits_until_registered() {
        let (root, project, state) = fixture();
        let routine_store = RoutineStore::new(root.clone(), &project).unwrap();
        let mut config = routine_store.load().unwrap();
        config.routines.truncate(1);
        config.routines[0].trigger = Trigger::Cron("* * * * *".into());
        config.routines[0].command = vec!["/bin/echo".into(), "scheduled".into()];
        routine_store.save(config, 1).unwrap();

        let unrelated = root.join("registered-only");
        fs::create_dir_all(&unrelated).unwrap();
        let registry = crate::RegistryStore::new(root.clone());
        registry
            .add(
                0,
                crate::Project {
                    name: "unrelated".into(),
                    working_dir: unrelated,
                },
            )
            .unwrap();
        let epoch_minute = 1_000_000;

        tick(&root, epoch_minute, &state);
        drain_workers(&state);
        let before = routine_store.load_runtime().unwrap();

        registry
            .add(
                1,
                crate::Project {
                    name: "scheduled".into(),
                    working_dir: project.clone(),
                },
            )
            .unwrap();
        tick(&root, epoch_minute, &state);
        drain_workers(&state);
        let after = routine_store.load_runtime().unwrap();

        assert_eq!(
            (
                before.claims.contains_key("one"),
                before.runs.contains_key("one"),
                after.claims.get("one").copied(),
                after
                    .runs
                    .get("one")
                    .and_then(|runs| runs.last())
                    .map(|run| run.status.clone())
            ),
            (false, false, Some(epoch_minute), Some(RunStatus::Succeeded))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn enabled_toggle_persists_over_typed_protocol_and_rejects_stale_revision() {
        let (root, project, _state) = fixture();
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
        let client = super::super::RoutineClient::new(root.clone());

        let toggled = client
            .request(&request(
                &project,
                Action::SetEnabled {
                    revision: 1,
                    name: "one".into(),
                    enabled: false,
                },
            ))
            .unwrap();
        assert!(matches!(toggled, Response::Ok { revision: Some(2) }));

        let stale = client
            .request(&request(
                &project,
                Action::SetEnabled {
                    revision: 1,
                    name: "one".into(),
                    enabled: true,
                },
            ))
            .unwrap_err();
        assert!(matches!(
            stale,
            RoutineError::RemoteDaemon {
                kind: RoutineErrorKind::Conflict,
                ..
            }
        ));

        let missing = client
            .request(&request(
                &project,
                Action::SetEnabled {
                    revision: 2,
                    name: "missing".into(),
                    enabled: false,
                },
            ))
            .unwrap_err();
        assert!(matches!(
            missing,
            RoutineError::RemoteDaemon {
                kind: RoutineErrorKind::NotFound,
                ..
            }
        ));

        let shown = client
            .request(&request(&project, Action::Show { name: "one".into() }))
            .unwrap();
        assert!(matches!(
            shown,
            Response::Routine {
                revision: 2,
                routine,
            } if !routine.routine.enabled
        ));
        client
            .request(&request(&project, Action::Shutdown))
            .unwrap();
        server.join().unwrap().unwrap();
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
                        cause: RunCause::Manual,
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
        config.routines[0].trigger = Trigger::Cron("* * * * *".into());
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
        config.routines[0].trigger = Trigger::Cron("* * * * *".into());
        config.routines[0].command.clear();
        fs::write(store.project_file(), toml::to_string(&config).unwrap()).unwrap();

        tick(&root, now_epoch() / 60, &state);

        assert!(!is_active(&state, &project, "one"));
        assert!(!store.load_runtime().unwrap().claims.contains_key("one"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_and_show_report_corrupt_runtime_state() {
        let (root, project, state) = fixture();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        fs::create_dir_all(store.runtime_file().parent().unwrap()).unwrap();
        fs::write(store.runtime_file(), "not valid = [toml").unwrap();

        for action in [Action::List, Action::Show { name: "one".into() }] {
            assert!(matches!(
                process(&root, request(&project, action), &state),
                Err(RoutineError::Corrupt(_))
            ));
        }
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
                        cause: RunCause::Cron {
                            scheduled_epoch_minute: 123,
                        },
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
            version: TRANSACTION_VERSION,
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
    fn cancel_does_not_signal_group_after_leader_identity_is_gone() {
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
                        cause: RunCause::Manual,
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
        assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
        assert!(process_group_exists(leader.id() as i32));
        unsafe {
            libc::kill(-(leader.id() as i32), libc::SIGKILL);
        }
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
                        cause: RunCause::Manual,
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
            let id = format!("1-{index}");
            let run_dir = routine_dir.join(&id);
            fs::create_dir_all(&run_dir).unwrap();
            records.push(RunRecord {
                id,
                routine: "one".into(),
                started_epoch: index as i64,
                finished_epoch: Some(index as i64),
                scheduled_epoch_minute: None,
                cause: RunCause::Manual,
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
        assert_eq!(retained.first().unwrap().id, "1-2");
        assert!(!routine_dir.join("1-0").exists());
        assert!(!routine_dir.join("1-1").exists());
        assert!(routine_dir.join("1-2").exists());
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
                        trigger: Trigger::Cron("0 0 1 1 *".into()),
                        command: vec!["/bin/echo".into(), "done".into()],
                        prompt: String::new(),
                        enabled: true,
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
    fn idle_client_cannot_prevent_socket_shutdown() {
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
        let idle = UnixStream::connect(&socket).unwrap();
        std::thread::sleep(Duration::from_millis(50));

        super::super::ipc::send(&socket, &request(&project, Action::Shutdown)).unwrap();
        let started = Instant::now();
        server.join().unwrap().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        drop(idle);
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

#[cfg(test)]
#[path = "daemon_contract_tests.rs"]
mod daemon_contract_tests;
