// Direct projection of the provider-neutral wsx agent state.

use wsx_core::{
    model::workspace::{Project, SessionInfo},
    runtime::AgentState,
};

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
    pub fn priority(self) -> u8 {
        match self {
            Self::Blocked | Self::Error => 0,
            Self::Done => 1,
            Self::Working | Self::Running => 2,
            Self::Idle | Self::Unknown | Self::Muted => 3,
        }
    }

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
    derive_status(
        session.agent_status,
        session.muted,
        session.outcome_acknowledged,
        !session.is_agentic() && session.has_foreground_job(),
    )
}

pub fn derive_status(
    state: AgentState,
    muted: bool,
    acknowledged: bool,
    foreground_job: bool,
) -> SessionHeuristic {
    if muted {
        return SessionHeuristic::Muted;
    }
    if foreground_job {
        return SessionHeuristic::Running;
    }
    match state {
        AgentState::Idle => SessionHeuristic::Idle,
        AgentState::Working => SessionHeuristic::Working,
        AgentState::Blocked => SessionHeuristic::Blocked,
        AgentState::Done if acknowledged => SessionHeuristic::Idle,
        AgentState::Done => SessionHeuristic::Done,
        AgentState::Unknown => SessionHeuristic::Unknown,
        AgentState::Error => SessionHeuristic::Error,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FoldedStatus {
    pub dominant: Option<SessionHeuristic>,
    pub active_sessions: usize,
}

// ^ docs/ui-state-ownership.md: folding/stale provenance never change runtime authority.
pub fn folded_status<'a>(
    sessions: impl IntoIterator<Item = &'a SessionInfo>,
    muted_terminals: &std::collections::HashSet<String>,
) -> FoldedStatus {
    let mut summary = FoldedStatus::default();
    for session in sessions {
        let mut active = false;
        let mut include = |state: AgentState, acknowledged: bool, foreground: bool, muted: bool| {
            let raw = derive_status(state, false, acknowledged, foreground);
            active |= matches!(raw, SessionHeuristic::Working | SessionHeuristic::Running);
            let visible = derive_status(state, muted, acknowledged, foreground);
            if summary
                .dominant
                .is_none_or(|current| visible.priority() < current.priority())
            {
                summary.dominant = Some(visible);
            }
        };
        if session.panes.is_empty() {
            include(
                session.agent_status,
                session.outcome_acknowledged,
                false,
                session.muted || muted_terminals.contains(&session.terminal_id.to_string()),
            );
        } else {
            for pane in &session.panes {
                let state = if pane.exited && pane.agent_status == AgentState::Working {
                    AgentState::Unknown
                } else {
                    pane.agent_status
                };
                include(
                    state,
                    pane.outcome_acknowledged,
                    !pane.exited && pane.agent.is_none() && pane.foreground_job,
                    muted_terminals.contains(&pane.terminal_id.to_string())
                        || (session.muted && pane.terminal_id == session.terminal_id),
                );
            }
        }
        summary.active_sessions += usize::from(active);
    }
    summary
}

// ^ docs/terminal-context.md: title projection and project-local navigation use
// the same live normalized order; expansion never filters this collection.
pub fn context_sessions(project: &Project) -> impl Iterator<Item = (usize, usize, &SessionInfo)> {
    (0..4).flat_map(move |tier| {
        project
            .worktrees
            .iter()
            .enumerate()
            .flat_map(move |(wi, worktree)| {
                worktree
                    .sessions
                    .iter()
                    .enumerate()
                    .filter_map(move |(si, session)| {
                        let priority = derive(session).priority();
                        (priority == tier).then_some((wi, si, session))
                    })
            })
    })
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

    fn folded_status<'a>(sessions: impl IntoIterator<Item = &'a SessionInfo>) -> FoldedStatus {
        super::folded_status(sessions, &std::collections::HashSet::new())
    }

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
    fn folded_status_retains_attention_and_active_sessions_independent_of_order() {
        let working = session(AgentState::Working, false);
        let blocked = session(AgentState::Blocked, false);
        let done = session(AgentState::Done, false);
        let idle = session(AgentState::Idle, false);
        for entries in [
            [&working, &blocked, &done, &idle],
            [&idle, &done, &blocked, &working],
        ] {
            assert_eq!(
                folded_status(entries),
                FoldedStatus {
                    dominant: Some(SessionHeuristic::Blocked),
                    active_sessions: 1,
                }
            );
        }
        let mut muted = working.clone();
        muted.muted = true;
        assert_eq!(
            folded_status([&muted]),
            FoldedStatus {
                dominant: Some(SessionHeuristic::Muted),
                active_sessions: 1,
            }
        );
        let mut acknowledged = done.clone();
        acknowledged.outcome_acknowledged = true;
        assert_eq!(
            folded_status([&acknowledged, &working]).dominant,
            Some(SessionHeuristic::Working)
        );
        assert_eq!(folded_status([]), FoldedStatus::default());
        assert_eq!(working.agent_status, AgentState::Working);
    }

    #[test]
    fn folded_status_includes_unfocused_panes_counts_sessions_once_and_fences_exit() {
        let mut entry = session(AgentState::Idle, false);
        let pane = |id, state, exited, agent, foreground_job| PaneInfo {
            pane_id: PaneId(id),
            terminal_id: TerminalId(id),
            label: "pane".into(),
            agent,
            agent_status: state,
            revision: 1,
            exited,
            listening_ports: vec![],
            foreground_job,
            outcome_acknowledged: false,
        };
        entry.panes = vec![
            pane(1, AgentState::Idle, false, Some("pi".into()), false),
            pane(2, AgentState::Working, false, Some("pi".into()), false),
            pane(3, AgentState::Unknown, false, None, true),
        ];
        assert_eq!(
            folded_status([&entry]),
            FoldedStatus {
                dominant: Some(SessionHeuristic::Working),
                active_sessions: 1,
            }
        );
        entry.panes[1].exited = true;
        entry.panes[2].exited = true;
        assert_eq!(folded_status([&entry]).active_sessions, 0);
        entry.panes[2].exited = false;
        assert_eq!(
            folded_status([&entry]).dominant,
            Some(SessionHeuristic::Running)
        );
        entry.panes[1].exited = false;
        entry.panes[1].agent_status = AgentState::Error;
        let muted = std::collections::HashSet::from(["2".to_string()]);
        let summary = super::folded_status([&entry], &muted);
        assert_eq!(summary.dominant, Some(SessionHeuristic::Running));
        assert_eq!(summary.active_sessions, 1);
        assert_eq!(muted, std::collections::HashSet::from(["2".to_string()]));
        assert_eq!(entry.panes[1].agent_status, AgentState::Error);
        assert_eq!(
            folded_status([&entry]).dominant,
            Some(SessionHeuristic::Error)
        );
    }

    #[test]
    fn mute_overrides_runtime_state() {
        let session = session(AgentState::Blocked, true);
        assert_eq!(derive(&session), SessionHeuristic::Muted);
        assert_eq!(status_label(&session), "muted");
    }
}
