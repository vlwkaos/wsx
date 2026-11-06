//! Files of one task under `$XDG_STATE_HOME/wsx/tasks/<project-id>/<n>/`.
//! ref: docs/tasks.md "Store"

use super::{now_unix_ms, LogEntry, LogEvent, TaskRecord};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_LOG_LINE_BYTES: usize = 160 * 1024;
/// Past this size only non-progress events are appended.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Hard stop, so a stream of steers cannot fill the disk either.
const MAX_LOG_HARD_BYTES: u64 = 16 * 1024 * 1024;
/// `TaskDir::last_event` looks at this much of the log's end before it reads all of it.
const LOG_TAIL_SCAN_BYTES: u64 = 512 * 1024;
const MAX_PROJECT_ID_BYTES: usize = 64;
pub const RECORD_FILE: &str = "task.json";
pub const LOG_FILE: &str = "log.jsonl";
pub const RESULT_FILE: &str = "result.json";
pub const REPORT_FILE: &str = "report.md";
/// The logical name of a task host's socket; the socket itself is in `socket_dir()`.
pub const SOCKET_FILE: &str = "host.sock";
const COUNTER_FILE: &str = "next";

pub use crate::runtime::state_dir;

fn tasks_root() -> PathBuf {
    state_dir().join("tasks")
}

/// The owner-only folder of host sockets: `<TMPDIR>/wsx-<uid>`, or `/tmp/wsx-<uid>` when
/// `TMPDIR` is too long for a socket name. A host and its callers must share `TMPDIR`.
/// ^ `sun_path` holds 104 bytes on macOS, and a socket in the store passed that with a
/// long `XDG_STATE_HOME` or project name. ref: docs/tasks.md "Store"
fn socket_dir() -> PathBuf {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let name = format!("wsx-{uid}");
    let base = std::env::temp_dir();
    // ^ Room for "/wsx-<uid>/t-<16 hex>.sock" within 103 bytes.
    if base.is_absolute() && base.as_os_str().len() <= 64 {
        base.join(name)
    } else {
        PathBuf::from("/tmp").join(name)
    }
}

/// A host's socket: `<socket_dir>/<kind>-<16 hex of sha256(logical path)>.sock`. Every
/// process that names the same store path names the same socket, and the path is at most
/// about 90 bytes.
fn socket_for(kind: char, logical: &Path) -> PathBuf {
    let digest = Sha256::digest(logical.as_os_str().as_encoded_bytes());
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    socket_dir().join(format!("{kind}-{hash}.sock"))
}

/// Create the socket folder owner-only, or check one that exists: a real folder of this
/// user that no one else can enter. A host calls it before it binds.
pub fn prepare_socket_dir(socket: &Path) -> Result<()> {
    let dir = socket
        .parent()
        .ok_or_else(|| anyhow!("socket {} has no folder", socket.display()))?;
    create_private_dir(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        bail!(
            "{} must belong to this user and be closed to others (chmod 700)",
            dir.display()
        );
    }
    Ok(())
}

/// `<name-slug>-<8 hex of sha256(path)>`: readable, and unique per project path.
pub fn project_id(name: &str, path: &Path) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    slug.truncate(40);
    let slug = slug.trim_matches('-');
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    if slug.is_empty() {
        hash
    } else {
        format!("{slug}-{hash}")
    }
}

fn validate_project_id(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_PROJECT_ID_BYTES
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if valid {
        Ok(())
    } else {
        bail!("invalid task project ID {value:?}: use 1..{MAX_PROJECT_ID_BYTES} of a-z, 0-9 and -")
    }
}

/// A task number as typed: decimal, 1 or more, at most u32.
pub fn parse_task_number(value: &str) -> Result<u32> {
    let value = value.strip_prefix('t').unwrap_or(value);
    match value.parse::<u32>() {
        Ok(n) if n > 0 && value.bytes().all(|b| b.is_ascii_digit()) => Ok(n),
        _ => bail!("invalid task number {value:?}: use a number such as 3 or t3"),
    }
}

pub(super) fn create_private_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => require_real_dir(path),
        Err(error) => Err(error).with_context(|| format!("cannot create {}", path.display())),
    }
}

fn require_real_dir(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("{} is not a directory (a link is refused)", path.display());
    }
    Ok(())
}

pub(super) fn project_dir(project_id: &str) -> Result<PathBuf> {
    validate_project_id(project_id)?;
    Ok(tasks_root().join(project_id))
}

/// Create `tasks/<project-id>/` and its parents, each owner-only.
pub(super) fn ensure_project_dir(project_id: &str) -> Result<PathBuf> {
    let root = tasks_root();
    if let Some(wsx) = root.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(wsx)
            .with_context(|| format!("cannot create {}", wsx.display()))?;
    }
    create_private_dir(&root)?;
    let dir = project_dir(project_id)?;
    create_private_dir(&dir)?;
    Ok(dir)
}

