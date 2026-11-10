//! Ranked search across projects: the manager chats and role chats, the manager's notes, and
//! each task's title and results. One digest per project keeps the words of what was said, not
//! the tool noise; it is built from the logs, caught up on read and rebuilt on request.
//! ref: docs/tasks.md "Search"

use super::manager::{says_nothing, split_quote, Author, ManagerEntry, ManagerEvent, QUOTE_REF};
use super::store::{self, contains_bytes, ManagerDir, TaskDir, FIRST_READ_BYTES, RESULT_FILE};
use super::text::first_line;
use super::{history, roles, view};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const DIGEST_FILE: &str = "search.jsonl";
const LOCK_FILE: &str = "search.lock";
/// A record keeps this much of a text; the log and the task keep the rest.
const MAX_TEXT_BYTES: usize = 4096;
/// A `History` record keeps this much of a line; the log keeps the rest.
const MAX_HISTORY_BYTES: usize = 1024;
/// The longest note file read.
const MAX_NOTES_BYTES: u64 = 2 * 1024 * 1024;
/// The longest digest read: past it the search says to reindex or to build the inverted index
/// ("Search" in docs/tasks.md names the trigger).
const MAX_DIGEST_BYTES: u64 = 1 << 30;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An owner's, another manager's or a role's message.
    Message,
    /// A manager's reply that says something.
    Reply,
    /// A task's title and prompt.
    Task,
    /// A task's latest result: summary, memo, decisions, what is left open.
    Result,
    /// A line of a task's story: an add or a steer, a turn's result, a revision, its checks, a
    /// stop, its landing or close (`history::line`); read from the task's log.
    History,
    /// A line of the manager's `notes/decisions.md`; read from the file, never kept.
    Decision,
    /// A line of the manager's `notes/context.md` or its archive; read from the file.
    Memo,
    /// The newest id a catch-up scanned, so the next one stops there; no text.
    Mark,
}

/// One searchable record, a line of a digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Doc {
    kind: Kind,
    /// The role whose chat it is; none is the manager's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<u32>,
    at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    text: String,
    /// A `History` line: the byte offset after it in the task's log, so a catch-up reads the log
    /// from the newest one it has.
    #[serde(default, skip_serializing_if = "is_zero")]
    end: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A project to search: its registered name and its store id.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub project_id: String,
}

/// What a search found at one place, ranked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub project: String,
    pub kind: Kind,
    /// The role of a role chat's message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The message id in that chat, or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<u32>,
    pub unix_ms: u64,
    pub by: String,
    pub line: String,
    pub score: f64,
    /// Where it is: `<project>:<id>`, `<project>:<role>:<id>`, `<project>#<task>`, or
    /// `<project>:notes/<file>`.
    pub reference: String,
    /// The first line of a reply that quotes it by id (`resolve_quote`): only a message of the
    /// manager chat of the project the search ran in can be quoted by id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// Where the hit's task is now ("landed 11-07 cd67238e1f90", "closed: discarded", "ready r2"),
    /// read when the search ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Found {
    pub hits: Vec<Hit>,
    /// Records searched and projects they came from.
    pub docs: usize,
    pub projects: usize,
    /// A project that could not be read, with the reason; the others are searched.
    pub problems: Vec<String>,
}

