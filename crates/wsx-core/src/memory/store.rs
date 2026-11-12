//! A scope's store: `records.jsonl` and its lock in one folder. ref: docs/memory.md "Store"

use super::record::{duplicate, fold, Folded, Kind, Line, Record, Status, MAX_TEXT_BYTES};
use crate::task::checks::{self, CheckState};
use crate::task::store::{self as task_store, ManagerDir, TaskDir};
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

pub(super) fn usage<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Usage(message.into()))
}

/// A scope name: `a-z`, `0-9`, `-` and `_`, 1 to 64 characters, not starting with `-`.
pub fn validate_scope(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SCOPE_BYTES
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    match valid {
        true => Ok(()),
        false => usage(format!(
            "{value:?}: use 1 to 64 of a-z, 0-9, - and _, not first -"
        )),
    }
}

/// `task:<project>#<n>`: a task of another registered project, as the fact of a task that its
/// manager ran there is verified (docs/memory.md "Capture"). The project's name and the number.
pub fn foreign_task(value: &str) -> Option<(&str, &str)> {
    value
        .strip_prefix("task:")?
        .rsplit_once('#')
        .filter(|(name, number)| !name.is_empty() && !number.is_empty())
}

/// The task store ID and folder of the registered project `name`.
pub fn registered(name: &str) -> Option<(String, PathBuf)> {
    let (config, _) = crate::config::global::GlobalConfig::load().ok()?;
    let project = config
        .projects
        .iter()
        .find(|project| project.name == name)?;
    Some((
        task_store::project_id(&project.name, &project.path),
        project.path.clone(),
    ))
}

