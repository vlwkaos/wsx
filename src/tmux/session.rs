// tmux session management via CLI
// ref: tmux(1)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use anyhow::{bail, Result};
use super::{tmux_cmd, tmux_silent};

/// Check if tmux is available.
pub fn is_available() -> bool {
    tmux_cmd(&["-V"]).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

/// Returns true when running inside a tmux session.
pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Return (session_name, session_path) pairs for all active sessions.
pub fn list_sessions_with_paths() -> Vec<(String, PathBuf)> {
    let Ok(output) = tmux_cmd(&["list-sessions", "-F", "#{session_name}:#{session_path}"])
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
    tmux_silent(&["has-session", "-t", name])
        .status().map(|s| s.success()).unwrap_or(false)
}

/// Create a new session with starting directory, detached.
pub fn create_session(name: &str, start_dir: &Path) -> Result<()> {
    let status = tmux_silent(&["new-session", "-d", "-s", name, "-c", &start_dir.to_string_lossy()])
        .status()?;
    if !status.success() { bail!("tmux new-session failed for {}", name); }
    Ok(())
}

/// Kill a session by name.
pub fn kill_session(name: &str) -> Result<()> {
    tmux_silent(&["kill-session", "-t", name]).status()?;
    Ok(())
}

/// Rename a tmux session.
pub fn rename_session(old_name: &str, new_name: &str) -> Result<()> {
    let status = tmux_silent(&["rename-session", "-t", old_name, new_name]).status()?;
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

/// Returns true if the user has a tmux config file (~/.tmux.conf or XDG path).
fn user_has_tmux_config() -> bool {
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".config"));
    dirs::home_dir().map(|h| h.join(".tmux.conf").exists()).unwrap_or(false)
        || xdg.join("tmux/tmux.conf").exists()
}

/// Apply wsx runtime defaults to a session if the user has no tmux config.
/// Best-effort, non-fatal. Skipped when user config exists (let it take over).
pub fn apply_session_defaults(session: &str) {
    let _ = tmux_silent(&["set-option", "-t", session, "mouse", "on"]).status();
    if !user_has_tmux_config() {
        let _ = tmux_silent(&["set-option", "-t", session, "prefix", "C-a"]).status();
        let _ = tmux_silent(&["bind-key", "-T", "prefix", "a", "send-prefix"]).status();
    }
}

/// switch-client (inside tmux path).
pub fn switch_client(name: &str) -> Result<()> {
    let status = tmux_silent(&["switch-client", "-t", name]).status()?;
    if !status.success() { bail!("tmux switch-client failed for {}", name); }
    Ok(())
}

/// attach-session (outside tmux path) — takes over the terminal.
pub fn attach_foreground(name: &str) -> Result<()> {
    tmux_cmd(&["attach-session", "-t", name]).status()?;
    Ok(())
}

/// Send keys to a session's active pane.
pub fn send_keys(session: &str, keys: &str) -> Result<()> {
    tmux_silent(&["send-keys", "-t", session, keys, "Enter"]).status()?;
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
