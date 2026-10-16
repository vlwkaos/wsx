//! Files of one task under `$XDG_STATE_HOME/wsx/tasks/<project-id>/<n>/`.
//! ref: docs/tasks.md "Store"

use super::{now_unix_ms, LogEntry, LogEvent, TaskRecord};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_LOG_LINE_BYTES: usize = 160 * 1024;
/// Past this size only non-progress events are appended.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Hard stop, so a stream of steers cannot fill the disk either.
const MAX_LOG_HARD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PROJECT_ID_BYTES: usize = 64;
pub const RECORD_FILE: &str = "task.json";
pub const LOG_FILE: &str = "log.jsonl";
pub const RESULT_FILE: &str = "result.json";
pub const REPORT_FILE: &str = "report.md";
pub const SOCKET_FILE: &str = "host.sock";
const COUNTER_FILE: &str = "next";

fn tasks_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("wsx/tasks")
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

fn create_private_dir(path: &Path) -> Result<()> {
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

fn project_dir(project_id: &str) -> Result<PathBuf> {
    validate_project_id(project_id)?;
    Ok(tasks_root().join(project_id))
}

/// Create `tasks/<project-id>/` and its parents, each owner-only.
fn ensure_project_dir(project_id: &str) -> Result<PathBuf> {
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

fn open_nofollow(path: &Path, options: &mut OpenOptions) -> Result<File> {
    options
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = open_nofollow(path, OpenOptions::new().read(true))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.len() as u64 > limit {
        bail!("{} is larger than {limit} bytes", path.display());
    }
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = open_nofollow(
            &temporary,
            OpenOptions::new().write(true).create(true).truncate(true),
        )?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("cannot write {}", path.display()))
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
        let mut counter = open_nofollow(
            &counter_path,
            OpenOptions::new().read(true).write(true).create(true),
        )?;
        // SAFETY: flock on an open descriptor; released when `counter` closes.
        if unsafe { libc::flock(counter.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot lock {}", counter_path.display()));
        }
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
        let file = match open_nofollow(&path, OpenOptions::new().read(true)) {
            Ok(file) => file,
            Err(_) if !path.exists() => return Ok(Vec::new()),
            Err(error) => return Err(error),
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
        let path = self.file(name);
        if !path.exists() {
            return Ok(None);
        }
        read_bounded(&path, limit).map(Some)
    }

    pub fn socket_path(&self) -> PathBuf {
        self.file(SOCKET_FILE)
    }

    /// Remove the folder of a task that never started.
    pub fn remove(self) -> Result<()> {
        fs::remove_dir_all(&self.path)
            .with_context(|| format!("cannot remove {}", self.path.display()))
    }
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
