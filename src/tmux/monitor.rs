// Bell/activity detection from tmux sessions.
// ref: tmux(1) — list-windows, session_alerts, window_activity

use std::collections::HashMap;
use super::tmux_cmd;

pub struct SessionStatus {
    pub has_bell: bool,
    pub last_activity_ts: u64,  // Unix timestamp, 0 if unknown
    pub has_running_app: bool,  // foreground process is not a bare shell
}

fn is_shell(cmd: &str) -> bool {
    matches!(cmd.trim(), "bash" | "zsh" | "sh" | "fish" | "csh" | "tcsh" | "ksh" | "dash" | "elvish")
}

// Passive watchers/servers — continuously running but not "needing attention".
fn is_passive(cmd: &str) -> bool {
    matches!(cmd.trim(),
        // output viewers
        "watch" | "tail" | "less" | "more" | "man" | "top" | "htop" | "btop" | "bat" |
        // dev servers / watch-mode runtimes
        "node" | "dotenvx" | "bun"
    )
}

/// Single tmux call: returns bell flag, last window_activity timestamp, and foreground
/// process per session. has_running_app is true if any window's active pane is not a shell.
pub fn session_activity() -> HashMap<String, SessionStatus> {
    let Ok(output) = tmux_cmd(&[
        "list-windows", "-a", "-F",
        "#{session_name}\t#{session_alerts}\t#{window_activity}\t#{pane_current_command}",
    ]).output()
    else { return HashMap::new() };

    let mut result: HashMap<String, SessionStatus> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(4, '\t');
        let Some(name)     = parts.next() else { continue };
        let Some(alerts)   = parts.next() else { continue };
        let Some(ts_str)   = parts.next() else { continue };
        let cmd            = parts.next().unwrap_or("").trim();
        let name = name.trim().to_string();
        let has_bell = !alerts.trim().is_empty() && alerts.trim() != "0";
        let ts = ts_str.trim().parse::<u64>().unwrap_or(0);
        let entry = result.entry(name).or_insert(SessionStatus {
            has_bell: false,
            last_activity_ts: 0,
            has_running_app: false,
        });
        entry.has_bell |= has_bell;
        if ts > entry.last_activity_ts { entry.last_activity_ts = ts; }
        if !cmd.is_empty() && !is_shell(cmd) && !is_passive(cmd) { entry.has_running_app = true; }
    }
    result
}