/// The hits for `words` in `targets`, best first, at most `limit`. `current`: the project the
/// asker works in, whose manager chat a hit can quote by id. `reindex` rebuilds the digests
/// from the logs and the task files first. `only`: the hits of this task alone.
/// ref: docs/tasks.md "Search"
pub fn search(
    targets: &[Target],
    current: Option<&str>,
    words: &str,
    limit: usize,
    reindex: bool,
    only: Option<u32>,
) -> Result<Found> {
    let words = words.to_lowercase();
    let mut terms: Vec<&str> = Vec::new();
    each_token(&words, |token| {
        if !terms.contains(&token) {
            terms.push(token)
        }
    });
    if terms.is_empty() {
        bail!("search needs some words");
    }
    let mut found = Found::default();
    let mut docs: Vec<(usize, Doc)> = Vec::new();
    for (at, target) in targets.iter().enumerate() {
        match gather(target, reindex) {
            Ok(list) => {
                found.projects += 1;
                docs.extend(
                    list.into_iter()
                        .filter(|doc| only.is_none_or(|task| doc.task == Some(task)))
                        .map(|doc| (at, doc)),
                );
            }
            Err(error) => found.problems.push(format!("{}: {error:#}", target.name)),
        }
    }
    // ^ A search that could read no project at all failed; with several, the rest answer.
    if found.projects == 0 && !found.problems.is_empty() {
        bail!("{}", found.problems.join("; "));
    }
    found.docs = docs.len();
    let index: HashMap<&str, usize> = terms.iter().enumerate().map(|(i, t)| (*t, i)).collect();
    // ^ First pass: each record's length and its count of every term; the second scores them
    // with the term frequencies of all projects together.
    let mut seen: Vec<(usize, usize, Vec<u32>)> = Vec::new();
    let mut df = vec![0u32; terms.len()];
    let mut total = 0usize;
    for (at, (_, doc)) in docs.iter().enumerate() {
        let lower = doc.text.to_lowercase();
        let mut counts = vec![0u32; terms.len()];
        let mut length = 0usize;
        each_token(&lower, |token| {
            length += 1;
            if let Some(&term) = index.get(token) {
                counts[term] += 1;
            }
        });
        total += length;
        if counts.iter().any(|count| *count > 0) {
            for (term, count) in counts.iter().enumerate() {
                df[term] += u32::from(*count > 0);
            }
            seen.push((at, length, counts));
        }
    }
    let count = docs.len() as f64;
    let average = (total as f64 / count.max(1.0)).max(1.0);
    let mut scored: Vec<(f64, usize)> = seen
        .iter()
        .map(|(at, length, counts)| {
            let score = counts
                .iter()
                .enumerate()
                .filter(|(_, tf)| **tf > 0)
                .map(|(term, tf)| {
                    let (tf, df) = (f64::from(*tf), f64::from(df[term]));
                    let idf = (1.0 + (count - df + 0.5) / (df + 0.5)).ln();
                    idf * tf * (BM25_K1 + 1.0)
                        / (tf + BM25_K1 * (1.0 - BM25_B + BM25_B * *length as f64 / average))
                })
                .sum();
            (score, *at)
        })
        .collect();
    // ^ Equal scores: the newer first.
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(docs[b.1].1.at.cmp(&docs[a.1].1.at))
    });
    found.hits = scored
        .into_iter()
        .take(limit)
        .map(|(score, at)| {
            let (target, doc) = &docs[at];
            let mut found = hit(&targets[*target].name, doc, score, current);
            found.outcome = doc.task.and_then(|task| outcome(&targets[*target], task));
            found
        })
        .collect();
    Ok(found)
}

/// Where task `task` is now, for a hit that names it; none when its files are gone.
fn outcome(target: &Target, task: u32) -> Option<String> {
    let dir = TaskDir::open(&target.project_id, task).ok()?;
    Some(history::outcome(&dir, &dir.read_record().ok()?))
}

fn hit(project: &str, doc: &Doc, score: f64, current: Option<&str>) -> Hit {
    let role = Some(doc.role.clone()).filter(|role| !role.is_empty());
    let reference = match (doc.kind, doc.id, doc.task) {
        (Kind::Message | Kind::Reply, Some(id), _) => match &role {
            Some(role) => format!("{project}:{role}:{id}"),
            None => format!("{project}:{id}"),
        },
        (Kind::Decision, ..) => format!("{project}:notes/decisions.md"),
        (Kind::Memo, ..) => format!("{project}:notes/context.md"),
        (_, _, Some(task)) => format!("{project}#{task}"),
        _ => project.to_string(),
    };
    let quote = match (doc.kind, doc.id) {
        (Kind::Message | Kind::Reply, Some(id)) if role.is_none() && current == Some(project) => {
            Some(format!("{QUOTE_REF}{id}"))
        }
        _ => None,
    };
    Hit {
        project: project.to_string(),
        kind: doc.kind,
        role,
        id: doc.id,
        task: doc.task,
        unix_ms: doc.at,
        by: doc.by.clone(),
        line: first_line(&doc.text),
        score,
        reference,
        quote,
        outcome: None,
    }
}

