//! The manager's notes (`manager/notes/`): what a fresh conversation opens with, what it is
//! told is missing, the lines of the notes that match an owner's message, and how a finished
//! task's memo joins `context.md`. For the manager the files are views of the records
//! (`wsx_core::memory::manage`). ref: docs/tasks.md "Notes budget"

use super::manager::{read_note, CONTEXT_FILE, DECISIONS_FILE};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use wsx_core::task::store;
use wsx_core::task::view::clip;

/// The most of both files a fresh conversation's first message carries.
const MAX_NOTES_BYTES: usize = 10 * 1024;
/// What `context.md` keeps of that budget when `decisions.md` is long: the owner's rules come
/// first, so `decisions.md` takes the rest (6 KiB), and a short file leaves its part to the other.
const CONTEXT_FLOOR_BYTES: usize = 4 * 1024;
/// Past this many bytes the oldest memo lines move to `ARCHIVE_FILE`.
const CONTEXT_CEILING_BYTES: usize = 32 * 1024;
const ARCHIVE_FILE: &str = "context-archive.md";
/// The rules a later one replaced; a view of the records.
const DECISIONS_ARCHIVE_FILE: &str = "decisions-archive.md";
/// A memo line in `context.md`; the task's full memo stays in its result.
const MEMO_LINE_CHARS: usize = 240;
/// The lines and bytes of notes attached to one owner message, and one line's length.
const MATCH_LINES: usize = 5;
const MATCH_BYTES: usize = 1024;
const MATCH_LINE_CHARS: usize = 240;
/// Ranges named in one notice.
const NOTICE_RANGES: usize = 4;

/// Words that say nothing about a topic; a rare-word rule would otherwise let them through.
const STOP_WORDS: &[&str] = &[
    "about", "also", "been", "could", "does", "done", "from", "give", "have", "into", "just",
    "know", "look", "make", "need", "please", "show", "should", "than", "that", "their", "them",
    "then", "there", "they", "this", "tell", "want", "were", "what", "when", "where", "which",
    "will", "with", "would", "your",
];

/// One notes file as read: its lines in file order and the indexes the first message shows.
struct View {
    name: &'static str,
    path: PathBuf,
    /// The file the lines that left this one for its size are in.
    archive: &'static str,
    lines: Vec<String>,
    /// In display order.
    shown: Vec<usize>,
}

impl View {
    /// The lines with text that the first message leaves out, in file order.
    fn hidden(&self) -> Vec<usize> {
        (0..self.lines.len())
            .filter(|at| !self.lines[*at].trim().is_empty() && !self.shown.contains(at))
            .collect()
    }

    /// `lines 14-54` for the hidden lines (runs split only by blank lines join), at most
    /// `NOTICE_RANGES` of them.
    fn ranges(&self, hidden: &[usize]) -> String {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for &at in hidden {
            match runs.last_mut() {
                Some((_, end))
                    if self.lines[*end + 1..at]
                        .iter()
                        .all(|line| line.trim().is_empty()) =>
                {
                    *end = at
                }
                _ => runs.push((at, at)),
            }
        }
        let name = |(from, to): &(usize, usize)| match from == to {
            true => format!("{}", from + 1),
            false => format!("{}-{}", from + 1, to + 1),
        };
        let mut text: Vec<String> = runs.iter().take(NOTICE_RANGES).map(name).collect();
        if runs.len() > NOTICE_RANGES {
            text.push(format!("and {} more ranges", runs.len() - NOTICE_RANGES));
        }
        format!("lines {}", text.join(", "))
    }
}

/// The leading items of `order` whose lines fit `budget` bytes (a newline each).
fn fit(lines: &[String], order: impl Iterator<Item = usize>, budget: usize) -> Vec<usize> {
    let mut used = 0;
    order
        .take_while(|at| {
            used += lines[*at].len() + 1;
            used <= budget
        })
        .collect()
}

fn size(lines: &[String], at: &[usize]) -> usize {
    at.iter().map(|at| lines[*at].len() + 1).sum()
}

