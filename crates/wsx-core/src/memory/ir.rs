//! The `ir` engine of the `memory` contract (`memory.engine = "ir"`): wsx writes a scope's
//! records and its project's transcripts as markdown into the store's `corpus/`, and the `ir`
//! command indexes and ranks them (BM25: no daemon, no model). wsx keeps the truths; the corpus
//! and ir's index are derived and rebuilt from them. ref: docs/design/memory-layers.md "Search engine: ir"

use super::backend::{line_hit, record_hit, run, searchable, Backend, Flags, Hit, Query};
use super::index::{self, Doc, Target};
use super::record::Record;
use super::store::{Error, Store};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The oldest ir with `--json`, `-f meta.<field>` and preprocessors on a collection.
const MIN_VERSION: (u32, u32) = (0, 19);
/// How long an ir call may take: an `update` of a large corpus reads every file.
const IR_TIMEOUT: Duration = Duration::from_secs(60);
const INSTALL: &str = "brew install vlwkaos/tap/ir, or cargo install ir-search";

/// The engine: ir run with wsx's own config folder, so the owner's collections and searches never
/// see wsx's, and wsx's index lives under wsx's state.
pub struct Ir {
    config_dir: PathBuf,
}

/// What a corpus file stands for, so an ir hit maps back to the contract's hit; `Gone` is a file
/// no truth names any more.
enum Source {
    Record(usize),
    Line(usize),
    Gone,
}

/// The mark of an update that took the whole corpus in (`sync`).
const INDEXED: &str = ".indexed";

struct Corpus {
    records: Vec<Record>,
    docs: Vec<Doc>,
    target: Option<Target>,
    files: HashMap<String, Source>,
}

#[derive(Deserialize)]
struct IrHit {
    path: String,
    score: f64,
}

impl Ir {
    /// The engine, when the `ir` command answers with a version this code knows; else why not.
    pub fn open() -> Result<Self, Error> {
        let ir = Self {
            config_dir: crate::runtime::state_dir().join("ir"),
        };
        let out = ir.call(&["--version"])?;
        let said = String::from_utf8_lossy(&out);
        let version = said
            .split_whitespace()
            .nth(1)
            .and_then(|v| {
                let mut parts = v.split('.').map(|n| n.parse::<u32>().ok());
                Some((parts.next()??, parts.next()??))
            })
            .ok_or_else(|| unavailable(format!("ir --version said {:?}", said.trim())))?;
        if version < MIN_VERSION {
            return Err(unavailable(format!(
                "ir {}.{} is older than {}.{} ({INSTALL})",
                version.0, version.1, MIN_VERSION.0, MIN_VERSION.1
            )));
        }
        Ok(ir)
    }

    fn call(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        let mut command = std::process::Command::new("ir");
        command.args(args).env("IR_CONFIG_DIR", &self.config_dir);
        run(&mut command, "ir", IR_TIMEOUT).map_err(|error| match error {
            Error::Store(message) if message.starts_with("cannot run") => {
                unavailable(format!("memory.engine ir needs the ir command ({INSTALL})"))
            }
            other => other,
        })
    }

    fn collection(store: &Store) -> String {
        format!("wsx-{}", store.scope())
    }

    /// Bring the corpus and the collection up to the truths: write the files that are missing,
    /// delete the ones no truth names, register the collection when ir has none, and update the
    /// index when anything changed or the last update did not finish. Every file name holds its
    /// content's digest, so a changed record is a new file and the old one goes. One sync at a
    /// time on the machine (a chat host and a CLI search share a corpus and wsx's ir config); a
    /// store without transcripts leaves `lines/` to the searches that have them.
    fn sync(&self, store: &Store) -> Result<Corpus, Error> {
        fs::create_dir_all(&self.config_dir)?;
        let _lock = crate::task::store::open_locked(
            &self.config_dir.join("wsx.lock"),
            fs::OpenOptions::new().write(true).create(true),
        )
        .map_err(|e| unavailable(format!("{e:#}")))?;
        let corpus = gather(store)?;
        let root = store.dir().join("corpus");
        let indexed = root.join(INDEXED);
        let mut changed = !indexed.exists();
        let owned: &[&str] = if corpus.target.is_some() {
            &["records", "lines"]
        } else {
            &["records"]
        };
        let mut writes = Vec::new();
        for (name, source) in &corpus.files {
            let path = root.join(name);
            if path.exists() {
                continue;
            }
            writes.push((path, source));
        }
        for dir in owned {
            let Ok(entries) = fs::read_dir(root.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = format!("{dir}/{}", entry.file_name().to_string_lossy());
                if !corpus.files.contains_key(&name) {
                    writes.push((entry.path(), &Source::Gone));
                }
            }
        }
        // ^ The mark goes before the first change and comes back after the update that took it
        // all in: an update that fails or times out is run again by the next sync.
        if !writes.is_empty() {
            changed = true;
            remove_if_exists(&indexed)?;
        }
        for (path, source) in writes {
            match source {
                Source::Record(at) => write(&path, &record_file(&corpus.records[*at]))?,
                Source::Line(at) => write(&path, &line_file(&corpus.docs[*at]))?,
                Source::Gone => remove_if_exists(&path)?,
            }
        }
        let name = Self::collection(store);
        let mut registered = self.registered(&name)?;
        // ^ An index that is gone while its name stays is built again from the corpus.
        if registered
            && !self
                .config_dir
                .join("collections")
                .join(format!("{name}.sqlite"))
                .exists()
        {
            self.call(&["collection", "rm", &name])?;
            registered = false;
        }
        if !registered {
            self.configure()?;
            let root_text = root.display().to_string();
            fs::create_dir_all(&root)?;
            let mut args = vec!["collection", "add", &name, &root_text];
            if self.has_ko()? {
                args.extend(["-p", "ko"]);
            }
            self.call(&args)?;
            changed = true;
        }
        if changed {
            remove_if_exists(&indexed)?;
            self.call(&["update", &name])?;
            write(&indexed, "")?;
        }
        Ok(corpus)
    }

