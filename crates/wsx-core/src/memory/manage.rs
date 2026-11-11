//! The manager's notes as views of the records: import of the old notes files, the write path
//! (records first, then the views), the top-5 check of a new rule or fact, and drift.
//! ref: docs/memory.md "Notes", docs/design/memory-system.md "Decisions stay the owner's words"

use super::backend::{record_hit, searchable, Hit};
use super::index::{self, day_ms};
use super::record::{Kind, Record, Status};
use super::store::{Error, NewRecord, Patch, Store};
use crate::task::chat::{read_chat, Window};
use crate::task::manager::{Author, ManagerEvent};
use crate::task::store::{read_bounded_if_exists, write_note, ManagerDir};
use crate::task::text::day_of;
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
/// The old notes, copied here once before the first view replaces them.
const COPY_SUFFIX: &str = ".pre-memory";
/// A hand-edited view, copied here before it is rewritten from the records.
const DRIFT_SUFFIX: &str = ".drift";
/// The hash of each view as rendered, in the store's folder.
const HASHES_FILE: &str = "views.json";
/// Past this many bytes the oldest memos of `context.md` show in `context-archive.md` only.
const CONTEXT_CEILING_BYTES: usize = 32 * 1024;
const MAX_VIEW_BYTES: u64 = 4 << 20;
/// A new rule or fact reports the records of its kind that rank in this many and score this much.
const SIMILAR_KEEP: usize = 5;
const SIMILAR_MIN: f64 = 0.1;

/// The records an import added.
#[derive(Debug, Default, Serialize)]
pub struct Imported {
    pub rules: usize,
    pub archived: usize,
    pub facts: usize,
    pub episodes: usize,
}

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

fn usage<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::Usage(message.into()))
}

fn io<T>(result: anyhow::Result<T>) -> Result<T, Error> {
    result.map_err(|e| Error::Store(format!("{e:#}")))
}

fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The slug of a memo: its task and its words, so the same memo has one record.
pub fn memo_slug(task: u32, text: &str) -> String {
    format!("memo-{task}-{}", digest(text))
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
fn decided(r: &Record) -> u64 {
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
            // ^ Never rendered: the file is the old notes, which wait for the import (no copy
            // yet) or equal their copy. Anything else was written by hand after the import.
            (Some(now), None) => read_view(&notes.join(format!("{name}{COPY_SUFFIX}")))?
                .is_some_and(|copy| copy != *now && *now != wanted),
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
pub fn render(store: &Store) -> Result<Vec<Drift>, Error> {
    let Some(notes) = store.notes_dir() else {
        return Ok(Vec::new());
    };
    // ^ Old notes that wait for an import (a writer that may not give rules cannot do it) stay.
    if pending(store, &notes) {
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

// ^ Import: the four old files become records, once. The first run copies each file to
// `<file>.pre-memory`; every run reads the copies only, so a line added by hand after the first
// view is drift, not a late import, and a rerun finds every slug and adds nothing.

/// Whether an old file waits to be imported: it has no copy yet and was never rendered.
fn pending(store: &Store, notes: &Path) -> bool {
    let hashes = read_hashes(store);
    VIEWS.iter().any(|name| {
        // ^ A link is no old file: the notes never read one.
        fs::symlink_metadata(notes.join(name)).is_ok_and(|m| m.is_file())
            && !notes.join(format!("{name}{COPY_SUFFIX}")).exists()
            && !hashes.contains_key(*name)
    })
}

fn copy_once(notes: &Path, name: &str, rendered: bool) -> Result<(), Error> {
    let (from, to) = (notes.join(name), notes.join(format!("{name}{COPY_SUFFIX}")));
    if rendered || to.exists() || !from.is_file() {
        return Ok(());
    }
    let Some(text) = read_view(&from)? else {
        return Ok(());
    };
    let modified = fs::metadata(&from).and_then(|m| m.modified());
    io(write_note(&to, &text))?;
    // ^ The copy keeps the file's time: the memos of an old file are as old as it.
    if let Ok(modified) = modified {
        let _ = fs::File::options()
            .write(true)
            .open(&to)
            .and_then(|f| f.set_modified(modified));
    }
    Ok(())
}

fn copy_time_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// `- #7: text` as `(7, text)`.
fn parse_memo(line: &str) -> Option<(u32, String)> {
    let (task, text) = line.strip_prefix("- #")?.split_once(':')?;
    let text = text.trim();
    (!text.is_empty()).then_some((task.parse().ok()?, text.to_string()))
}

/// `- 2026-10-22 words` as the day's start and the words; a line with no date has none.
fn parse_rule(line: &str) -> Option<(Option<u64>, String)> {
    let body = line.strip_prefix("- ")?.trim();
    if body.is_empty() {
        return None;
    }
    if let Some((date, rest)) = body.split_once(' ') {
        let day = day_ms(date).filter(|ms| day_of(*ms) == date);
        if let (Some(ms), false) = (day, rest.trim().is_empty()) {
            return Some((Some(ms), rest.trim().to_string()));
        }
    }
    Some((None, body.to_string()))
}

fn clip_to(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[derive(Clone, Copy)]
enum Class {
    Rule,
    Archived,
    Fact,
    Episode,
}

fn old_record(slug: String, kind: Kind, text: String, source: String, updated: u64) -> Record {
    Record {
        slug,
        kind,
        status: Status::Active,
        text,
        title: None,
        source,
        updated,
        verified_by: None,
        commit: None,
        superseded_by: None,
        lesson: None,
        description: None,
        keywords: Vec::new(),
        words: None,
        decided: None,
        extra: Default::default(),
    }
}

/// The records of the copies, with what each is, in file order.
fn old_records(store: &Store, notes: &Path) -> Result<Vec<(Record, Class)>, Error> {
    let limit = store.text_bytes();
    let copy = |name: &str| notes.join(format!("{name}{COPY_SUFFIX}"));
    let text =
        |name: &str| -> Result<String, Error> { Ok(read_view(&copy(name))?.unwrap_or_default()) };
    let mut out = Vec::new();
    for (name, status) in [
        (DECISIONS, Status::Active),
        (DECISIONS_ARCHIVE, Status::Superseded),
    ] {
        let (lines, at) = (text(name)?, copy_time_ms(&copy(name)));
        let lines: Vec<(usize, &str)> = lines.lines().enumerate().collect();
        for (i, (number, line)) in lines.iter().enumerate() {
            let Some((day, words)) = parse_rule(line) else {
                continue;
            };
            let updated = match day {
                Some(ms) => ms + i as u64,
                None => at.saturating_sub(lines.len() as u64) + i as u64,
            };
            let mut r = old_record(
                format!("rule-{}", digest(line.trim())),
                Kind::Rule,
                clip_to(&words, limit),
                format!("path:{name}:{}", number + 1),
                updated,
            );
            r.status = status;
            r.verified_by = Some(format!(
                "owner:transcript:{}#L{}",
                copy(name).display(),
                number + 1
            ));
            r.words = Some(words);
            r.decided = Some(updated);
            if day.is_none() {
                r.extra.insert("undated".into(), true.into());
            }
            let class = match status {
                Status::Active => Class::Rule,
                _ => Class::Archived,
            };
            out.push((r, class));
        }
    }
    // ^ The archive holds the older memos: they come first, so the order of the file is kept.
    let (older, newer) = (text(CONTEXT_ARCHIVE)?, text(CONTEXT)?);
    let at = copy_time_ms(&copy(CONTEXT));
    let total = (older.lines().count() + newer.lines().count()) as u64;
    let mut index = 0u64;
    for (name, body) in [(CONTEXT_ARCHIVE, &older), (CONTEXT, &newer)] {
        for (number, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            index += 1;
            let updated = at.saturating_sub(total) + index;
            let source = format!("path:{name}:{}", number + 1);
            let r = match parse_memo(line) {
                Some((task, memo)) => {
                    let by = format!("task:{task}");
                    let landed = store.can_verify(&by);
                    let mut r = old_record(
                        memo_slug(task, &memo),
                        if landed { Kind::Fact } else { Kind::Episode },
                        clip_to(&memo, limit),
                        source,
                        updated,
                    );
                    r.verified_by = landed.then_some(by);
                    let class = if landed { Class::Fact } else { Class::Episode };
                    out.push((r, class));
                    continue;
                }
                None if name == CONTEXT => line.trim_end(),
                None => continue,
            };
            let mut r = old_record(
                format!("section-{}", digest(r)),
                Kind::Fact,
                clip_to(r, limit),
                source,
                updated,
            );
            r.verified_by = Some(format!(
                "owner:transcript:{}#L{}",
                copy(CONTEXT).display(),
                number + 1
            ));
            out.push((r, Class::Fact));
        }
    }
    Ok(out)
}

/// Make records of the old notes, once, and write the views from them. Idempotent: the copies
/// never change, so a rerun adds nothing. A scope without a manager imports nothing.
pub fn import(store: &Store) -> Result<(Imported, Vec<Drift>), Error> {
    let Some(notes) = store.notes_dir() else {
        return Ok((Imported::default(), Vec::new()));
    };
    store.may_write(Kind::Rule)?;
    let hashes = read_hashes(store);
    for name in VIEWS {
        copy_once(&notes, name, hashes.contains_key(name))?;
    }
    let found = old_records(store, &notes)?;
    // ^ A slug twice (an archive line that repeats an active rule) is the first one: the store
    // keeps the first of them too.
    let mut class: HashMap<String, Class> = HashMap::new();
    for (r, c) in &found {
        class.entry(r.slug.clone()).or_insert(*c);
    }
    let added = store.import(found.into_iter().map(|(r, _)| r).collect())?;
    let mut counts = Imported::default();
    for slug in added {
        match class[&slug] {
            Class::Rule => counts.rules += 1,
            Class::Archived => counts.archived += 1,
            Class::Fact => counts.facts += 1,
            Class::Episode => counts.episodes += 1,
        }
    }
    Ok((counts, render(store)?))
}

fn ensure_imported(store: &Store) -> Result<(), Error> {
    let waits = store
        .notes_dir()
        .is_some_and(|notes| pending(store, &notes));
    // ^ Only a writer that may give rules imports; the others leave the notes as they are.
    if waits && store.may_write(Kind::Rule).is_ok() {
        import(store)?;
    }
    Ok(())
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
/// words must stand in that message, which the owner wrote.
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
    let dir = io(ManagerDir::existing(project))?;
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
        None => usage(format!("no message {id} of the owner in this project")),
    }
}

/// The active records of `kind` that rank in the top 5 for `text` with a score of at least
/// 0.1, best first: the rule or fact it may replace.
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

/// `Store::add`, with the checks of the notes: the old notes imported first, the owner's words
/// found in the message, the close records of the same kind named, `supersedes` replaced by the
/// new record in the same call, and the views written after.
pub fn add(store: &Store, new: NewRecord, supersedes: Option<&str>) -> Result<Added, Error> {
    ensure_imported(store)?;
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

/// A finished task's memo as a record: a fact verified by the task when it landed, else an
/// episode. The same memo is one record.
pub fn add_memo(store: &Store, task: u32, memo: &str) -> Result<Added, Error> {
    let by = format!("task:{task}");
    let landed = store.can_verify(&by);
    add(
        store,
        NewRecord {
            kind: if landed { Kind::Fact } else { Kind::Episode },
            slug: memo_slug(task, memo),
            text: memo.into(),
            title: None,
            source: by.clone(),
            verified_by: landed.then_some(by),
            commit: None,
            description: None,
            keywords: Vec::new(),
            words: None,
        },
        None,
    )
}

pub fn update(store: &Store, patch: Patch) -> Result<(Record, Vec<Drift>), Error> {
    ensure_imported(store)?;
    let record = store.update(patch)?;
    Ok((record, views_after_write(store)))
}

pub fn supersede(
    store: &Store,
    old: &str,
    new: &str,
    source: &str,
) -> Result<(Record, Vec<Drift>), Error> {
    ensure_imported(store)?;
    let record = store.supersede(old, new, source)?;
    Ok((record, views_after_write(store)))
}
