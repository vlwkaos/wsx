pub mod info;
pub mod ops;
pub mod worktree;

use std::path::Path;
use std::process::Command;

/// Base git command scoped to `repo` via `-C`.
/// Stdin is null so git never blocks waiting for credentials or hook input.
pub fn git_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).stdin(std::process::Stdio::null());
    cmd
}