    /// `wsx task search` in one project: the transcript lines of `store` that match `words`, at
    /// most `limit`, `only` one task's; with the number of lines searched. ref: docs/tasks.md "Search"
    pub(super) fn lines(
        &self,
        store: &Store,
        words: &str,
        limit: usize,
        only: Option<u32>,
        current: Option<&str>,
    ) -> Result<(Vec<Hit>, usize), Error> {
        let mut filters = vec!["path~lines/".to_string()];
        if let Some(task) = only {
            filters.push(format!("meta.task={task}"));
        }
        self.rank(store, words, filters, limit, current)
    }

    /// The hits of `words` in `store`'s collection under the `-f` `filters`, best first, at most
    /// `limit`, as the contract's hits; `current` is the asker's project (its main chat's lines
    /// can be quoted by id). Also the number of corpus files.
    fn rank(
        &self,
        store: &Store,
        words: &str,
        mut filters: Vec<String>,
        limit: usize,
        current: Option<&str>,
    ) -> Result<(Vec<Hit>, usize), Error> {
        // ^ A word that starts with `-` is FTS5's NOT, a syntax error alone: the words are the query.
        let words: Vec<&str> = words
            .split_whitespace()
            .map(|word| word.trim_start_matches('-'))
            .filter(|word| !word.is_empty())
            .collect();
        if words.is_empty() {
            return Err(Error::Usage("the query has no words".into()));
        }
        let words = words.join(" ");
        let corpus = self.sync(store)?;
        let name = Self::collection(store);
        let limit = limit.to_string();
        if corpus.target.is_none() {
            filters.push("path~records/".to_string());
        }
        let mut args = vec![
            "search", "-c", &name, "--mode", "bm25", "--json", "-q", "-n", &limit,
        ];
        for filter in &filters {
            args.extend(["-f", filter]);
        }
        // ^ After `--` a query that starts with `-` is still a query.
        args.extend(["--", &words]);
        let out = self.call(&args)?;
        let found: Vec<IrHit> = serde_json::from_slice(&out)
            .map_err(|e| unavailable(format!("ir's answer is no list of hits: {e}")))?;
        let hits = found
            .into_iter()
            .filter(|hit| hit.score.is_finite())
            .filter_map(|hit| {
                let score = hit.score.clamp(0.0, 1.0);
                match (corpus.files.get(&hit.path)?, &corpus.target) {
                    (Source::Record(at), _) => Some(record_hit(store, &corpus.records[*at], score)),
                    (Source::Line(at), Some(target)) => Some(line_hit(
                        store.scope(),
                        target,
                        &corpus.docs[*at],
                        score,
                        current,
                    )),
                    (Source::Line(_), None) | (Source::Gone, _) => None,
                }
            })
            .collect();
        Ok((hits, corpus.files.len()))
    }