/// Calls `f` with each word of lowercase `text`: a run of letters and digits is one token; a
/// run of Han, Kana or Hangul gives each pair of neighbours (a lone character gives itself),
/// as these scripts have no spaces and a word carries particles. The query is cut the same way.
fn each_token<'a>(text: &'a str, mut f: impl FnMut(&'a str)) {
    let mut word: Option<usize> = None;
    // ^ The start and the length in characters of the current CJK run, and where its last
    // character begins.
    let mut run: Option<(usize, usize, usize)> = None;
    let close_run =
        |run: &mut Option<(usize, usize, usize)>, end: usize, f: &mut dyn FnMut(&'a str)| {
            if let Some((start, 1, _)) = run.take() {
                f(&text[start..end]);
            }
        };
    for (at, c) in text.char_indices() {
        if is_cjk(c) {
            if let Some(start) = word.take() {
                f(&text[start..at]);
            }
            run = Some(match run {
                None => (at, 1, at),
                Some((start, chars, last)) => {
                    f(&text[last..at + c.len_utf8()]);
                    (start, chars + 1, at)
                }
            });
        } else if c.is_alphanumeric() {
            close_run(&mut run, at, &mut f);
            word.get_or_insert(at);
        } else {
            close_run(&mut run, at, &mut f);
            if let Some(start) = word.take() {
                f(&text[start..at]);
            }
        }
    }
    close_run(&mut run, text.len(), &mut f);
    if let Some(start) = word {
        f(&text[start..]);
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Every record of the project: its digest, caught up with the logs and the task files, and
/// its notes. `reindex` drops the digest first.
fn gather(target: &Target, reindex: bool) -> Result<Vec<Doc>> {
    let dir = store::project_dir(&target.project_id)?;
    if !std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
        return Ok(Vec::new());
    }
    // ^ One catch-up at a time per project; the lock holds until the file closes.
    let _lock = store::open_locked(
        &dir.join(LOCK_FILE),
        OpenOptions::new().write(true).create(true),
    )?;
    let path = dir.join(DIGEST_FILE);
    if reindex {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot remove {}", path.display()))
            }
        }
    }
    let (mut docs, torn) = load(&path)?;
    let mut fresh = Vec::new();
    catch_up_chats(&target.project_id, &docs, &mut fresh)?;
    catch_up_tasks(&target.project_id, &docs, &mut fresh)?;
    if !fresh.is_empty() {
        let mut bytes = Vec::new();
        // ^ A crash can leave a line cut: the next record starts on a line of its own.
        if torn {
            bytes.push(b'\n');
        }
        for doc in &fresh {
            serde_json::to_writer(&mut bytes, doc)?;
            bytes.push(b'\n');
        }
        store::open_nofollow(&path, OpenOptions::new().append(true).create(true))?
            .write_all(&bytes)
            .with_context(|| format!("cannot append to {}", path.display()))?;
        docs.extend(fresh);
        supersede(&mut docs);
    }
    docs.retain(|doc| doc.kind != Kind::Mark);
    docs.extend(notes(&target.project_id));
    Ok(docs)
}

/// The digest's records, and whether its last line is cut. A line that does not parse is
/// skipped.
fn load(path: &Path) -> Result<(Vec<Doc>, bool)> {
    let Some(bytes) = store::read_bounded_if_exists(path, MAX_DIGEST_BYTES).context(
        "cannot read the search digest (too large? `wsx task search --reindex` builds it again)",
    )?
    else {
        return Ok((Vec::new(), false));
    };
    let mut docs: Vec<Doc> = bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    supersede(&mut docs);
    Ok((docs, bytes.last().is_some_and(|last| *last != b'\n')))
}

/// A task's newest title record and newest result record replace the older ones.
fn supersede(docs: &mut Vec<Doc>) {
    let mut last: HashMap<(Kind, u32), usize> = HashMap::new();
    for (at, doc) in docs.iter().enumerate() {
        if let (Kind::Task | Kind::Result, Some(task)) = (doc.kind, doc.task) {
            last.insert((doc.kind, task), at);
        }
    }
    let mut at = 0;
    docs.retain(|doc| {
        at += 1;
        match (doc.kind, doc.task) {
            (Kind::Task | Kind::Result, Some(task)) => last[&(doc.kind, task)] == at - 1,
            _ => true,
        }
    });
}

