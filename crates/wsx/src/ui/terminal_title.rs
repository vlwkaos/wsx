//! One-row terminal context, not a second session navigator.
//! ^ docs/terminal-context.md documents the shared project order and prefix cycle.
use crate::session_state;
use ratatui::{prelude::*, widgets::Paragraph};
use wsx_core::{
    model::workspace::{PaneInfo, Project, SessionInfo, WorktreeInfo},
    runtime::SessionId,
};

use super::{
    theme,
    workspace_tree::{agent_state_icon, session_icon, truncate_to_width},
};

pub struct TerminalTitleView<'a> {
    pub project: &'a Project,
    pub worktree: &'a WorktreeInfo,
    pub session: &'a SessionInfo,
    pub pane: Option<&'a PaneInfo>,
    pub animation_frame: usize,
}

pub fn render(frame: &mut Frame, area: Rect, view: TerminalTitleView<'_>) {
    frame.render_widget(
        Paragraph::new(title_line(&view, usize::from(area.width)))
            .style(theme::terminal_titlebar()),
        area,
    );
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn title_line(view: &TerminalTitleView<'_>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let (icon, color) = view.pane.map_or_else(
        || session_icon(view.session, view.animation_frame),
        |pane| {
            agent_state_icon(
                pane.agent_status,
                view.session.muted,
                pane.outcome_acknowledged,
                pane.agent.is_none() && pane.foreground_job,
                view.animation_frame,
            )
        },
    );
    if width <= 3 {
        return Line::from(Span::styled(icon, theme::terminal_current().fg(color)));
    }
    let peer_count = view
        .project
        .worktrees
        .iter()
        .flat_map(|worktree| &worktree.sessions)
        .filter(|session| session.session_id != view.session.session_id)
        .count();
    let usable = width - 2;
    // Reserve only a count on narrow screens. The active identity always wins.
    let count_reserve = if peer_count > 0 && usable >= 24 {
        format!(" +{peer_count}").len()
    } else {
        0
    };
    let current_limit = if usable >= 60 && peer_count > 0 {
        (usable * 3 / 5).min(usable.saturating_sub(count_reserve))
    } else {
        usable.saturating_sub(count_reserve)
    };
    let name = if let Some(pane) = view.pane {
        format!("{} / {}", view.session.display_name, pane.label)
    } else {
        view.session.display_name.clone()
    };
    let name = truncate_to_width(&name, current_limit.saturating_sub(2));
    let mut current = vec![
        Span::styled(icon, theme::terminal_current().fg(color)),
        Span::styled(format!(" {name}"), theme::terminal_current()),
    ];
    let agent = view
        .pane
        .map_or(view.session.agent.as_deref(), |pane| pane.agent.as_deref());
    if let Some(label) = session_state::agent_label(agent) {
        if spans_width(&current) + Line::from(label.as_str()).width() <= current_limit {
            current.push(Span::styled(
                label,
                theme::terminal_current().fg(theme::TEXT_MUTED),
            ));
        }
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(current);
    let mut used = spans_width(&spans);
    let remaining = width.saturating_sub(used + 1);
    // Context is secondary to both active identity and room for peer state.
    let context = format!("{}/{}", view.project.name, view.worktree.display_name());
    let context_limit = if peer_count == 0 {
        remaining.saturating_sub(2)
    } else if remaining >= 30 {
        (remaining / 3).min(28)
    } else {
        0
    };
    if context_limit > 0 {
        let context = truncate_to_width(&context, context_limit);
        spans.push(Span::styled(
            format!("  {context}"),
            theme::terminal_context(),
        ));
        used = spans_width(&spans);
    }
    let peer_width = width.saturating_sub(used + 2);
    let peer_spans = peer_line(view, peer_count, peer_width);
    let peer_used = spans_width(&peer_spans);
    if peer_used > 0 {
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(used + peer_used + 1)),
        ));
        spans.extend(peer_spans);
    }
    Line::from(spans)
}

