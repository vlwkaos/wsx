//! Root-confined file access for the one granted project.
//! ^ Ported from the 0.30 preview (abe5197 wsx-web/src/lib.rs) after review: every
//! component is opened with O_NOFOLLOW relative to a descriptor of the pinned root,
//! so a symlink or a swapped directory cannot move a read or write outside it.
use std::{
    ffi::{CStr, CString},
    fs::{self, File},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
    sync::Mutex,
};

pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_DIRECTORY_SCAN: usize = 2048;
const MAX_DIRECTORY_ITEMS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxError {
    Forbidden,
    InvalidPath,
    NotFound,
    TooLarge,
    Conflict,
}

#[derive(Debug, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
}

pub struct Root {
    path: PathBuf,
    device: u64,
    inode: u64,
    write_lock: Mutex<()>,
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

impl Root {
    pub fn pin(path: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            write_lock: Mutex::new(()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Map an absolute path, or a path relative to the root, to a path below the
    /// root, textually. Only a candidate: `open_file` still enforces confinement.
    pub fn relative(&self, candidate: &Path) -> Option<PathBuf> {
        let relative = candidate.strip_prefix(&self.path).unwrap_or(candidate);
        valid(relative, false).then(|| relative.to_owned())
    }

    pub fn list(&self, relative: &Path) -> Result<(Vec<Entry>, bool), SandboxError> {
        if !valid(relative, true) {
            return Err(SandboxError::InvalidPath);
        }
        let mut directory = self.open_root()?;
        for component in relative.components() {
            directory = open_nofollow(
                directory.as_raw_fd(),
                component.as_os_str().as_bytes(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(path_error)?;
        }
        // fdopendir owns the duplicate; `directory` stays open for fstatat.
        let copy = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(SandboxError::Forbidden);
        }
        let stream = unsafe { libc::fdopendir(copy) };
        if stream.is_null() {
            unsafe {
                libc::close(copy);
            }
            return Err(SandboxError::Forbidden);
        }
        let stream = DirectoryStream(stream);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut scanned = 0;
        loop {
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." || bytes == b".git" {
                continue;
            }
            scanned += 1;
            if scanned > MAX_DIRECTORY_SCAN {
                truncated = true;
                break;
            }
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `name` is NUL-terminated and fstatat initializes `info` on success.
            let status = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    info.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status != 0 {
                continue;
            }
            let kind = unsafe { info.assume_init() }.st_mode & libc::S_IFMT;
            if kind == libc::S_IFDIR || kind == libc::S_IFREG {
                entries.push(Entry {
                    name: text.to_owned(),
                    directory: kind == libc::S_IFDIR,
                });
            }
        }
        entries.sort_by(|left, right| {
            right
                .directory
                .cmp(&left.directory)
                .then_with(|| left.name.cmp(&right.name))
        });
        if entries.len() > MAX_DIRECTORY_ITEMS {
            entries.truncate(MAX_DIRECTORY_ITEMS);
            truncated = true;
        }
        Ok((entries, truncated))
    }

    /// Open one regular file below the root without following any symlink.
    pub fn open_file(&self, relative: &Path) -> Result<File, SandboxError> {
        let (directory, name) = self.open_parent(relative)?;
        let file = open_nofollow(
            directory.as_raw_fd(),
            name.as_bytes(),
            libc::O_RDONLY | libc::O_NONBLOCK,
        )
        .map_err(path_error)?;
        if !file
            .metadata()
            .map_err(|_| SandboxError::Forbidden)?
            .is_file()
        {
            return Err(SandboxError::Forbidden);
        }
        Ok(file)
    }

    /// Both the advertised and the actual size are bounded: the file may grow after open.
    pub fn read(&self, relative: &Path, limit: u64) -> Result<Vec<u8>, SandboxError> {
        read_bounded(self.open_file(relative)?, limit)
    }

    /// Replace an existing regular text file. `original` is an optimistic revision
    /// token: the write is refused when the file changed since the browser read it.
    pub fn write_text(
        &self,
        relative: &Path,
        original: &str,
        content: &str,
    ) -> Result<(), SandboxError> {
        if content.len() as u64 > MAX_TEXT_BYTES || content.contains('\0') {
            return Err(SandboxError::TooLarge);
        }
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| SandboxError::Forbidden)?;
        let (directory, name) = self.open_parent(relative)?;
        let existing = open_nofollow(
            directory.as_raw_fd(),
            name.as_bytes(),
            libc::O_RDONLY | libc::O_NONBLOCK,
        )
        .map_err(path_error)?;
        let metadata = existing.metadata().map_err(|_| SandboxError::Forbidden)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(SandboxError::Forbidden);
        }
        let current = read_bounded(existing, MAX_TEXT_BYTES)?;
        if current != original.as_bytes() {
            return Err(SandboxError::Conflict);
        }
        let nonce = crate::devices::random_bytes::<8>().map_err(|_| SandboxError::Forbidden)?;
        let temporary = CString::new(format!(".wsx-write-{}", crate::devices::hex(&nonce)))
            .expect("hex name has no NUL");
        let mut replacement = open_nofollow(
            directory.as_raw_fd(),
            temporary.as_bytes(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(path_error)?;
        let result = (|| -> Result<(), SandboxError> {
            replacement
                .write_all(content.as_bytes())
                .and_then(|_| {
                    replacement.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))
                })
                .and_then(|_| replacement.sync_all())
                .map_err(|_| SandboxError::Forbidden)?;
            let mut before = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: both pointers are valid and fstatat initializes `before` on success.
            let status = unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    before.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status != 0 {
                return Err(SandboxError::Conflict);
            }
            let before = unsafe { before.assume_init() };
            if before.st_ino != metadata.ino() || before.st_nlink != 1 {
                return Err(SandboxError::Conflict);
            }
            // A same-user writer that ignores this lock can still race the rename.
            // The revision check above protects ordinary concurrent browser edits.
            let status = unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            };
            if status != 0 {
                return Err(SandboxError::Forbidden);
            }
            let _ = directory.sync_all();
            Ok(())
        })();
        // Only this call created the random exclusive entry; after a rename it is gone.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
        result
    }