/// The manager's and each role's messages and replies newer than the digest holds. Ids grow,
/// so the read from the log's end stops at the newest one it has.
fn catch_up_chats(project_id: &str, docs: &[Doc], fresh: &mut Vec<Doc>) -> Result<()> {
    let mut chats: Vec<(String, ManagerDir)> = Vec::new();
    if let Some(dir) = ManagerDir::existing(project_id)? {
        chats.push((String::new(), dir));
    }
    for role in roles::names(project_id) {
        if let Some(dir) = ManagerDir::existing_as(project_id, Some(&role))? {
            chats.push((role, dir));
        }
    }
    for (role, dir) in chats {
        let seen = docs
            .iter()
            .filter(|doc| doc.role == role)
            .filter_map(|doc| doc.id)
            .max();
        let log = dir.read_back_where(
            FIRST_READ_BYTES,
            |line| {
                contains_bytes(line, br#""kind":"message""#)
                    || contains_bytes(line, br#""kind":"reply""#)
            },
            |log| {
                seen.is_some_and(|seen| {
                    log.iter()
                        .any(|entry| entry.id().is_some_and(|id| id <= seen))
                })
            },
        )?;
        let mut top = seen;
        for entry in &log {
            let Some(id) = entry.id().filter(|id| seen.is_none_or(|seen| *id > seen)) else {
                continue;
            };
            top = top.max(Some(id));
            fresh.extend(chat_doc(entry, &role));
        }
        if top != seen
            && !fresh
                .last()
                .is_some_and(|doc| doc.role == role && doc.id == top)
        {
            fresh.push(Doc {
                kind: Kind::Mark,
                role,
                id: top,
                task: None,
                at: 0,
                by: String::new(),
                text: String::new(),
                end: 0,
            });
        }
    }
    Ok(())
}

/// The record of a chat entry: what the owner, a manager or a role said, and a reply that says
/// something. Wsx's events, recaps and quiet replies are not conversation.
fn chat_doc(entry: &ManagerEntry, role: &str) -> Option<Doc> {
    let (kind, id, by, text) = match &entry.event {
        ManagerEvent::Message { id, from, text, .. } => {
            let by = match from {
                Author::Owner => "owner".to_string(),
                Author::Manager { project, .. } => format!("manager:{project}"),
                Author::Role { name, .. } => format!("role:{name}"),
                Author::Events | Author::Recap => return None,
            };
            (Kind::Message, *id, by, text)
        }
        ManagerEvent::Reply {
            id,
            text,
            quiet: false,
            ..
        } if !says_nothing(text) => {
            let by = Some(role)
                .filter(|role| !role.is_empty())
                .unwrap_or("manager");
            (Kind::Reply, *id, by.to_string(), text)
        }
        _ => return None,
    };
    // ^ The words of the message; the line it quotes is another message's.
    let body = split_quote(text).1.trim();
    (!body.is_empty()).then(|| Doc {
        kind,
        role: role.to_string(),
        id: Some(id),
        task: None,
        at: entry.unix_ms,
        by,
        text: clip(body),
        end: 0,
    })
}

/// Each task's title and prompt once, and its result again when `result.json` is newer than the
/// record kept.
fn catch_up_tasks(project_id: &str, docs: &[Doc], fresh: &mut Vec<Doc>) -> Result<()> {
    let kept: HashMap<(Kind, u32), &Doc> = docs
        .iter()
        .filter_map(|doc| Some(((doc.kind, doc.task?), doc)))
        .collect();
    // ^ Per task, how far its log is read: the end of the newest line kept.
    let mut scanned: HashMap<u32, u64> = HashMap::new();
    for doc in docs.iter().filter(|doc| doc.kind == Kind::History) {
        if let Some(task) = doc.task {
            let end = scanned.entry(task).or_default();
            *end = (*end).max(doc.end);
        }
    }
    for task in store::task_numbers(project_id)? {
        let Ok(dir) = TaskDir::open(project_id, task) else {
            continue;
        };
        catch_up_history(&dir, task, scanned.get(&task).copied().unwrap_or(0), fresh);
        if !kept.contains_key(&(Kind::Task, task)) {
            if let Ok(record) = dir.read_record() {
                let title = view::title(&dir, &record).unwrap_or_default();
                let prompt = dir.prompt().ok().flatten().unwrap_or_default();
                fresh.push(Doc {
                    kind: Kind::Task,
                    role: String::new(),
                    id: None,
                    task: Some(task),
                    at: record.created_unix_ms,
                    by: "task".into(),
                    text: clip(&format!("{title}\n{prompt}")),
                    end: 0,
                });
            }
        }
        let Ok(meta) = std::fs::metadata(dir.file(RESULT_FILE)) else {
            continue;
        };
        let at = (meta.mtime() as u64) * 1000 + (meta.mtime_nsec() as u64) / 1_000_000;
        let old = kept.get(&(Kind::Result, task));
        if old.is_some_and(|old| old.at >= at) {
            continue;
        }
        let text = view::read_result(&dir)
            .map(|value| result_text(&value))
            .unwrap_or_default();
        if !text.is_empty() && old.is_none_or(|old| old.text != text) {
            fresh.push(Doc {
                kind: Kind::Result,
                role: String::new(),
                id: None,
                task: Some(task),
                at,
                by: "task".into(),
                text,
                end: 0,
            });
        }
    }
    Ok(())
}

/// The lines of the task's story that its log holds past `from`. A log that is not longer has
/// nothing new (one stat); progress lines are skipped by their bytes, before any parse.
fn catch_up_history(dir: &TaskDir, task: u32, from: u64, fresh: &mut Vec<Doc>) {
    const KINDS: [&[u8]; 9] = [
        br#""kind":"created""#,
        br#""kind":"message""#,
        br#""kind":"result""#,
        br#""kind":"revision""#,
        br#""kind":"checks""#,
        br#""kind":"landed""#,
        br#""kind":"closed""#,
        br#""kind":"stopped""#,
        br#""kind":"land_stopped""#,
    ];
    let Ok(entries) = dir.read_log_since(from, |line| {
        // ^ `unix_ms` comes first, so the kind is in the first bytes: a long progress line is not scanned.
        let head = &line[..line.len().min(64)];
        KINDS.iter().any(|kind| contains_bytes(head, kind))
    }) else {
        return;
    };
    for (end, entry) in entries {
        // ^ The prompt is the task's own record, with its title.
        let Some((label, text)) =
            history::line(&entry.event).filter(|(label, _)| *label != "prompt")
        else {
            continue;
        };
        fresh.push(Doc {
            kind: Kind::History,
            role: String::new(),
            id: None,
            task: Some(task),
            at: entry.unix_ms,
            by: label.into(),
            text: clip_to(&text, MAX_HISTORY_BYTES),
            end,
        });
    }
}

/// The summary, memo, decisions and open points of a `result.json`.
fn result_text(value: &serde_json::Value) -> String {
    let result = value.get("result").unwrap_or(value);
    let text = |key: &str| result.get(key).and_then(|v| v.as_str()).unwrap_or_default();
    let mut lines = vec![text("summary").to_string(), text("memo").to_string()];
    for decision in result
        .get("decisions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let part = |key: &str| {
            decision
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        };
        lines.push(format!("{}: {}", part("choice"), part("reason")));
    }
    for open in result
        .get("left_open")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        lines.push(open.as_str().unwrap_or_default().to_string());
    }
    lines.retain(|line| !line.trim().is_empty() && line.trim() != ":");
    clip(&lines.join("\n"))
}

