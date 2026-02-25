pub mod session;
pub mod capture;
pub mod monitor;

use std::process::{Command, Stdio};

/// tmux command with pre-set args.
pub fn tmux_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("tmux");
    cmd.args(args);
    cmd
}

/// tmux command with stdout/stderr suppressed.
pub fn tmux_silent(args: &[&str]) -> Command {
    let mut cmd = Command::new("tmux");
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::null());
    cmd
}
