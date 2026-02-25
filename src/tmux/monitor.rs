// Bell/activity detection from tmux sessions.
// ref: tmux(1) — list-windows, session_alerts, window_activity

use std::collections::HashMap;
use super::tmux_cmd;

pub struct SessionStatus {
    pub has_bell: bool,
    pub last_activity_ts: u64,  // Unix timestamp, 0 if unknown
}

/// Single tmux call: returns bell flag + last window_activity timestamp per session.
pub fn session_activity() -> HashMap<String, SessionStatus> {
    let Ok(output) = tmux_cmd(&["list-windows", "-a", "-F", "#{session_name}\t#{session_alerts}\t#{window_activity}"])
        .output()
    else { return HashMap::new() };

    let mut result: HashMap<String, SessionStatus> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(name)     = parts.next() else { continue };
        let Some(alerts)   = parts.next() else { continue };
        let Some(ts_str)   = parts.next() else { continue };
        let name = name.trim().to_string();
        let has_bell = !alerts.trim().is_empty() && alerts.trim() != "0";
        let ts = ts_str.trim().parse::<u64>().unwrap_or(0);
        let entry = result.entry(name).or_insert(SessionStatus { has_bell: false, last_activity_ts: 0 });
        entry.has_bell |= has_bell;
        if ts > entry.last_activity_ts { entry.last_activity_ts = ts; }
    }
    result
}