pub(super) fn open_nofollow(path: &Path, options: &mut OpenOptions) -> Result<File> {
    options
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// `flock(fd, operation)` on `path`'s descriptor: `false` when `LOCK_NB` finds it held.
fn flock(fd: i32, operation: i32, path: &Path) -> Result<bool> {
    // SAFETY: flock on a descriptor the caller keeps open; released when it closes.
    if unsafe { libc::flock(fd, operation) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(error).with_context(|| format!("cannot lock {}", path.display()))
}

/// `open_nofollow`, then an exclusive lock that holds until the file closes.
pub(super) fn open_locked(path: &Path, options: &mut OpenOptions) -> Result<File> {
    let file = open_nofollow(path, options)?;
    flock(file.as_raw_fd(), libc::LOCK_EX, path)?;
    Ok(file)
}

/// `open_nofollow` for reading; `None` when `path` does not exist (a dangling link counts as
/// missing). Any other failure is an error.
pub(super) fn open_if_exists(path: &Path) -> Result<Option<File>> {
    match open_nofollow(path, OpenOptions::new().read(true)) {
        Ok(file) => Ok(Some(file)),
        Err(_) if !path.exists() => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    read_open_bounded(
        open_nofollow(path, OpenOptions::new().read(true))?,
        path,
        limit,
    )
}

/// `read_bounded`, or `None` when `path` does not exist.
pub(super) fn read_bounded_if_exists(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    open_if_exists(path)?
        .map(|file| read_open_bounded(file, path, limit))
        .transpose()
}

fn read_open_bounded(file: File, path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!("{} is larger than {limit} bytes", path.display());
    }
    Ok(bytes)
}

/// With `WSX_TRACE_READ_LOG` set, one stderr line per whole-log read: a scenario counts them
/// to prove a reader does not parse a whole log it does not need.
fn trace_read_log(path: &Path) {
    if std::env::var_os("WSX_TRACE_READ_LOG").is_some() {
        eprintln!("wsx-trace read_log {}", path.display());
    }
}

/// The last `limit` bytes of `file`, and how many bytes came before them. When that count
/// is not 0 the first line of the bytes may be cut (see `after_cut_line`).
pub fn read_tail_bytes(file: &mut File, limit: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let start = file.metadata()?.len().saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    Read::take(&mut *file, limit).read_to_end(&mut bytes)?;
    Ok((bytes, start))
}

/// The bytes of `read_tail_bytes` without the first line when the read started inside the
/// file, since that line may be cut.
fn after_cut_line(bytes: &[u8], skipped: u64) -> &[u8] {
    if skipped == 0 {
        return bytes;
    }
    match bytes.iter().position(|byte| *byte == b'\n') {
        Some(end) => &bytes[end + 1..],
        None => &[],
    }
}

/// The lines of `paths` (oldest file first) from their end, as `parse` reads them, in reading
/// order: the last `first` bytes, then four times as many until `enough` holds for them, the
/// files are read whole or `limit` bytes are read. Each step reads and parses only the bytes
/// before the last step's. Lines `parse` refuses are skipped. The one backward read of the
/// task logs: a view needs a log's end, not all of it (docs/tasks.md "Log window").
fn read_back<T>(
    paths: &[PathBuf],
    first: u64,
    limit: u64,
    parse: impl Fn(&[u8]) -> Option<T>,
    enough: impl Fn(&[T]) -> bool,
) -> Result<Vec<T>> {
    // ^ Newest file first: its handle, where its read part starts, and the start of the line
    // cut there, which the next step completes.
    let mut files: Vec<(&PathBuf, File, u64, Vec<u8>)> = Vec::new();
    for path in paths.iter().rev() {
        if let Some(file) = open_if_exists(path)? {
            let len = file
                .metadata()
                .with_context(|| format!("cannot read {}", path.display()))?
                .len();
            files.push((path, file, len, Vec::new()));
        }
    }
    let (mut entries, mut read, mut want) = (Vec::new(), 0u64, first.min(limit));
    for (path, file, start, cut) in &mut files {
        while *start > 0 {
            if read >= want {
                if read >= limit || enough(&entries) {
                    return Ok(entries);
                }
                want = want.saturating_mul(4).min(limit);
            }
            let from = start.saturating_sub(want - read);
            file.seek(SeekFrom::Start(from))
                .with_context(|| format!("cannot read {}", path.display()))?;
            let mut bytes = Vec::new();
            Read::take(&mut *file, *start - from)
                .read_to_end(&mut bytes)
                .with_context(|| format!("cannot read {}", path.display()))?;
            read += *start - from;
            *start = from;
            bytes.append(cut);
            let whole = match from {
                0 => &bytes[..],
                _ => match bytes.iter().position(|byte| *byte == b'\n') {
                    Some(end) => {
                        *cut = bytes[..end].to_vec();
                        &bytes[end + 1..]
                    }
                    None => {
                        *cut = bytes;
                        continue;
                    }
                },
            };
            let mut older: Vec<T> = whole
                .split(|byte| *byte == b'\n')
                .filter_map(&parse)
                .collect();
            older.append(&mut entries);
            entries = older;
        }
    }
    Ok(entries)
}

pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::fs_util::write_atomic(
        path,
        bytes,
        crate::fs_util::AtomicWrite::replace().mode(0o600),
    )
    .with_context(|| format!("cannot write {}", path.display()))
}

/// The most `wsx task post --key` remembers a key.
const POST_KEY_KEEP_SECS: u64 = 24 * 60 * 60;

/// Run `post` unless a post with the same `key` was kept in the last `window_secs` of this
/// project: false then, and nothing runs. The time of a kept post is saved in
/// `post-keys.json` of the project's folder after `post` succeeded, so a dropped post never
/// renews the window; keys older than a day are dropped from the file. A file that cannot be
/// read counts as empty. ref: docs/tasks.md "Routines"
pub fn post_once(
    project_id: &str,
    key: &str,
    now_secs: u64,
    window_secs: u64,
    post: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let path = ensure_project_dir(project_id)?.join("post-keys.json");
    let mut keys: std::collections::BTreeMap<String, u64> = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if keys
        .get(key)
        .is_some_and(|last| now_secs.saturating_sub(*last) < window_secs)
    {
        return Ok(false);
    }
    post()?;
    keys.retain(|_, last| now_secs.saturating_sub(*last) < POST_KEY_KEEP_SECS);
    keys.insert(key.to_string(), now_secs);
    write_atomic(&path, &serde_json::to_vec(&keys)?)?;
    Ok(true)
}

/// Replace a note of the manager's notes folder.
pub fn write_note(path: &Path, text: &str) -> Result<()> {
    write_atomic(path, text.as_bytes())
}

/// `tasks/agents/<account><suffix>`, one account's file (`claude`, `pi-anthropic`), shared by
/// every host on this machine. ref: docs/tasks.md "Agents"
fn account_file(account: &str, suffix: &str) -> Result<PathBuf> {
    if account.is_empty()
        || !account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("no account {account}");
    }
    Ok(tasks_root()
        .join("agents")
        .join(format!("{account}{suffix}")))
}

