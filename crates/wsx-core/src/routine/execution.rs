use super::store::RoutineStore;
use super::{Routine, RoutineError, RunRecord, RunStatus, MAX_RUNS};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn expanded_argv(routine: &Routine) -> (Vec<String>, Option<&[u8]>) {
    if routine.command.iter().any(|arg| arg == "{prompt}") {
        (
            routine
                .command
                .iter()
                .map(|arg| {
                    if arg == "{prompt}" {
                        routine.prompt.clone()
                    } else {
                        arg.clone()
                    }
                })
                .collect(),
            None,
        )
    } else {
        (routine.command.clone(), Some(routine.prompt.as_bytes()))
    }
}

pub fn execute(
    store: &RoutineStore,
    routine: &Routine,
    scheduled: Option<i64>,
) -> Result<RunRecord, RoutineError> {
    let lock_dir = store.logs_dir().join("locks");
    fs::create_dir_all(&lock_dir)?;
    fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o700))?;
    let run_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_dir.join(hex_name(&routine.name)))?;
    run_lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(run_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(RoutineError::AlreadyRunning(routine.name.clone()));
    }
    let started = now_epoch();
    let id = format!("{}-{}", started, now_nanos());
    let run_dir = store.logs_dir().join(hex_name(&routine.name)).join(&id);
    fs::create_dir_all(&run_dir)?;
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))?;
    let stdout_path = run_dir.join("stdout.log");
    let stderr_path = run_dir.join("stderr.log");
    let mut record = RunRecord {
        id: id.clone(),
        routine: routine.name.clone(),
        started_epoch: started,
        finished_epoch: None,
        scheduled_epoch_minute: scheduled,
        status: RunStatus::Running,
        exit_code: None,
        pid: None,
        final_output: String::new(),
        stdout_path: stdout_path.clone(),
        stderr_path: stderr_path.clone(),
    };
    append_record(store, record.clone())?;

    let (argv, stdin_bytes) = expanded_argv(routine);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(store.project())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let stdout_file = File::create(&stdout_path)?;
            stdout_file.set_permissions(fs::Permissions::from_mode(0o600))?;
            let mut stderr_file = File::create(&stderr_path)?;
            stderr_file.set_permissions(fs::Permissions::from_mode(0o600))?;
            stderr_file.write_all(error.to_string().as_bytes())?;
            stderr_file.sync_all()?;
            record.status = RunStatus::SpawnFailed;
            record.finished_epoch = Some(now_epoch());
            record.final_output = error.to_string();
            replace_record(store, record.clone())?;
            return Ok(record);
        }
    };
    record.pid = Some(child.id() as i32);
    replace_record(store, record.clone())?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(bytes) = stdin_bytes {
            stdin.write_all(bytes)?;
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RoutineError::Io("stdout pipe missing".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RoutineError::Io("stderr pipe missing".into()))?;
    let out_thread = drain(stdout, stdout_path.clone());
    let err_thread = drain(stderr, stderr_path.clone());
    let status = child.wait()?;
    out_thread
        .join()
        .map_err(|_| RoutineError::Io("stdout reader panicked".into()))??;
    err_thread
        .join()
        .map_err(|_| RoutineError::Io("stderr reader panicked".into()))??;
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    record.finished_epoch = Some(now_epoch());
    record.exit_code = status.code();
    let cancelled = store
        .load_runtime()
        .ok()
        .and_then(|state| {
            state
                .runs
                .get(&routine.name)
                .and_then(|runs| runs.iter().find(|run| run.id == id))
                .cloned()
        })
        .is_some_and(|run| run.status == RunStatus::Cancelled);
    record.status = if cancelled {
        RunStatus::Cancelled
    } else if status.success() {
        RunStatus::Succeeded
    } else {
        RunStatus::Failed
    };
    record.pid = None;
    record.final_output = extract_final_output(&stdout, &stderr);
    replace_record(store, record.clone())?;
    Ok(record)
}

fn drain(
    mut source: impl Read + Send + 'static,
    path: std::path::PathBuf,
) -> thread::JoinHandle<Result<(), RoutineError>> {
    thread::spawn(move || {
        let mut file = File::create(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        std::io::copy(&mut source, &mut file)?;
        file.sync_all()?;
        Ok(())
    })
}

fn append_record(store: &RoutineStore, record: RunRecord) -> Result<(), RoutineError> {
    store.modify_runtime(|state| {
        state
            .runs
            .entry(record.routine.clone())
            .or_default()
            .push(record);
        Ok(())
    })
}

fn replace_record(store: &RoutineStore, record: RunRecord) -> Result<(), RoutineError> {
    let removed = store.modify_runtime(|state| {
        let runs = state.runs.entry(record.routine.clone()).or_default();
        if let Some(existing) = runs.iter_mut().find(|run| run.id == record.id) {
            *existing = record;
        }
        Ok(if runs.len() > MAX_RUNS {
            runs.drain(0..runs.len() - MAX_RUNS).collect::<Vec<_>>()
        } else {
            vec![]
        })
    })?;
    for old in removed {
        if let Some(dir) = old.stdout_path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }
    Ok(())
}

pub fn extract_final_output(stdout: &str, stderr: &str) -> String {
    let mut codex = None;
    let mut claude = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("item.completed") {
            let item = &value["item"];
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                codex = item.get("text").and_then(Value::as_str).map(str::to_owned);
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("result") {
            claude = value
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_owned);
        } else if value.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(text) = last_text(&value) {
                claude = Some(text);
            }
        }
    }
    codex
        .or(claude)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if !stdout.trim().is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            }
        })
}

fn last_text(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| map.values().filter_map(last_text).next_back()),
        Value::Array(items) => items.iter().filter_map(last_text).next_back(),
        _ => None,
    }
}

fn hex_name(name: &str) -> String {
    name.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prompt_is_replaced_otherwise_stdin_is_used() {
        let replaced = Routine {
            name: "x".into(),
            cron: "* * * * *".into(),
            command: vec!["echo".into(), "{prompt}".into()],
            prompt: "hi".into(),
        };
        assert_eq!(expanded_argv(&replaced).0, vec!["echo", "hi"]);
        assert!(expanded_argv(&replaced).1.is_none());
        let stdin = Routine {
            command: vec!["cat".into()],
            ..replaced
        };
        assert_eq!(expanded_argv(&stdin).1, Some("hi".as_bytes()));
    }

    #[test]
    fn extracts_codex_claude_and_fallback_output() {
        assert_eq!(extract_final_output("{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n", ""), "done");
        assert_eq!(
            extract_final_output("{\"type\":\"result\",\"result\":\"answer\"}\n", ""),
            "answer"
        );
        assert_eq!(extract_final_output("plain\n", "err"), "plain");
        assert_eq!(extract_final_output("", "err\n"), "err");
    }
}
