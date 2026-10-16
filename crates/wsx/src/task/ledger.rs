//! `wsx task ledger`: the bounded project state a manager reads before it acts.
//! ref: docs/tasks.md "Ledger"

use super::{
    deploy_status, overlaps, print_json, project_entry, project_tasks, task_summary,
    MAX_RESULT_READ_BYTES,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;
use wsx_core::config::project::load_task_config;
use wsx_core::task::{
    store::{TaskDir, RESULT_FILE},
    verification, TaskRecord, TaskState,
};

/// The whole ledger fits in this many bytes of JSON.
pub(super) const MAX_LEDGER_BYTES: usize = 16 * 1024;
const RECENT_TASKS: usize = 10;
const SUMMARY_CHARS: usize = 240;
const TITLE_CHARS: usize = 80;

/// Attention order (knowledge "Manager Mode Attention UX"): what needs the owner first.
fn attention(state: TaskState) -> u8 {
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

pub(super) fn clip(text: &str, chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= chars {
        line.to_string()
    } else {
        line.chars().take(chars).collect::<String>() + "…"
    }
}

/// A task's `result.json`, or none when it is missing or unreadable.
pub(super) fn read_result(dir: &TaskDir) -> Option<Value> {
    dir.read_file(RESULT_FILE, MAX_RESULT_READ_BYTES)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

pub(super) fn ledger(project: Option<&str>) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    print_json(&build(&project_name, &main)?)
}

pub(super) fn build(project_name: &str, main: &Path) -> Result<Value> {
    let settings = load_task_config(main).map_err(|error| anyhow::anyhow!(error))?;
    let tasks = project_tasks(&wsx_core::task::store::project_id(project_name, main))?;
    let records: Vec<&TaskRecord> = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .collect();
    let mut open = Vec::new();
    let mut recent = Vec::new();
    for (dir, record) in &tasks {
        let Ok(record) = record else { continue };
        let summary = task_summary(dir, record);
        let title = clip(summary["title"].as_str().unwrap_or_default(), TITLE_CHARS);
        if !record.state.is_open() {
            recent.push((
                record.updated_unix_ms,
                json!({ "id": record.id, "title": title, "state": record.state, "revision": record.revision }),
            ));
            continue;
        }
        let result = read_result(dir);
        let log = dir.read_log().unwrap_or_default();
        let verdict = verification(&log, record.revision).map(|entry| {
            let mut value = serde_json::to_value(&entry.event).unwrap_or_default();
            value["unix_ms"] = json!(entry.unix_ms);
            value.as_object_mut().map(|fields| fields.remove("note"));
            value
        });
        open.push((
            attention(record.state),
            record.id,
            json!({
                "id": record.id,
                "title": title,
                "state": record.state,
                "revision": record.revision,
                "overlaps": overlaps(record, &records, false),
                "verification": verdict,
                "human_verify": result.as_ref().map_or(0, |r| r["result"]["human_verify"].as_array().map_or(0, Vec::len)),
                "summary": result.as_ref().and_then(|r| r["result"]["summary"].as_str()).map(|s| clip(s, SUMMARY_CHARS)),
                "pr_url": record.pr_url,
            }),
        ));
    }
    open.sort_by_key(|(order, id, _)| (*order, *id));
    recent.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    recent.truncate(RECENT_TASKS);
    let mut ledger = json!({
        "project": project_name,
        "main": main_checkout(main),
        "deploys": deploy_status(project_name, main, &settings, false).unwrap_or(Value::Null),
        "recent": recent.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "open": open.iter().map(|(_, _, row)| row.clone()).collect::<Vec<_>>(),
        "open_omitted": 0,
        "approvals": wsx_core::task::store::read_approvals(&wsx_core::task::store::project_id(project_name, main))
            .map(|lines| json!(wsx_core::task::manager::pending_approvals(&lines)))
            .unwrap_or(Value::Null),
    });
    // ^ Least urgent open tasks go first when the ledger is over its size.
    let mut omitted = 0;
    while serde_json::to_vec(&ledger)?.len() > MAX_LEDGER_BYTES {
        let rows = ledger["open"].as_array_mut().expect("open is an array");
        if rows.pop().is_none() {
            ledger["recent"] = json!([]);
            break;
        }
        omitted += 1;
        ledger["open_omitted"] = json!(omitted);
    }
    Ok(ledger)
}

/// The main checkout's branch and how many files it has changed (tracked or new).
fn main_checkout(main: &Path) -> Value {
    let git = |args: &[&str]| {
        wsx_core::git::git_cmd(main)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    json!({
        "path": main,
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]).map(|branch| branch.trim().to_string()),
        "changed_files": git(&["status", "--porcelain"]).map(|status| status.lines().count()),
    })
}