fn peer_sessions(
    project: &Project,
    current: SessionId,
) -> impl Iterator<Item = (&WorktreeInfo, &SessionInfo)> {
    session_state::context_sessions(project)
        .filter(move |(_, _, session)| session.session_id != current)
        .map(|(wi, _, session)| (&project.worktrees[wi], session))
}

fn peer_line(view: &TerminalTitleView<'_>, count: usize, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0;
    let mut shown = 0;
    // The shared iterator preserves Workspace order within tiers; only visible
    // identities are formatted and no navigation list is retained by rendering.
    for (worktree, session) in peer_sessions(view.project, view.session.session_id) {
        let (icon, color) = session_icon(session, view.animation_frame);
        let remaining = count - shown - 1;
        let suffix_width = if remaining > 0 {
            format!("  +{remaining}").len()
        } else {
            0
        };
        let gap = if shown == 0 { 0 } else { 2 };
        let identity_width = width.saturating_sub(used + gap + 2 + suffix_width).min(22);
        if identity_width < 6 {
            break;
        }
        let identity = if worktree.path == view.worktree.path {
            truncate_to_width(&session.display_name, identity_width)
        } else {
            // Preserve attribution even when identical long session names truncate.
            let worktree_name =
                truncate_to_width(worktree.display_name(), (identity_width / 2).min(10));
            let name_width =
                identity_width.saturating_sub(Line::from(worktree_name.as_str()).width() + 1);
            format!(
                "{}@{worktree_name}",
                truncate_to_width(&session.display_name, name_width)
            )
        };
        let label_width = 2 + Line::from(identity.as_str()).width();
        if gap > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(icon, theme::terminal_context().fg(color)));
        spans.push(Span::styled(
            format!(" {identity}"),
            theme::terminal_context(),
        ));
        used += gap + label_width;
        shown += 1;
    }
    if shown < count {
        let suffix = format!("{}+{}", if shown > 0 { "  " } else { "" }, count - shown);
        if used + suffix.len() <= width {
            spans.push(Span::styled(suffix, theme::terminal_context()));
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use wsx_core::runtime::{AgentState, PaneId, PaneLayout, SessionId, TerminalId};

    fn session(id: u64, name: &str, state: AgentState) -> SessionInfo {
        SessionInfo {
            session_id: SessionId(id),
            pane_id: PaneId(id),
            terminal_id: TerminalId(id),
            display_name: name.into(),
            agent: Some("codex".into()),
            agent_status: state,
            revision: 1,
            layout: PaneLayout::Leaf {
                pane_id: PaneId(id),
            },
            panes: vec![],
            muted: false,
            outcome_acknowledged: false,
        }
    }

    fn worktree(name: &str, sessions: Vec<SessionInfo>) -> WorktreeInfo {
        WorktreeInfo {
            name: name.into(),
            branch: name.into(),
            path: format!("/demo/{name}").into(),
            is_main: name == "main",
            alias: None,
            sessions,
            expanded: false,
            git_info: None,
            fetch_failed: false,
            fetch_fail_count: 0,
            fetch_fail_reason: None,
            last_fetched: None,
            git_info_fetched_at: None,
        }
    }

    fn project() -> Project {
        Project {
            name: "demo".into(),
            path: "/demo".into(),
            default_branch: "main".into(),
            last_agent_active_unix_ms: None,
            last_terminal_active_unix_ms: None,
            worktrees: vec![
                worktree(
                    "main",
                    vec![
                        session(1, "current", AgentState::Working),
                        session(2, "shell", AgentState::Idle),
                        session(3, "worker", AgentState::Working),
                        session(4, "finished", AgentState::Done),
                    ],
                ),
                worktree("fix", vec![session(5, "approval", AgentState::Blocked)]),
            ],
            routines: vec![],
            routine_revision: 0,
            routines_expanded: false,
            config: None,
            expanded: false,
            missing: false,
        }
    }

    fn line(project: &Project, width: usize) -> Line<'static> {
        title_line(
            &TerminalTitleView {
                project,
                worktree: &project.worktrees[0],
                session: &project.worktrees[0].sessions[0],
                pane: None,
                animation_frame: 2,
            },
            width,
        )
    }

    #[test]
    fn attention_peers_cross_collapsed_worktrees_and_follow_live_acknowledgement() {
        let mut project = project();
        let text = line(&project, 140).to_string();
        assert!(text.starts_with(" ● current (codex)"), "{text}");
        assert_eq!(text.matches("current").count(), 1);
        let order =
            ["approval@fix", "finished", "worker", "shell"].map(|name| text.find(name).unwrap());
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        // Acknowledgement moves Done out of attention without remounting a view.
        project.worktrees[0].sessions[3].outcome_acknowledged = true;
        let acknowledged = line(&project, 140).to_string();
        assert!(acknowledged.find("worker").unwrap() < acknowledged.find("finished").unwrap());
        project.worktrees[1].sessions[0].muted = true;
        let muted = line(&project, 140).to_string();
        assert!(muted.find("worker").unwrap() < muted.find("approval@fix").unwrap());
        project.worktrees[1].sessions[0].muted = false;
        assert_eq!(line(&project, 140).to_string(), acknowledged);
    }

    #[test]
    fn long_peer_names_keep_their_cross_worktree_attribution() {
        let mut project = project();
        project.worktrees[1].sessions[0].display_name = "开发👩‍💻e\u{301}".repeat(10);
        for width in 70..=160 {
            let title = line(&project, width);
            assert!(title.to_string().contains("@fix"), "width {width}: {title}");
            assert!(title.width() <= width);
        }
        project.worktrees[1].alias = Some("长工作区名称".repeat(10));
        for width in 70..=160 {
            let title = line(&project, width);
            assert!(title.to_string().contains("@长"), "width {width}: {title}");
            assert!(title.width() <= width);
        }
    }

    #[test]
    fn narrow_titles_keep_current_identity_and_count_without_ports_or_controls() {
        let mut project = project();
        project.worktrees[0].sessions[0].panes.push(PaneInfo {
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            label: "terminal".into(),
            agent: Some("codex".into()),
            agent_status: AgentState::Working,
            revision: 1,
            exited: false,
            listening_ports: vec![5173],
            foreground_job: false,
            outcome_acknowledged: false,
        });
        let narrow = line(&project, 28).to_string();
        assert!(narrow.contains("current"), "{narrow}");
        assert!(narrow.contains("+4"), "{narrow}");
        for width in 0..=160 {
            let title = line(&project, width);
            assert!(title.width() <= width, "width {width}: {title}");
            let text = title.to_string();
            assert!(!text.contains(":5173"));
            assert!(!text.contains('‹') && !text.contains('›'));
        }
        project.worktrees[0].sessions[0].display_name = "开发👩‍💻e\u{301}".repeat(12);
        for width in 0..=160 {
            let title = line(&project, width);
            assert!(title.width() <= width, "width {width}: {title}");
        }
    }

    #[test]
    fn pane_state_is_local_and_title_background_does_not_touch_terminal_content() {
        use ratatui::{backend::TestBackend, Terminal};
        let project = project();
        let pane = PaneInfo {
            pane_id: PaneId(6),
            terminal_id: TerminalId(6),
            label: "logs".into(),
            agent: None,
            agent_status: AgentState::Unknown,
            revision: 1,
            exited: false,
            listening_ports: vec![3000],
            foreground_job: true,
            outcome_acknowledged: false,
        };
        let mut terminal = Terminal::new(TestBackend::new(90, 4)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(3, 2, 84, 1),
                    TerminalTitleView {
                        project: &project,
                        worktree: &project.worktrees[0],
                        session: &project.worktrees[0].sessions[0],
                        pane: Some(&pane),
                        animation_frame: 0,
                    },
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (3..87).map(|x| buffer[(x, 2)].symbol()).collect();
        assert!(text.contains("● current / logs"));
        assert!(!text.contains("(codex)"));
        assert!(!text.contains(":3000"));
        for x in 3..87 {
            assert_ne!(buffer[(x, 2)].bg, Color::Reset);
            assert_eq!(buffer[(x, 1)].bg, Color::Reset);
            assert_eq!(buffer[(x, 3)].bg, Color::Reset);
        }
    }
}
