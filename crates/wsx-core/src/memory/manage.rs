//! The manager's notes as views of the records: the write path (records first, then the views),
//! the top-5 check of a new rule or fact, and drift. Old notes files are not imported: a store
//! that still holds them is refused (`refuse_unimported`).
//! ref: docs/memory.md "Notes", docs/design/memory-system.md "Decisions stay the owner's words"

use super::backend::{record_hit, searchable, Hit};
use super::index;
use super::record::{Kind, Record, Status};
use super::store::{usage, Error, NewRecord, Patch, Store};
use crate::task::chat::{read_chat, Window};
use crate::task::chats;
use crate::task::manager::{Author, ManagerEvent};
use crate::task::store::{read_bounded_if_exists, write_note, ManagerDir, TaskDir};
use crate::task::text::day_of;
use crate::task::view::clip;
use crate::task::{Actor, LogEvent, TaskRecord, TaskState};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const DECISIONS: &str = "decisions.md";
const DECISIONS_ARCHIVE: &str = "decisions-archive.md";
const CONTEXT: &str = "context.md";
const CONTEXT_ARCHIVE: &str = "context-archive.md";
/// The four files, which a view each: the rules, the replaced rules, the section and memos
/// that fit the ceiling, and the older memos.
// ^ Each archive comes before its file, so a crash between the two writes shows a memo twice
// and never loses one.
const VIEWS: [&str; 4] = [DECISIONS_ARCHIVE, DECISIONS, CONTEXT_ARCHIVE, CONTEXT];
/// The last release that imports the old notes files (`wsx memory import`).
const IMPORTING_RELEASE: &str = "v0.43.0";
/// A hand-edited view, copied here before it is rewritten from the records.
const DRIFT_SUFFIX: &str = ".drift";
/// The hash of each view as rendered, in the store's folder.
const HASHES_FILE: &str = "views.json";
/// Past this many bytes the oldest memos of `context.md` show in `context-archive.md` only.
const CONTEXT_CEILING_BYTES: usize = 32 * 1024;
const MAX_VIEW_BYTES: u64 = 4 << 20;
/// A new rule or fact reports the records of its kind that rank in this many and score this much (docs/memory.md "Notes": measured).
const SIMILAR_KEEP: usize = 5;
const SIMILAR_MIN: f64 = 0.09;

