// An open TUI follows a live daemon upgrade: it replaces its own process with the
// installed wsx. See docs/daemon-live-upgrade.md.

use std::{io, os::unix::process::CommandExt, path::PathBuf, process::Command};
use wsx_core::runtime;

/// The wsx file this process was started from, as the user's shell resolved it.
pub struct Installed {
    path: PathBuf,
    started_as: String,
    // An install that could not be started is not tried again.
    refused: Option<String>,
}

/// A replaced install whose adjacent wsxd is the running daemon.
pub struct Target {
    identity: String,
    pub daemon_version: String,
}

impl Installed {
    pub fn capture() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let started_as = runtime::binary_identity(&path).ok()?;
        Some(Self {
            path,
            started_as,
            refused: None,
        })
    }

    /// The identity of the file now at the start path, when it is another file.
    pub fn replaced(&self) -> Option<(PathBuf, String)> {
        let now = runtime::binary_identity(&self.path).ok()?;
        (now != self.started_as && self.refused.as_ref() != Some(&now))
            .then(|| (self.path.clone(), now))
    }

    pub fn refuse(&mut self, target: Target) {
        self.refused = Some(target.identity);
    }

    /// Replace this process. Returns only when the installed wsx could not start.
    pub fn exec(&self, mobile: bool, pane: Option<runtime::PaneId>) -> io::Error {
        let mut command = Command::new(&self.path);
        if mobile {
            command.arg("--mobile");
        }
        command.args(["--followed-from", runtime::WSX_VERSION]);
        if let Some(pane) = pane {
            command.args(["--follow-pane", &pane.0.to_string()]);
        }
        command.exec()
    }
}

/// Runs off the UI thread: it asks the daemon which binary it is.
/// The packages ship wsx and wsxd together, so a running daemon that is the wsxd beside
/// the replaced wsx means that wsx is the daemon's own release.
pub fn target(client: &runtime::Client, path: PathBuf, identity: String) -> Option<Target> {
    let status = client.lifecycle().ok()?;
    let daemon = path.with_file_name(format!("wsxd{}", std::env::consts::EXE_SUFFIX));
    runtime::binary_identity_matches_file(&daemon, &status.binary_id)
        .ok()?
        .then_some(Target {
            identity,
            daemon_version: status.version,
        })
}
