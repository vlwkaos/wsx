// Bell/activity detection from tmux sessions.
// ref: tmux(1) — list-windows, session_alerts, window_activity

use super::tmux_cmd;
use crate::model::workspace::ForegroundKind;
use std::collections::HashMap;

pub struct SessionStatus {
    pub has_bell: bool,
    pub last_activity_ts: u64, // Unix timestamp, 0 if unknown
    pub foreground: ForegroundKind,
    pub is_running_wsx: bool, // foreground process is wsx itself
    pub wsx_muted: bool,      // @wsx-muted user option set on this session
}

fn is_shell(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "bash" | "zsh" | "sh" | "fish" | "csh" | "tcsh" | "ksh" | "dash" | "elvish"
    )
}

// Pure output viewers — a live process, but classified PassiveViewer so they
// never read as Active. Runtimes (node, bun, etc.) are classified separately.
fn is_passive(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "watch" | "tail" | "less" | "more" | "man" | "top" | "htop" | "btop" | "bat"
    )
}

fn is_agent(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "claude" | "codex" | "aider" | "opencode" | "gemini" | "qwen"
    )
}

fn is_runtime(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "node"
            | "bun"
            | "deno"
            | "npm"
            | "pnpm"
            | "yarn"
            | "npx"
            | "dotenvx"
            | "watchexec"
            | "entr"
            | "reflex"
    )
}

fn classify_foreground(cmd: &str) -> ForegroundKind {
    if cmd.is_empty() {
        ForegroundKind::Unknown
    } else if is_shell(cmd) {
        ForegroundKind::Shell
    } else if is_passive(cmd) {
        ForegroundKind::PassiveViewer
    } else if is_agent(cmd) {
        ForegroundKind::Agent
    } else if is_runtime(cmd) {
        ForegroundKind::Runtime
    } else {
        ForegroundKind::InteractiveApp
    }
}

// Multi-window aggregation priority — the most significant foreground wins,
// independent of window order.
fn foreground_rank(kind: ForegroundKind) -> u8 {
    match kind {
        ForegroundKind::Unknown => 0,
        ForegroundKind::Shell => 1,
        ForegroundKind::PassiveViewer => 2,
        ForegroundKind::Runtime => 3,
        ForegroundKind::InteractiveApp => 4,
        ForegroundKind::Agent => 5,
    }
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
            foreground: ForegroundKind::Unknown,
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
        let foreground = classify_foreground(cmd);
        if foreground_rank(foreground) > foreground_rank(entry.foreground) {
            entry.foreground = foreground;
        }
        if cmd == "wsx" {
            entry.is_running_wsx = true;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::foreground_rank;
    use crate::model::workspace::ForegroundKind;

    // Strict ordering: Unknown < Shell < PassiveViewer < Runtime
    //                          < InteractiveApp < Agent.
    // Multi-window aggregation keeps the highest rank, so this ordering is what
    // makes classification order-independent.
    #[test]
    fn rank_unknown_lt_shell() {
        assert!(foreground_rank(ForegroundKind::Unknown) < foreground_rank(ForegroundKind::Shell));
    }
    #[test]
    fn rank_shell_lt_passive_viewer() {
        assert!(
            foreground_rank(ForegroundKind::Shell) < foreground_rank(ForegroundKind::PassiveViewer)
        );
    }
    #[test]
    fn rank_passive_viewer_lt_runtime() {
        assert!(
            foreground_rank(ForegroundKind::PassiveViewer)
                < foreground_rank(ForegroundKind::Runtime)
        );
    }
    #[test]
    fn rank_runtime_lt_interactive_app() {
        assert!(
            foreground_rank(ForegroundKind::Runtime)
                < foreground_rank(ForegroundKind::InteractiveApp)
        );
    }
    #[test]
    fn rank_interactive_app_lt_agent() {
        assert!(
            foreground_rank(ForegroundKind::InteractiveApp)
                < foreground_rank(ForegroundKind::Agent)
        );
    }
    #[test]
    fn rank_unknown_lt_agent() {
        assert!(foreground_rank(ForegroundKind::Unknown) < foreground_rank(ForegroundKind::Agent));
    }
}
