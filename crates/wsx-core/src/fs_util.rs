//! Atomic file writes and bounded private reads shared by every wsx-owned file.
// ^ AGENTS.md: wsx state files are owner-only and are replaced, never edited in place.

use std::{
    fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// True when `a` and `b` name the same folder: equal, or one resolves to the other through a
/// symlink. A project folder can be a symlink while wsxd stores the real path.
/// ref: docs/daemon-discovery.md
pub fn same_folder(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Distinguishes the temporary names of one process, so two threads never share one.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Names to try before `write_atomic` gives up on a directory full of stale temporary files.
const TEMP_ATTEMPTS: usize = 16;

/// How the finished temporary file becomes `path`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Publish {
    /// Rename over any existing file.
    Replace,
    /// Hard-link: fails with `AlreadyExists` when `path` exists.
    Create,
}

/// Options of [`write_atomic`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AtomicWrite {
    /// Exact permission bits of the file, whatever the umask. `None` keeps the umask default.
    pub mode: Option<u32>,
    /// `fsync` the file before it is published.
    pub sync_file: bool,
    /// `fsync` the parent directory after it is published.
    pub sync_dir: bool,
    pub publish: Publish,
}

impl AtomicWrite {
    /// Replace `path`, `fsync` the file, keep the default mode, skip the directory `fsync`.
    pub const fn replace() -> Self {
        Self {
            mode: None,
            sync_file: true,
            sync_dir: false,
            publish: Publish::Replace,
        }
    }

    /// Like [`AtomicWrite::replace`], but fail when `path` exists.
    pub const fn create() -> Self {
        Self {
            publish: Publish::Create,
            ..Self::replace()
        }
    }

    pub const fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// `fsync` both the file and its directory, or neither.
    pub const fn durable(mut self, durable: bool) -> Self {
        self.sync_file = durable;
        self.sync_dir = durable;
        self
    }

    pub const fn sync_dir(mut self, sync_dir: bool) -> Self {
        self.sync_dir = sync_dir;
        self
    }
}

/// Write `bytes` to a new sibling file of `path`, then publish it as `path`. A reader sees
/// the old content or the new one, never a part. The temporary name holds the process id and
/// a process-wide counter, is created with `O_EXCL | O_NOFOLLOW`, and is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8], options: AtomicWrite) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (mut file, temporary) = create_temporary(parent, &name, options.mode)?;
    let result = (|| {
        if let Some(mode) = options.mode {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        if options.sync_file {
            file.sync_all()?;
        }
        drop(file);
        match options.publish {
            Publish::Replace => fs::rename(&temporary, path)?,
            Publish::Create => {
                fs::hard_link(&temporary, path)?;
                let _ = fs::remove_file(&temporary);
            }
        }
        if options.sync_dir {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_temporary(
    parent: &Path,
    name: &str,
    mode: Option<u32>,
) -> io::Result<(fs::File, PathBuf)> {
    let mut last = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..TEMP_ATTEMPTS {
        let temporary = parent.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut open = fs::OpenOptions::new();
        open.write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(mode.unwrap_or(0o666));
        match open.open(&temporary) {
            Ok(file) => return Ok((file, temporary)),
            // A stale file of a dead process with the same id and count.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Create `dir` and its missing parents owner-only, then confirm `dir` is a directory, not a
/// symlink, owned by the effective user with no group or other permission bits.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = fs::symlink_metadata(dir)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe wsx state directory {}", dir.display()),
        ));
    }
    Ok(())
}

/// Text of a small file that only this user may read or write: a regular file, not a
/// symlink, owned by the effective user, with no group or other permission bits, at most
/// `cap` bytes. `None` when any of it fails or the file is missing.
pub(crate) fn read_private_small(path: &Path, cap: u64) -> Option<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > cap
    {
        return None;
    }
    let mut text = String::new();
    file.take(cap + 1).read_to_string(&mut text).ok()?;
    Some(text)
}

/// An exclusive `flock` on the open descriptor `fd`, held until it closes. `nonblocking`:
/// `Ok(false)` when another holds it, else the call waits.
pub fn flock(fd: std::os::fd::RawFd, nonblocking: bool) -> io::Result<bool> {
    let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    // SAFETY: flock on a descriptor the caller keeps open.
    if unsafe { libc::flock(fd, operation) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        true => Ok(false),
        false => Err(error),
    }
}

/// The owner-only lock file at `path`, locked by this process, or `None` while another holds
/// it. Refused unless it is a regular file with one link, owned by the effective user, with
/// no group or other permission bits.
pub fn try_lock_file(path: &Path) -> io::Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        // ^ O_NONBLOCK: a FIFO planted at `path` must not block the open.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe lock file {}", path.display()),
        ));
    }
    Ok(flock(file.as_raw_fd(), true)?.then_some(file))
}
