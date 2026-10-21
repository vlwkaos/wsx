//! The project's "To check" list: the Human Verify items of landed tasks, which the owner
//! checks at their own time and before a public release.
//! ref: docs/tasks.md "To check"

use super::store::{ensure_project_dir, open_locked, project_dir, read_bounded};
use super::{now_unix_ms, HumanVerify};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Write};

pub const CHECKS_FILE: &str = "checks.jsonl";
const MAX_CHECKS_BYTES: u64 = 4 * 1024 * 1024;
/// Longest note on a failed item.
pub const MAX_NOTE_BYTES: usize = super::MAX_MESSAGE_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Open,
    Passed,
    Failed,
}

/// One line of `checks.jsonl`. The file only grows: a later line with the same id is the
/// item's current row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckItem {
    /// `c<n>`, numbered in the order items were collected.
    pub id: String,
    pub task: u32,
    pub revision: u32,
    pub commit: String,
    pub check: String,
    pub pass_if: String,
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The task a fail started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_task: Option<u32>,
    pub unix_ms: u64,
}

/// Every item in its current row, in the order collected.
pub fn read(project_id: &str) -> Result<Vec<CheckItem>> {
    let path = project_dir(project_id)?.join(CHECKS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(fold(&read_bounded(&path, MAX_CHECKS_BYTES)?))
}

pub fn open_items(project_id: &str) -> Result<Vec<CheckItem>> {
    let mut items = read(project_id)?;
    items.retain(|item| item.state == CheckState::Open);
    Ok(items)
}

fn fold(bytes: &[u8]) -> Vec<CheckItem> {
    let mut items: Vec<CheckItem> = Vec::new();
    for item in bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<CheckItem>(line).ok())
    {
        match items.iter_mut().find(|known| known.id == item.id) {
            Some(known) => *known = item,
            None => items.push(item),
        }
    }
    items
}

/// Change the list under a lock on the file: `change` gets the current items and returns
/// the rows to append (new items with an empty id get the next `c<n>`). Returns them as written.
pub fn change(
    project_id: &str,
    change: impl FnOnce(&[CheckItem]) -> Result<Vec<CheckItem>>,
) -> Result<Vec<CheckItem>> {
    let path = ensure_project_dir(project_id)?.join(CHECKS_FILE);
    let mut file = open_locked(
        &path,
        OpenOptions::new().read(true).append(true).create(true),
    )?;
    let mut bytes = Vec::new();
    (&file).take(MAX_CHECKS_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CHECKS_BYTES {
        bail!("{} is full ({MAX_CHECKS_BYTES} bytes)", path.display());
    }
    let items = fold(&bytes);
    let mut rows = change(&items)?;
    let mut next = items.len() + 1;
    let mut out = Vec::new();
    for row in &mut rows {
        if row.id.is_empty() {
            row.id = format!("c{next}");
            next += 1;
        }
        out.extend(serde_json::to_vec(row)?);
        out.push(b'\n');
    }
    file.write_all(&out)
        .with_context(|| format!("cannot append to {}", path.display()))?;
    Ok(rows)
}

/// Add the Human Verify items of a landed revision as open items, once per task and revision.
pub fn collect(
    project_id: &str,
    task: u32,
    revision: u32,
    commit: &str,
    verify: &[HumanVerify],
) -> Result<Vec<CheckItem>> {
    if verify.is_empty() {
        return Ok(Vec::new());
    }
    change(project_id, |items| {
        if items
            .iter()
            .any(|item| item.task == task && item.revision == revision)
        {
            return Ok(Vec::new());
        }
        let now = now_unix_ms();
        Ok(verify
            .iter()
            .map(|item| CheckItem {
                id: String::new(),
                task,
                revision,
                commit: commit.to_string(),
                check: item.check.clone(),
                pass_if: item.pass_if.clone(),
                state: CheckState::Open,
                note: None,
                fix_task: None,
                unix_ms: now,
            })
            .collect())
    })
}

/// Mark open items passed: the named ones, or every open one when `ids` is empty.
/// An unknown id or one that is not open refuses the whole call.
pub fn pass(project_id: &str, ids: &[String]) -> Result<Vec<CheckItem>> {
    change(project_id, |items| {
        let chosen = if ids.is_empty() {
            items
                .iter()
                .filter(|item| item.state == CheckState::Open)
                .collect()
        } else {
            ids.iter()
                .map(|id| find_open(items, id))
                .collect::<Result<Vec<_>>>()?
        };
        let now = now_unix_ms();
        Ok(chosen
            .into_iter()
            .map(|item| CheckItem {
                state: CheckState::Passed,
                unix_ms: now,
                ..item.clone()
            })
            .collect())
    })
}

/// The open item `id`, or why it cannot change.
pub fn find_open<'a>(items: &'a [CheckItem], id: &str) -> Result<&'a CheckItem> {
    let item = items
        .iter()
        .find(|item| item.id == id)
        .with_context(|| format!("no item {id}; `wsx task check` lists the open ones"))?;
    if item.state != CheckState::Open {
        bail!("item {id} is already {}", state_name(item.state));
    }
    Ok(item)
}

/// Mark the open item `id` failed with the owner's note and the task that fixes it.
pub fn fail(project_id: &str, id: &str, note: &str, fix_task: u32) -> Result<CheckItem> {
    let rows = change(project_id, |items| {
        let item = find_open(items, id)?;
        Ok(vec![CheckItem {
            state: CheckState::Failed,
            note: Some(note.to_string()),
            fix_task: Some(fix_task),
            unix_ms: now_unix_ms(),
            ..item.clone()
        }])
    })?;
    rows.into_iter().next().context("the fail wrote no row")
}

pub fn state_name(state: CheckState) -> &'static str {
    match state {
        CheckState::Open => "open",
        CheckState::Passed => "passed",
        CheckState::Failed => "failed",
    }
}
