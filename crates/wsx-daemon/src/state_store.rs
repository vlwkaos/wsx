// ^ docs/daemon-state-store.md: a change is visible in memory first; its reply waits for the save.
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct Loaded<T> {
    pub(super) state: T,
    pub(super) recovered_from_backup: bool,
}

#[derive(Debug)]
enum LoadFailure {
    Missing,
    Invalid(io::Error),
    Unsafe(io::Error),
}

pub(super) fn load<T>(path: &Path, validate: fn(&T) -> io::Result<()>) -> io::Result<Loaded<T>>
where
    T: DeserializeOwned + Default,
{
    match load_one(path, validate) {
        Ok(state) => Ok(Loaded {
            state,
            recovered_from_backup: false,
        }),
        Err(LoadFailure::Missing) => match load_one(&backup_path(path), validate) {
            Ok(state) => {
                eprintln!("wsxd recovered missing primary state from last-known-good backup");
                Ok(Loaded {
                    state,
                    recovered_from_backup: true,
                })
            }
            Err(LoadFailure::Missing) => Ok(Loaded {
                state: T::default(),
                recovered_from_backup: false,
            }),
            Err(LoadFailure::Invalid(error) | LoadFailure::Unsafe(error)) => Err(error),
        },
        Err(LoadFailure::Unsafe(error)) => Err(error),
        Err(LoadFailure::Invalid(primary_error)) => {
            let backup = match load_one(&backup_path(path), validate) {
                Ok(state) => state,
                Err(LoadFailure::Missing) => return Err(primary_error),
                Err(LoadFailure::Invalid(backup_error) | LoadFailure::Unsafe(backup_error)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "primary state is invalid ({primary_error}); backup is unusable ({backup_error})"
                        ),
                    ))
                }
            };
            let quarantine = quarantine_path(path);
            fs::rename(path, &quarantine)?;
            sync_parent(path)?;
            eprintln!(
                "wsxd quarantined invalid state at {} and loaded the last-known-good backup",
                quarantine.display()
            );
            Ok(Loaded {
                state: backup,
                recovered_from_backup: true,
            })
        }
    }
}

