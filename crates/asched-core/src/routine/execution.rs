use super::store::RoutineStore;
use super::{Routine, RoutineError, RunCause, RunRecord, RunStatus, MAX_RUNS};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const FINAL_OUTPUT_BYTES: usize = 16 * 1024;
const STRUCTURED_LINE_BYTES: usize = 64 * 1024;
const MAX_RUN_LOG_BYTES: usize = 8 * 1024 * 1024;
const RUN_LOG_TAIL_BYTES: usize = 128 * 1024;
const TRUNCATION_MARKER: &[u8] = b"\n[asched: middle output truncated]\n";

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
    if routine.command.is_empty() {
        return Err(RoutineError::Validation(
            "command must contain at least one argv item".into(),
        ));
    }
    let cause = scheduled.map_or(RunCause::Manual, |scheduled_epoch_minute| RunCause::Cron {
        scheduled_epoch_minute,
    });
    let record = prepare_run_record(store, routine, cause);
    append_record(store, record.clone())?;
    execute_prepared_supervised(store, routine, record, None, on_started)
}

pub(crate) fn prepare_run_record(
    store: &RoutineStore,
    routine: &Routine,
    cause: RunCause,
) -> RunRecord {
    let started = now_epoch();
    let id = format!("{}-{}", started, now_nanos());
    let run_dir = store.logs_dir().join(hex_name(&routine.name)).join(&id);
    let scheduled_epoch_minute = match cause {
        RunCause::Cron {
            scheduled_epoch_minute,
        } => Some(scheduled_epoch_minute),
        _ => None,
    };
    RunRecord {
        id,
        routine: routine.name.clone(),
        started_epoch: started,
        finished_epoch: None,
        cause,
        scheduled_epoch_minute,
        status: RunStatus::Running,
        exit_code: None,
        pid: None,
        process_start: None,
        final_output: String::new(),
        stdout_path: run_dir.join("stdout.log"),
        stderr_path: run_dir.join("stderr.log"),
    }
}