/// What a fresh conversation shows of the two files: the decisions newest first, then the
/// project section the manager wrote (up to half of its budget) and the newest memo lines.
fn split(notes: &Path) -> (View, View) {
    let decisions: Vec<String> = read_note(&notes.join(DECISIONS_FILE))
        .lines()
        .map(|line| line.trim().to_string())
        .collect();
    let context: Vec<String> = read_note(&notes.join(CONTEXT_FILE))
        .lines()
        .map(str::to_string)
        .collect();
    let by_newest: Vec<usize> = (0..decisions.len())
        .rev()
        .filter(|at| !decisions[*at].is_empty())
        .collect();
    // ^ The first lines are the project section, which says what the project is; the memo
    // lines (`- #<n>: ...`) are appended and the oldest go first.
    let (memos, section): (Vec<usize>, Vec<usize>) =
        (0..context.len()).partition(|at| context[*at].starts_with("- #"));
    let decisions_budget = MAX_NOTES_BYTES
        - CONTEXT_FLOOR_BYTES.min(size(&context, &section) + size(&context, &memos));
    let decisions_shown = fit(&decisions, by_newest.into_iter(), decisions_budget);
    let context_budget = MAX_NOTES_BYTES - size(&decisions, &decisions_shown);
    let section = fit(&context, section.into_iter(), context_budget / 2);
    let mut memos = fit(
        &context,
        memos.into_iter().rev(),
        context_budget - size(&context, &section),
    );
    memos.reverse();
    (
        View {
            name: DECISIONS_FILE,
            path: notes.join(DECISIONS_FILE),
            archive: DECISIONS_ARCHIVE_FILE,
            shown: decisions_shown,
            lines: decisions,
        },
        View {
            name: CONTEXT_FILE,
            path: notes.join(CONTEXT_FILE),
            archive: ARCHIVE_FILE,
            shown: section.into_iter().chain(memos).collect(),
            lines: context,
        },
    )
}

/// The notes a fresh conversation starts with. Whatever is left out is named with its line
/// ranges and the file's absolute path. ref: docs/tasks.md "Notes budget"
pub(super) fn preamble(notes: &Path) -> String {
    let (decisions, context) = split(notes);
    let mut out = String::new();
    for (view, title) in [
        (
            &decisions,
            "The owner's decisions (decisions.md, newest first):",
        ),
        (&context, "Your project context (context.md):"),
    ] {
        let hidden = view.hidden();
        if view.shown.is_empty() && hidden.is_empty() {
            continue;
        }
        out.push_str(title);
        out.push('\n');
        for at in &view.shown {
            out.push_str(&view.lines[*at]);
            out.push('\n');
        }
        if !hidden.is_empty() {
            out.push_str(&format!(
                "NOT SHOWN: {} {} of {} (over the start budget). Read {} for them; wsx also \
                 attaches the lines that match an owner message.\n",
                view.name,
                view.ranges(&hidden),
                view.lines.len(),
                view.path.display()
            ));
        }
        let archive = notes.join(view.archive);
        if !read_note(&archive).trim().is_empty() {
            out.push_str(&format!(
                "Older lines that left {} for its size are in {}.\n",
                view.name,
                archive.display()
            ));
        }
        out.push('\n');
    }
    out
}

/// The manager's memo as a record (a line of at most `MEMO_LINE_CHARS`); the views follow.
/// Returns the views that were edited by hand meanwhile. ref: docs/tasks.md "Memo"
pub(super) fn record_memo(
    store: &wsx_core::memory::Store,
    task: u32,
    memo: &str,
) -> Result<Vec<wsx_core::memory::manage::Drift>> {
    let added = wsx_core::memory::manage::add_memo(store, task, &clip(memo, MEMO_LINE_CHARS))?;
    Ok(added.drift)
}

/// A role's notes are files of its own, not records: add a finished task's memo to its
/// `context.md` as one line of at most `MEMO_LINE_CHARS`, unless
/// the file holds it. Past `CONTEXT_CEILING_BYTES` the oldest memo lines move to the archive
/// (written first, so a failed write loses none); the manager's own lines stay.
/// ref: docs/tasks.md "Memo"
pub(super) fn fold_memo(notes: &Path, task: u32, memo: &str) -> Result<()> {
    let path = notes.join(CONTEXT_FILE);
    let text = read_note(&path);
    let memo = clip(memo, MEMO_LINE_CHARS);
    let mut lines: Vec<&str> = text.lines().collect();
    if lines
        .iter()
        .any(|line| line.contains(memo.trim_end_matches('…')))
    {
        return Ok(());
    }
    let added = format!("- #{task}: {memo}");
    lines.push(&added);
    let mut moved: Vec<&str> = Vec::new();
    while lines.iter().map(|line| line.len() + 1).sum::<usize>() > CONTEXT_CEILING_BYTES {
        let Some(oldest) = lines.iter().position(|line| line.starts_with("- #")) else {
            break;
        };
        moved.push(lines.remove(oldest));
    }
    if !moved.is_empty() {
        let archive = notes.join(ARCHIVE_FILE);
        let old = read_note(&archive);
        store::write_note(&archive, &format!("{old}{}\n", moved.join("\n")))?;
    }
    store::write_note(&path, &(lines.join("\n") + "\n"))
}