/// A record slug: `a-z`, `0-9` and `-`, 1 to 64 characters, not starting with `-`; a `.` may
/// stand inside (`release-0.10.0`), not first or last and not twice in a row.
pub fn validate_slug(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_SCOPE_BYTES
        && !value.starts_with(['-', '.'])
        && !value.ends_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    match valid {
        true => Ok(()),
        false => usage(format!(
            "slug {value:?}: use 1 to 64 of a-z, 0-9, - and . (. inside only, not twice), not first -"
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
    pub description: Option<String>,
    pub keywords: Vec<String>,
    /// The paths or globs the record is about (docs/memory.md "Start set").
    pub paths: Vec<String>,
    /// A rule's: the owner's words (the caller has checked them against the message).
    pub words: Option<String>,
}

#[derive(Default)]
pub struct Patch {
    pub slug: String,
    pub text: Option<String>,
    pub source: String,
    pub verified_by: Option<String>,
    pub commit: Option<String>,
    pub description: Option<String>,
    /// Replaces the keywords when given.
    pub keywords: Option<Vec<String>>,
    /// The lesson of a `--status failed` update (INVALIDATE).
    pub fail: Option<String>,
}

pub struct Store {
    scope: String,
    dir: PathBuf,
    /// A project scope: the task ID, for `task:` and `check:` in `verified_by`, and the
    /// registered name, for the references of the project's transcripts.
    project: Option<(String, String)>,
    /// Where a relative `verified_by` path lives: the project path, else the current folder.
    root: PathBuf,
    /// Whether a `rule` record may be written or changed (the manager's and a chat's).
    rules: bool,
    /// Whether a search also reads the project's transcripts.
    transcripts: bool,
}

impl Store {
    /// The store of a registered project: `tasks/<project-id>/memory/`.
    pub fn project(project_id: &str, name: &str, root: &Path) -> Result<Self> {
        validate_scope(project_id)?;
        let dir = task_store::project_dir(project_id)
            .map_err(|e| Error::Usage(e.to_string()))?
            .join("memory");
        Ok(Self {
            scope: project_id.into(),
            dir,
            project: Some((project_id.into(), name.into())),
            root: root.into(),
            rules: true,
            transcripts: true,
        })
    }

    /// The store of `default` or another unbound name: `$XDG_STATE_HOME/wsx/memory/<name>/`.
    pub fn named(name: &str, root: &Path) -> Result<Self> {
        validate_scope(name)?;
        Ok(Self {
            scope: name.into(),
            dir: memory_root().join(name),
            project: None,
            root: root.into(),
            rules: true,
            transcripts: true,
        })
    }

    /// A store that refuses to add, change or replace a `rule` record: an agent that cannot
    /// give the owner's word. ref: docs/memory.md "Agents"
    pub fn without_rules(mut self) -> Self {
        self.rules = false;
        self
    }

    /// A store whose search reads records only: an agent asking about another project's scope
    /// gets its records, not what was said in that project's chats. ref: docs/memory.md "Agents"
    pub fn without_transcripts(mut self) -> Self {
        self.transcripts = false;
        self
    }

    pub(super) fn may_write(&self, kind: Kind) -> Result<()> {
        match kind == Kind::Rule && !self.rules {
            true => {
                usage("a rule needs the owner's word, so only the manager or a chat writes one")
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

    /// Where the project's files are (a project scope), else the folder of the call.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The most bytes a record's text may hold in this store; `status` declares it, and a
    /// reader takes it from there. ref: docs/memory.md "Limits"
    pub const fn text_bytes(&self) -> usize {
        MAX_TEXT_BYTES
    }

    /// A project scope: its task ID and its registered name.
    pub fn project_ids(&self) -> Option<(&str, &str)> {
        self.project
            .as_ref()
            .map(|(id, name)| (id.as_str(), name.as_str()))
    }

    /// The project whose transcripts a search of this store reads, if any.
    pub fn transcripts_of(&self) -> Option<(&str, &str)> {
        self.project_ids().filter(|_| self.transcripts)
    }

    fn project_id(&self) -> Option<&str> {
        self.project.as_ref().map(|(id, _)| id.as_str())
    }

    /// The file the records live in, as the `path` of a hit.
    pub fn log_path(&self) -> PathBuf {
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
        self.append_all(op, std::slice::from_ref(record))
    }

    fn append_all(&self, op: &str, records: &[Record]) -> Result<()> {
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
        for record in records {
            let line = Line {
                op: op.into(),
                record: record.clone(),
            };
            bytes.extend(serde_json::to_vec(&line).map_err(|e| Error::Store(e.to_string()))?);
            bytes.push(b'\n');
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(())
    }

    /// Add a record. A near duplicate returns the old record, and the flag is false: nothing
    /// was written.
    pub fn add(&self, new: NewRecord) -> Result<(Record, bool)> {
        validate_slug(&new.slug)?;
        self.may_write(new.kind)?;
        let text = self.check_text(&new.text)?;
        check_source(&new.source)?;
        let description = new
            .description
            .as_deref()
            .map(check_description)
            .transpose()?;
        let keywords = check_keywords(new.keywords)?;
        let paths = check_paths(new.paths)?;
        if new.words.is_some() && new.kind != Kind::Rule {
            return usage("the owner's words belong to a rule");
        }
        if matches!(new.title.as_deref(), Some(t) if t.contains('\n')) {
            return usage("the title is one line");
        }
        match (&new.verified_by, new.kind) {
            (None, Kind::Rule | Kind::Fact) => {
                return usage(
                    "a rule or fact needs --verified-by (owner:<id>, owner:transcript:<file>, a path, task:<n>, check:<id>)",
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
        let now = now_unix_ms();
        let record = Record {
            slug: new.slug,
            kind: new.kind,
            status: Status::Active,
            text,
            title: new.title,
            source: new.source,
            updated: now,
            verified_by: new.verified_by,
            commit: new.commit,
            superseded_by: None,
            lesson: None,
            description,
            keywords,
            paths,
            words: new.words,
            decided: (new.kind == Kind::Rule).then_some(now),
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
        let description = patch
            .description
            .as_deref()
            .map(check_description)
            .transpose()?;
        let keywords = patch.keywords.map(check_keywords).transpose()?;
        if text.is_none()
            && patch.verified_by.is_none()
            && patch.commit.is_none()
            && patch.fail.is_none()
            && description.is_none()
            && keywords.is_none()
        {
            return usage(
                "nothing to update: give --text-file, --verified-by, --commit, --description, --keywords or --status failed",
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
        record.description = description.or(record.description);
        record.keywords = keywords.unwrap_or(record.keywords);
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

    /// Add the records whose slug the log does not hold, in one write under the lock; the slugs
    /// added. A rerun adds none. ref: docs/memory.md "Import"
    pub fn import(&self, records: Vec<Record>) -> Result<Vec<String>> {
        let _lock = self.lock(true)?;
        let held = self.load()?.records;
        let mut seen = std::collections::HashSet::new();
        let fresh: Vec<Record> = records
            .into_iter()
            .filter(|r| !held.contains_key(&r.slug) && seen.insert(r.slug.clone()))
            .collect();
        if !fresh.is_empty() {
            self.append_all("import", &fresh)?;
        }
        Ok(fresh.into_iter().map(|r| r.slug).collect())
    }

    /// Give the facts that hold no `commit` and no `paths` the pair `find` has for them, in one
    /// write under the lock. Their time stays, so the views keep their
    /// order. A rerun finds nothing to give. ref: docs/memory.md "Capture"
    pub fn backfill(&self, find: impl Fn(&Record) -> Option<(String, Vec<String>)>) -> Result<()> {
        let _lock = self.lock(true)?;
        let mut changed = Vec::new();
        for mut r in self.load()?.records.into_values() {
            if r.kind != Kind::Fact || r.commit.is_some() || !r.paths.is_empty() {
                continue;
            }
            let Some((commit, paths)) = find(&r) else {
                continue;
            };
            r.paths = check_paths(paths)?;
            if !r.paths.is_empty() {
                r.commit = Some(commit);
                changed.push(r);
            }
        }
        if !changed.is_empty() {
            self.append_all("backfill", &changed)?;
        }
        Ok(())
    }

    /// Run `f` on the current records with the store locked, so no write lands meanwhile.
    pub fn locked<T>(&self, f: impl FnOnce(&[Record]) -> Result<T>) -> Result<T> {
        let _lock = self.lock(true)?;
        let records: Vec<Record> = self.load()?.records.into_values().collect();
        f(&records)
    }

    /// Whether `by` would pass as a `verified_by` here.
    pub fn can_verify(&self, by: &str) -> bool {
        self.check_verified_by(by).is_ok()
    }

    /// A project scope: the manager's `notes/` folder, when the project has a manager.
    pub fn notes_dir(&self) -> Option<PathBuf> {
        let id = self.project_id()?;
        let dir = ManagerDir::existing(id).ok().flatten()?;
        Some(dir.path.join("notes"))
    }

    /// One record by slug.
    pub fn get(&self, slug: &str) -> Result<Record> {
        let _lock = self.lock(false)?;
        self.find(slug)
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

    /// Every current record, for the index to rank.
    pub fn records(&self) -> Result<Vec<Record>> {
        let _lock = self.lock(false)?;
        Ok(self.load()?.records.into_values().collect())
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
            n if n > self.text_bytes() => usage(format!(
                "the text is {n} bytes; this store keeps at most {}",
                self.text_bytes()
            )),
            _ => Ok(text.into()),
        }
    }

    /// `owner:transcript:<ref>`: the owner's words outside wsx. `<ref>` is a path to an existing
    /// transcript file, absolute or relative to the root, with `#L<line>` after it for a
    /// line. The file is not read: a date or a name alone is no proof.
    fn check_transcript(&self, reference: &str) -> Result<()> {
        let (path, line) = match reference.rsplit_once("#L") {
            Some((path, line)) => (path, Some(line)),
            None => (reference, None),
        };
        if line.is_some_and(|l| l.parse::<u64>().ok().is_none_or(|n| n == 0)) {
            return usage(format!(
                "owner:transcript:{reference}: the line is #L<number> from 1"
            ));
        }
        match path.is_empty() || !self.root.join(path).is_file() {
            true => usage(format!(
                "owner:transcript:{reference}: no such transcript file (a path, absolute or under {})",
                self.root.display()
            )),
            false => Ok(()),
        }
    }

    /// `owner:<digits>` or `owner:transcript:<ref>`, `task:<n>` of a landed task, `check:<id>` of a passed item (the two
    /// need a project scope), or a relative path that exists under the root.
    fn check_verified_by(&self, value: &str) -> Result<()> {
        let project = |what: &str| {
            self.project_id()
                .map_or_else(|| usage(format!("{what} needs a project scope")), Ok)
        };
        if let Some(url) = value.strip_prefix("url:") {
            return check_url(url);
        }
        if let Some(reference) = value.strip_prefix("owner:transcript:") {
            return self.check_transcript(reference);
        }
        if let Some(id) = value.strip_prefix("owner:") {
            return match !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                true => Ok(()),
                false => usage(format!(
                    "{value:?}: owner:<message id> or owner:transcript:<file>"
                )),
            };
        }
        if let Some(number) = value.strip_prefix("task:") {
            let (id, number) = match foreign_task(value) {
                Some((name, number)) => (
                    registered(name).map(|(id, _)| id).ok_or_else(|| {
                        Error::Usage(format!("{value}: no registered project {name}"))
                    })?,
                    number,
                ),
                None => (project("task:<n>")?.to_string(), number),
            };
            let landed = task_store::parse_task_number(number)
                .and_then(|n| TaskDir::open(&id, n))
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
                "{value:?}: use owner:<id>, owner:transcript:<file>, task:<n>, check:<id>, url:<https URL> or a relative path without .."
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
    if let Some(url) = source.strip_prefix("url:") {
        return check_url(url);
    }
    let ok = ["msg:", "task:", "path:", "transcript:"].iter().any(|p| {
        source
            .strip_prefix(p)
            .is_some_and(|rest| !rest.trim().is_empty())
    });
    match ok && !source.contains('\n') {
        true => Ok(()),
        false => usage(format!(
            "source {source:?}: use msg:<id>, task:<n>, path:<file>, url:<https URL> or transcript:<ref>"
        )),
    }
}

const MAX_URL_BYTES: usize = 2048;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_KEYWORDS: usize = 32;
/// Paths one record keeps, and their bytes together: a landed task's diff, as `TaskRecord.paths`.
const MAX_PATHS: usize = 500;
const MAX_PATH_BYTES: usize = 32 * 1024;
pub(super) const MAX_KEYWORD_BYTES: usize = 64;

/// An http or https URL with a host name, as text: nothing is fetched. ref: docs/memory.md
fn check_url(url: &str) -> Result<()> {
    let bad = |why: &str| usage(format!("url:{url}: {why}"));
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return bad("only http and https");
    };
    if url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("no spaces, no control characters, at most 2048 bytes");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    let host_ok = !host.is_empty()
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let port_ok =
        port.is_none_or(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
    match host_ok && port_ok {
        true => Ok(()),
        false => bad("no valid host (a name or an IPv4 address, no user name)"),
    }
}

fn check_description(text: &str) -> Result<String> {
    let text = text.trim();
    match text.is_empty() || text.contains('\n') || text.len() > MAX_DESCRIPTION_BYTES {
        true => usage("the description is one line of 1 to 512 bytes"),
        false => Ok(text.into()),
    }
}

/// The keywords of a write: trimmed, empty ones dropped, no repeats.
fn check_keywords(words: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for word in words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        if word.len() > MAX_KEYWORD_BYTES || word.contains('\n') {
            return usage("a keyword is one line of at most 64 bytes");
        }
        if !out.iter().any(|k| k == word) {
            out.push(word.into());
        }
    }
    match out.len() > MAX_KEYWORDS {
        true => usage("at most 32 keywords"),
        false => Ok(out),
    }
}

/// The paths of a write: relative, no `..`, nothing git would read as an option or a pathspec
/// magic (`-`, `:`), no repeats, at most `MAX_PATHS` of `MAX_PATH_BYTES` together.
fn check_paths(paths: Vec<String>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for path in paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let bad = path.starts_with(['/', '-', ':'])
            || path.chars().any(char::is_control)
            || path.split('/').any(|part| part == "..");
        if bad {
            return usage(format!(
                "path {path:?}: relative to the project, no .., not starting with - or :"
            ));
        }
        if !out.iter().any(|p| p == path) {
            out.push(path.into());
        }
    }
    match out.len() > MAX_PATHS || out.iter().map(String::len).sum::<usize>() > MAX_PATH_BYTES {
        true => usage(format!(
            "at most {MAX_PATHS} paths and {MAX_PATH_BYTES} bytes of them"
        )),
        false => Ok(out),
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
