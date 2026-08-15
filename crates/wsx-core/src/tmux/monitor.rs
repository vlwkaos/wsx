// Bell/activity detection from tmux sessions.
// ref: tmux(1) — list-windows, session_alerts, window_activity

use super::{capture, tmux_cmd};
use crate::model::workspace::ForegroundKind;
use crate::proc_tree::{normalize_comm, ProcTree};
use std::collections::HashMap;

const AGENT_TAIL_LINES: u32 = 12;
const MAX_AGENT_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct AgentMonitorState {
    pub tmux_activity_ts: u64,
    pub has_agent_tail: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentTailSample {
    #[default]
    NotSampled,
    Captured(String),
    Unavailable,
}

pub struct SessionStatus {
    pub has_bell: bool,
    pub last_activity_ts: u64, // Unix timestamp, 0 if unknown
    pub foreground: ForegroundKind,
    pub agent_tail: AgentTailSample,
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
        "claude" | "codex" | "aider" | "opencode" | "gemini" | "qwen" | "pi"
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

fn normalize_agent_tail(raw: &str) -> String {
    let normalized = raw
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = normalized.trim_matches('\n');
    if normalized.len() <= MAX_AGENT_TAIL_BYTES {
        return normalized.to_string();
    }
    let start = normalized
        .char_indices()
        .find_map(|(idx, _)| (normalized.len() - idx <= MAX_AGENT_TAIL_BYTES).then_some(idx))
        .unwrap_or(0);
    normalized[start..].to_string()
}

fn is_pane_id(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn should_sample_agent(previous: Option<&AgentMonitorState>, current_activity_ts: u64) -> bool {
    previous
        .map(|state| !state.has_agent_tail || state.tmux_activity_ts != current_activity_ts)
        .unwrap_or(true)
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

/// Pick the highest-ranked foreground from a process subtree rooted at
/// `pane_pid`. This is what makes `claude` win over its node child (whose
/// `comm` is the version number tmux's `pane_current_command` reports).
/// Returns `None` if `pane_pid` is not present in the tree.
fn classify_pane(pane_pid: u32, tree: &ProcTree) -> Option<ForegroundKind> {
    let descendants = tree.descendants(pane_pid);
    if descendants.is_empty() {
        return None;
    }
    descendants
        .into_iter()
        .map(|(_, comm)| classify_foreground(normalize_comm(comm)))
        .max_by_key(|k| foreground_rank(*k))
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

/// Read process/activity state and sample bounded agent tails only when tmux's
/// raw activity timestamp changes or a baseline is missing.
pub fn session_activity_since(
    previous: &HashMap<String, AgentMonitorState>,
) -> HashMap<String, SessionStatus> {
    let Ok(output) = tmux_cmd(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name}\t#{session_alerts}\t#{window_activity}\t#{pane_current_command}\t#{@wsx-muted}\t#{pane_pid}\t#{pane_id}",
    ])
    .output() else {
        return HashMap::new();
    };

    // One ps call shared across every pane. Agents nested under shells need
    // the tree walk; `pane_current_command` can hide the real foreground.
    let tree = ProcTree::snapshot();
    let mut result: HashMap<String, SessionStatus> = HashMap::new();
    let mut agent_panes: HashMap<String, Vec<String>> = HashMap::new();

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(7, '\t');
        let Some(name) = parts.next() else { continue };
        let Some(alerts) = parts.next() else { continue };
        let Some(ts_str) = parts.next() else { continue };
        let cmd = parts.next().unwrap_or("").trim();
        let muted_str = parts.next().unwrap_or("").trim();
        let pane_pid: Option<u32> = parts.next().and_then(|s| s.trim().parse().ok());
        let pane_id = parts.next().unwrap_or("").trim();
        let name = name.trim().to_string();
        let alerts = alerts.trim();
        let has_bell = !alerts.is_empty() && alerts != "0";
        let ts = ts_str.trim().parse::<u64>().unwrap_or(0);
        let wsx_muted = muted_str == "1";
        let entry = result.entry(name.clone()).or_insert(SessionStatus {
            has_bell: false,
            last_activity_ts: 0,
            foreground: ForegroundKind::Unknown,
            agent_tail: AgentTailSample::NotSampled,
            is_running_wsx: false,
            wsx_muted,
        });
        entry.has_bell |= has_bell;
        entry.wsx_muted |= wsx_muted;
        entry.last_activity_ts = entry.last_activity_ts.max(ts);

        let foreground = pane_pid
            .and_then(|pid| classify_pane(pid, &tree))
            .unwrap_or_else(|| classify_foreground(cmd));
        if foreground == ForegroundKind::Agent && is_pane_id(pane_id) {
            agent_panes
                .entry(name)
                .or_default()
                .push(pane_id.to_string());
        }
        if foreground_rank(foreground) > foreground_rank(entry.foreground) {
            entry.foreground = foreground;
        }

        let wsx_in_tree = pane_pid
            .map(|pid| {
                tree.descendants(pid)
                    .iter()
                    .any(|(_, c)| normalize_comm(c) == "wsx")
            })
            .unwrap_or(false);
        entry.is_running_wsx |= wsx_in_tree || cmd == "wsx";
    }

    for (name, mut panes) in agent_panes {
        let Some(status) = result.get_mut(&name) else {
            continue;
        };
        if !should_sample_agent(previous.get(&name), status.last_activity_ts) {
            continue;
        }
        panes.sort_unstable();
        let mut captures = Vec::with_capacity(panes.len());
        for pane in panes {
            let Some(raw) = capture::capture_pane_plain_window(&pane, Some(AGENT_TAIL_LINES), 0)
            else {
                captures.clear();
                break;
            };
            captures.push(format!("{pane}\n{}", normalize_agent_tail(&raw)));
        }
        status.agent_tail = if captures.is_empty() {
            AgentTailSample::Unavailable
        } else {
            AgentTailSample::Captured(captures.join("\n\u{1e}\n"))
        };
    }

    result
}

pub fn session_activity() -> HashMap<String, SessionStatus> {
    session_activity_since(&HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::{
        foreground_rank, is_pane_id, normalize_agent_tail, should_sample_agent, AgentMonitorState,
    };
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

    #[test]
    fn given_identical_cells_with_trailing_padding_when_normalized_then_equal() {
        assert_eq!(
            normalize_agent_tail("working   \n\n"),
            normalize_agent_tail("working\n")
        );
    }

    #[test]
    fn given_tmux_pane_target_when_validated_then_only_percent_digits_are_accepted() {
        assert!(is_pane_id("%42"));
        assert!(!is_pane_id("%"));
        assert!(!is_pane_id("%42;display-message"));
        assert!(!is_pane_id("session:0.0"));
    }

    #[test]
    fn given_missing_baseline_or_changed_tmux_timestamp_when_checked_then_samples() {
        assert!(should_sample_agent(None, 10));
        let previous = AgentMonitorState {
            tmux_activity_ts: 10,
            has_agent_tail: true,
        };
        assert!(!should_sample_agent(Some(&previous), 10));
        assert!(should_sample_agent(Some(&previous), 11));
    }

    #[test]
    fn given_missing_tail_when_timestamp_is_stable_then_retries_sample() {
        let previous = AgentMonitorState {
            tmux_activity_ts: 10,
            has_agent_tail: false,
        };
        assert!(should_sample_agent(Some(&previous), 10));
    }

    // ── classify_pane (tree-walking foreground picker) ───────────────────────

    use super::classify_pane;
    use crate::proc_tree::ProcTree;

    #[test]
    fn given_single_zsh_node_when_classified_then_shell() {
        let tree = ProcTree::from_rows(&[(100, 1, "zsh")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Shell));
    }

    #[test]
    fn given_chain_zsh_claude_node_when_classified_then_agent_wins() {
        // claude (Agent, rank 5) > node (Runtime, rank 3) > zsh (Shell, rank 1)
        let tree =
            ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "claude"), (300, 200, "node")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Agent));
    }

    #[test]
    fn given_zsh_with_node_child_when_classified_then_runtime() {
        let tree = ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "node")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Runtime));
    }

    #[test]
    fn given_node_with_pi_child_when_classified_then_pi_agent_wins() {
        // ^ Pi runs under a Node launcher; process-tree classification stays provider-neutral.
        let tree = ProcTree::from_rows(&[(100, 1, "node"), (200, 100, "pi")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Agent));
    }

    #[test]
    fn given_zsh_with_vim_child_when_classified_then_interactive_app() {
        let tree = ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "vim")]);
        assert_eq!(
            classify_pane(100, &tree),
            Some(ForegroundKind::InteractiveApp)
        );
    }

    #[test]
    fn given_zsh_with_less_child_when_classified_then_passive_viewer() {
        let tree = ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "less")]);
        assert_eq!(
            classify_pane(100, &tree),
            Some(ForegroundKind::PassiveViewer)
        );
    }

    #[test]
    fn given_zsh_with_claude_and_node_siblings_when_classified_then_agent_wins() {
        let tree =
            ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "claude"), (300, 100, "node")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Agent));
    }

    #[test]
    fn given_agent_under_runtime_parent_when_classified_then_agent_wins() {
        // Parent is Runtime, child is Agent — max_by_key must walk the whole
        // subtree, not just the root.
        let tree =
            ProcTree::from_rows(&[(100, 1, "zsh"), (200, 100, "node"), (300, 200, "claude")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Agent));
    }

    #[test]
    fn given_mid_chain_pid_when_classified_then_only_subtree_seen() {
        // From a non-root pid: only that subtree is visible, ancestors are not.
        let tree =
            ProcTree::from_rows(&[(100, 1, "claude"), (200, 100, "zsh"), (300, 200, "less")]);
        // From 200, subtree is {zsh, less} → PassiveViewer wins over Shell.
        assert_eq!(
            classify_pane(200, &tree),
            Some(ForegroundKind::PassiveViewer)
        );
    }

    #[test]
    fn given_pane_pid_absent_when_classified_then_none() {
        let tree = ProcTree::from_rows(&[(100, 1, "zsh")]);
        assert_eq!(classify_pane(999, &tree), None);
    }

    #[test]
    fn given_empty_tree_when_classified_then_none() {
        let tree = ProcTree::from_rows(&[]);
        assert_eq!(classify_pane(100, &tree), None);
    }

    #[test]
    fn given_normalized_path_and_dash_shells_when_classified_then_shell() {
        let path_tree = ProcTree::from_rows(&[(100, 1, "/bin/zsh")]);
        assert_eq!(classify_pane(100, &path_tree), Some(ForegroundKind::Shell));
        let dash_tree = ProcTree::from_rows(&[(101, 1, "-zsh")]);
        assert_eq!(classify_pane(101, &dash_tree), Some(ForegroundKind::Shell));
    }

    #[test]
    fn given_root_pid_is_agent_alone_when_classified_then_agent() {
        let tree = ProcTree::from_rows(&[(100, 1, "claude")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Agent));
    }

    #[test]
    fn given_unknown_comm_when_classified_then_interactive_app() {
        // Name not in any matcher → InteractiveApp. Catches a regression where
        // unknown names start returning Unknown and silently demote real apps.
        let tree = ProcTree::from_rows(&[(100, 1, "weirdproc-2.1.x")]);
        assert_eq!(
            classify_pane(100, &tree),
            Some(ForegroundKind::InteractiveApp)
        );
    }

    #[test]
    fn given_empty_comm_row_when_classified_then_unknown_rank_minimum() {
        // from_rows allows comm=""; normalize_comm("") → "" → Unknown.
        let tree = ProcTree::from_rows(&[(100, 1, "")]);
        assert_eq!(classify_pane(100, &tree), Some(ForegroundKind::Unknown));
    }
}
