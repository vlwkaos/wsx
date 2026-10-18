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
        // ^ A brief waits for the owner's start, like an approval request.
        TaskState::Briefed => 3,
        TaskState::Ready => 4,
        TaskState::InReview => 5,
        TaskState::Checking | TaskState::Working | TaskState::Starting => 6,
        TaskState::Stopped => 7,
        TaskState::Landed | TaskState::Closed => 8,
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

/// What wsx ran on the task's latest revision, each check as `passed`, `failed` or
/// `timed out`: the manager states these, not the worker's words (docs/tasks.md "Ledger").
pub fn check_outcomes(result: &Value) -> Vec<(String, &'static str)> {
    result["checks"]
        .as_array()
        .map(|checks| {
            checks
                .iter()
                .map(|check| {
                    let outcome = if check["timed_out"].as_bool() == Some(true) {
                        "timed out"
                    } else if check["exit"].as_i64() == Some(0) {
                        "passed"
                    } else {
                        "failed"
                    };
                    (
                        clip(check["command"].as_str().unwrap_or_default(), 120),
                        outcome,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The longest `task new --title`. ref: docs/tasks.md "Store"
pub const MAX_TITLE_CHARS: usize = 60;

/// A `--title` as given, trimmed; refused when empty, too long or with control characters.
pub fn checked_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("the title is empty");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        anyhow::bail!("the title is longer than {MAX_TITLE_CHARS} characters");
    }
    if title.chars().any(char::is_control) {
        anyhow::bail!("the title has control characters");
    }
    Ok(title.to_string())
}

/// The task's title: its `--title`, else the first line of its prompt (at most 120 characters),
/// shown without a leading `Owner <words>:`, which is how the manager's prompts open. Only the
/// shown title changes; the record and the prompt keep what was written. ref: docs/tasks.md "Store"
pub fn title(dir: &TaskDir, record: &TaskRecord) -> Option<String> {
    if let Some(title) = &record.title {
        return Some(match owner_lead(title) {
            Some((_, rest)) if !rest.is_empty() => rest.to_string(),
            _ => title.clone(),
        });
    }
    dir.prompt().ok().flatten().map(|prompt| {
        let mut lines = prompt
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let first = lines.next().unwrap_or_default();
        let line = match owner_lead(first) {
            Some((_, rest)) if !rest.is_empty() => rest,
            // ^ A bare `Owner report (what it is about):` names its subject in the brackets.
            Some((lead, _)) => bracketed(lead).or_else(|| lines.next()).unwrap_or(first),
            None => first,
        };
        line.chars().take(TITLE_CHARS).collect()
    })
}

/// The longest lead-in before the colon, as in `Owner report (UX failures in how a task asks):`.
const OWNER_LEAD_CHARS: usize = 80;

/// A line opening `Owner <words>:` as its words and the text after the colon (maybe empty);
/// `None` for anything else, such as `Owner: Alice's tool`. A colon inside brackets, or one
/// without a space after it (`http://`), is part of the words.
fn owner_lead(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("Owner ")?;
    let mut depth = 0i32;
    let colon = rest.char_indices().find_map(|(at, char)| {
        match char {
            '(' => depth += 1,
            ')' => depth -= 1,
            ':' if depth <= 0
                && rest[at + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace) =>
            {
                return Some(at)
            }
            _ => {}
        }
        None
    })?;
    let lead = rest[..colon].trim();
    if !lead.starts_with(char::is_alphanumeric) || lead.chars().count() > OWNER_LEAD_CHARS {
        return None;
    }
    Some((lead, rest[colon + 1..].trim()))
}

/// `report (UX failures)` → `UX failures`.
fn bracketed(lead: &str) -> Option<&str> {
    let inner = lead[lead.find('(')? + 1..].strip_suffix(')')?.trim();
    (!inner.is_empty()).then_some(inner)
}

pub fn task_summary(dir: &TaskDir, record: &TaskRecord) -> Value {
    json!({
        "id": record.id,
        "state": record.state,
        "title": title(dir, record),
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
