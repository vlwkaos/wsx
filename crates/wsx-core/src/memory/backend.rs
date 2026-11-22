//! Search backends of the `memory` contract: the built-in BM25 index over a scope's records and
//! its transcripts, and an external command that follows the contract's `search`, `reindex`
//! and `status`. The built-in always answers; a backend adds hits, and a backend that fails
//! (exit 3, 10 s, bad output) is named in a notice and left out. `lines` is the built-in
//! over the transcripts alone, for `wsx task search`; both answer with the one `Hit`.
//! ref: docs/memory.md "Search backend"

use super::index::{self, Doc, Target};
use super::record::{self, Record, Status, EXCERPT_CHARS};
use super::store::{Error, Store};
use crate::process::{run_bounded, Bound, Options, Stderr};
use crate::task::manager::QUOTE_REF;
use crate::task::text::first_line;
use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

/// How long a backend call may take.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest answer read from a backend.
const MAX_ANSWER_BYTES: usize = 4 << 20;
/// Hits `recall` reads before it keeps the best of them, and how many it keeps.
const RECALL_POOL: usize = 20;
const RECALL_KEEP: usize = 5;

/// A hit of the contract, and of `wsx task search`. `kind` is a record kind, or for a transcript
/// of a project the kind of the line (`message`, `reply`, `task`, `result`, `history`,
/// `decision`, `memo`), with `project`, `chat`, `id`, `task`, `unix_ms`, `by`, `line`,
/// `reference`, `quote` and `outcome` in `extra`; a reader ignores what it does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub slug: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub path: String,
    pub kind: String,
    #[serde(default = "active")]
    pub status: String,
    pub score: f64,
    #[serde(default)]
    pub excerpt: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn active() -> String {
    Status::Active.as_str().into()
}

/// The `--kind` of a search: a record kind or a transcript kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindFilter(String);

impl FromStr for KindFilter {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        let kinds: Vec<&str> = record::Kind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .chain(index::Kind::ALL.iter().map(|kind| kind.as_str()))
            .collect();
        match kinds.contains(&value) {
            true => Ok(Self(value.into())),
            false => Err(format!("kind {value:?}: use {}", kinds.join(", "))),
        }
    }
}

pub struct Query {
    pub words: String,
    pub limit: usize,
    pub include_inactive: bool,
    pub kind: Option<KindFilter>,
}

/// What a backend declares in `status` (`search` key); the built-in has neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flags {
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub model: bool,
}

/// One way to search a scope. An `Err` is the contract's exit code: `Store` is exit 3, the
/// backend is unavailable.
pub trait Backend {
    fn name(&self) -> String;
    fn search(&self, store: &Store, query: &Query) -> Result<Vec<Hit>, Error>;
    fn reindex(&self, store: &Store) -> Result<(), Error>;
    fn flags(&self, store: &Store) -> Result<Flags, Error>;
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::Store(message.into())
}

/// BM25 (`index::rank`) over the scope's records and, for a project scope, the project's
/// transcripts; the score is the BM25 score over its limit for the query, so it is in 0..1.
pub struct Builtin;

impl Backend for Builtin {
    fn name(&self) -> String {
        "builtin".into()
    }

