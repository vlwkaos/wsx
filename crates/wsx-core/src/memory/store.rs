//! A scope's store: `records.jsonl` and its lock in one folder. ref: docs/memory.md "Store"

use super::record::{
    duplicate, fold, query_words, score, Folded, Hit, Kind, Line, Record, Status, EXCERPT_CHARS,
    MAX_TEXT_BYTES,
};
use crate::task::checks::{self, CheckState};
use crate::task::store::{self as task_store, TaskDir};
use crate::task::{now_unix_ms, TaskState};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};

const LOG_FILE: &str = "records.jsonl";
const LOCK_FILE: &str = "records.lock";
const MAX_SCOPE_BYTES: usize = 64;
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// The failures of the contract; `code` is the exit code. ref: docs/memory.md "Exit codes"
#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Usage(String),
    Store(String),
}

impl Error {
    pub fn code(&self) -> i32 {
        match self {
            Self::NotFound(_) => 1,
            Self::Usage(_) => 2,
            Self::Store(_) => 3,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::NotFound(m) | Self::Usage(m) | Self::Store(m)) = self;
        f.write_str(m)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Store(error.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

fn usage<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Usage(message.into()))
}

/// A scope or record name: `a-z`, `0-9` and `-`, 1 to 64 characters, not starting with `-`.
pub fn validate_scope(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SCOPE_BYTES
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    match valid {
        true => Ok(()),
        false => usage(format!(
            "{value:?}: use 1 to 64 of a-z, 0-9 and -, not first -"
        )),
    }
}

fn memory_root() -> PathBuf {
    crate::runtime::state_dir().join("memory")
}

pub struct NewRecord {
    pub kind: Kind,
    pub slug: String,
    pub text: String,
    pub title: Option<String>,
    pub source: String,
    pub verified_by: Option<String>,
    pub commit: Option<String>,
}

#[derive(Default)]
pub struct Patch {
    pub slug: String,
    pub text: Option<String>,
    pub source: String,
    pub verified_by: Option<String>,
    pub commit: Option<String>,
    /// The lesson of a `--status failed` update (INVALIDATE).
    pub fail: Option<String>,
}

pub struct SearchOptions {
    pub limit: usize,
    pub include_inactive: bool,
    pub kind: Option<Kind>,
}

pub struct Store {
    scope: String,
    dir: PathBuf,
    /// A project scope: the task ID, for `task:` and `check:` in `verified_by`.
    project_id: Option<String>,
    /// Where a relative `verified_by` path lives: the project path, else the current folder.
    root: PathBuf,
    /// Whether a `rule` record may be written or changed (the manager's and a role's).
    rules: bool,
}

impl Store {
    /// The store of a registered project: `tasks/<project-id>/memory/`.
    pub fn project(project_id: &str, root: &Path) -> Result<Self> {
        validate_scope(project_id)?;
        let dir = task_store::project_dir(project_id)
            .map_err(|e| Error::Usage(e.to_string()))?
            .join("memory");
        Ok(Self {
            scope: project_id.into(),
            dir,
            project_id: Some(project_id.into()),
            root: root.into(),
            rules: true,
        })
    }

    /// The store of `default` or another unbound name: `$XDG_STATE_HOME/wsx/memory/<name>/`.
    pub fn named(name: &str, root: &Path) -> Result<Self> {
        validate_scope(name)?;
        Ok(Self {
            scope: name.into(),
            dir: memory_root().join(name),
            project_id: None,
            root: root.into(),
            rules: true,
        })
    }

    /// A store that refuses to add, change or replace a `rule` record: an agent that cannot
    /// give the owner's word. ref: docs/memory.md "Agents"
    pub fn without_rules(mut self) -> Self {
        self.rules = false;
        self
    }