    /// Whether wsx's ir config names the collection.
    fn registered(&self, name: &str) -> Result<bool, Error> {
        let config = self.read_config()?;
        Ok(config
            .get("collections")
            .and_then(|list| list.as_sequence())
            .is_some_and(|list| {
                list.iter()
                    .any(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
            }))
    }

    fn has_ko(&self) -> Result<bool, Error> {
        Ok(self
            .read_config()?
            .get("preprocessors")
            .and_then(|p| p.get("ko"))
            .is_some())
    }

    fn read_config(&self) -> Result<yaml_serde::Value, Error> {
        match fs::read_to_string(self.config_dir.join("config.yml")) {
            Ok(text) => yaml_serde::from_str(&text)
                .map_err(|e| unavailable(format!("wsx's ir config.yml: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(yaml_serde::Value::Null),
            Err(e) => Err(e.into()),
        }
    }

    /// A first config for wsx's ir: the owner's `ko` preprocessor, so Korean is split into
    /// morphemes as in the owner's own collections. Without one, ir's default tokenizer runs.
    fn configure(&self) -> Result<(), Error> {
        let path = self.config_dir.join("config.yml");
        if path.exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.config_dir)?;
        let mut config = yaml_serde::Mapping::new();
        if let Some(ko) = owner_ko() {
            let mut preprocessors = yaml_serde::Mapping::new();
            preprocessors.insert("ko".into(), ko.into());
            config.insert("preprocessors".into(), preprocessors.into());
        }
        let text = yaml_serde::to_string(&config)
            .map_err(|e| unavailable(format!("wsx's ir config.yml: {e}")))?;
        write(&path, &text)
    }
}

impl Backend for Ir {
    fn name(&self) -> String {
        "ir".into()
    }

    fn search(&self, store: &Store, query: &Query) -> Result<Vec<Hit>, Error> {
        let mut filters = Vec::new();
        if !query.include_inactive {
            filters.push("meta.status=active".to_string());
        }
        if let Some(kind) = &query.kind {
            filters.push(format!("meta.kind={}", kind.as_str()));
        }
        let current = store.transcripts_of().map(|(_, name)| name.to_string());
        self.rank(
            store,
            &query.words,
            filters,
            query.limit,
            current.as_deref(),
        )
        .map(|(hits, _)| hits)
    }

    /// Drops the corpus and the index, then builds both again from the truths.
    fn reindex(&self, store: &Store) -> Result<(), Error> {
        let name = Self::collection(store);
        if self.registered(&name)? {
            self.call(&["collection", "rm", &name])?;
        }
        match fs::remove_dir_all(store.dir().join("corpus")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.sync(store).map(drop)
    }

    fn flags(&self, _: &Store) -> Result<Flags, Error> {
        Ok(Flags::default())
    }
}

/// The scope's truths as corpus files: its records, and the transcripts of its project.
fn gather(store: &Store) -> Result<Corpus, Error> {
    let records = store.records()?;
    let target = store.transcripts_of().map(|(id, name)| Target {
        name: name.into(),
        project_id: id.into(),
    });
    let docs = match &target {
        Some(target) => {
            index::gather(target, false, true).map_err(|e| unavailable(format!("{e:#}")))?
        }
        None => Vec::new(),
    };
    let mut files = HashMap::new();
    for (at, record) in records.iter().enumerate() {
        files.insert(
            file_name("records", &record_file(record)),
            Source::Record(at),
        );
    }
    let mut seen = HashSet::new();
    for (at, doc) in docs.iter().enumerate() {
        let name = file_name("lines", &line_file(doc));
        // ^ Two lines with the same text and identity are one document.
        if seen.insert(name.clone()) {
            files.insert(name, Source::Line(at));
        }
    }
    Ok(Corpus {
        records,
        docs,
        target,
        files,
    })
}

fn file_name(dir: &str, text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("{dir}/{hex}.md")
}

/// A record as ir reads it: `kind` and `status` for the filters, then the words it is ranked by.
fn record_file(record: &Record) -> String {
    format!(
        "---\nkind: {}\nstatus: {}\nslug: {:?}\n---\n{}\n",
        record.kind.as_str(),
        record.status.as_str(),
        record.slug,
        searchable(record)
    )
}

/// A transcript line: active always; its identity rides in the frontmatter so equal texts of two
/// messages stay two files.
fn line_file(doc: &Doc) -> String {
    format!(
        "---\nkind: {}\nstatus: active\nchat: {}\nid: {}\ntask: {}\nat: {}\nend: {}\n---\n{}\n",
        doc.kind.as_str(),
        serde_json::Value::from(doc.chat.as_str()),
        doc.id.map_or(String::new(), |id| id.to_string()),
        doc.task.map_or(String::new(), |task| task.to_string()),
        doc.at,
        doc.end,
        doc.text
    )
}

fn remove_if_exists(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write by a temp file renamed into place, so ir never reads half a file.
fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temp, text)?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })?;
    Ok(())
}

/// The owner's `ko` preprocessor command from their ir config (`IR_CONFIG_DIR`, else
/// `$XDG_CONFIG_HOME/ir`, else `~/.config/ir`), with `$IR_DIR` spelled out, since wsx's ir runs
/// with another folder.
fn owner_ko() -> Option<String> {
    let dir = std::env::var_os("IR_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(|v| PathBuf::from(v).join("ir"))
        })
        .or_else(|| dirs::home_dir().map(|home| home.join(".config/ir")))?;
    let text = fs::read_to_string(dir.join("config.yml")).ok()?;
    let config: yaml_serde::Value = yaml_serde::from_str(&text).ok()?;
    let ko = config.get("preprocessors")?.get("ko")?.as_str()?;
    Some(ko.replace("$IR_DIR", &dir.display().to_string()))
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::Store(message.into())
}
