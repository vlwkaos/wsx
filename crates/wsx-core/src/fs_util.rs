//! Atomic file writes and bounded private reads shared by every wsx-owned file.
// ^ AGENTS.md: wsx state files are owner-only and are replaced, never edited in place.

use std::{
    fs,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

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
