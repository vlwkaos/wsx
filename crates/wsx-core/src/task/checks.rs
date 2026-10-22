//! The project's "To check" list: the Human Verify items of landed tasks, which the owner
//! checks at their own time and before a public release.
//! ref: docs/tasks.md "To check"

use super::store::{
    create_private_dir, ensure_project_dir, open_locked, project_dir, read_bounded, TaskDir,
};
use super::{now_unix_ms, HumanVerify};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

pub const CHECKS_FILE: &str = "checks.jsonl";
const MAX_CHECKS_BYTES: u64 = 4 * 1024 * 1024;
/// Longest note on a failed item.
pub const MAX_NOTE_BYTES: usize = super::MAX_MESSAGE_BYTES;
const MAX_CAPTURE_BYTES: u64 = 2 * 1024 * 1024;
const CAPTURES: &str = "captures";

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
    /// Where a person looks (see `HumanVerify`).
    #[serde(default, rename = "where", skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// The capture kept in the task folder, such as `captures/r3-1.png`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
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
/// Each `capture` is already the name kept in the task folder, or empty.
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
                place: item.place.clone(),
                capture: Some(item.capture.clone()).filter(|name| !name.is_empty()),
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

/// Keep a worker's capture in the task folder as `captures/r<revision>-<index>.<ext>`: a PNG,
/// JPEG or WebP image of at most 2 MiB inside the working copy.
pub fn keep_capture(
    dir: &TaskDir,
    working_copy: &Path,
    revision: u32,
    index: usize,
    path: &str,
) -> Result<String> {
    let root = working_copy.canonicalize()?;
    let source = root
        .join(path)
        .canonicalize()
        .with_context(|| format!("no file {path} in the working copy"))?;
    if !source.starts_with(&root) {
        bail!("{path} is outside the working copy");
    }
    let bytes = read_bounded(&source, MAX_CAPTURE_BYTES)?;
    let (ext, _) =
        capture_type(&bytes).with_context(|| format!("{path} is not a PNG, JPEG or WebP image"))?;
    create_private_dir(&dir.file(CAPTURES))?;
    let name = format!("{CAPTURES}/r{revision}-{index}.{ext}");
    dir.write_file(&name, &bytes)?;
    Ok(name)
}

/// A kept capture's bytes and media type. Only names `keep_capture` writes are read.
pub fn read_capture(dir: &TaskDir, name: &str) -> Result<(Vec<u8>, &'static str)> {
    let file = name
        .strip_prefix(CAPTURES)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|file| {
            !file.is_empty()
                && file
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
                && !file.starts_with('.')
        })
        .with_context(|| format!("{name} is not a kept capture"))?;
    let bytes = read_bounded(&dir.file(CAPTURES).join(file), MAX_CAPTURE_BYTES)?;
    let (_, media) = capture_type(&bytes).context("the capture is not an image")?;
    Ok((bytes, media))
}

/// The image type by its first bytes.
fn capture_type(head: &[u8]) -> Option<(&'static str, &'static str)> {
    let at = |offset: usize, magic: &[u8]| head.get(offset..offset + magic.len()) == Some(magic);
    if at(0, b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if at(0, b"\xff\xd8\xff") {
        Some(("jpg", "image/jpeg"))
    } else if at(0, b"RIFF") && at(8, b"WEBP") {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}
