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
    execute_supervised(store, routine, scheduled, |_| {})
}

pub fn execute_supervised(
    store: &RoutineStore,
    routine: &Routine,
    scheduled: Option<i64>,
    on_started: impl FnOnce(&RunRecord),
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
        process_start: None,
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
    record.process_start = process_start(child.id() as i32);
    if let Err(error) = replace_record(store, record.clone()) {
        return fail_started_run(
            store,
            child,
            record,
            &format!("failed to persist spawned process: {error}"),
            None,
            None,
        );
    }
    on_started(&record);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return fail_started_run(store, child, record, "stdout pipe missing", None, None),
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => return fail_started_run(store, child, record, "stderr pipe missing", None, None),
    };
    let out_thread = drain(stdout, stdout_path.clone());
    let err_thread = drain(stderr, stderr_path.clone());
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(bytes) = stdin_bytes {
            if let Err(error) = stdin.write_all(bytes) {
                return fail_started_run(
                    store,
                    child,
                    record,
                    &format!("prompt delivery failed: {error}"),
                    Some(out_thread),
                    Some(err_thread),
                );
            }
        }
    }
    let status = child.wait()?;
    out_thread
        .join()
        .map_err(|_| RoutineError::Io("stdout reader panicked".into()))??;
    err_thread
        .join()
        .map_err(|_| RoutineError::Io("stderr reader panicked".into()))??;
    let stdout = fs::read(&stdout_path).unwrap_or_default();
    let stderr = fs::read(&stderr_path).unwrap_or_default();
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
    record.process_start = None;
    record.final_output = extract_final_output_bytes(&stdout, &stderr);
    replace_record(store, record.clone())?;
    Ok(record)
}

fn fail_started_run(
    store: &RoutineStore,
    mut child: std::process::Child,
    mut record: RunRecord,
    message: &str,
    out_thread: Option<thread::JoinHandle<Result<(), RoutineError>>>,
    err_thread: Option<thread::JoinHandle<Result<(), RoutineError>>>,
) -> Result<RunRecord, RoutineError> {
    let pid = child.id() as i32;
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    let mut status = None;
    for _ in 0..20 {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(value)) => status = Some(value),
                Ok(None) => {}
                Err(_) => break,
            }
        }
        if !process_group_exists(pid) {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }
    if status.is_none() || process_group_exists(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        if status.is_none() {
            status = child.wait().ok();
        }
    }
    if let Some(handle) = out_thread {
        let _ = handle.join();
    }
    if let Some(handle) = err_thread {
        let _ = handle.join();
    }
    record.finished_epoch = Some(now_epoch());
    record.status = RunStatus::Failed;
    record.exit_code = status.and_then(|status| status.code());
    record.pid = None;
    record.process_start = None;
    record.final_output = message.to_string();
    replace_record(store, record)?;
    Err(RoutineError::Io(message.to_string()))
}

fn process_group_exists(pgid: i32) -> bool {
    if unsafe { libc::kill(-pgid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

pub(crate) fn process_start(pid: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
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
    extract_final_output_bytes(stdout.as_bytes(), stderr.as_bytes())
}

fn extract_final_output_bytes(stdout: &[u8], stderr: &[u8]) -> String {
    const FALLBACK_TAIL_BYTES: usize = 16 * 1024;
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
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
            if let Some(text) = claude_assistant_text(&value) {
                claude = Some(text);
            }
        }
    }
    codex
        .or(claude)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if !stdout.trim().is_empty() {
                text_tail(stdout.trim(), FALLBACK_TAIL_BYTES)
            } else {
                text_tail(stderr.trim(), FALLBACK_TAIL_BYTES)
            }
        })
}

fn claude_assistant_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn text_tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

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

    #[test]
    fn plain_fallback_is_bounded_to_the_log_tail() {
        let output = format!("HEAD\n{}TAIL", "progress\n".repeat(4_000));
        let extracted = extract_final_output(&output, "");
        assert!(extracted.len() <= 16 * 1024);
        assert!(extracted.ends_with("TAIL"));
        assert!(!extracted.contains("HEAD"));
    }

    #[test]
    fn claude_extraction_ignores_nested_tool_payload_text() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[",
            "{\"type\":\"text\",\"text\":\"final answer\"},",
            "{\"type\":\"tool_use\",\"input\":{\"text\":\"tool payload\"}}]}}\n"
        );
        assert_eq!(extract_final_output(output, ""), "final answer");
    }

    #[test]
    fn tool_only_structured_output_uses_bounded_plain_fallback() {
        let output = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[",
            "{\"type\":\"tool_use\",\"input\":{\"text\":\"not an answer\"}}]}}\n"
        );
        assert_eq!(extract_final_output(output, ""), output.trim());
    }

    #[test]
    fn invalid_utf8_is_preserved_in_plain_fallback() {
        assert_eq!(
            extract_final_output_bytes(b"before\xffafter\n", b""),
            "before\u{fffd}after"
        );
    }

    #[test]
    fn prompt_write_failure_terminates_and_reaps_process_group() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/routine-execution-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let project =
            fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let store = RoutineStore::new(root.clone(), &project).unwrap();
        let routine = Routine {
            name: "broken-stdin".into(),
            cron: "* * * * *".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "exec 0<&-; trap '' TERM; sleep 30".into(),
            ],
            prompt: "x".repeat(1024 * 1024),
        };
        let pid = AtomicI32::new(0);
        let result = execute_supervised(&store, &routine, None, |record| {
            pid.store(record.pid.unwrap(), Ordering::SeqCst);
        });
        assert!(matches!(result, Err(RoutineError::Io(_))));
        let pid = pid.load(Ordering::SeqCst);
        assert!(pid > 0);
        assert_eq!(unsafe { libc::kill(-pid, 0) }, -1);
        let record = store.load_runtime().unwrap().runs["broken-stdin"]
            .last()
            .unwrap()
            .clone();
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.pid.is_none());
        let _ = fs::remove_dir_all(root);
    }
}
