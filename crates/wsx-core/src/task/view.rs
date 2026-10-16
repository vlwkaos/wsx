//! Read-side views of a project's tasks, shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Ledger", docs/web-client.md "Tasks"

use super::store::{self, TaskDir, RESULT_FILE};
use super::{TaskRecord, TaskState};
use anyhow::Result;
use serde_json::{json, Value};

pub const MAX_RESULT_READ_BYTES: u64 = 1024 * 1024;
/// Paths shown per overlapping task in `show`.
const OVERLAP_PATHS_SHOWN: usize = 20;
const TITLE_CHARS: usize = 120;

/// Attention order (knowledge "Manager Mode Attention UX"): what needs the owner first.
pub fn attention(state: TaskState) -> u8 {
    match state {
        TaskState::Blocked => 0,
        TaskState::Failed => 1,
        TaskState::Conflict => 2,
        TaskState::Ready => 3,
        TaskState::InReview => 4,
        TaskState::Checking | TaskState::Working | TaskState::Starting => 5,
        TaskState::Stopped => 6,
        TaskState::Landed | TaskState::Closed => 7,
    }
}

/// The first line of `text`, at most `chars` characters with an ellipsis when cut.
pub fn clip(text: &str, chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= chars {
        line.to_string()
    } else {
        line.chars().take(chars).collect::<String>() + "…"
    }
}

/// A task's `result.json`, or none when it is missing or unreadable.
pub fn read_result(dir: &TaskDir) -> Option<Value> {
    dir.read_file(RESULT_FILE, MAX_RESULT_READ_BYTES)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// The first line of the task's prompt, at most 120 characters.
pub fn title(dir: &TaskDir) -> Option<String> {
    dir.prompt().ok().flatten().map(|prompt| {
        prompt
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(TITLE_CHARS)
            .collect()
    })
}

pub fn task_summary(dir: &TaskDir, record: &TaskRecord) -> Value {
    json!({
        "id": record.id,
        "state": record.state,
        "title": title(dir),
        "branch": record.branch,
        "revision": record.revision,
        "updated_unix_ms": record.updated_unix_ms,
    })
}

/// Every task of a project with its record, or the error that hid it.
pub fn project_tasks(project_id: &str) -> Result<Vec<(TaskDir, Result<TaskRecord>)>> {
    store::task_numbers(project_id)?
        .into_iter()
        .map(|id| {
            let dir = TaskDir::open(project_id, id)?;
            let record = dir.read_record();
            Ok((dir, record))
        })
        .collect()
}

/// The other open tasks that changed a path this task changed. Paths are capped per
/// record, so `complete` is false when either side kept fewer paths than it changed.
pub fn overlaps(record: &TaskRecord, others: &[&TaskRecord], with_paths: bool) -> Value {
    let mine: std::collections::HashSet<&str> = record.paths.iter().map(String::as_str).collect();
    let rows: Vec<_> = others
        .iter()
        .filter(|other| other.id != record.id && other.state.is_open())
        .filter_map(|other| {
            let common: Vec<&str> = other
                .paths
                .iter()
                .map(String::as_str)
                .filter(|path| mine.contains(path))
                .collect();
            if common.is_empty() {
                return None;
            }
            let complete = record.paths.len() as u32 == record.paths_total
                && other.paths.len() as u32 == other.paths_total;
            let mut row = json!({ "id": other.id, "count": common.len(), "complete": complete });
            if with_paths {
                row["paths"] = json!(common.iter().take(OVERLAP_PATHS_SHOWN).collect::<Vec<_>>());
            }
            Some(row)
        })
        .collect();
    json!(rows)
}

/// The verdict that applies to `revision`, with its time, or null.
pub fn verification_value(log: &[super::LogEntry], revision: u32) -> Value {
    super::verification(log, revision).map_or(Value::Null, |entry| {
        let mut value = serde_json::to_value(&entry.event).unwrap_or_default();
        value["unix_ms"] = json!(entry.unix_ms);
        value
    })
}
