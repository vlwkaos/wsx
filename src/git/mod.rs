pub mod worktree;
pub mod info;

use std::path::Path;
use std::process::Command;

/// Base git command scoped to `repo` via `-C`.
pub fn git_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd
}