    fn may_write(&self, kind: Kind) -> Result<()> {
        match kind == Kind::Rule && !self.rules {
            true => {
                usage("a rule needs the owner's word, so only the manager or a role writes one")
            }
            false => Ok(()),
        }
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    /// The exclusive lock of the store, held until the file closes. `None` for a read of a
    /// store that does not exist yet.
    fn lock(&self, create: bool) -> Result<Option<File>> {
        if create {
            crate::fs_util::private_dir(&self.dir)?;
        } else if !self.dir.is_dir() {
            return Ok(None);
        }
        let file = open(
            &self.dir.join(LOCK_FILE),
            OpenOptions::new().write(true).create(true),
        )?;
        crate::fs_util::flock(file.as_raw_fd(), false)?;
        Ok(Some(file))
    }

    /// The current records. A line that does not parse is skipped with a notice, never fatal.
    fn load(&self) -> Result<Folded> {
        let mut bytes = Vec::new();
        match open(&self.log_path(), OpenOptions::new().read(true)) {
            Ok(file) => {
                file.take(MAX_LOG_BYTES + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_LOG_BYTES {
                    return Err(Error::Store(format!(
                        "{} is too large",
                        self.log_path().display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let folded = fold(&bytes);
        if folded.skipped > 0 {
            eprintln!(
                "wsx: memory: skipped {} unreadable line(s) in {}",
                folded.skipped,
                self.log_path().display()
            );
        }
        Ok(folded)
    }

    /// Append `record` as the state after `op`. A last line cut short by a crash gets its
    /// newline first, so the new line is not glued to it. One `write` under the lock.
    fn append(&self, op: &str, record: &Record) -> Result<()> {
        let mut file = open(
            &self.log_path(),
            OpenOptions::new().read(true).append(true).create(true),
        )?;
        let len = file.metadata()?.len();
        let mut bytes = Vec::new();
        if len > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                bytes.push(b'\n');
            }
        }
        let line = Line {
            op: op.into(),
            record: record.clone(),
        };
        bytes.extend(serde_json::to_vec(&line).map_err(|e| Error::Store(e.to_string()))?);
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(())
    }

    /// Add a record. A near duplicate returns the old record, and the flag is false: nothing
    /// was written.
    pub fn add(&self, new: NewRecord) -> Result<(Record, bool)> {
        validate_scope(&new.slug)
            .map_err(|_| Error::Usage(format!("slug {:?}: use a-z, 0-9 and -", new.slug)))?;
        self.may_write(new.kind)?;
        let text = self.check_text(&new.text)?;
        check_source(&new.source)?;
        if matches!(new.title.as_deref(), Some(t) if t.contains('\n')) {
            return usage("the title is one line");
        }
        match (&new.verified_by, new.kind) {
            (None, Kind::Rule | Kind::Fact) => {
                return usage(
                    "a rule or fact needs --verified-by (owner:<id>, a path, task:<n>, check:<id>)",
                )
            }
            (Some(by), _) => self.check_verified_by(by)?,
            _ => {}
        }
        let _lock = self.lock(true)?;
        let folded = self.load()?;
        if let Some(old) = duplicate(&folded.records, new.kind, &text) {
            return Ok((old.clone(), false));
        }
        if folded.records.contains_key(&new.slug) {
            return usage(format!(
                "slug {} exists with other text; use update",
                new.slug
            ));
        }
        let record = Record {
            slug: new.slug,
            kind: new.kind,
            status: Status::Active,
            text,
            title: new.title,
            source: new.source,
            updated: now_unix_ms(),
            verified_by: new.verified_by,
            commit: new.commit,
            superseded_by: None,
            lesson: None,
            extra: Default::default(),
        };
        self.append("add", &record)?;
        Ok((record, true))
    }

    /// Change a record; the old text stays in the log. `fail` is INVALIDATE.
    pub fn update(&self, patch: Patch) -> Result<Record> {
        check_source(&patch.source)?;
        let text = patch
            .text
            .as_deref()
            .map(|t| self.check_text(t))
            .transpose()?;
        if text.is_none()
            && patch.verified_by.is_none()
            && patch.commit.is_none()
            && patch.fail.is_none()
        {
            return usage(
                "nothing to update: give --text-file, --verified-by, --commit or --status failed",
            );
        }
        if let Some(by) = &patch.verified_by {
            self.check_verified_by(by)?;
        }
        if matches!(&patch.fail, Some(l) if l.trim().is_empty()) {
            return usage("--status failed needs a --lesson");
        }
        let _lock = self.lock(true)?;
        let mut record = self.find(&patch.slug)?;
        self.may_write(record.kind)?;
        if text.is_some() && record.status != Status::Active {
            return usage(format!(
                "{} is not active; its text stays as it was",
                record.slug
            ));
        }
        record.text = text.unwrap_or(record.text);
        record.verified_by = patch.verified_by.or(record.verified_by);
        record.commit = patch.commit.or(record.commit);
        record.source = patch.source;
        record.updated = now_unix_ms();
        let op = match patch.fail {
            Some(lesson) => {
                record.status = Status::Failed;
                record.lesson = Some(lesson);
                "invalidate"
            }
            None => "update",
        };
        self.append(op, &record)?;
        Ok(record)
    }

    /// `old` is replaced by the active record `new`.
    pub fn supersede(&self, old: &str, new: &str, source: &str) -> Result<Record> {
        check_source(source)?;
        if old == new {
            return usage(format!("{old} cannot replace itself"));
        }
        let _lock = self.lock(true)?;
        let mut records = self.load()?.records;
        let mut record = records.remove(old).ok_or_else(|| self.missing(old))?;
        let newer = records.get(new).ok_or_else(|| self.missing(new))?;
        self.may_write(record.kind)?;
        self.may_write(newer.kind)?;
        if newer.status != Status::Active || record.status == Status::Superseded {
            return usage(format!(
                "{new} must be active and {old} must not be superseded already"
            ));
        }
        record.status = Status::Superseded;
        record.superseded_by = Some(new.into());
        record.source = source.into();
        record.updated = now_unix_ms();
        self.append("supersede", &record)?;
        Ok(record)
    }

    fn find(&self, slug: &str) -> Result<Record> {
        self.load()?
            .records
            .remove(slug)
            .ok_or_else(|| self.missing(slug))
    }

    fn missing(&self, slug: &str) -> Error {
        Error::NotFound(format!("no record {slug} in scope {}", self.scope))
    }

    /// Records whose slug, title or text hold a query word, best first; newer first on a tie.
    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<Vec<Hit>> {
        let words = query_words(query)?;
        let _lock = self.lock(false)?;
        let path = self.log_path().display().to_string();
        let mut hits: Vec<(Hit, u64)> = self
            .load()?
            .records
            .into_values()
            .filter(|r| options.include_inactive || r.status == Status::Active)
            .filter(|r| options.kind.is_none_or(|k| k == r.kind))
            .filter_map(|r| {
                let score = score(&r, &words);
                (score > 0.0).then(|| {
                    let hit = Hit {
                        slug: r.slug,
                        scope: self.scope.clone(),
                        path: path.clone(),
                        kind: r.kind,
                        status: r.status,
                        score,
                        excerpt: r.text.chars().take(EXCERPT_CHARS).collect(),
                    };
                    (hit, r.updated)
                })
            })
            .collect();
        hits.sort_by(|a, b| b.0.score.total_cmp(&a.0.score).then(b.1.cmp(&a.1)));
        hits.truncate(options.limit);
        Ok(hits.into_iter().map(|(hit, _)| hit).collect())
    }

    /// The count of records per status.
    pub fn counts(&self) -> Result<[usize; 3]> {
        let _lock = self.lock(false)?;
        let mut counts = [0; 3];
        for record in self.load()?.records.values() {
            counts[record.status as usize] += 1;
        }
        Ok(counts)
    }

    fn check_text(&self, text: &str) -> Result<String> {
        let text = text.trim_end();
        match text.len() {
            0 => usage("the text is empty"),
            n if n > MAX_TEXT_BYTES => {
                usage(format!("the text is {n} bytes; at most {MAX_TEXT_BYTES}"))
            }
            _ => Ok(text.into()),
        }
    }

    /// `owner:<digits>`, `task:<n>` of a landed task, `check:<id>` of a passed item (the two
    /// need a project scope), or a relative path that exists under the root.
    fn check_verified_by(&self, value: &str) -> Result<()> {
        let project = |what: &str| {
            self.project_id
                .as_deref()
                .map_or_else(|| usage(format!("{what} needs a project scope")), Ok)
        };
        if let Some(id) = value.strip_prefix("owner:") {
            return match !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                true => Ok(()),
                false => usage(format!("{value:?}: owner:<message id>")),
            };
        }
        if let Some(number) = value.strip_prefix("task:") {
            let project = project("task:<n>")?;
            let landed = task_store::parse_task_number(number)
                .and_then(|n| TaskDir::open(project, n))
                .and_then(|dir| dir.read_record())
                .map_err(|e| Error::Usage(format!("{value}: {e}")))?
                .state
                == TaskState::Landed;
            return if landed {
                Ok(())
            } else {
                usage(format!("{value}: the task has not landed"))
            };
        }
        if let Some(id) = value.strip_prefix("check:") {
            let items =
                checks::read(project("check:<id>")?).map_err(|e| Error::Store(e.to_string()))?;
            return match items
                .iter()
                .any(|i| i.id == id && i.state == CheckState::Passed)
            {
                true => Ok(()),
                false => usage(format!("{value}: no passed check item {id}")),
            };
        }
        let relative = Path::new(value);
        let plain = !value.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !plain {
            return usage(format!(
                "{value:?}: use owner:<id>, task:<n>, check:<id> or a relative path without .."
            ));
        }
        // ^ A link under the root may point out of it: compare the real paths.
        let inside = fs::canonicalize(self.root.join(relative))
            .ok()
            .zip(fs::canonicalize(&self.root).ok());
        match inside {
            Some((path, root)) if path.starts_with(&root) => Ok(()),
            _ => usage(format!(
                "{value}: no such path under {}",
                self.root.display()
            )),
        }
    }
}

fn check_source(source: &str) -> Result<()> {
    let ok = ["msg:", "task:", "path:", "transcript:"].iter().any(|p| {
        source
            .strip_prefix(p)
            .is_some_and(|rest| !rest.trim().is_empty())
    });
    match ok && !source.contains('\n') {
        true => Ok(()),
        false => usage(format!(
            "source {source:?}: use msg:<id>, task:<n>, path:<file> or transcript:<ref>"
        )),
    }
}

fn open(path: &Path, options: &mut OpenOptions) -> std::io::Result<File> {
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// `(scope, store folder)` of `default`, each project and each unbound name with a folder.
pub fn known_scopes(project_ids: &[String]) -> Vec<(String, PathBuf)> {
    let mut scopes = vec![("default".to_string(), memory_root().join("default"))];
    for id in project_ids {
        if let Ok(dir) = task_store::project_dir(id) {
            scopes.push((id.clone(), dir.join("memory")));
        }
    }
    let mut named: Vec<String> = fs::read_dir(memory_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| validate_scope(n).is_ok() && !scopes.iter().any(|(s, _)| s == n))
        .collect();
    named.sort();
    scopes.extend(
        named
            .into_iter()
            .map(|n| (n.clone(), memory_root().join(n))),
    );
    scopes
}