    fn search(&self, store: &Store, query: &Query) -> Result<Vec<Hit>, Error> {
        let terms = index::terms(&query.words)
            .map_err(|_| Error::Usage("the query has no words".into()))?;
        let records = store.records()?;
        let target = store.transcripts_of().map(|(id, name)| Target {
            name: name.into(),
            project_id: id.into(),
        });
        let docs = match &target {
            Some(target) => index::gather(target, false, true).unwrap_or_else(|error| {
                eprintln!(
                    "wsx: memory: transcripts of {} not searched: {error:#}",
                    target.name
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        let texts: Vec<String> = records.iter().map(searchable).collect();
        let ranked = index::rank(
            texts
                .iter()
                .map(String::as_str)
                .chain(docs.iter().map(|d| d.text.as_str())),
            &terms,
        );
        let wanted = query.kind.as_ref().map(|k| k.0.as_str());
        // ^ Filters apply after the ranking: an inactive record still counts in the word statistics.
        let kept = ranked
            .scored
            .into_iter()
            .map(|(raw, at)| ((raw / ranked.bound).clamp(0.0, 1.0), at))
            .filter(|(_, at)| match records.get(*at) {
                Some(r) => {
                    (query.include_inactive || r.status == Status::Active)
                        && wanted.is_none_or(|k| k == r.kind.as_str())
                }
                None => {
                    let doc = &docs[at - records.len()];
                    wanted.is_none_or(|k| k == doc.kind.as_str())
                }
            })
            .collect();
        let newest = |at: usize| match records.get(at) {
            Some(r) => r.updated,
            None => docs[at - records.len()].at,
        };
        Ok(best(kept, newest, query.limit)
            .into_iter()
            .filter_map(|(score, at)| match (records.get(at), &target) {
                (Some(r), _) => Some(record_hit(store, r, score)),
                (None, Some(target)) => Some(line_hit(
                    store.scope(),
                    target,
                    &docs[at - records.len()],
                    score,
                    Some(&target.name),
                )),
                (None, None) => None,
            })
            .collect())
    }

    /// Drops the project's digest and builds it again from the logs; a scope without
    /// transcripts only checks that its log folds.
    fn reindex(&self, store: &Store) -> Result<(), Error> {
        store.counts()?;
        if let Some((id, name)) = store.project_ids() {
            let target = Target {
                name: name.into(),
                project_id: id.into(),
            };
            index::gather(&target, true, true).map_err(|e| unavailable(format!("{e:#}")))?;
        }
        Ok(())
    }

    fn flags(&self, _: &Store) -> Result<Flags, Error> {
        Ok(Flags::default())
    }
}

/// What a record is ranked by: its slug, title, description, keywords and text together.
pub(super) fn searchable(r: &Record) -> String {
    format!(
        "{} {} {} {} {}",
        r.slug,
        r.title.as_deref().unwrap_or(""),
        r.description.as_deref().unwrap_or(""),
        r.keywords.join(" "),
        r.text
    )
}

/// A record as a hit; its description and keywords ride along when it has them.
pub(super) fn record_hit(store: &Store, r: &Record, score: f64) -> Hit {
    let mut extra = Map::new();
    if let Some(description) = &r.description {
        extra.insert("description".into(), Value::from(description.as_str()));
    }
    if !r.keywords.is_empty() {
        extra.insert("keywords".into(), Value::from(r.keywords.clone()));
    }
    Hit {
        slug: r.slug.clone(),
        scope: store.scope().into(),
        path: store.log_path().display().to_string(),
        kind: r.kind.as_str().into(),
        status: r.status.as_str().into(),
        score,
        excerpt: r.text.chars().take(EXCERPT_CHARS).collect(),
        extra,
    }
}

/// A transcript line as a hit: the slug is its `reference`, and its other keys ride along.
/// `current`: the project the asker works in, whose manager chat a hit can quote by id.
fn line_hit(scope: &str, target: &Target, doc: &Doc, score: f64, current: Option<&str>) -> Hit {
    let project = target.name.as_str();
    // ^ The repo's main chat is the manager's: its hits carry no chat name and can be quoted by id.
    let main = crate::task::store::main_chat(&target.project_id);
    let chat =
        Some(doc.chat.as_str()).filter(|chat| !chat.is_empty() && main.as_deref() != Some(chat));
    let reference = match (doc.kind, doc.id, doc.task) {
        (index::Kind::Message | index::Kind::Reply, Some(id), _) => match chat {
            Some(chat) => format!("{project}:{chat}:{id}"),
            None => format!("{project}:{id}"),
        },
        (index::Kind::Decision, ..) => format!("{project}:notes/decisions.md"),
        (index::Kind::Memo, ..) => format!("{project}:notes/context.md"),
        (_, _, Some(task)) => format!("{project}#{task}"),
        _ => project.to_string(),
    };
    let mut extra = Map::new();
    extra.insert("project".into(), project.into());
    if let Some(chat) = chat {
        extra.insert("chat".into(), chat.into());
    }
    if let Some(id) = doc.id {
        extra.insert("id".into(), id.into());
    }
    if let Some(task) = doc.task {
        extra.insert("task".into(), task.into());
        if let Some(outcome) = index::outcome(target, task) {
            extra.insert("outcome".into(), outcome.into());
        }
    }
    extra.insert("unix_ms".into(), doc.at.into());
    extra.insert("by".into(), doc.by.as_str().into());
    extra.insert("line".into(), first_line(&doc.text).into());
    extra.insert("reference".into(), reference.as_str().into());
    if let (index::Kind::Message | index::Kind::Reply, Some(id), None, true) =
        (doc.kind, doc.id, chat, current == Some(project))
    {
        extra.insert("quote".into(), format!("{QUOTE_REF}{id}").into());
    }
    Hit {
        slug: reference,
        scope: scope.into(),
        path: index::source_path(&target.project_id, doc),
        kind: doc.kind.as_str().into(),
        status: active(),
        score,
        excerpt: doc.text.chars().take(EXCERPT_CHARS).collect(),
        extra,
    }
}

/// The best `limit` of `scored` (score, position), best first; equal scores go to the newer.
fn best(
    mut scored: Vec<(f64, usize)>,
    newest: impl Fn(usize) -> u64,
    limit: usize,
) -> Vec<(f64, usize)> {
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(newest(b.1).cmp(&newest(a.1))));
    scored.truncate(limit);
    scored
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

/// The transcript hits for `words` in `targets`, best first, at most `limit`, with raw BM25
/// scores; the words of all projects count together. `current`: the project the asker works
/// in. `reindex` rebuilds the digests from the logs and the task files first. `only` cuts
/// the records to one task before the ranking. ref: docs/tasks.md "Search"
pub fn lines(
    targets: &[Target],
    current: Option<&str>,
    words: &str,
    limit: usize,
    reindex: bool,
    only: Option<u32>,
) -> anyhow::Result<Found> {
    let terms = index::terms(words)?;
    let mut found = Found::default();
    let mut docs: Vec<(usize, Doc)> = Vec::new();
    for (at, target) in targets.iter().enumerate() {
        match index::gather(target, reindex, false) {
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
    let ranked = index::rank(docs.iter().map(|(_, doc)| doc.text.as_str()), &terms);
    found.hits = best(ranked.scored, |at| docs[at].1.at, limit)
        .into_iter()
        .map(|(score, at)| {
            let (target, doc) = &docs[at];
            let target = &targets[*target];
            line_hit(&target.project_id, target, doc, score, current)
        })
        .collect();
    Ok(found)
}

/// An external program that follows the contract, run as `<argv> <command> --scope S --json`.
pub struct Command {
    argv: Vec<String>,
}

impl Command {
    /// None for an empty command line (no backend configured).
    pub fn new(argv: Vec<String>) -> Option<Self> {
        (!argv.is_empty()).then_some(Self { argv })
    }

    /// Run it with `args` for at most 10 s; its standard output on exit 0.
    fn call(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        let mut command = std::process::Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .args(args)
            .stdin(Stdio::null());
        let options = Options {
            timeout: BACKEND_TIMEOUT,
            stdout: Bound::Limit(MAX_ANSWER_BYTES),
            stderr: Stderr::Own(Bound::Tail(300)),
            reap: true,
        };
        let run = run_bounded(&mut command, &options)
            .map_err(|e| unavailable(format!("cannot run {}: {e}", self.argv[0])))?;
        if run.timed_out {
            return Err(unavailable(format!(
                "no answer in {} s",
                BACKEND_TIMEOUT.as_secs()
            )));
        }
        match run.status.code() {
            Some(0) => Ok(run.stdout),
            code => {
                let said = String::from_utf8_lossy(&run.stderr);
                Err(unavailable(match code {
                    Some(code) => format!("exit {code}: {}", said.trim()),
                    None => format!("ended by a signal: {}", said.trim()),
                }))
            }
        }
    }
}

impl Backend for Command {
    fn name(&self) -> String {
        self.argv.join(" ")
    }

    fn search(&self, store: &Store, query: &Query) -> Result<Vec<Hit>, Error> {
        let limit = query.limit.to_string();
        let mut args = vec!["search", "--scope", store.scope(), "--limit", &limit];
        if query.include_inactive {
            args.push("--include-inactive");
        }
        if let Some(KindFilter(kind)) = &query.kind {
            args.extend(["--kind", kind]);
        }
        // ^ After `--` a query that starts with `-` is still a query.
        args.extend(["--json", "--", &query.words]);
        let out = self.call(&args)?;
        let mut hits: Vec<Hit> = serde_json::from_slice(&out)
            .map_err(|e| unavailable(format!("the answer is no list of hits: {e}")))?;
        hits.retain(|hit| {
            hit.score.is_finite()
                && (query.include_inactive || hit.status == "active")
                && query.kind.as_ref().is_none_or(|k| k.0 == hit.kind)
        });
        for hit in &mut hits {
            if hit.scope.is_empty() {
                hit.scope = store.scope().into();
            }
            hit.score = hit.score.clamp(0.0, 1.0);
            hit.excerpt = hit.excerpt.chars().take(EXCERPT_CHARS).collect();
        }
        Ok(hits)
    }

    fn reindex(&self, store: &Store) -> Result<(), Error> {
        self.call(&["reindex", "--scope", store.scope(), "--json"])
            .map(drop)
    }

    fn flags(&self, store: &Store) -> Result<Flags, Error> {
        let out = self.call(&["status", "--scope", store.scope(), "--json"])?;
        let status: Value = serde_json::from_slice(&out)
            .map_err(|e| unavailable(format!("the status is no JSON: {e}")))?;
        Ok(status
            .get("search")
            .and_then(|flags| serde_json::from_value(flags.clone()).ok())
            .unwrap_or_default())
    }
}

/// Hits of all stores, and what went wrong with the backend (a notice per failed call).
pub struct Answer {
    pub hits: Vec<Hit>,
    pub notices: Vec<String>,
}

/// The built-in's hits and the backend's, one per `(scope, slug)` with the higher score, best
/// first, at most `query.limit`. A backend that fails costs a notice, not the answer.
pub fn search(stores: &[Store], backend: Option<&Command>, query: &Query) -> Result<Answer, Error> {
    let mut answer = Answer {
        hits: Vec::new(),
        notices: Vec::new(),
    };
    for store in stores {
        let mut found = Builtin.search(store, query)?;
        if let Some(backend) = backend {
            match backend.search(store, query) {
                Ok(more) => found.extend(more),
                Err(error) => answer.notices.push(format!(
                    "memory backend {} unavailable ({error}); the built-in answered",
                    backend.name()
                )),
            }
        }
        answer.hits.extend(found);
    }
    let mut best: HashMap<(String, String), usize> = HashMap::new();
    let mut kept: Vec<Hit> = Vec::new();
    for hit in std::mem::take(&mut answer.hits) {
        match best.get(&(hit.scope.clone(), hit.slug.clone())) {
            Some(&at) if kept[at].score >= hit.score => {}
            Some(&at) => kept[at] = hit,
            None => {
                best.insert((hit.scope.clone(), hit.slug.clone()), kept.len());
                kept.push(hit);
            }
        }
    }
    // ^ A stable sort: equal scores keep the order they came in (the built-in first).
    kept.sort_by(|a, b| b.score.total_cmp(&a.score));
    kept.truncate(query.limit);
    answer.hits = kept;
    answer.notices.dedup();
    Ok(answer)
}

/// The best few hits for a question: the top 5 by score of the top 20 of a search. A model
/// that reads those 20 comes in a later step.
pub fn recall(
    stores: &[Store],
    backend: Option<&Command>,
    question: &str,
) -> Result<Answer, Error> {
    let query = Query {
        words: question.into(),
        limit: RECALL_POOL,
        include_inactive: false,
        kind: None,
    };
    let mut answer = search(stores, backend, &query)?;
    answer.hits.truncate(RECALL_KEEP);
    Ok(answer)
}

/// The backend `status` reports for `store`: its name, what it declares, and why it cannot be
/// reached (then the built-in alone answers).
pub fn describe(store: &Store, backend: Option<&Command>) -> (String, Flags, Option<String>) {
    match backend {
        None => (Builtin.name(), Flags::default(), None),
        Some(backend) => match backend.flags(store) {
            Ok(flags) => (backend.name(), flags, None),
            Err(error) => (backend.name(), Flags::default(), Some(error.to_string())),
        },
    }
}

/// `reindex` for `store`: the built-in's digest, then the backend's own index.
pub fn reindex(store: &Store, backend: Option<&Command>) -> Result<(), Error> {
    Builtin.reindex(store)?;
    backend.map_or(Ok(()), |backend| backend.reindex(store))
}
