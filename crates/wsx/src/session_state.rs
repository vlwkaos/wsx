// Direct projection of Herdr's protocol-20 agent state.

use wsx_core::{herdr::AgentStatus, model::workspace::SessionInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSessionState {
    Idle,
    Active,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHeuristic {
    Muted,
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl SessionHeuristic {
    pub fn app_state(self) -> AppSessionState {
        match self {
            Self::Muted | Self::Idle | Self::Unknown => AppSessionState::Idle,
            Self::Working => AppSessionState::Active,
            Self::Blocked | Self::Done => AppSessionState::NeedsAttention,
        }
    }
}

pub fn derive(session: &SessionInfo) -> SessionHeuristic {
    if session.muted {
        return SessionHeuristic::Muted;
    }
    match session.agent_status {
        AgentStatus::Idle => SessionHeuristic::Idle,
        AgentStatus::Working => SessionHeuristic::Working,
        AgentStatus::Blocked => SessionHeuristic::Blocked,
        AgentStatus::Done => SessionHeuristic::Done,
        AgentStatus::Unknown => SessionHeuristic::Unknown,
    }
}

pub fn status_label(session: &SessionInfo) -> &'static str {
    match derive(session) {
        SessionHeuristic::Muted => "muted",
        SessionHeuristic::Idle => "idle",
        SessionHeuristic::Working => "working",
        SessionHeuristic::Blocked => "blocked",
        SessionHeuristic::Done => "done",
        SessionHeuristic::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: AgentStatus, muted: bool) -> SessionInfo {
        SessionInfo {
            pane_id: "pane-1".into(),
            terminal_id: "terminal-1".into(),
            workspace_id: "workspace-1".into(),
            tab_id: "tab-1".into(),
            display_name: "agent".into(),
            agent_status: status,
            revision: 1,
            pane_capture: None,
            muted,
        }
    }

    #[test]
    fn projects_every_herdr_status_exhaustively() {
        let cases = [
            (
                AgentStatus::Idle,
                SessionHeuristic::Idle,
                AppSessionState::Idle,
                "idle",
            ),
            (
                AgentStatus::Working,
                SessionHeuristic::Working,
                AppSessionState::Active,
                "working",
            ),
            (
                AgentStatus::Blocked,
                SessionHeuristic::Blocked,
                AppSessionState::NeedsAttention,
                "blocked",
            ),
            (
                AgentStatus::Done,
                SessionHeuristic::Done,
                AppSessionState::NeedsAttention,
                "done",
            ),
            (
                AgentStatus::Unknown,
                SessionHeuristic::Unknown,
                AppSessionState::Idle,
                "unknown",
            ),
        ];
        for (status, heuristic, projected, label) in cases {
            let session = session(status, false);
            assert_eq!(derive(&session), heuristic);
            assert_eq!(heuristic.app_state(), projected);
            assert_eq!(status_label(&session), label);
        }
    }

    #[test]
    fn mute_takes_precedence_over_every_herdr_status() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Done,
            AgentStatus::Unknown,
        ] {
            let session = session(status, true);
            assert_eq!(derive(&session), SessionHeuristic::Muted);
            assert_eq!(status_label(&session), "muted");
        }
    }
}
