//! Read-side views of a project's tasks, shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Ledger", docs/web-client-tasks.md

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
        TaskState::Waiting => 0,
        TaskState::Failed => 1,
        TaskState::Ready => 2,
        TaskState::Checking | TaskState::Working => 3,
        TaskState::Landed | TaskState::Closed => 4,
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
const MAX_MODEL_BYTES: usize = 256;

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

/// The name the web shows: the `--title`, else the first sentence of the worker's latest
/// summary, else `title` (docs/web-client-tasks.md). Events, the ledger and a pull request
/// keep `title`, which a worker's words never change.
pub fn readable_title(dir: &TaskDir, record: &TaskRecord) -> Option<String> {
    if record.title.is_some() {
        return title(dir, record);
    }
    summary_sentence(dir).or_else(|| title(dir, record))
}

/// The first sentence of the worker's latest summary, at most 120 characters.
pub fn summary_sentence(dir: &TaskDir) -> Option<String> {
    let summary = read_result(dir)
        .and_then(|file| Some(file.get("result")?.get("summary")?.as_str()?.to_string()))?;
    Some(first_sentence(&summary))
        .filter(|sentence| !sentence.is_empty())
        .map(|sentence| clip(&sentence, TITLE_CHARS))
}

/// A worker model named by `task new --model`: one word of at most 256 bytes, as
/// `tasks.agent.model` allows. ref: docs/tasks.md "Config"
pub fn checked_model(model: &str) -> Result<String> {
    if model.is_empty() {
        anyhow::bail!("the model is empty");
    }
    if model.len() > MAX_MODEL_BYTES {
        anyhow::bail!("the model is longer than {MAX_MODEL_BYTES} bytes");
    }
    if model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("the model must be one word");
    }
    // ^ It becomes the value of claude's `--model`; a leading dash would read as an option.
    if model.starts_with('-') {
        anyhow::bail!("the model must not start with '-'");
    }
    Ok(model.to_string())
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

/// The first sentence of the first line of `text`: up to a `.`, `!` or `?` followed by a
/// space or the end ("Did x. Then y" → "Did x."), as the web's `firstSentence`.
fn first_sentence(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    let end = line.char_indices().find(|&(index, mark)| {
        matches!(mark, '.' | '!' | '?')
            && line[index + mark.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    });
    end.map_or(line, |(index, mark)| &line[..index + mark.len_utf8()])
        .to_string()
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