/// The words of `text` that name a topic: lowercase, cut to 6 characters so `restart` meets
/// `restarted`; ASCII words need 4 characters, others 2.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= if word.is_ascii() { 4 } else { 2 })
        .map(str::to_lowercase)
        .filter(|word| !STOP_WORDS.contains(&word.as_str()))
        .map(|word| word.chars().take(6).collect())
        .collect()
}

/// The lines of the notes and of earlier conversations that share a rare word with `message`,
/// that the first message of the conversation did not show: lines it left out of both files,
/// their archives, and `earlier` (the exchanges before the last rotation). A word is rare when at
/// most a quarter of those lines hold it; lines rank by the rarity of the words they share.
/// At most `MATCH_LINES` lines and `MATCH_BYTES` bytes. Empty when nothing matches.
/// ref: docs/tasks.md "Notes budget"
pub(super) fn matching(
    notes: &Path,
    message: &str,
    earlier: impl FnOnce() -> Vec<String>,
) -> String {
    let wanted = words(message);
    if wanted.is_empty() {
        return String::new();
    }
    let (decisions, context) = split(notes);
    let mut pool: Vec<(String, Option<PathBuf>, String)> = Vec::new();
    for view in [&decisions, &context] {
        for at in view.hidden() {
            pool.push((
                format!("{}:{}", view.name, at + 1),
                Some(view.path.clone()),
                view.lines[at].clone(),
            ));
        }
    }
    for view in [&decisions, &context] {
        let archive = notes.join(view.archive);
        for (at, line) in read_note(&archive).lines().enumerate() {
            pool.push((
                format!("{}:{}", view.archive, at + 1),
                Some(archive.clone()),
                line.to_string(),
            ));
        }
    }
    for line in earlier() {
        pool.push(("earlier conversation".to_string(), None, line));
    }
    let sets: Vec<BTreeSet<String>> = pool.iter().map(|(_, _, line)| words(line)).collect();
    let total = sets.len() as f64;
    let rare_at = (sets.len() / 4).max(1);
    let df = |word: &String| sets.iter().filter(|set| set.contains(word)).count();
    let weights: Vec<(&String, usize)> = wanted.iter().map(|word| (word, df(word))).collect();
    let mut ranked: Vec<(f64, usize)> = sets
        .iter()
        .enumerate()
        .filter_map(|(at, set)| {
            let shared: Vec<&(&String, usize)> = weights
                .iter()
                .filter(|(word, _)| set.contains(*word))
                .collect();
            let rare = shared.iter().any(|(_, df)| *df <= rare_at);
            let score: f64 = shared
                .iter()
                .map(|(_, df)| (1.0 + total / *df as f64).ln())
                .sum();
            rare.then_some((score, at))
        })
        .collect();
    // ^ Equal scores: the later line, which is the newer.
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = String::new();
    let mut files: Vec<String> = Vec::new();
    for (_, at) in ranked.into_iter().take(MATCH_LINES) {
        let (label, path, text) = &pool[at];
        let line = format!("- {label}: {}\n", clip(text.trim(), MATCH_LINE_CHARS));
        if out.len() + line.len() > MATCH_BYTES {
            break;
        }
        out.push_str(&line);
        if let Some(path) = path.as_ref().map(|path| path.display().to_string()) {
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    match out.is_empty() {
        true => out,
        false => format!(
            "Notes that match this message and are not in your context{}:\n{out}\n",
            match files.is_empty() {
                true => String::new(),
                false => format!(" (read {})", files.join(", ")),
            }
        ),
    }
}