    fn open_parent(&self, relative: &Path) -> Result<(File, CString), SandboxError> {
        if !valid(relative, false) {
            return Err(SandboxError::InvalidPath);
        }
        let components: Vec<_> = relative.components().collect();
        let mut directory = self.open_root()?;
        for component in &components[..components.len() - 1] {
            directory = open_nofollow(
                directory.as_raw_fd(),
                component.as_os_str().as_bytes(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(path_error)?;
        }
        let name = CString::new(components[components.len() - 1].as_os_str().as_bytes())
            .map_err(|_| SandboxError::InvalidPath)?;
        Ok((directory, name))
    }

    fn open_root(&self) -> Result<File, SandboxError> {
        let directory = open_nofollow(
            libc::AT_FDCWD,
            self.path.as_os_str().as_bytes(),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(path_error)?;
        let metadata = directory.metadata().map_err(|_| SandboxError::Forbidden)?;
        if metadata.dev() != self.device || metadata.ino() != self.inode {
            return Err(SandboxError::Forbidden);
        }
        Ok(directory)
    }
}

/// Only plain names: no root, `.`, `..` or prefix component, and a bounded depth.
/// `.git` is refused everywhere: its config can run commands as the host user.
fn valid(relative: &Path, allow_empty: bool) -> bool {
    let mut count = 0;
    for component in relative.components() {
        if !matches!(component, Component::Normal(name) if name != ".git") {
            return false;
        }
        count += 1;
    }
    relative.as_os_str().len() <= 2048 && count <= 32 && (allow_empty || count > 0)
}

pub fn read_bounded(file: File, limit: u64) -> Result<Vec<u8>, SandboxError> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SandboxError::Forbidden)?;
    if bytes.len() as u64 > limit {
        return Err(SandboxError::TooLarge);
    }
    Ok(bytes)
}

fn path_error(error: io::Error) -> SandboxError {
    if error.kind() == io::ErrorKind::NotFound {
        SandboxError::NotFound
    } else {
        SandboxError::Forbidden
    }
}

fn open_nofollow(parent: libc::c_int, name: &[u8], flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: `name` is NUL-terminated; openat returns an owned descriptor on success.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
