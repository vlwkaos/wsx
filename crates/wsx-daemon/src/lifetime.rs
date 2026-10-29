//! How long a wsxd lives when what it serves goes away (docs/daemon-discovery.md "Lifetime").

use super::*;

const EXIT_WITH_PID_ENV: &str = "WSX_EXIT_WITH_PID";
const POLL: Duration = Duration::from_secs(1);

/// Watches the directory a daemon serves and the optional `WSX_EXIT_WITH_PID` process.
pub(crate) struct Lifetime {
    dir: PathBuf,
    identity: (u64, u64),
    pid: Option<libc::pid_t>,
}

impl Lifetime {
    pub(crate) fn new(dir: &Path) -> io::Result<Self> {
        let metadata = fs::metadata(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            identity: (metadata.dev(), metadata.ino()),
            pid: exit_with_pid()?,
        })
    }

    /// The directory is gone or replaced, or the process the daemon exits with has ended.
    fn ended(&self) -> bool {
        let replaced = fs::metadata(&self.dir).map_or(true, |metadata| {
            (metadata.dev(), metadata.ino()) != self.identity
        });
        // ^ EPERM still means the process exists.
        let exited = self.pid.is_some_and(|pid| {
            (unsafe { libc::kill(pid, 0) }) == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
        replaced || exited
    }

    /// Calls `stop` once, from a watcher thread, when the lifetime ends.
    pub(crate) fn watch(self, stop: impl FnOnce() + Send + 'static) -> io::Result<()> {
        thread::Builder::new()
            .name("wsxd-lifetime".into())
            .spawn(move || {
                while !self.ended() {
                    thread::sleep(POLL);
                }
                stop();
            })
            .map(drop)
    }
}

fn exit_with_pid() -> io::Result<Option<libc::pid_t>> {
    let Some(raw) = std::env::var_os(EXIT_WITH_PID_ENV).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    raw.to_str()
        .and_then(|value| value.parse::<libc::pid_t>().ok())
        .filter(|pid| *pid > 1)
        .map(Some)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{EXIT_WITH_PID_ENV} must be a process id above 1"),
            )
        })
}
