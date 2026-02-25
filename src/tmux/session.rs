// tmux session management via CLI
// ref: tmux(1)

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use anyhow::{bail, Result};

/// Check if tmux is available.
pub fn is_available() -> bool {
    Command::new("tmux").arg("-V").stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

/// Returns true when running inside a tmux session.
pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Return (session_name, session_path) pairs for all active sessions.
pub fn list_sessions_with_paths() -> Vec<(String, PathBuf)> {
    let Ok(output) = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}:#{session_path}"])
        .output()
    else { return vec![] };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ':');
            let name = parts.next()?.trim().to_string();
            let path = parts.next()?.trim().to_string();
            if name.is_empty() || path.is_empty() { return None; }
            Some((name, PathBuf::from(path)))
        })
        .collect()
}

/// Return true if a named session exists.
pub fn session_exists(name: &str) -> bool {
    Command::new("tmux").args(["has-session", "-t", name])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

/// Create a new session with starting directory, detached.
pub fn create_session(name: &str, start_dir: &Path) -> Result<()> {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", &start_dir.to_string_lossy()])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()?;
    if !status.success() { bail!("tmux new-session failed for {}", name); }
    Ok(())
}

/// Create an ephemeral session that runs a command directly (dies on exit).
pub fn create_ephemeral_session(name: &str, start_dir: &Path, command: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", &start_dir.to_string_lossy(), command])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()?;
    if !status.success() { bail!("tmux new-session (ephemeral) failed for {}", name); }
    Ok(())
}

/// Kill a session by name.
pub fn kill_session(name: &str) -> Result<()> {
    Command::new("tmux").args(["kill-session", "-t", name])
        .stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    Ok(())
}

/// Rename a tmux session.
pub fn rename_session(old_name: &str, new_name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["rename-session", "-t", old_name, new_name])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status()?;
    if !status.success() { bail!("tmux rename-session failed"); }
    Ok(())
}

pub fn attach_session_cmd(name: &str) -> AttachCommand {
    if is_inside_tmux() {
        AttachCommand::SwitchClient(name.to_string())
    } else {
        AttachCommand::Attach(name.to_string())
    }
}

pub enum AttachCommand {
    SwitchClient(String),
    Attach(String),
}

/// switch-client (inside tmux path).
pub fn switch_client(name: &str) -> Result<()> {
    let status = Command::new("tmux").args(["switch-client", "-t", name])
        .stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    if !status.success() { bail!("tmux switch-client failed for {}", name); }
    Ok(())
}

/// Send keys to a session's active pane.
pub fn send_keys(session: &str, keys: &str) -> Result<()> {
    Command::new("tmux").args(["send-keys", "-t", session, keys, "Enter"])
        .stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    Ok(())
}

/// Generate a unique session name that doesn't conflict with existing sessions.
pub fn unique_session_name(base: &str) -> String {
    if !session_exists(base) { return base.to_string(); }
    let mut n = 2;
    loop {
        let candidate = format!("{}_{}", base, n);
        if !session_exists(&candidate) { return candidate; }
        n += 1;
    }
}