/// Add a turn's cost to `account`'s spend of the day that starts at `day_ms`, and return that
/// day's total. One file per account holds only the current day. Two hosts writing at once can
/// lose one turn's cost, which a daily budget can bear. ref: docs/tasks.md "Agents"
pub fn add_spend(account: &str, day_ms: u64, micro_usd: u64) -> Result<u64> {
    let path = account_file(account, ".spend.json")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let spent = read_bounded_if_exists(&path, 4096)?
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .filter(|record| record["day_ms"].as_u64() == Some(day_ms))
        .and_then(|record| record["micro_usd"].as_u64())
        .unwrap_or(0)
        .saturating_add(micro_usd);
    write_atomic(
        &path,
        serde_json::json!({ "day_ms": day_ms, "micro_usd": spent })
            .to_string()
            .as_bytes(),
    )?;
    Ok(spent)
}

/// The largest `rate_limit_info` kept; a bigger one is not Claude's and is not written.
const RATE_LIMIT_BYTES: u64 = 16 * 1024;
/// A rate limit record this old is stale past every window Claude reports, and is removed.
const RATE_LIMIT_KEEP_MS: u64 = 8 * 24 * 3_600_000;

/// Keep `account`'s newest `rate_limit_info` as the agent sent it, with when it came: one file
/// per account, rewritten. ref: docs/tasks.md "Usage"
pub fn note_rate_limit(account: &str, info: &serde_json::Value, now_ms: u64) -> Result<()> {
    let path = account_file(account, ".rate.json")?;
    let bytes = serde_json::json!({ "unix_ms": now_ms, "info": info }).to_string();
    if !info.is_object() || bytes.len() as u64 > RATE_LIMIT_BYTES {
        bail!("no rate limit info");
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    write_atomic(&path, bytes.as_bytes())
}

/// `account`'s newest rate limit record (`{unix_ms, info}`). One older than 8 days is removed.
pub fn rate_limit(account: &str, now_ms: u64) -> Option<serde_json::Value> {
    let path = account_file(account, ".rate.json").ok()?;
    let bytes = read_bounded_if_exists(&path, RATE_LIMIT_BYTES + 64).ok()??;
    let record = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    if record["unix_ms"].as_u64()? + RATE_LIMIT_KEEP_MS < now_ms {
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(record)
}

/// A backoff that starts within this time after the last limit ended continues its row: a
/// limit with no reset time that keeps coming back becomes one long limit.
/// ref: docs/tasks.md "Agents"
const LIMIT_ROW_GAP_MS: u64 = 15 * 60_000;

/// An account's limit on record: refused since `since_ms`, until `until_ms` (unix ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountLimit {
    pub since_ms: u64,
    pub until_ms: u64,
}

fn read_limit(account: &str) -> Option<AccountLimit> {
    let bytes = read_bounded_if_exists(&account_file(account, ".json").ok()?, 4096).ok()??;
    let record = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    let until_ms = record["until_ms"].as_u64()?;
    Some(AccountLimit {
        since_ms: record["since_ms"].as_u64().unwrap_or(until_ms),
        until_ms,
    })
}

/// Record that `account` refuses turns until `until_ms`, as of `now_ms`. A limit that `continues` the last one (a backoff with no reset time, or the same
/// wait again) keeps its start when it follows within the row gap.
pub fn mark_limited(account: &str, until_ms: u64, now_ms: u64, continues: bool) -> Result<()> {
    let path = account_file(account, ".json")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let since_ms = continues
        .then(|| read_limit(account))
        .flatten()
        .filter(|last| last.until_ms.saturating_add(LIMIT_ROW_GAP_MS) >= now_ms)
        .map_or(now_ms, |last| last.since_ms.min(now_ms));
    write_atomic(
        &path,
        serde_json::json!({ "since_ms": since_ms, "until_ms": until_ms })
            .to_string()
            .as_bytes(),
    )
}

/// `account`'s recorded limit, if it still holds at `now_ms`. An unreadable record counts as
/// none: the agent's own answer decides then.
pub fn account_limit(account: &str, now_ms: u64) -> Option<AccountLimit> {
    read_limit(account).filter(|limit| limit.until_ms > now_ms)
}

/// When `account`'s recorded limit ends, if it is still ahead of `now_ms`.
pub fn limited_until(account: &str, now_ms: u64) -> Option<u64> {
    account_limit(account, now_ms).map(|limit| limit.until_ms)
}

/// One task's folder. Every path inside it is fixed; no caller-given name reaches the disk.
#[derive(Debug, Clone)]
pub struct TaskDir {
    pub project_id: String,
    pub id: u32,
    pub path: PathBuf,
}

impl TaskDir {
    pub fn open(project_id: &str, id: u32) -> Result<Self> {
        let path = project_dir(project_id)?.join(id.to_string());
        require_real_dir(&path)
            .map_err(|_| anyhow!("task t{id} does not exist in {project_id}"))?;
        Ok(Self {
            project_id: project_id.into(),
            id,
            path,
        })
    }

    /// Take the next number for the project. A number is never reused, also
    /// when the task that took it failed to start and was removed.
    pub fn allocate(project_id: &str) -> Result<Self> {
        let dir = ensure_project_dir(project_id)?;
        let counter_path = dir.join(COUNTER_FILE);
        let mut counter = open_locked(
            &counter_path,
            OpenOptions::new().read(true).write(true).create(true),
        )?;
        let mut text = String::new();
        (&counter).take(32).read_to_string(&mut text)?;
        let mut next = match text.trim() {
            "" => 1,
            value => value
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .with_context(|| format!("{} is not a task number", counter_path.display()))?,
        };
        // ^ A folder left by an older counter still wins: mkdir must create a new one.
        loop {
            let path = dir.join(next.to_string());
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let following = next.checked_add(1).context("task numbers are used up")?;
                    counter.set_len(0)?;
                    std::io::Seek::rewind(&mut counter)?;
                    counter.write_all(following.to_string().as_bytes())?;
                    counter.sync_all()?;
                    return Ok(Self {
                        project_id: project_id.into(),
                        id: next,
                        path,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    next = next.checked_add(1).context("task numbers are used up")?;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot create {}", path.display()))
                }
            }
        }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn read_record(&self) -> Result<TaskRecord> {
        let bytes = read_bounded(&self.file(RECORD_FILE), MAX_RECORD_BYTES)?;
        let record: TaskRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot parse {}", self.file(RECORD_FILE).display()))?;
        if record.id != self.id || record.project_id != self.project_id {
            bail!("{} names another task", self.file(RECORD_FILE).display());
        }
        Ok(record)
    }

    pub fn write_record(&self, record: &TaskRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            bail!("task record is larger than {MAX_RECORD_BYTES} bytes");
        }
        write_atomic(&self.file(RECORD_FILE), &bytes)
    }

    /// Append one event. Returns false when a progress event was dropped
    /// because the log is full.
    pub fn append(&self, event: LogEvent) -> Result<bool> {
        let path = self.file(LOG_FILE);
        let mut file = open_nofollow(&path, OpenOptions::new().append(true).create(true))?;
        let size = file.metadata()?.len();
        if size >= MAX_LOG_HARD_BYTES || (event.is_progress() && size >= MAX_LOG_BYTES) {
            if event.is_progress() {
                return Ok(false);
            }
            bail!("{} is full ({MAX_LOG_HARD_BYTES} bytes)", path.display());
        }
        let mut line = serde_json::to_vec(&LogEntry {
            unix_ms: now_unix_ms(),
            event,
        })?;
        if line.len() > MAX_LOG_LINE_BYTES {
            bail!("log event is larger than {MAX_LOG_LINE_BYTES} bytes");
        }
        line.push(b'\n');
        file.write_all(&line)
            .with_context(|| format!("cannot append to {}", path.display()))?;
        Ok(true)
    }

    pub fn read_log(&self) -> Result<Vec<LogEntry>> {
        let path = self.file(LOG_FILE);
        trace_read_log(&path);
        let Some(file) = open_if_exists(&path)? else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        for (index, line) in BufReader::new(file.take(MAX_LOG_HARD_BYTES + 1))
            .split(b'\n')
            .enumerate()
        {
            let line = line.with_context(|| format!("cannot read {}", path.display()))?;
            if line.len() > MAX_LOG_LINE_BYTES {
                bail!("{} line {} is too long", path.display(), index + 1);
            }
            if line.is_empty() {
                continue;
            }
            // ^ A line cut by a crash mid-write is the last one; skip it rather than fail.
            match serde_json::from_slice(&line) {
                Ok(entry) => entries.push(entry),
                Err(_) => continue,
            }
        }
        Ok(entries)
    }

    /// The entries in the last `bytes` of the log (`read_back`); a line cut at the start is
    /// skipped.
    pub fn read_log_tail(&self, bytes: u64) -> Result<Vec<LogEntry>> {
        read_back(
            &[self.file(LOG_FILE)],
            bytes,
            bytes,
            |line| serde_json::from_slice(line).ok(),
            |_| true,
        )
    }

    /// What `pick` makes of the newest event it accepts, scanning the log from its end: the
    /// last `LOG_TAIL_SCAN_BYTES` first, more only when they hold no hit (`read_back`). Gives
    /// the answer of `read_log().iter().rev().find_map(..)` without a full parse per call.
    pub fn last_event<T>(&self, pick: impl Fn(&LogEvent) -> Option<T>) -> Result<Option<T>> {
        self.last_entry(|entry| pick(&entry.event))
    }

    /// `last_event` with the whole entry, for its time.
    pub fn last_entry<T>(&self, pick: impl Fn(&LogEntry) -> Option<T>) -> Result<Option<T>> {
        let newest = |entries: &[LogEntry]| entries.iter().rev().find_map(&pick);
        let entries = read_back(
            &[self.file(LOG_FILE)],
            LOG_TAIL_SCAN_BYTES,
            MAX_LOG_HARD_BYTES + 1,
            |line| serde_json::from_slice(line).ok(),
            |entries| newest(entries).is_some(),
        )?;
        Ok(newest(&entries))
    }

    /// The original prompt, read from the start of the log only.
    pub fn prompt(&self) -> Result<Option<String>> {
        let file = open_nofollow(&self.file(LOG_FILE), OpenOptions::new().read(true))?;
        let reader = BufReader::new(file.take(2 * MAX_LOG_LINE_BYTES as u64));
        for line in reader.split(b'\n').take(2) {
            let entry: LogEntry = match serde_json::from_slice(&line?) {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if let LogEvent::Message { text, .. } = entry.event {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }

    pub fn write_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.file(name), bytes)
    }

    pub fn read_file(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        read_bounded_if_exists(&self.file(name), limit)
    }

    pub fn socket_path(&self) -> PathBuf {
        socket_for('t', &self.file(SOCKET_FILE))
    }

    /// The socket on which the task host runs the worker's `wsx task check-run`.
    pub fn broker_path(&self) -> PathBuf {
        socket_for('w', &self.file("broker.sock"))
    }

    /// Remove the folder of a task that never started.
    pub fn remove(self) -> Result<()> {
        fs::remove_dir_all(&self.path)
            .with_context(|| format!("cannot remove {}", self.path.display()))
    }
}

const DEPLOYS_FILE: &str = "deploys.jsonl";
const DEPLOYS_OLD_FILE: &str = "deploys.1.jsonl";
/// Deploy records are read from the end of the file only.
const MAX_DEPLOYS_READ_BYTES: u64 = 1024 * 1024;
/// The deploy file rolls over to `deploys.1.jsonl` past this size; one old file is kept,
/// and `read_deploys` reads across both.
const MAX_DEPLOYS_BYTES: u64 = 4 * 1024 * 1024;

const DEPLOY_LOCK_FILE: &str = "deploy.lock";
const FOLLOW_UP_LOG_FILE: &str = "follow-up.log";
const MAX_FOLLOW_UP_LOG_BYTES: u64 = 4 * 1024 * 1024;

/// The output of what a land starts by itself (a deploy, the close of the task), appended;
/// emptied first once it passes 4 MiB. ref: docs/tasks.md "Deploy and release"
pub fn follow_up_log(project_id: &str) -> Result<File> {
    let path = follow_up_log_path(project_id)?;
    let file = open_nofollow(&path, OpenOptions::new().append(true).create(true))?;
    if file.metadata()?.len() > MAX_FOLLOW_UP_LOG_BYTES {
        file.set_len(0)?;
    }
    Ok(file)
}

pub fn follow_up_log_path(project_id: &str) -> Result<PathBuf> {
    Ok(ensure_project_dir(project_id)?.join(FOLLOW_UP_LOG_FILE))
}

/// Held while a deploy or release of the project runs, so they run one at a time.
pub fn lock_deploys(project_id: &str) -> Result<File> {
    let path = ensure_project_dir(project_id)?.join(DEPLOY_LOCK_FILE);
    open_locked(&path, OpenOptions::new().write(true).create(true))
}

/// `lock_deploys` without waiting: `None` while another deploy or release runs.
pub fn try_lock_deploys(project_id: &str) -> Result<Option<File>> {
    let path = ensure_project_dir(project_id)?.join(DEPLOY_LOCK_FILE);
    let file = open_nofollow(&path, OpenOptions::new().write(true).create(true))?;
    Ok(flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB, &path)?.then_some(file))
}

/// The deploy lock a detached run was started with, as descriptor `fd`: refused unless `fd`
/// is the project's `deploy.lock` and holds its lock. ref: docs/tasks.md "Deploy and release"
pub fn adopt_deploy_lock(project_id: &str, fd: i32) -> Result<File> {
    let path = project_dir(project_id)?.join(DEPLOY_LOCK_FILE);
    let expected =
        fs::symlink_metadata(&path).with_context(|| format!("cannot read {}", path.display()))?;
    // SAFETY: fstat only writes the zeroed struct it is given.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0
        || stat.st_dev as u64 != expected.dev()
        || stat.st_ino as u64 != expected.ino()
    {
        bail!("descriptor {fd} is not {}", path.display());
    }
    // ^ The descriptor that holds the lock locks again at once; another one would wait.
    if !flock(fd, libc::LOCK_EX | libc::LOCK_NB, &path)? {
        bail!(
            "descriptor {fd} does not hold the lock of {}",
            path.display()
        );
    }
    // SAFETY: `fd` is open (fstat passed) and this process owns it from here on.
    Ok(unsafe { <File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) })
}

