use super::execution::execute;
use super::ipc::{Action, Request, Response, RoutineView};
use super::store::{ProjectRoutines, RoutineStore};
use super::{Capabilities, CronSchedule, LocalTime, RoutineError, RunStatus, PROTOCOL_VERSION};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
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
    while !stopping.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.to_path_buf();
                let stopping = stopping.clone();
                std::thread::spawn(move || {
                    let _ = handle_stream(&root, stream, &stopping);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        let minute = now_epoch() / 60;
        if minute != last_minute {
            last_minute = minute;
            tick(root, minute);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn handle_stream(
    root: &Path,
    mut stream: UnixStream,
    stopping: &AtomicBool,
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
        Ok(request) => process(root, request).unwrap_or_else(Response::error),
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

fn process(root: &Path, request: Request) -> Result<Response, RoutineError> {
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
            let config = store.load()?;
            Ok(Response::Routines {
                revision: config.revision,
                routines: views(&store, &config),
            })
        }
        Action::Show { name } => {
            let config = store.load()?;
            let routine = config
                .routines
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| RoutineError::NotFound(name.clone()))?;
            Ok(Response::Routine {
                revision: config.revision,
                routine: view(&store, routine),
            })
        }
        Action::Add { revision, routine } => {
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
            let routine = routine.validated()?;
            let mut config = store.load()?;
            let index = config
                .routines
                .iter()
                .position(|r| r.name == old_name)
                .ok_or_else(|| RoutineError::NotFound(old_name.clone()))?;
            if routine.name != old_name && is_running(&store, &old_name) {
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
            if is_running(&store, &name) {
                return Err(RoutineError::AlreadyRunning(name));
            }
            let config = store.load()?;
            let routine = config
                .routines
                .iter()
                .find(|r| r.name == name)
                .ok_or(RoutineError::NotFound(name))?;
            Ok(Response::Run {
                run: execute(&store, routine, None)?,
            })
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

fn views(store: &RoutineStore, config: &ProjectRoutines) -> Vec<RoutineView> {
    config
        .routines
        .iter()
        .map(|routine| view(store, routine))
        .collect()
}

fn view(store: &RoutineStore, routine: &super::Routine) -> RoutineView {
    let runs = store
        .load_runtime()
        .ok()
        .and_then(|state| state.runs.get(&routine.name).cloned())
        .unwrap_or_default();
    let running = runs
        .last()
        .is_some_and(|run| run.status == RunStatus::Running);
    RoutineView {
        routine: routine.clone(),
        capabilities: Capabilities::for_running(running),
        latest_run: runs.last().cloned(),
    }
}

fn is_running(store: &RoutineStore, name: &str) -> bool {
    store
        .load_runtime()
        .ok()
        .and_then(|state| state.runs.get(name).and_then(|runs| runs.last()).cloned())
        .is_some_and(|run| run.status == RunStatus::Running)
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

fn tick(root: &Path, epoch_minute: i64) {
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
                && !is_running(&store, &routine.name)
                && store.claim(&routine.name, epoch_minute).unwrap_or(false)
            {
                let root = root.to_path_buf();
                let project = config.project_path.clone();
                let routine = routine.clone();
                std::thread::spawn(move || {
                    if let Ok(store) = RoutineStore::new(root, &project) {
                        let _ = execute(&store, &routine, Some(epoch_minute));
                    }
                });
            }
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
