// Direct projection of the provider-neutral wsx agent state.

use wsx_core::{model::workspace::SessionInfo, runtime::AgentState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSessionState {
    Idle,
    Running,
    Active,
    NeedsAttention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHeuristic {
    Muted,
    Idle,
    Running,
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
            Self::Running => AppSessionState::Running,
            Self::Working => AppSessionState::Active,
            Self::Blocked | Self::Done | Self::Error => AppSessionState::NeedsAttention,
        }
    }
}

pub fn derive(session: &SessionInfo) -> SessionHeuristic {
    if session.muted {
        return SessionHeuristic::Muted;
    }
    if !session.is_agentic() && session.has_foreground_job() {
        return SessionHeuristic::Running;
    }
    match session.agent_status {
        AgentState::Idle => SessionHeuristic::Idle,
        AgentState::Working => SessionHeuristic::Working,
        AgentState::Blocked => SessionHeuristic::Blocked,
        AgentState::Done if session.outcome_acknowledged => SessionHeuristic::Idle,
        AgentState::Done => SessionHeuristic::Done,
        AgentState::Unknown => SessionHeuristic::Unknown,
        AgentState::Error => SessionHeuristic::Error,
    }
}

pub fn agent_label(agent: Option<&str>) -> Option<String> {
    agent.map(|agent| format!(" ({agent})"))
}

pub fn status_label(session: &SessionInfo) -> &'static str {
    match derive(session) {
        SessionHeuristic::Muted => "muted",
        SessionHeuristic::Idle => "idle",
        SessionHeuristic::Running => "running",
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
    use wsx_core::{
        model::workspace::PaneInfo,
        runtime::{PaneId, SessionId, TerminalId},
    };

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
            muted,
            outcome_acknowledged: false,
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
    fn agent_identity_is_parenthesized_only_when_reported() {
        assert_eq!(agent_label(Some("pi")).as_deref(), Some(" (pi)"));
        assert_eq!(agent_label(None), None);
    }

    #[test]
    fn shell_foreground_job_is_running_without_overriding_agent_lifecycle() {
        let mut shell = session(AgentState::Unknown, false);
        shell.agent = None;
        shell.panes = vec![PaneInfo {
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            label: "terminal".into(),
            agent: None,
            agent_status: AgentState::Unknown,
            revision: 1,
            exited: false,
            listening_ports: vec![],
            foreground_job: true,
            outcome_acknowledged: false,
        }];
        assert_eq!(derive(&shell), SessionHeuristic::Running);
        assert_eq!(derive(&shell).app_state(), AppSessionState::Running);
        assert_eq!(status_label(&shell), "running");

        shell.agent = Some("pi".into());
        shell.agent_status = AgentState::Idle;
        assert_eq!(derive(&shell), SessionHeuristic::Idle);
        shell.agent_status = AgentState::Working;
        assert_eq!(derive(&shell), SessionHeuristic::Working);
    }

    #[test]
    fn acknowledged_done_becomes_idle_without_changing_authoritative_state() {
        let mut session = session(AgentState::Done, false);
        session.outcome_acknowledged = true;

        assert_eq!(derive(&session), SessionHeuristic::Idle);
        assert_eq!(derive(&session).app_state(), AppSessionState::Idle);
        assert_eq!(status_label(&session), "idle");
        assert_eq!(session.agent_status, AgentState::Done);
    }

    #[test]
    fn mute_overrides_runtime_state() {
        let session = session(AgentState::Blocked, true);
        assert_eq!(derive(&session), SessionHeuristic::Muted);
        assert_eq!(status_label(&session), "muted");
    }
}