/// Append one deploy or release record to `tasks/<project-id>/deploys.jsonl`.
pub fn append_deploy(project_id: &str, record: &super::DeployRecord) -> Result<()> {
    let dir = ensure_project_dir(project_id)?;
    let path = dir.join(DEPLOYS_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.len() > MAX_DEPLOYS_BYTES) {
        fs::rename(&path, dir.join(DEPLOYS_OLD_FILE))
            .with_context(|| format!("cannot roll over {}", path.display()))?;
    }
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    open_nofollow(&path, OpenOptions::new().append(true).create(true))?
        .write_all(&line)
        .with_context(|| format!("cannot append to {}", path.display()))
}

/// The project's deploy and release records, oldest first, from the last MiB of the
/// records: the end of the current file, and when that is shorter, the end of the old one.
pub fn read_deploys(project_id: &str) -> Result<Vec<super::DeployRecord>> {
    let dir = project_dir(project_id)?;
    let mut records = Vec::new();
    let mut room = MAX_DEPLOYS_READ_BYTES;
    for name in [DEPLOYS_FILE, DEPLOYS_OLD_FILE] {
        let Some(mut file) = open_if_exists(&dir.join(name))? else {
            continue;
        };
        let (bytes, skipped) = read_tail_bytes(&mut file, room)?;
        let found: Vec<super::DeployRecord> = after_cut_line(&bytes, skipped)
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect();
        records.splice(0..0, found);
        // ^ The old file only fills what the current one left of the MiB.
        room = room.saturating_sub(bytes.len() as u64);
        if skipped > 0 || room == 0 {
            break;
        }
    }
    Ok(records)
}

