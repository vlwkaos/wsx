// Direct projection of the provider-neutral wsx agent state.

use wsx_core::{model::workspace::SessionInfo, runtime::AgentState};

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
    Error,
}

impl SessionHeuristic {
    pub fn app_state(self) -> AppSessionState {
        match self {
            Self::Muted | Self::Idle | Self::Unknown => AppSessionState::Idle,
            Self::Working => AppSessionState::Active,
            Self::Blocked | Self::Done | Self::Error => AppSessionState::NeedsAttention,
        }
    }
}

pub fn derive(session: &SessionInfo) -> SessionHeuristic {
    if session.muted {
        return SessionHeuristic::Muted;
    }
    match session.agent_status {
        AgentState::Idle => SessionHeuristic::Idle,
        AgentState::Working => SessionHeuristic::Working,
        AgentState::Blocked => SessionHeuristic::Blocked,
        AgentState::Done => SessionHeuristic::Done,
        AgentState::Unknown => SessionHeuristic::Unknown,
        AgentState::Error => SessionHeuristic::Error,
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
        SessionHeuristic::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wsx_core::runtime::{PaneId, SessionId, TerminalId};

    fn session(status: AgentState, muted: bool) -> SessionInfo {
        SessionInfo {
            session_id: SessionId(1),
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            agent: Some("codex".into()),
            display_name: "agent".into(),
            agent_status: status,
            revision: 1,
            layout: wsx_core::runtime::PaneLayout::Leaf { pane_id: PaneId(1) },
            panes: vec![],
            terminal_frame: None,
            muted,
        }
    }

    #[test]
    fn projects_every_runtime_status_exhaustively() {
        let cases = [
            (
                AgentState::Idle,
                SessionHeuristic::Idle,
                AppSessionState::Idle,
                "idle",
            ),
            (
                AgentState::Working,
                SessionHeuristic::Working,
                AppSessionState::Active,
                "working",
            ),
            (
                AgentState::Blocked,
                SessionHeuristic::Blocked,
                AppSessionState::NeedsAttention,
                "blocked",
            ),
            (
                AgentState::Done,
                SessionHeuristic::Done,
                AppSessionState::NeedsAttention,
                "done",
            ),
            (
                AgentState::Unknown,
                SessionHeuristic::Unknown,
                AppSessionState::Idle,
                "unknown",
            ),
            (
                AgentState::Error,
                SessionHeuristic::Error,
                AppSessionState::NeedsAttention,
                "error",
            ),
        ];
        for (status, heuristic, state, label) in cases {
            let session = session(status, false);
            assert_eq!(derive(&session), heuristic);
            assert_eq!(heuristic.app_state(), state);
            assert_eq!(status_label(&session), label);
        }
    }

    #[test]
    fn mute_overrides_runtime_state() {
        let session = session(AgentState::Blocked, true);
        assert_eq!(derive(&session), SessionHeuristic::Muted);
        assert_eq!(status_label(&session), "muted");
    }
}