/// The manager's decisions and memo lines, read from the notes as they are now: the files stay,
/// and a line the manager merged or fixed is not found in its old form.
fn notes(project_id: &str) -> Vec<Doc> {
    let Ok(Some(dir)) = ManagerDir::existing(project_id) else {
        return Vec::new();
    };
    let mut docs = Vec::new();
    for (name, kind) in [
        ("decisions.md", Kind::Decision),
        ("decisions-archive.md", Kind::Decision),
        ("context.md", Kind::Memo),
        ("context-archive.md", Kind::Memo),
    ] {
        let path = dir.path.join("notes").join(name);
        let Ok(Some(bytes)) = store::read_bounded_if_exists(&path, MAX_NOTES_BYTES) else {
            continue;
        };
        let modified = std::fs::metadata(&path).map_or(0, |meta| meta.mtime() as u64 * 1000);
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Some(text) = line
                .strip_prefix("- ")
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                continue;
            };
            let task = text
                .strip_prefix('#')
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(n, _)| n.parse().ok());
            docs.push(Doc {
                kind,
                role: String::new(),
                id: None,
                task,
                at: text.split(' ').next().and_then(day_ms).unwrap_or(modified),
                by: "notes".into(),
                text: clip(text),
                end: 0,
            });
        }
    }
    docs
}

/// `YYYY-MM-DD` as the unix ms of that day's start (UTC).
fn day_ms(date: &str) -> Option<u64> {
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // ^ Days from the civil date (Howard Hinnant's algorithm).
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let of_year = year - era * 400;
    let of_day = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let of_era = of_year * 365 + of_year / 4 - of_year / 100 + of_day;
    let days = era * 146_097 + of_era - 719_468;
    u64::try_from(days * 86_400_000).ok()
}

fn clip(text: &str) -> String {
    clip_to(text, MAX_TEXT_BYTES)
}

fn clip_to(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