const TRIAL_FILE: &str = "trial.json";
const TRIAL_LOCK_FILE: &str = "trial.lock";
const MAX_TRIAL_BYTES: u64 = 4096;

/// The project's trial, when one holds the live site. ref: docs/tasks.md "Hold and trial"
pub fn read_trial(project_id: &str) -> Result<Option<super::Trial>> {
    let path = project_dir(project_id)?.join(TRIAL_FILE);
    let Some(bytes) = read_bounded_if_exists(&path, MAX_TRIAL_BYTES)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
        format!("cannot parse {}", path.display())
    })?))
}

/// Change the project's trial under its lock: `change` gets the current one and returns
/// the new one (`None` removes it) with its answer.
pub fn change_trial<T>(
    project_id: &str,
    change: impl FnOnce(Option<super::Trial>) -> (Option<super::Trial>, T),
) -> Result<T> {
    let dir = ensure_project_dir(project_id)?;
    let _lock = open_locked(
        &dir.join(TRIAL_LOCK_FILE),
        OpenOptions::new().write(true).create(true),
    )?;
    let current = read_trial(project_id)?;
    let (next, answer) = change(current.clone());
    if next != current {
        let path = dir.join(TRIAL_FILE);
        match &next {
            Some(trial) => write_atomic(&path, &serde_json::to_vec_pretty(trial)?)?,
            None => fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?,
        }
    }
    Ok(answer)
}