/// Validates and serializes `state` for `write`: the checks that can reject a change, so they
/// run before the change is visible.
pub(super) fn encode<T>(state: &T, validate: fn(&T) -> io::Result<()>) -> io::Result<Vec<u8>>
where
    T: Serialize,
{
    validate(state)?;
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "state exceeds limit",
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
pub(super) fn save<T>(path: &Path, state: &T, validate: fn(&T) -> io::Result<()>) -> io::Result<()>
where
    T: Serialize + DeserializeOwned,
{
    write(path, &encode(state, validate)?, validate)
}

/// Replaces the state file atomically and keeps the previous one as `.backup`.
// ^ docs/daemon-state-store.md "Syncs": one full drive flush per save.
pub(super) fn write<T>(
    path: &Path,
    bytes: &[u8],
    validate: fn(&T) -> io::Result<()>,
) -> io::Result<()>
where
    T: DeserializeOwned,
{
    let (temporary, mut file) = create_temporary(path, "tmp")?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        sync_before_rename(&file)?;

        let backup = backup_path(path);
        // ^ No directory sync between the two renames: a crash between them leaves no primary,
        // and `load` then reads the backup.
        let had_primary = match load_one::<T>(path, validate) {
            Ok(_) => {
                fs::rename(path, &backup)?;
                true
            }
            Err(LoadFailure::Missing) => false,
            Err(LoadFailure::Invalid(error) | LoadFailure::Unsafe(error)) => return Err(error),
        };
        if let Err(error) = fs::rename(&temporary, path) {
            if had_primary {
                let _ = fs::rename(&backup, path);
            }
            return Err(error);
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        if !had_primary {
            let (backup_temporary, mut backup_file) = create_temporary(path, "backup.tmp")?;
            let backup_result = (|| {
                backup_file.write_all(bytes)?;
                sync_before_rename(&backup_file)?;
                fs::rename(&backup_temporary, &backup)
            })();
            if backup_result.is_err() {
                let _ = fs::remove_file(&backup_temporary);
            }
            backup_result?;
        }
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Puts a file's bytes on the drive ahead of the rename that publishes it. The final
/// `sync_parent` then makes them durable: fcntl(2) F_FULLFSYNC "drains the entire queue of the
/// device", so data fsync'd on it before is persisted when it returns.
#[cfg(target_os = "macos")]
fn sync_before_rename(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // ^ fcntl(2) F_BARRIERFSYNC: fsync, then a drive barrier, without the full cache flush.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn sync_before_rename(file: &File) -> io::Result<()> {
    file.sync_data()
}

const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(1);

/// The one writer of a state file, on its own thread, so no caller does disk I/O under its
/// state lock. A submission replaces the one not yet written: the newest state wins.
// ^ docs/daemon-state-store.md
pub(super) struct Saver {
    shared: Arc<Shared>,
}

struct Shared {
    path: PathBuf,
    write: fn(&Path, &[u8]) -> io::Result<()>,
    inner: Mutex<Inner>,
    work: Condvar,
    done: Condvar,
}

#[derive(Default)]
struct Inner {
    pending: Option<(u64, Vec<u8>)>,
    submitted: u64,
    written: u64,
    writing: bool,
    failures: u64,
    failure: Option<(u64, String)>,
    held: bool,
    stopped: bool,
}

impl Saver {
    pub(super) fn spawn(
        path: PathBuf,
        write: fn(&Path, &[u8]) -> io::Result<()>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            path,
            write,
            inner: Mutex::default(),
            work: Condvar::new(),
            done: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("wsxd-state-saver".into())
            .spawn(move || worker.run())?;
        Ok(Self { shared })
    }

    /// Queues `bytes` and returns their ticket for `wait`.
    pub(super) fn submit(&self, bytes: Vec<u8>) -> u64 {
        let mut inner = super::lock(&self.shared.inner);
        inner.submitted += 1;
        inner.pending = Some((inner.submitted, bytes));
        self.shared.work.notify_one();
        inner.submitted
    }

    /// Returns when the state of `ticket`, or a newer one, is on disk, or the error of the
    /// first failed write that started after this call. The saver keeps retrying.
    pub(super) fn wait(&self, ticket: u64) -> io::Result<()> {
        let mut inner = super::lock(&self.shared.inner);
        let failures = inner.failures;
        loop {
            if inner.written >= ticket {
                return Ok(());
            }
            if inner.failures > failures {
                if let Some((failed, error)) = &inner.failure {
                    if *failed >= ticket {
                        return Err(io::Error::other(error.clone()));
                    }
                }
            }
            if inner.stopped {
                return Err(io::Error::other("the state saver stopped"));
            }
            inner = self
                .shared
                .done
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// `wait` for everything submitted so far.
    pub(super) fn flush(&self) -> io::Result<()> {
        let ticket = super::lock(&self.shared.inner).submitted;
        self.wait(ticket)
    }

    /// Stops the thread's writes, after the one in progress. A live handoff holds the saver
    /// so the successor alone writes the file once it owns it; `release` resumes.
    pub(super) fn hold(&self) {
        let mut inner = super::lock(&self.shared.inner);
        inner.held = true;
        while inner.writing {
            inner = self
                .shared
                .done
                .wait(inner)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// While held, writes the newest submission on the caller's thread.
    pub(super) fn flush_held(&self) -> io::Result<()> {
        let Some((ticket, bytes)) = super::lock(&self.shared.inner).pending.take() else {
            return Ok(());
        };
        let result = (self.shared.write)(&self.shared.path, &bytes);
        let mut inner = super::lock(&self.shared.inner);
        match &result {
            Ok(()) => inner.written = inner.written.max(ticket),
            Err(_) if inner.pending.is_none() => inner.pending = Some((ticket, bytes)),
            Err(_) => {}
        }
        self.shared.done.notify_all();
        result
    }

    pub(super) fn release(&self) {
        super::lock(&self.shared.inner).held = false;
        self.shared.work.notify_one();
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        super::lock(&self.shared.inner).stopped = true;
        self.shared.work.notify_all();
        self.shared.done.notify_all();
    }
}

impl Shared {
    fn run(&self) {
        let mut inner = super::lock(&self.inner);
        loop {
            if inner.stopped {
                return;
            }
            let next = if inner.held {
                None
            } else {
                inner.pending.take()
            };
            let Some((ticket, bytes)) = next else {
                inner = self
                    .work
                    .wait(inner)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            };
            inner.writing = true;
            drop(inner);
            let result = (self.write)(&self.path, &bytes);
            inner = super::lock(&self.inner);
            inner.writing = false;
            match result {
                Ok(()) => {
                    inner.written = inner.written.max(ticket);
                    inner.failure = None;
                    self.done.notify_all();
                }
                Err(error) => {
                    if inner.failure.is_none() {
                        eprintln!(
                            "wsxd could not save {}: {error}; retrying",
                            self.path.display()
                        );
                    }
                    inner.failures += 1;
                    inner.failure = Some((ticket, error.to_string()));
                    if inner.pending.is_none() {
                        inner.pending = Some((ticket, bytes));
                    }
                    self.done.notify_all();
                    // ^ A newer submission or a stop ends the pause early.
                    inner = self
                        .work
                        .wait_timeout(inner, RETRY_AFTER_FAILURE)
                        .map(|(inner, _)| inner)
                        .unwrap_or_else(|poisoned| poisoned.into_inner().0);
                }
            }
        }
    }
}

/// Creates a new owner-only `<path>.json.<kind>.<pid>.<sequence>` file. A name an earlier
/// process with the same pid left, as one killed mid-save does, takes the next sequence.
fn create_temporary(path: &Path, kind: &str) -> io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let temporary = path.with_extension(format!(
            "json.{kind}.{}.{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temporary name beside the state file",
    ))
}

fn load_one<T>(path: &Path, validate: fn(&T) -> io::Result<()>) -> Result<T, LoadFailure>
where
    T: DeserializeOwned,
{
    let mut file = secure_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(LoadFailure::Unsafe)?;
    let state = serde_json::from_slice(&bytes)
        .map_err(|error| LoadFailure::Invalid(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    validate(&state).map_err(LoadFailure::Invalid)?;
    Ok(state)
}

fn secure_file(path: &Path) -> Result<File, LoadFailure> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                LoadFailure::Missing
            } else {
                LoadFailure::Unsafe(error)
            }
        })?;
    let metadata = file.metadata().map_err(LoadFailure::Unsafe)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(LoadFailure::Unsafe(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe wsx state file",
        )));
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(LoadFailure::Unsafe(io::Error::new(
            io::ErrorKind::InvalidData,
            "state file too large",
        )));
    }
    Ok(file)
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.backup")
}

fn quarantine_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    path.with_extension(format!("json.corrupt.{timestamp}"))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct Fixture {
        value: u64,
    }

    fn valid(_fixture: &Fixture) -> io::Result<()> {
        Ok(())
    }

    fn path(name: &str) -> PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(1);
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/wsx-state-store-tests");
        fs::create_dir_all(&directory).unwrap();
        directory.join(format!(
            "{name}-{}-{}.json",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn invalid_primary_falls_back_to_valid_backup_and_is_quarantined() {
        let path = path("fallback");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let loaded = load::<Fixture>(&path, valid).unwrap();
        assert_eq!(loaded.state, Fixture { value: 1 });
        assert!(loaded.recovered_from_backup);
        let prefix = path.file_stem().unwrap().to_string_lossy().to_string();
        let quarantined = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_name().to_string_lossy().starts_with(&prefix)
                    && entry.file_name().to_string_lossy().contains("corrupt")
            })
            .map(|entry| entry.path());
        assert!(quarantined.is_some());
        let _ = fs::remove_file(quarantined.unwrap());
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn save_never_rotates_an_invalid_primary_over_the_backup() {
        let path = path("preserve-backup");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::write(&path, b"not-json").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(save(&path, &Fixture { value: 3 }, valid).is_err());
        assert_eq!(
            load_one::<Fixture>(&backup_path(&path), valid).unwrap(),
            Fixture { value: 1 }
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn a_temporary_a_killed_save_left_does_not_fail_the_next_save() {
        // A test cancelled mid-save leaves its temporaries; a later process gets its pid.
        let path = path("stale-temporary");
        let next = TEMP_SEQUENCE.load(Ordering::Relaxed);
        let stale = (next..next + 2)
            .flat_map(|sequence| {
                ["tmp", "backup.tmp"].map(|kind| {
                    path.with_extension(format!("json.{kind}.{}.{sequence}", std::process::id()))
                })
            })
            .collect::<Vec<_>>();
        for file in &stale {
            fs::write(file, b"left by a killed save").unwrap();
        }
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        assert_eq!(
            load_one::<Fixture>(&path, valid).unwrap(),
            Fixture { value: 1 }
        );
        assert_eq!(
            load_one::<Fixture>(&backup_path(&path), valid).unwrap(),
            Fixture { value: 1 }
        );
        for file in stale {
            assert_eq!(fs::read(&file).unwrap(), b"left by a killed save");
            fs::remove_file(file).unwrap();
        }
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn unsafe_primary_does_not_fall_back() {
        let path = path("unsafe");
        save(&path, &Fixture { value: 1 }, valid).unwrap();
        save(&path, &Fixture { value: 2 }, valid).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            load::<Fixture>(&path, valid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }
}