pub(crate) fn execute_prepared_supervised(
    store: &RoutineStore,
    routine: &Routine,
    mut record: RunRecord,
    event_payload: Option<String>,
    on_started: impl FnOnce(&RunRecord),
) -> Result<RunRecord, RoutineError> {
    if routine.command.is_empty() {
        return Err(RoutineError::Validation(
            "command argv must not be empty".into(),
        ));
    }
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
    let run_dir = record
        .stdout_path
        .parent()
        .ok_or_else(|| RoutineError::Corrupt("run output path has no parent".into()))?;
    fs::create_dir_all(run_dir)?;
    fs::set_permissions(run_dir, fs::Permissions::from_mode(0o700))?;
    let stdout_path = record.stdout_path.clone();
    let stderr_path = record.stderr_path.clone();

    let (argv, stdin_bytes) = expanded_argv(routine);
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(store.project());
    if let Some(payload) = event_payload {
        command.env("ASCHED_EVENT_PAYLOAD", payload);
    }
    command
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
    let pid = child.id() as i32;
    let status = child.wait()?;
    terminate_remaining_process_group(pid);
    out_thread
        .join()
        .map_err(|_| RoutineError::Io("stdout reader panicked".into()))??;
    err_thread
        .join()
        .map_err(|_| RoutineError::Io("stderr reader panicked".into()))??;
    record.finished_epoch = Some(now_epoch());
    record.exit_code = status.code();
    let cancelled = store
        .load_runtime()
        .ok()
        .and_then(|state| {
            state
                .runs
                .get(&routine.name)
                .and_then(|runs| runs.iter().find(|run| run.id == record.id))
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
    record.final_output = extract_final_output_files(&stdout_path, &stderr_path);
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

fn terminate_remaining_process_group(pgid: i32) {
    if !process_group_exists(pgid) {
        return;
    }
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    for _ in 0..20 {
        if !process_group_exists(pgid) {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }
    // ^ Repeat KILL while polling to close pipes inherited by descendants that
    // forked during TERM handling before reader threads are joined.
    for _ in 0..20 {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        if !process_group_exists(pgid) {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }
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
        copy_bounded_log(&mut source, &mut file)?;
        file.sync_all()?;
        Ok(())
    })
}

fn copy_bounded_log(mut source: impl Read, mut target: impl Write) -> Result<(), RoutineError> {
    let prefix_limit = MAX_RUN_LOG_BYTES - RUN_LOG_TAIL_BYTES - TRUNCATION_MARKER.len();
    let mut prefix_written = 0;
    let mut tail = Vec::with_capacity(RUN_LOG_TAIL_BYTES);
    let mut truncated = false;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let mut chunk = &buffer[..read];
        if prefix_written < prefix_limit {
            let keep = chunk.len().min(prefix_limit - prefix_written);
            target.write_all(&chunk[..keep])?;
            prefix_written += keep;
            chunk = &chunk[keep..];
        }
        if chunk.is_empty() {
            continue;
        }
        if tail.len() + chunk.len() > RUN_LOG_TAIL_BYTES {
            truncated = true;
            let discard = (tail.len() + chunk.len()) - RUN_LOG_TAIL_BYTES;
            if discard >= tail.len() {
                tail.clear();
                let start = chunk.len().saturating_sub(RUN_LOG_TAIL_BYTES);
                tail.extend_from_slice(&chunk[start..]);
            } else {
                tail.drain(..discard);
                tail.extend_from_slice(chunk);
            }
        } else {
            tail.extend_from_slice(chunk);
        }
    }
    if truncated {
        target.write_all(TRUNCATION_MARKER)?;
    }
    target.write_all(&tail)?;
    Ok(())
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
    let routine = record.routine.clone();
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
        prune_run_logs(store, &routine, &old);
    }
    Ok(())
}

pub(crate) fn prune_run_logs(store: &RoutineStore, routine: &str, run: &RunRecord) {
    // ^ Runtime state is persisted input. Only remove a single validated child
    // derived from the trusted routine log root, never a stored output path.
    if run.routine != routine || !is_safe_run_id(&run.id) {
        return;
    }
    let dir = store.logs_dir().join(hex_name(routine)).join(&run.id);
    let _ = fs::remove_dir_all(dir);
}

fn is_safe_run_id(id: &str) -> bool {
    let Some((seconds, nanos)) = id.split_once('-') else {
        return false;
    };
    !seconds.is_empty()
        && !nanos.is_empty()
        && seconds.bytes().all(|byte| byte.is_ascii_digit())
        && nanos.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn extract_final_output(stdout: &str, stderr: &str) -> String {
    extract_final_output_bytes(stdout.as_bytes(), stderr.as_bytes())
}

fn extract_final_output_bytes(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let mut codex = None;
    let mut claude = None;
    for line in stdout
        .lines()
        .filter(|line| line.len() <= STRUCTURED_LINE_BYTES)
    {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("item.completed") {
            let item = &value["item"];
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                codex = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text_tail(text, FINAL_OUTPUT_BYTES));
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("result") {
            claude = value
                .get("result")
                .and_then(Value::as_str)
                .map(|text| text_tail(text, FINAL_OUTPUT_BYTES));
        } else if value.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(text) = claude_assistant_text(&value) {
                claude = Some(text_tail(&text, FINAL_OUTPUT_BYTES));
            }
        }
    }
    codex
        .or(claude)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if !stdout.trim().is_empty() {
                text_tail(stdout.trim(), FINAL_OUTPUT_BYTES)
            } else {
                text_tail(stderr.trim(), FINAL_OUTPUT_BYTES)
            }
        })
}

fn extract_final_output_files(
    stdout_path: &std::path::Path,
    stderr_path: &std::path::Path,
) -> String {
    let (codex, claude) = File::open(stdout_path)
        .ok()
        .map(scan_structured_output)
        .unwrap_or_default();
    codex
        .or(claude)
        .filter(|output| !output.trim().is_empty())
        .unwrap_or_else(|| {
            let stdout = bounded_file_tail(stdout_path);
            if stdout.is_empty() {
                bounded_file_tail(stderr_path)
            } else {
                stdout
            }
        })
}