/// Whether a trial holds the live site of any project. Hosts do not follow a replaced wsx
/// binary meanwhile: it may be the trial's build. ref: docs/tasks.md "Host"
pub fn any_trial() -> bool {
    fs::read_dir(tasks_root()).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().join(TRIAL_FILE).exists())
    })
}

/// All task numbers of a project, ascending.
pub fn task_numbers(project_id: &str) -> Result<Vec<u32>> {
    let dir = project_dir(project_id)?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("cannot list {}", dir.display())),
    };
    let mut numbers: Vec<u32> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|n| *n > 0)
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

const MANAGER_DIR: &str = "manager";
const MANAGER_LOG: &str = "log.jsonl";
const MANAGER_OLD_LOG: &str = "log.1.jsonl";
const MANAGER_STATE: &str = "state.json";
const MANAGER_SOCKET: &str = "manager.sock";
/// The manager log rolls over to `log.1.jsonl` past this size; one old file is kept.
const MAX_MANAGER_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MANAGER_STATE_BYTES: u64 = 256 * 1024;

/// The project manager's folder, `tasks/<project-id>/manager/`, or a role's,
/// `tasks/<project-id>/roles/<name>/` (docs/tasks.md "Roles").
#[derive(Debug, Clone)]
pub struct ManagerDir {
    pub project_id: String,
    pub path: PathBuf,
    /// The role's name; none is the manager.
    pub role: Option<String>,
}

const ROLES_DIR: &str = "roles";

impl ManagerDir {
    /// Open the folder, creating it owner-only when it is missing.
    pub fn open(project_id: &str) -> Result<Self> {
        Self::open_as(project_id, None)
    }

    /// Open the manager's folder or a role's, creating it owner-only when it is missing.
    pub fn open_as(project_id: &str, role: Option<&str>) -> Result<Self> {
        let project = ensure_project_dir(project_id)?;
        let path = match role {
            None => project.join(MANAGER_DIR),
            Some(name) => {
                create_private_dir(&project.join(ROLES_DIR))?;
                project
                    .join(ROLES_DIR)
                    .join(super::roles::checked_name(name)?)
            }
        };
        create_private_dir(&path)?;
        Ok(Self {
            project_id: project_id.into(),
            path,
            role: role.map(str::to_string),
        })
    }

    /// The folder when it exists; a reader never creates it.
    pub fn existing(project_id: &str) -> Result<Option<Self>> {
        Self::existing_as(project_id, None)
    }

    pub fn existing_as(project_id: &str, role: Option<&str>) -> Result<Option<Self>> {
        let path = match role {
            None => project_dir(project_id)?.join(MANAGER_DIR),
            Some(name) => roles_dir(project_id)?.join(super::roles::checked_name(name)?),
        };
        Ok(require_real_dir(&path).is_ok().then(|| Self {
            project_id: project_id.into(),
            path,
            role: role.map(str::to_string),
        }))
    }

    /// The name the owner reads and the session label's end: the role's, or `manager`.
    pub fn name(&self) -> &str {
        self.role.as_deref().unwrap_or("manager")
    }

    pub fn socket_path(&self) -> PathBuf {
        socket_for('m', &self.path.join(MANAGER_SOCKET))
    }

    /// The socket on which the manager host runs the manager agent's `wsx task` commands.
    /// ref: docs/tasks.md "Permissions"
    pub fn broker_path(&self) -> PathBuf {
        socket_for('b', &self.path.join("broker.sock"))
    }

    /// `manager/notes/`, the manager's notes and decisions; created owner-only.
    pub fn notes(&self) -> Result<PathBuf> {
        let path = self.path.join("notes");
        create_private_dir(&path)?;
        Ok(path)
    }

