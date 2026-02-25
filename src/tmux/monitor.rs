// Bell/activity detection from tmux sessions.

use std::collections::HashMap;
use std::process::Command;

pub fn session_activity() -> HashMap<String, bool> {
    let Ok(output) = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}:#{session_alerts}"])
        .output()
    else { return HashMap::new() };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ':');
            let name = parts.next()?.trim().to_string();
            let alerts = parts.next()?.trim();
            Some((name, !alerts.is_empty() && alerts != "0"))
        })
        .collect()
}