fn scan_structured_output(file: File) -> (Option<String>, Option<String>) {
    let mut codex = None;
    let mut claude = None;
    let mut reader = BufReader::new(file);
    loop {
        let Some(line) = read_bounded_line(&mut reader, STRUCTURED_LINE_BYTES) else {
            break;
        };
        let Some(line) = line else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("item.completed") {
            let item = &value["item"];
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                codex = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text_tail(text, FINAL_OUTPUT_BYTES));
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("result") {
            claude = value
                .get("result")
                .and_then(Value::as_str)
                .map(|text| text_tail(text, FINAL_OUTPUT_BYTES));
        } else if value.get("type").and_then(Value::as_str) == Some("assistant") {
            if let Some(text) = claude_assistant_text(&value) {
                claude = Some(text_tail(&text, FINAL_OUTPUT_BYTES));
            }
        }
    }
    (codex, claude)
}

fn read_bounded_line(reader: &mut impl BufRead, max_bytes: usize) -> Option<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf().ok()?;
        if buffer.is_empty() {
            return (!line.is_empty() || oversized).then_some((!oversized).then_some(line));
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        let content_len = newline.unwrap_or(buffer.len());
        if !oversized && line.len().saturating_add(content_len) <= max_bytes {
            line.extend_from_slice(&buffer[..content_len]);
        } else {
            oversized = true;
            line.clear();
        }
        reader.consume(consumed);
        if newline.is_some() {
            return Some((!oversized).then_some(line));
        }
    }
}

