//! The start set: what the first prompt of a worker holds from the memory: the area briefs of
//! the project config. No fact records: the paid evaluation found
//! they gave no gain (docs/design/memory-eval.md); `wsx memory recall` and `search` find them on
//! demand. Nothing is cut silently: what does not fit the budget is named, with the file that
//! holds it. This module also holds the one rule that says a fact is stale (search marks it).
//! ref: docs/memory.md "Start set", docs/design/memory-system.md "Who gets what"

use super::manage::clip_to;
use super::record::{Kind, Record};
use super::store::{foreign_task, registered, Store};
use crate::git::{git_cmd, output_with_timeout_limit};
use crate::task::view::meet;
use std::io::Read;
use std::path::{Component, Path};
use std::time::Duration;

/// The bytes of the start block of a task (the task context's budget since memos).
pub const BUDGET_BYTES: usize = 4 * 1024;
/// `git diff --quiet` of one fact, as in the ledger.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes kept for the line that names what is not shown.
const RESERVE: usize = 320;
/// The least of a brief worth a cut; a smaller room names it as not shown.
const MIN_CUT: usize = 200;
const MAX_BRIEF_BYTES: u64 = 8 * 1024;
/// Names listed in the line of what is not shown.
const NAMES_SHOWN: usize = 8;

/// What a worker is about to do: the `--files` globs of its task, the `memory.areas` of the
/// project config as `(glob, brief file)`.
pub struct Start<'a> {
    pub files: &'a [String],
    pub areas: &'a [(String, String)],
}

fn is_hash(commit: &str) -> bool {
    (7..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `git diff --quiet <commit> HEAD -- <paths>` shows a change. None: git could not
/// answer (it did not start, or took more than `GIT_TIMEOUT`).
fn changed(root: &Path, commit: &str, paths: &[String]) -> Option<bool> {
    // ^ A commit is a hash: anything else could pass for an option.
    if !is_hash(commit) {
        return Some(true);
    }
    let mut cmd = git_cmd(root);
    cmd.args(["diff", "--quiet", commit, "HEAD", "--"])
        .args(paths);
    let output = output_with_timeout_limit(&mut cmd, GIT_TIMEOUT, 64 * 1024).ok()?;
    Some(!output.status.success())
}

/// A fact with `commit` and `paths` is stale when those paths differ between the commit and
/// `HEAD` of the project: it was true then, and the code moved. A fact git cannot answer for
/// counts as stale, so it never joins a prompt unchecked. Other records are never stale.
pub fn stale(store: &Store, r: &Record) -> bool {
    stale_of(store, r).unwrap_or(true)
}

fn stale_of(store: &Store, r: &Record) -> Option<bool> {
    match &r.commit {
        Some(commit) if stale_of_needs_git(r) => {
            // ^ The paths of a fact verified by another project's task (`task:<project>#<n>`) name
            // that project's files: git runs in its folder. A project that is no longer
            // registered cannot answer.
            let root = match r.verified_by.as_deref().and_then(foreign_task) {
                Some((name, _)) => registered(name)?.1,
                None => store.root().to_path_buf(),
            };
            changed(&root, commit, &r.paths)
        }
        _ => Some(false),
    }
}

/// One piece of the block, and the name it has in the line of what is not shown.
struct Piece {
    name: String,
    text: String,
}

/// The start block of a task, within `budget` bytes; empty when there is nothing for it.
pub fn block(store: &Store, start: &Start, budget: usize) -> String {
    let mut pieces = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (glob, file) in start.areas {
        if start.files.iter().any(|f| meet(glob, f)) && !seen.contains(&file.as_str()) {
            seen.push(file);
            pieces.push(area(store.root(), glob, file));
        }
    }
    let mut room = budget.saturating_sub(RESERVE);
    let mut out = Vec::new();
    let mut left = Vec::new();
    for piece in pieces {
        match fit(&piece, room) {
            Some(text) => {
                room = room.saturating_sub(text.len() + 1);
                out.push(text);
            }
            None => left.push(piece.name),
        }
    }
    if !left.is_empty() {
        out.push(not_shown(&left, budget));
    }
    out.join("\n")
}

/// The whole piece, its head with the cut named, or none when there is no room for a head.
fn fit(piece: &Piece, room: usize) -> Option<String> {
    if piece.text.len() < room {
        return Some(piece.text.clone());
    }
    let note = format!(
        "\n[cut: {} bytes of {name}; read {name}]",
        piece.text.len(),
        name = piece.name
    );
    let keep = room
        .checked_sub(note.len())
        .filter(|keep| *keep >= MIN_CUT)?;
    Some(format!("{}{note}", clip_to(&piece.text, keep)))
}

/// The brief of an area, read from the project (it stays inside the project folder).
fn area(root: &Path, glob: &str, file: &str) -> Piece {
    let text = match read_brief(root, file) {
        Ok(body) => format!("Area brief {file} (files {glob}):\n{}", body.trim_end()),
        Err(why) => format!("Area brief {file} (files {glob}) could not be read: {why}"),
    };
    Piece {
        name: file.into(),
        text,
    }
}

fn read_brief(root: &Path, file: &str) -> Result<String, String> {
    let plain = !file.is_empty()
        && Path::new(file)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    let real = plain
        .then(|| std::fs::canonicalize(root.join(file)).ok())
        .flatten();
    let inside = real
        .filter(|path| std::fs::canonicalize(root).is_ok_and(|root| path.starts_with(root)))
        .ok_or("no such file in the project")?;
    let mut bytes = Vec::new();
    std::fs::File::open(inside)
        .and_then(|f| f.take(MAX_BRIEF_BYTES).read_to_end(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn stale_of_needs_git(r: &Record) -> bool {
    r.kind == Kind::Fact && r.commit.is_some() && !r.paths.is_empty()
}

/// The names of up to `NAMES_SHOWN` of `slugs`, and how many more there are.
fn names(slugs: &[String]) -> String {
    let shown = slugs
        .iter()
        .take(NAMES_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match slugs.len().saturating_sub(NAMES_SHOWN) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

fn not_shown(left: &[String], budget: usize) -> String {
    format!(
        "Not shown, over the {budget}-byte start budget: {}.",
        names(left)
    )
}
