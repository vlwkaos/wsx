// Bell/activity detection from tmux sessions.
// ref: tmux(1) — list-windows, session_alerts, window_activity

use super::tmux_cmd;
use std::collections::HashMap;

pub struct SessionStatus {
    pub has_bell: bool,
    pub last_activity_ts: u64, // Unix timestamp, 0 if unknown
    pub has_running_app: bool, // foreground process is not a bare shell
    pub is_running_wsx: bool,  // foreground process is wsx itself
    pub wsx_muted: bool,       // @wsx-muted user option set on this session
}

fn is_shell(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "bash" | "zsh" | "sh" | "fish" | "csh" | "tcsh" | "ksh" | "dash" | "elvish"
    )
}

// Pure output viewers — running but not "needing attention"; do not set has_running_app.
// Runtimes (node, bun, etc.) are intentionally excluded: they run agents/servers that warrant yellow.
fn is_passive(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "watch" | "tail" | "less" | "more" | "man" | "top" | "htop" | "btop" | "bat"
    )
}

/// Single tmux call: returns bell flag, last window_activity timestamp, foreground process,
/// and @wsx-muted per session.
pub fn session_activity() -> HashMap<String, SessionStatus> {
    let Ok(output) = tmux_cmd(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name}\t#{session_alerts}\t#{window_activity}\t#{pane_current_command}\t#{@wsx-muted}",
    ])
    .output() else {
        return HashMap::new();
    };

    let mut result: HashMap<String, SessionStatus> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(5, '\t');
        let Some(name) = parts.next() else { continue };
        let Some(alerts) = parts.next() else { continue };
        let Some(ts_str) = parts.next() else { continue };
        let cmd = parts.next().unwrap_or("").trim();
        let muted_str = parts.next().unwrap_or("").trim();
        let name = name.trim().to_string();
        let alerts = alerts.trim();
        let has_bell = !alerts.is_empty() && alerts != "0";
        let ts = ts_str.trim().parse::<u64>().unwrap_or(0);
        let wsx_muted = muted_str == "1";
        let entry = result.entry(name).or_insert(SessionStatus {
            has_bell: false,
            last_activity_ts: 0,
            has_running_app: false,
            is_running_wsx: false,
            wsx_muted,
        });
        entry.has_bell |= has_bell;
        // @wsx-muted is a session option but tmux reports it per-window; OR across windows
        // so any window with the flag set treats the whole session as muted.
        entry.wsx_muted |= wsx_muted;
        if ts > entry.last_activity_ts {
            entry.last_activity_ts = ts;
        }
        // Multi-window priority: has_running_app uses OR — any window with an active
        // process marks the session yellow. Pure viewers (is_passive) don't count.
        if !cmd.is_empty() && !is_shell(cmd) && !is_passive(cmd) {
            entry.has_running_app = true;
        }
        if cmd == "wsx" {
            entry.is_running_wsx = true;
        }
    }
    result
}
