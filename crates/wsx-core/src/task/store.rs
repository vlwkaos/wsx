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

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::fs_util::write_atomic(
        path,
        bytes,
        crate::fs_util::AtomicWrite::replace().mode(0o600),
    )
    .with_context(|| format!("cannot write {}", path.display()))
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

    /// What `pick` makes of the newest event it accepts, scanning the log from its end: the
    /// last `LOG_TAIL_SCAN_BYTES` first, the whole log only when they hold no hit. Gives
    /// the answer of `read_log().iter().rev().find_map(..)` without a full parse per call.
    pub fn last_event<T>(&self, pick: impl Fn(&LogEvent) -> Option<T>) -> Result<Option<T>> {
        let path = self.file(LOG_FILE);
        let Some(mut file) = open_if_exists(&path)? else {
            return Ok(None);
        };
        let (bytes, skipped) = read_tail_bytes(&mut file, LOG_TAIL_SCAN_BYTES)
            .with_context(|| format!("cannot read {}", path.display()))?;
        // ^ A line that does not parse is skipped, as `read_log` does.
        let hit = after_cut_line(&bytes, skipped)
            .split(|byte| *byte == b'\n')
            .rev()
            .filter_map(|line| serde_json::from_slice::<LogEntry>(line).ok())
            .find_map(|entry| pick(&entry.event));
        if hit.is_some() || skipped == 0 {
            return Ok(hit);
        }
        Ok(self
            .read_log()?
            .iter()
            .rev()
            .find_map(|entry| pick(&entry.event)))
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

/// The project manager's folder, `tasks/<project-id>/manager/`.
#[derive(Debug, Clone)]
pub struct ManagerDir {
    pub project_id: String,
    pub path: PathBuf,
}

impl ManagerDir {
    /// Open the folder, creating it owner-only when it is missing.
    pub fn open(project_id: &str) -> Result<Self> {
        let path = ensure_project_dir(project_id)?.join(MANAGER_DIR);
        create_private_dir(&path)?;
        Ok(Self {
            project_id: project_id.into(),
            path,
        })
    }

    /// The folder when it exists; a reader never creates it.
    pub fn existing(project_id: &str) -> Result<Option<Self>> {
        let path = project_dir(project_id)?.join(MANAGER_DIR);
        Ok(require_real_dir(&path).is_ok().then(|| Self {
            project_id: project_id.into(),
            path,
        }))
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

    /// The old log, then the current one; unreadable lines are skipped.
    pub fn read_log(&self) -> Result<Vec<super::manager::ManagerEntry>> {
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

    /// The entries in the last `bytes` of the current log, for a quick look at the latest
    /// ones; a line cut at the start is skipped.
    pub fn read_tail(&self, bytes: u64) -> Result<Vec<super::manager::ManagerEntry>> {
        let path = self.path.join(MANAGER_LOG);
        let Some(mut file) = open_if_exists(&path)? else {
            return Ok(Vec::new());
        };
        let (tail, skipped) = read_tail_bytes(&mut file, bytes)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok(after_cut_line(&tail, skipped)
            .split(|byte| *byte == b'\n')
            .filter_map(super::manager::ManagerEntry::read)
            .collect())
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

/// The files a reader follows to see every change to a project's tasks, its manager
/// conversation and its approvals. Missing ones are left out.
pub fn watched_files(project_id: &str) -> Result<Vec<PathBuf>> {
    let dir = project_dir(project_id)?;
    let mut files = vec![
        dir.join(APPROVALS_FILE),
        dir.join(MANAGER_DIR).join(MANAGER_LOG),
        dir.join(PAGE_FILE),
        dir.join(super::checks::CHECKS_FILE),
        dir.join(TRIAL_FILE),
    ];
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

pub const PAGE_FILE: &str = "page.md";
pub const MAX_PAGE_BYTES: u64 = 256 * 1024;

/// `tasks/<project-id>/page.md`, the project's one-page picture that the manager edits.
/// ref: docs/tasks.md "Project page"
pub fn page_path(project_id: &str) -> Result<PathBuf> {
    Ok(project_dir(project_id)?.join(PAGE_FILE))
}

/// Create the page with its sections when it is missing, owner-only; an existing page stays.
pub fn ensure_page(project_id: &str, project: &str) -> Result<PathBuf> {
    let path = ensure_project_dir(project_id)?.join(PAGE_FILE);
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

/// The page's text and when it last changed, or none when it is missing.
pub fn read_page(project_id: &str) -> Result<Option<(String, u64)>> {
    let path = page_path(project_id)?;
    let Some(bytes) = read_bounded_if_exists(&path, MAX_PAGE_BYTES)? else {
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
