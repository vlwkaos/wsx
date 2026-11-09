//! A project's own agent sessions that need the owner, as chat lines. The manager host reads
//! wsxd's snapshot and posts each session once per transition into `exited`, `error` or
//! `blocked`, worded as the web's Home words it. ref: docs/tasks.md "Session news"

use crate::runtime::{AgentState, Pane, Snapshot};
use std::collections::BTreeMap;
use std::path::Path;

/// A session that belongs to a task or to the manager: `task-<n>`, `task-<n>-hand`,
/// `task-manager` or a role's `task-<role>` (provider `task` or `manager`). The web's
/// `isTaskSession` (store.js) is the same rule.
pub fn is_task_session(label: &str, provider: Option<&str>) -> bool {
    let named = label.strip_prefix("task-").is_some_and(|rest| {
        let number = rest.strip_suffix("-hand").unwrap_or(rest);
        rest == "manager" || number.parse::<u32>().is_ok()
    });
    named || matches!(provider, Some("task" | "manager"))
}

/// One session in need and the line that tells so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNews {
    pub session: u64,
    /// Changes when the session leaves the state and enters it again.
    pub marker: String,
    /// The chat event title: `session claude in main`.
    pub event: String,
    /// The chat post: `[claude in main](session/12) exited`.
    pub text: String,
}

/// The sessions of the project at `main` that are in an attention state now, with an agent
/// that is not a task's or the manager's. The pane is the web's: the agent's, else the first.
pub fn attention(snapshot: &Snapshot, main: &Path) -> Vec<SessionNews> {
    let same = |path: &Path| crate::fs_util::same_folder(path, main);
    let Some(project) = snapshot.projects.iter().find(|p| same(&p.path)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for session in &snapshot.sessions {
        let Some(worktree) = snapshot
            .worktrees
            .iter()
            .find(|w| w.id == session.worktree_id && w.project_id == project.id)
        else {
            continue;
        };
        let panes = || {
            session
                .panes
                .iter()
                .filter_map(|id| snapshot.panes.iter().find(|p| p.id == *id))
        };
        let pane: Option<&Pane> = panes()
            .find(|p| p.agent.is_some())
            .or_else(|| panes().find(|p| p.id == session.primary_pane));
        let Some((pane, agent)) = pane.and_then(|p| Some((p, p.agent.as_ref()?))) else {
            continue;
        };
        if is_task_session(&session.label, Some(&agent.provider)) {
            continue;
        }
        let (word, kind) = match (pane.exited, agent.attached, agent.state) {
            (true, ..) => ("exited", "exited"),
            (_, true, AgentState::Blocked) => ("needs input", "blocked"),
            (_, true, AgentState::Error) => ("ended in an error", "error"),
            _ => continue,
        };
        let who = format!("{} in {}", agent.provider, worktree.branch);
        found.push(SessionNews {
            session: session.id.0,
            marker: format!("{kind}:{}", agent.state_since_unix_ms.unwrap_or(0)),
            event: format!("session {who}"),
            text: format!("[{who}](session/{}) {word}", session.id.0),
        });
    }
    found
}

/// The news not yet posted, and the map to keep: the sessions in need now by marker. A session
/// that left a state and entered it again has a new marker. With no `seen` (the host's first
/// look) nothing is news: what is in need then is the baseline.
pub fn unposted(
    found: Vec<SessionNews>,
    seen: Option<&BTreeMap<u64, String>>,
) -> (Vec<SessionNews>, BTreeMap<u64, String>) {
    let keep = found
        .iter()
        .map(|n| (n.session, n.marker.clone()))
        .collect();
    let fresh = match seen {
        None => Vec::new(),
        Some(seen) => found
            .into_iter()
            .filter(|n| seen.get(&n.session) != Some(&n.marker))
            .collect(),
    };
    (fresh, keep)
}