/// A view that was changed by hand: the lines it has and the records do not hold, and the lines
/// the records hold and it lacks.
#[derive(Debug, Clone, Serialize)]
pub struct Drift {
    pub view: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl Drift {
    /// One sentence for a notice.
    pub fn notice(&self, saved: bool) -> String {
        format!(
            "{} was edited by hand ({} line(s) the records do not hold, {} missing){}",
            self.view,
            self.added.len(),
            self.removed.len(),
            match saved {
                true => format!(
                    "; the edited file is kept as {}{DRIFT_SUFFIX} and the view is written again from the records",
                    self.view
                ),
                false => "; the next write rewrites it from the records".into(),
            }
        )
    }
}

/// A write's answer: the record, whether it was written (a duplicate is not), the active records
/// of its kind that are close to it, and the views that were edited by hand.
pub struct Added {
    pub record: Record,
    pub written: bool,
    pub similar: Vec<Hit>,
    pub drift: Vec<Drift>,
}

fn io<T>(result: anyhow::Result<T>) -> Result<T, Error> {
    result.map_err(|e| Error::Store(format!("{e:#}")))
}

fn digest(text: &str) -> String {
    hash(text)[..16].to_string()
}

/// The slug of a memo: its task and its words, so the same memo has one record.
fn memo_slug(task: u32, text: &str) -> String {
    format!("memo-{task}-{}", digest(text))
}

/// A project name as a part of a slug: `a-z`, `0-9` and `-`, at most 24.
fn slug_of(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(24)
        .collect();
    slug.trim_matches('-').to_string()
}

fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

// ^ The views. A rule shows with the date of the record and the owner's words, as `decisions.md`
// always did; a memo as `- #<task>: <text>`.

fn rule_line(r: &Record) -> String {
    let words = one_line(r.words.as_deref().unwrap_or(&r.text));
    match r.extra.contains_key("undated") {
        true => format!("- {words}"),
        false => format!("- {} {words}", day_of(decided(r))),
    }
}

/// When the record counts from: a rule's decision, else its last write.
pub(super) fn decided(r: &Record) -> u64 {
    r.decided.unwrap_or(r.updated)
}

fn memo_task(r: &Record) -> Option<u32> {
    r.slug
        .strip_prefix("memo-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn by_age(records: &[Record], keep: impl Fn(&Record) -> bool) -> Vec<&Record> {
    let mut found: Vec<&Record> = records.iter().filter(|r| keep(r)).collect();
    found.sort_by(|a, b| decided(a).cmp(&decided(b)).then(a.slug.cmp(&b.slug)));
    found
}

fn join(lines: Vec<String>) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// The four views as `(file, text)`.
fn render_views(records: &[Record]) -> Vec<(&'static str, String)> {
    let rules = |status| by_age(records, move |r| r.kind == Kind::Rule && r.status == status);
    let decisions = join(rules(Status::Active).into_iter().map(rule_line).collect());
    let archive = join(
        rules(Status::Superseded)
            .into_iter()
            .map(rule_line)
            .collect(),
    );
    let section: Vec<String> = by_age(records, |r| {
        r.kind == Kind::Fact && r.status == Status::Active && r.slug.starts_with("section-")
    })
    .iter()
    .map(|r| r.text.clone())
    .collect();
    let memos: Vec<String> = by_age(records, |r| {
        r.kind != Kind::Rule && r.status == Status::Active && memo_task(r).is_some()
    })
    .iter()
    .filter_map(|r| Some(format!("- #{}: {}", memo_task(r)?, one_line(&r.text))))
    .collect();
    let mut total: usize = section.iter().chain(&memos).map(|l| l.len() + 1).sum();
    let mut older = 0;
    while total > CONTEXT_CEILING_BYTES && older < memos.len() {
        total -= memos[older].len() + 1;
        older += 1;
    }
    let context = join(
        section
            .into_iter()
            .chain(memos[older..].iter().cloned())
            .collect(),
    );
    vec![
        (DECISIONS_ARCHIVE, archive),
        (DECISIONS, decisions),
        (CONTEXT_ARCHIVE, join(memos[..older].to_vec())),
        (CONTEXT, context),
    ]
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A view file as text. A link, a file too large and one that cannot be read are none, as the
/// manager's own reader treats them: the next render replaces them.
fn read_view(path: &Path) -> Result<Option<String>, Error> {
    Ok(read_bounded_if_exists(path, MAX_VIEW_BYTES)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

fn read_hashes(store: &Store) -> HashMap<String, String> {
    hashes_in(store.dir())
}

fn hashes_in(dir: &Path) -> HashMap<String, String> {
    read_bounded_if_exists(&dir.join(HASHES_FILE), 1 << 16)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn lines_of(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A view as it is, as it should be, and whether someone else wrote it since the last render.
struct Seen {
    name: &'static str,
    wanted: String,
    now: Option<String>,
    drift: Option<Drift>,
}

fn inspect(store: &Store, notes: &Path, records: &[Record]) -> Result<Vec<Seen>, Error> {
    let hashes = read_hashes(store);
    let mut seen = Vec::new();
    for (name, wanted) in render_views(records) {
        let now = read_view(&notes.join(name))?;
        let edited = match (&now, hashes.get(name)) {
            (None, _) => false,
            (Some(now), Some(known)) => hash(now) != *known,
            // ^ Never rendered: old notes, which `refuse_unimported` keeps from any write.
            (Some(_), None) => false,
        };
        let drift = (edited && now.as_deref().is_some_and(|n| !n.trim().is_empty())).then(|| {
            let (have, want) = (lines_of(now.as_deref().unwrap_or("")), lines_of(&wanted));
            Drift {
                view: name.into(),
                added: have.iter().filter(|l| !want.contains(l)).cloned().collect(),
                removed: want.iter().filter(|l| !have.contains(l)).cloned().collect(),
            }
        });
        seen.push(Seen {
            name,
            wanted,
            now,
            drift: drift.filter(|d| !d.added.is_empty() || !d.removed.is_empty()),
        });
    }
    Ok(seen)
}

/// The notes files of a project that are views of its records. The index reads the records for
/// them, so it leaves their lines out.
pub(super) fn rendered_names(project_id: &str) -> Vec<String> {
    crate::task::store::project_dir(project_id)
        .map(|dir| hashes_in(&dir.join("memory")).into_keys().collect())
        .unwrap_or_default()
}

/// The views that were edited by hand since they were rendered; writes nothing.
pub fn drift(store: &Store) -> Result<Vec<Drift>, Error> {
    let Some(notes) = store.notes_dir() else {
        return Ok(Vec::new());
    };
    Ok(inspect(store, &notes, &store.records()?)?
        .into_iter()
        .filter_map(|seen| seen.drift)
        .collect())
}

/// Write the views from the records, under the store's lock. A view edited by hand is first
/// kept as `<view>.drift` and returned. Nothing happens for a scope without a manager.
fn render(store: &Store) -> Result<Vec<Drift>, Error> {
    let Some(notes) = store.notes_dir() else {
        return Ok(Vec::new());
    };
    // ^ Old notes that were never imported stay as they are.
    if unimported(store, &notes) {
        return Ok(Vec::new());
    }
    store.locked(|records| {
        io(crate::fs_util::private_dir(&notes).map_err(Into::into))?;
        let mut hashes = read_hashes(store);
        let mut found = Vec::new();
        for seen in inspect(store, &notes, records)? {
            if let Some(drift) = seen.drift {
                let kept = notes.join(format!("{}{DRIFT_SUFFIX}", seen.name));
                io(write_note(&kept, seen.now.as_deref().unwrap_or("")))?;
                found.push(drift);
            }
            let missing_and_empty = seen.now.is_none() && seen.wanted.is_empty();
            if seen.now.as_deref() != Some(&seen.wanted) && !missing_and_empty {
                io(write_note(&notes.join(seen.name), &seen.wanted))?;
            }
            hashes.insert(seen.name.into(), hash(&seen.wanted));
        }
        let text = serde_json::to_string(&hashes).map_err(|e| Error::Store(e.to_string()))?;
        io(write_note(&store.dir().join(HASHES_FILE), &text))?;
        Ok(found)
    })
}

pub(super) fn clip_to(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The commit a task landed with, when it has landed: what `capture` stores with the paths of
/// its diff (the record's `paths`).
pub fn landed_commit(dir: &TaskDir, record: &TaskRecord) -> Option<String> {
    if record.state != TaskState::Landed {
        return None;
    }
    dir.last_event(|event| match event {
        LogEvent::Landed { commit } => Some(commit.clone()),
        _ => None,
    })
    .ok()
    .flatten()
}

// ^ The write path: the records first, then the views. A view that cannot be written is a
// notice, not a failure: the record is stored and the next write renders again.

fn views_after_write(store: &Store) -> Vec<Drift> {
    render(store).unwrap_or_else(|error| {
        eprintln!("wsx: memory: cannot write the notes views: {error}");
        Vec::new()
    })
}

/// A rule's `--words` are the owner's: `--source msg:<id>`, `--verified-by owner:<id>`, and the
/// words must stand in that message, which the owner wrote in the calling chat (message ids are
/// per chat; no chat, as for the owner or a worker, is the repo's main chat).
fn check_words(store: &Store, new: &NewRecord, words: &str) -> Result<(), Error> {
    let id = new
        .source
        .strip_prefix("msg:")
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| new.verified_by.as_deref() == Some(&format!("owner:{id}")));
    let Some(id) = id else {
        return usage(
            "--words needs --source msg:<id> and --verified-by owner:<id> of that message",
        );
    };
    let Some((project, _)) = store.project_ids() else {
        return usage("--words needs a project scope");
    };
    let actor = Actor::from_env();
    let dir = io(ManagerDir::existing_in(project, actor.chat()))?;
    if let (Some(chat), Some(dir)) = (actor.chat(), &dir) {
        // ^ A chat records a rule only in the scope of a repo it is part of.
        if !chats::repos(dir).iter().any(|repo| repo == project) {
            return usage(format!("chat {chat} is not part of project {project}"));
        }
    }
    let entries = match dir {
        Some(dir) => io(read_chat(&dir, &Window::Around(id)))?,
        None => Vec::new(),
    };
    let squeeze = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
    let said = entries.iter().find_map(|entry| match &entry.event {
        ManagerEvent::Message {
            id: at,
            from: Author::Owner,
            text,
            ..
        } if *at == id => Some(squeeze(text)),
        _ => None,
    });
    match said {
        Some(said) if said.contains(&squeeze(words)) => Ok(()),
        Some(_) => usage(format!("those words are not in the owner's message {id}")),
        None => usage(format!("no message {id} of the owner in this chat")),
    }
}

/// The active records of `kind` that rank in the top 5 for `text` with a score of at least
/// `SIMILAR_MIN`, best first: the rule or fact it may replace.
fn similar(store: &Store, kind: Kind, text: &str) -> Result<Vec<Hit>, Error> {
    let Ok(terms) = index::terms(text) else {
        return Ok(Vec::new());
    };
    let records = store.records()?;
    let pool: Vec<&Record> = records
        .iter()
        .filter(|r| r.status == Status::Active && r.kind == kind)
        .collect();
    let texts: Vec<String> = pool.iter().map(|r| searchable(r)).collect();
    let ranked = index::rank(texts.iter().map(String::as_str), &terms);
    let mut hits: Vec<(f64, u64, &Record)> = ranked
        .scored
        .into_iter()
        .map(|(raw, at)| {
            (
                (raw / ranked.bound).clamp(0.0, 1.0),
                pool[at].updated,
                pool[at],
            )
        })
        .filter(|(score, ..)| *score >= SIMILAR_MIN)
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    hits.truncate(SIMILAR_KEEP);
    Ok(hits
        .into_iter()
        .map(|(score, _, r)| record_hit(store, r, score))
        .collect())
}

/// Whether a view file holds old notes: it is a file that no render has hashed.
fn unimported(store: &Store, notes: &Path) -> bool {
    let hashes = read_hashes(store);
    VIEWS.iter().any(|name| {
        // ^ A link is no old file: the notes never read one.
        fs::symlink_metadata(notes.join(name)).is_ok_and(|m| m.is_file())
            && !hashes.contains_key(*name)
    })
}

/// A write is refused while old notes wait: the first render would write the records over them.
/// A release that imports them did not run here.
fn refuse_unimported(store: &Store) -> Result<(), Error> {
    match store.notes_dir() {
        Some(notes) if unimported(store, &notes) => Err(Error::Store(format!(
            "{} holds notes from before the memory store: run `wsx memory import` with wsx {IMPORTING_RELEASE} once",
            notes.display()
        ))),
        _ => Ok(()),
    }
}

/// `Store::add`, with the checks of the notes: the owner's words
/// found in the message, the close records of the same kind named, `supersedes` replaced by the
/// new record in the same call, and the views written after.
pub fn add(store: &Store, new: NewRecord, supersedes: Option<&str>) -> Result<Added, Error> {
    // ^ An episode is no line of a rule or section: it is kept while old notes wait (`render` leaves them as they are).
    if new.kind != Kind::Episode {
        refuse_unimported(store)?;
    }
    if let Some(words) = new.words.as_deref() {
        check_words(store, &new, words)?;
    }
    if let Some(old) = supersedes {
        store.get(old)?;
    }
    let similar = match new.kind {
        Kind::Rule | Kind::Fact => similar(store, new.kind, &new.text)?,
        Kind::Episode => Vec::new(),
    };
    let (record, written) = store.add(new)?;
    let mut drift = Vec::new();
    if written {
        if let Some(old) = supersedes.filter(|old| *old != record.slug) {
            store.supersede(old, &record.slug, &record.source)?;
        }
        drift = views_after_write(store);
    }
    Ok(Added {
        record,
        written,
        similar,
        drift,
    })
}

/// A memo is one line of at most this many characters (the line of `context.md`).
pub const MEMO_LINE_CHARS: usize = 240;

/// A finished task's memo as a record: a fact verified by the task when it landed (with the
/// landed `commit` and the `paths` of its diff, which `start` and `stale` read), else an
/// episode with neither. The same memo is one record. ref: docs/memory.md "Capture"
///
/// `project` names the registered project that ran the task when it is not the store's own: the
/// fact is `task:<project>#<task>`, whose paths `stale` checks in that project's folder.
pub fn capture(
    store: &Store,
    project: Option<&str>,
    task: u32,
    memo: &str,
    commit: Option<&str>,
    paths: &[String],
) -> Result<Added, Error> {
    let by = match project {
        Some(project) => format!("task:{project}#{task}"),
        None => format!("task:{task}"),
    };
    let landed = store.can_verify(&by);
    let memo = clip(
        &memo.split_whitespace().collect::<Vec<_>>().join(" "),
        MEMO_LINE_CHARS,
    );
    add(
        store,
        NewRecord {
            kind: if landed { Kind::Fact } else { Kind::Episode },
            slug: match project {
                // ^ `in` is no number, so the views of the scope's own tasks skip it (`memo_task`).
                Some(project) => format!("memo-in-{}-{task}-{}", slug_of(project), digest(&memo)),
                None => memo_slug(task, &memo),
            },
            text: memo,
            title: None,
            source: by.clone(),
            verified_by: landed.then_some(by),
            commit: commit.filter(|_| landed).map(str::to_string),
            description: None,
            keywords: Vec::new(),
            paths: if landed { paths.to_vec() } else { Vec::new() },
            words: None,
        },
        None,
    )
}

/// What a conversation carried when it was dropped (a task's handoff notes, the manager's
/// handover), as an episode: `<slug>-` and a digest of the text, so one text is one record.
/// Text over the store's limit keeps its head and says where the whole is: `whole`.
pub fn add_episode(
    store: &Store,
    slug: &str,
    source: &str,
    text: &str,
    whole: &Path,
) -> Result<Added, Error> {
    let text = match text.len() > store.text_bytes() {
        true => {
            let note = format!(
                "\n[cut: {} bytes; the whole is in {}]",
                text.len(),
                whole.display()
            );
            let keep = store.text_bytes().saturating_sub(note.len());
            format!("{}{note}", clip_to(text, keep))
        }
        false => text.to_string(),
    };
    add(
        store,
        NewRecord {
            kind: Kind::Episode,
            slug: format!("{slug}-{}", digest(&text)),
            text,
            title: None,
            source: source.into(),
            verified_by: None,
            commit: None,
            description: None,
            keywords: Vec::new(),
            paths: Vec::new(),
            words: None,
        },
        None,
    )
}

pub fn update(store: &Store, patch: Patch) -> Result<(Record, Vec<Drift>), Error> {
    refuse_unimported(store)?;
    let record = store.update(patch)?;
    Ok((record, views_after_write(store)))
}

pub fn supersede(
    store: &Store,
    old: &str,
    new: &str,
    source: &str,
) -> Result<(Record, Vec<Drift>), Error> {
    refuse_unimported(store)?;
    let record = store.supersede(old, new, source)?;
    Ok((record, views_after_write(store)))
}