    pub fn append(&self, event: super::manager::ManagerEvent) -> Result<()> {
        let path = self.path.join(MANAGER_LOG);
        if fs::symlink_metadata(&path).is_ok_and(|meta| meta.len() > MAX_MANAGER_LOG_BYTES) {
            fs::rename(&path, self.path.join(MANAGER_OLD_LOG))
                .with_context(|| format!("cannot roll over {}", path.display()))?;
        }
        let mut line = serde_json::to_vec(&super::manager::ManagerEntry {
            unix_ms: now_unix_ms(),
            event,
        })?;
        if line.len() > MAX_LOG_LINE_BYTES {
            bail!("manager log entry is larger than {MAX_LOG_LINE_BYTES} bytes");
        }
        line.push(b'\n');
        open_nofollow(&path, OpenOptions::new().append(true).create(true))?
            .write_all(&line)
            .with_context(|| format!("cannot append to {}", path.display()))
    }

    /// The old log and the current one, in reading order.
    pub fn log_files(&self) -> [PathBuf; 2] {
        [self.path.join(MANAGER_OLD_LOG), self.path.join(MANAGER_LOG)]
    }

    /// The old log, then the current one; unreadable lines are skipped.
    pub fn read_log(&self) -> Result<Vec<super::manager::ManagerEntry>> {
        trace_read_log(&self.path);
        let mut entries = Vec::new();
        for name in [MANAGER_OLD_LOG, MANAGER_LOG] {
            let Some(bytes) = read_bounded_if_exists(
                &self.path.join(name),
                MAX_MANAGER_LOG_BYTES + MAX_LOG_LINE_BYTES as u64,
            )?
            else {
                continue;
            };
            entries.extend(
                bytes
                    .split(|byte| *byte == b'\n')
                    .filter_map(super::manager::ManagerEntry::read),
            );
        }
        Ok(entries)
    }

    /// The newest entries of the old log and the current one, in reading order: the last
    /// `first` bytes, more until `enough` holds for them (`read_back`). A chat needs the end of
    /// a log of up to 16 MiB, not all of it (docs/tasks.md "Log window").
    pub fn read_back(
        &self,
        first: u64,
        enough: impl Fn(&[super::manager::ManagerEntry]) -> bool,
    ) -> Result<Vec<super::manager::ManagerEntry>> {
        self.read_back_where(first, |_| true, enough)
    }

    /// `read_back` of only the lines `keep` accepts as bytes: a reader that needs few kinds
    /// of entries parses only those.
    pub(super) fn read_back_where(
        &self,
        first: u64,
        keep: impl Fn(&[u8]) -> bool,
        enough: impl Fn(&[super::manager::ManagerEntry]) -> bool,
    ) -> Result<Vec<super::manager::ManagerEntry>> {
        read_back(
            &self.log_files(),
            first,
            2 * (MAX_MANAGER_LOG_BYTES + MAX_LOG_LINE_BYTES as u64),
            |line| match keep(line) {
                true => super::manager::ManagerEntry::read(line),
                false => None,
            },
            enough,
        )
    }

    /// The entries in the last `bytes` of the log, for a quick look at the latest ones.
    pub fn read_tail(&self, bytes: u64) -> Result<Vec<super::manager::ManagerEntry>> {
        self.read_back(bytes, |_| true)
    }