fn bounded_file_tail(path: &std::path::Path) -> String {
    const READ_CHUNK_BYTES: usize = 4 * 1024;

    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(mut position) = file.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    let mut tail = Vec::with_capacity(FINAL_OUTPUT_BYTES);
    let mut found_content = false;
    while position > 0 && tail.len() < FINAL_OUTPUT_BYTES {
        let chunk_len = usize::try_from(position.min(READ_CHUNK_BYTES as u64)).unwrap_or(0);
        position -= chunk_len as u64;
        if file.seek(SeekFrom::Start(position)).is_err() {
            break;
        }
        let mut chunk = vec![0; chunk_len];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        if !found_content {
            while chunk.last().is_some_and(u8::is_ascii_whitespace) {
                chunk.pop();
            }
            found_content = !chunk.is_empty();
        }
        if found_content {
            let remaining = FINAL_OUTPUT_BYTES - tail.len();
            let start = chunk.len().saturating_sub(remaining);
            chunk.drain(..start);
            chunk.extend_from_slice(&tail);
            tail = chunk;
        }
    }
    String::from_utf8_lossy(&tail).trim().to_string()
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
    use crate::routine::Trigger;
    use std::cell::Cell;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct CountingReader {
        source: Cursor<Vec<u8>>,
        consumed: Rc<Cell<usize>>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.source.read(buffer)?;
            self.consumed.set(self.consumed.get() + read);
            Ok(read)
        }
    }

    #[test]
    fn given_log_within_cap_when_copied_then_every_byte_is_preserved_without_marker() {
        let input = b"begin\ncomplete tail".to_vec();
        let consumed = Rc::new(Cell::new(0));
        let reader = CountingReader {
            source: Cursor::new(input.clone()),
            consumed: Rc::clone(&consumed),
        };
        let mut output = Vec::new();

        copy_bounded_log(reader, &mut output).unwrap();

        assert_eq!(
            (
                output == input,
                output
                    .windows(TRUNCATION_MARKER.len())
                    .any(|window| window == TRUNCATION_MARKER),
                consumed.get(),
            ),
            (true, false, input.len())
        );
    }

    #[test]
    fn given_log_over_cap_when_copied_then_prefix_and_tail_are_bounded_and_input_is_consumed() {
        let mut input = b"BEGIN".to_vec();
        input.resize(MAX_RUN_LOG_BYTES + 4_096, b'x');
        input.extend_from_slice(b"FINAL-TAIL");
        let consumed = Rc::new(Cell::new(0));
        let reader = CountingReader {
            source: Cursor::new(input.clone()),
            consumed: Rc::clone(&consumed),
        };
        let mut output = Vec::new();

        copy_bounded_log(reader, &mut output).unwrap();
        let markers = output
            .windows(TRUNCATION_MARKER.len())
            .filter(|window| *window == TRUNCATION_MARKER)
            .count();

        assert_eq!(
            (
                output.len() <= MAX_RUN_LOG_BYTES,
                output.starts_with(b"BEGIN"),
                output.ends_with(b"FINAL-TAIL"),
                markers,
                consumed.get(),
            ),
            (true, true, true, 1, input.len())
        );
    }

    #[test]
    fn exact_prompt_is_replaced_otherwise_stdin_is_used() {
        let replaced = Routine {
            name: "x".into(),
            trigger: Trigger::Cron("* * * * *".into()),
            command: vec!["echo".into(), "{prompt}".into()],
            prompt: "hi".into(),
            enabled: true,
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
    fn file_extraction_streams_structured_output_and_bounds_plain_tail() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/routine-execution-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let stdout_path = root.join("stdout.log");
        let stderr_path = root.join("stderr.log");
        let mut stdout = File::create(&stdout_path).unwrap();
        for _ in 0..10_000 {
            writeln!(stdout, "progress output that must not be retained").unwrap();
        }
        writeln!(
            stdout,
            "{{\"type\":\"result\",\"result\":\"final answer\"}}"
        )
        .unwrap();
        fs::write(&stderr_path, "error").unwrap();

        assert_eq!(
            extract_final_output_files(&stdout_path, &stderr_path),
            "final answer"
        );

        fs::write(
            &stdout_path,
            format!("HEAD\n{}TAIL\n", "progress\n".repeat(4_000)),
        )
        .unwrap();
        let fallback = extract_final_output_files(&stdout_path, &stderr_path);
        assert!(fallback.len() <= 16 * 1024);
        assert!(fallback.ends_with("TAIL"));
        assert!(!fallback.contains("HEAD"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_structured_line_is_discarded_without_hiding_later_result() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/routine-execution-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let stdout_path = root.join("stdout.log");
        let stderr_path = root.join("stderr.log");
        let mut stdout = File::create(&stdout_path).unwrap();
        writeln!(
            stdout,
            "{{\"type\":\"result\",\"result\":\"{}\"}}",
            "x".repeat(STRUCTURED_LINE_BYTES)
        )
        .unwrap();
        writeln!(stdout, "{{\"type\":\"result\",\"result\":\"bounded\"}}").unwrap();
        fs::write(&stderr_path, "").unwrap();

        assert_eq!(
            extract_final_output_files(&stdout_path, &stderr_path),
            "bounded"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_public_command_returns_validation_instead_of_panicking() {
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
            name: "empty".into(),
            trigger: Trigger::Cron("* * * * *".into()),
            command: Vec::new(),
            prompt: String::new(),
            enabled: true,
        };

        assert!(matches!(
            execute(&store, &routine, None),
            Err(RoutineError::Validation(_))
        ));
        assert!(!store.load_runtime().unwrap().runs.contains_key("empty"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn successful_leader_exit_does_not_leave_pipe_holding_descendants() {
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
            name: "descendant".into(),
            trigger: Trigger::Cron("* * * * *".into()),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "(trap '' TERM; sleep 30) & exit 0".into(),
            ],
            prompt: String::new(),
            enabled: true,
        };
        let pid = AtomicI32::new(0);

        let record = execute_supervised(&store, &routine, None, |record| {
            pid.store(record.pid.unwrap(), Ordering::SeqCst);
        })
        .unwrap();

        assert_eq!(record.status, RunStatus::Succeeded);
        assert_eq!(unsafe { libc::kill(-pid.load(Ordering::SeqCst), 0) }, -1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_never_deletes_a_directory_from_persisted_output_paths() {
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
        let victim = root.join("must-survive");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("marker"), "present").unwrap();
        let record = RunRecord {
            id: "../must-survive".into(),
            routine: "safe".into(),
            started_epoch: 1,
            finished_epoch: Some(2),
            scheduled_epoch_minute: None,
            cause: RunCause::Manual,
            status: RunStatus::Succeeded,
            exit_code: Some(0),
            pid: None,
            process_start: None,
            final_output: String::new(),
            stdout_path: victim.join("stdout.log"),
            stderr_path: victim.join("stderr.log"),
        };

        prune_run_logs(&store, "safe", &record);

        assert!(victim.join("marker").exists());
        let _ = fs::remove_dir_all(root);
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
            trigger: Trigger::Cron("* * * * *".into()),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "exec 0<&-; trap '' TERM; sleep 30".into(),
            ],
            prompt: "x".repeat(1024 * 1024),
            enabled: true,
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
