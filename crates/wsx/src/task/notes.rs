//! The manager's notes (`notes/` in the chat's folder): what a fresh conversation opens with,
//! what it is told is missing, and how a finished task's memo joins `context.md`. For the manager
//! the files are views of the records (`wsx_core::memory::manage`). ref: docs/tasks.md "Notes budget"

use super::manager::{read_note, CONTEXT_FILE, DECISIONS_FILE};
use anyhow::Result;
use std::path::{Path, PathBuf};
use wsx_core::memory::backend;
use wsx_core::memory::{KindFilter, Query, Store};
use wsx_core::task::text::first_line;

/// The most of both files a fresh conversation's first message carries.
const MAX_NOTES_BYTES: usize = 10 * 1024;
/// What `context.md` keeps of that budget when `decisions.md` is long: the owner's rules come
/// first, so `decisions.md` takes the rest (6 KiB), and a short file leaves its part to the other.
const CONTEXT_FLOOR_BYTES: usize = 4 * 1024;
/// Ranges named in one notice.
const NOTICE_RANGES: usize = 4;
/// The records an owner message gets from the memory.
const MATCH_RECORDS: usize = 5;

/// One notes file as read: its lines in file order and the indexes the first message shows.
struct View {
    name: &'static str,
    path: PathBuf,
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
            shown: decisions_shown,
            lines: decisions,
        },
        View {
            name: CONTEXT_FILE,
            path: notes.join(CONTEXT_FILE),
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
                "NOT SHOWN: {} {} of {} (over the start budget). Read {} for them, or find them \
                 with `wsx task search`.\n",
                view.name,
                view.ranges(&hidden),
                view.lines.len(),
                view.path.display()
            ));
        }
        out.push('\n');
    }
    out
}

/// What an owner message gets from the project's memory: its best records of the `rule` and
/// `fact` kinds, as lines under a heading; empty when none matches. The one search of
/// `wsx memory search` ranks them, over the records alone. ref: docs/tasks.md "Notes budget"
pub(super) fn matching(store: Store, said: &str) -> String {
    let store = [store.without_transcripts()];
    let mut hits = Vec::new();
    for kind in ["rule", "fact"] {
        let query = Query {
            words: said.into(),
            limit: MATCH_RECORDS,
            include_inactive: false,
            kind: kind.parse::<KindFilter>().ok(),
        };
        hits.extend(backend::search(&store, None, &query).map_or_else(|_| Vec::new(), |a| a.hits));
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(MATCH_RECORDS);
    if hits.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("Memory records that match this message (`wsx memory search` finds more):\n");
    for hit in hits {
        out.push_str(&format!(
            "- {} {}: {}\n",
            hit.kind,
            hit.slug,
            first_line(&hit.excerpt)
        ));
    }
    out.push('\n');
    out
}

/// The manager's memo as a record (a line of at most `MEMO_LINE_CHARS`); the views follow.
/// `landed` is the commit and diff paths of a landed task; `project` the registered project
/// that ran the task when it is not the store's own. Returns the views that were edited
/// by hand meanwhile. ref: docs/tasks.md "Memo"
pub(super) fn record_memo(
    store: &wsx_core::memory::Store,
    project: Option<&str>,
    task: u32,
    memo: &str,
    landed: Option<(&str, &[String])>,
) -> Result<Vec<wsx_core::memory::manage::Drift>> {
    let (commit, paths) = landed.map_or((None, &[][..]), |(commit, paths)| (Some(commit), paths));
    Ok(wsx_core::memory::manage::capture(store, project, task, memo, commit, paths)?.drift)
}
