//! The start set: what the first prompt of a worker holds from the memory: the area briefs of
//! the project config, then the active rules whose `paths` meet its files (the owner's
//! decisions). No fact records: the paid evaluation found they gave no gain; `wsx memory
//! recall` and `search` find them on demand. Nothing is cut silently: what does not fit the
//! budget is named, with the file or slug that holds it.
//! ref: docs/memory.md "Start set"

use super::manage::{clip_to, decided};
use super::record::{Kind, Status};
use super::store::Store;
use crate::task::view::meet;
use std::io::Read;
use std::path::{Component, Path};

/// The bytes of the start block of a task (the task context's budget since memos).
pub const BUDGET_BYTES: usize = 4 * 1024;
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

/// One piece of the block, and the name it has in the line of what is not shown.
struct Piece {
    name: String,
    text: String,
    /// A brief may be cut to its head; a rule is shown whole or named, as half a rule misleads.
    cuttable: bool,
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
    pieces.extend(rules(store, start.files));
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
    if !piece.cuttable {
        return None;
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
        cuttable: true,
    }
}

/// The active rules whose `paths` meet one of `files`, newest decision first, one piece each.
/// A rule with no `paths` joins no block: `--paths '**'` says it is for every task.
fn rules(store: &Store, files: &[String]) -> Vec<Piece> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut found = match store.records() {
        Ok(records) => records,
        Err(why) => {
            return vec![Piece {
                name: "rules".into(),
                text: format!("Rules could not be read: {why}"),
                cuttable: false,
            }]
        }
    };
    found.retain(|r| {
        r.kind == Kind::Rule
            && r.status == Status::Active
            && r.paths
                .iter()
                .any(|glob| files.iter().any(|f| meet(glob, f)))
    });
    found.sort_by(|a, b| decided(b).cmp(&decided(a)).then(a.slug.cmp(&b.slug)));
    found
        .into_iter()
        .map(|r| {
            let said = r
                .words
                .as_deref()
                .map_or(String::new(), |w| format!(", the owner's words: \"{w}\""));
            Piece {
                text: format!(
                    "Rule {} (paths {}){said}\n{}",
                    r.slug,
                    r.paths.join(", "),
                    r.text.trim_end()
                ),
                name: r.slug,
                cuttable: false,
            }
        })
        .collect()
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