    pub fn read_state(&self) -> Result<super::manager::ManagerState> {
        let path = self.path.join(MANAGER_STATE);
        let Some(bytes) = read_bounded_if_exists(&path, MAX_MANAGER_STATE_BYTES)? else {
            return Ok(Default::default());
        };
        serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn write_state(&self, state: &super::manager::ManagerState) -> Result<()> {
        write_atomic(
            &self.path.join(MANAGER_STATE),
            &serde_json::to_vec_pretty(state)?,
        )
    }
}

const APPROVALS_FILE: &str = "approvals.jsonl";

/// `tasks/<project-id>/roles/`, which holds one folder per role.
pub fn roles_dir(project_id: &str) -> Result<PathBuf> {
    Ok(project_dir(project_id)?.join(ROLES_DIR))
}

/// The files a reader follows to see every change to a project's tasks, its manager's and
/// roles' conversations and its approvals. Missing ones are left out.
pub fn watched_files(project_id: &str) -> Result<Vec<PathBuf>> {
    let dir = project_dir(project_id)?;
    let mut files = vec![
        dir.join(APPROVALS_FILE),
        dir.join(MANAGER_DIR).join(MANAGER_LOG),
        dir.join(OVERVIEW_FILE),
        dir.join(super::checks::CHECKS_FILE),
        dir.join(TRIAL_FILE),
    ];
    for role in super::roles::names(project_id) {
        let role = dir.join(ROLES_DIR).join(role);
        files.extend([role.join(MANAGER_LOG), role.join(super::roles::CLOSED_FILE)]);
    }
    for id in task_numbers(project_id)? {
        let task = dir.join(id.to_string());
        // ^ The host writes the report after the log line that caused it.
        for name in [RECORD_FILE, LOG_FILE, RESULT_FILE, REPORT_FILE] {
            files.push(task.join(name));
        }
    }
    Ok(files)
}

/// Size and modification time of each file `watched_files` names that exists.
pub type Stamp = std::collections::HashMap<PathBuf, (u64, i64, i64)>;

/// The project's `Stamp`: a change in it means the ledger and the task views may differ.
pub fn stamp(project_id: &str) -> Stamp {
    watched_files(project_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| {
            let metadata = fs::metadata(&path).ok()?;
            Some((
                path,
                (metadata.len(), metadata.mtime(), metadata.mtime_nsec()),
            ))
        })
        .collect()
}

pub const OVERVIEW_FILE: &str = "overview.md";
/// The overview's hard size: more is cut in the web view and sent back to the manager.
pub const MAX_OVERVIEW_LINES: usize = 60;
pub const MAX_OVERVIEW_CHARS: usize = 4000;
pub const MAX_OVERVIEW_BYTES: u64 = 256 * 1024;
/// The name before the rename; `migrate_page` moves it.
const OLD_PAGE_FILE: &str = "page.md";

/// `tasks/<project-id>/overview.md`, the project's overview that the manager edits.
/// ref: docs/tasks.md "Project overview"
pub fn overview_path(project_id: &str) -> Result<PathBuf> {
    Ok(project_dir(project_id)?.join(OVERVIEW_FILE))
}

/// A project that has `page.md` and no `overview.md` gets it renamed, once.
fn migrate_page(dir: &Path) {
    let overview = dir.join(OVERVIEW_FILE);
    let old = dir.join(OLD_PAGE_FILE);
    if fs::symlink_metadata(&overview).is_err() && old.is_file() {
        let _ = fs::rename(old, overview);
    }
}

/// Lines and characters of an overview text.
pub fn overview_size(text: &str) -> (usize, usize) {
    (text.lines().count(), text.chars().count())
}

/// Whether the text is over the cap, and its first 60 lines and 4,000 characters.
pub fn cut_overview(text: &str) -> (String, bool) {
    let (lines, chars) = overview_size(text);
    if lines <= MAX_OVERVIEW_LINES && chars <= MAX_OVERVIEW_CHARS {
        return (text.to_string(), false);
    }
    let kept: String = text
        .split_inclusive('\n')
        .take(MAX_OVERVIEW_LINES)
        .collect();
    (kept.chars().take(MAX_OVERVIEW_CHARS).collect(), true)
}

/// Create the overview with its sections when it is missing, owner-only; an existing one stays.
pub fn ensure_overview(project_id: &str, project: &str) -> Result<PathBuf> {
    let dir = ensure_project_dir(project_id)?;
    migrate_page(&dir);
    let path = dir.join(OVERVIEW_FILE);
    let skeleton = format!(
        "# {project}\n\n## Goal\n\n_Not written yet._\n\n## Tasks\n\n```mermaid\ngraph LR\n  start[No tasks yet]\n```\n\n## Decisions\n\n_None yet._\n\n## Open questions\n\n_None yet._\n"
    );
    match open_nofollow(&path, OpenOptions::new().write(true).create_new(true)) {
        Ok(mut file) => file
            .write_all(skeleton.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))?,
        Err(_) if path.is_file() => {}
        Err(error) => return Err(error),
    }
    Ok(path)
}

/// The overview's text and when it last changed, or none when it is missing.
pub fn read_overview(project_id: &str) -> Result<Option<(String, u64)>> {
    migrate_page(&project_dir(project_id)?);
    let path = overview_path(project_id)?;
    let Some(bytes) = read_bounded_if_exists(&path, MAX_OVERVIEW_BYTES)? else {
        return Ok(None);
    };
    let modified = fs::symlink_metadata(&path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    Ok(Some((
        String::from_utf8_lossy(&bytes).into_owned(),
        modified,
    )))
}

const MAX_APPROVALS_BYTES: u64 = 4 * 1024 * 1024;

/// Append an approval line under a lock on the file. A `Request` with an empty id gets
/// the next `a<n>`; the line as written is returned.
pub fn append_approval(
    project_id: &str,
    mut line: super::manager::ApprovalLine,
) -> Result<super::manager::ApprovalLine> {
    use super::manager::ApprovalLine;
    let path = ensure_project_dir(project_id)?.join(APPROVALS_FILE);
    let mut file = open_locked(
        &path,
        OpenOptions::new().read(true).append(true).create(true),
    )?;
    let size = file.metadata()?.len();
    if size > MAX_APPROVALS_BYTES {
        bail!("{} is full ({MAX_APPROVALS_BYTES} bytes)", path.display());
    }
    if let ApprovalLine::Request { id, .. } = &mut line {
        if id.is_empty() {
            let mut text = String::new();
            (&file).read_to_string(&mut text)?;
            // ^ After the highest id of any line, also one this wsx cannot parse (an action
            // it no longer has): counting parsed requests reused a decided id, which hid
            // the new request.
            let highest = text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter_map(|line| line["id"].as_str()?.strip_prefix('a')?.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            *id = format!("a{}", highest + 1);
        }
    }
    let mut bytes = serde_json::to_vec(&line)?;
    bytes.push(b'\n');
    file.write_all(&bytes)
        .with_context(|| format!("cannot append to {}", path.display()))?;
    Ok(line)
}

pub fn read_approvals(project_id: &str) -> Result<Vec<super::manager::ApprovalLine>> {
    let path = project_dir(project_id)?.join(APPROVALS_FILE);
    let Some(bytes) =
        read_bounded_if_exists(&path, MAX_APPROVALS_BYTES + MAX_LOG_LINE_BYTES as u64)?
    else {
        return Ok(Vec::new());
    };
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect())
}

/// Free space for unprivileged writes on the volume of `path`, in MiB.
pub fn free_mb(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid C string and `stats` is written before it is read.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled `stats`.
    let stats = unsafe { stats.assume_init() };
    // ^ `into`: the field types differ between platforms.
    #[allow(clippy::useless_conversion)]
    let (blocks, size): (u64, u64) = (stats.f_bavail.into(), stats.f_frsize.into());
    Some(blocks.saturating_mul(size) / (1024 * 1024))
}
