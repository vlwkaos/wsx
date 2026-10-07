//! One-row session preview, not another input mode.
//! ^ docs/terminal-context.md: hierarchy and Prefix+h/l (j/k aliases) share stable project order.
use crate::session_state;
use ratatui::{prelude::*, widgets::Paragraph};
use wsx_core::model::workspace::{PaneInfo, Project, SessionInfo, WorktreeInfo};

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

fn current_state(view: &TerminalTitleView<'_>) -> (&'static str, Color) {
    view.pane.map_or_else(
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
    )
}

fn chip(
    view: &TerminalTitleView<'_>,
    worktree: &WorktreeInfo,
    session: &SessionInfo,
    current: bool,
    width: usize,
) -> Vec<Span<'static>> {
    if width == 0 {
        return vec![];
    }
    let (icon, color) = if current {
        current_state(view)
    } else {
        session_icon(session, view.animation_frame)
    };
    let style = if current {
        theme::terminal_current()
    } else {
        theme::terminal_peer().patch(theme::terminal_worktree(
            worktree.path == view.worktree.path,
        ))
    };
    if width < 4 {
        return vec![Span::styled(icon, style.fg(color))];
    }
    let name = if current {
        view.pane.map_or_else(
            || session.display_name.clone(),
            |pane| format!("{} / {}", session.display_name, pane.label),
        )
    } else {
        session.display_name.clone()
    };
    let agent = if current {
        view.pane
            .map_or(session.agent.as_deref(), |pane| pane.agent.as_deref())
    } else {
        None
    };
    let label = session_state::agent_label(agent).unwrap_or_default();
    let label_width = Line::from(label.as_str()).width();
    let body = width - 4;
    // Known provider identity wins over extra name detail, but not the entire name.
    let show_agent = !label.is_empty() && body >= label_width + 3;
    let identity_width = body.saturating_sub(if show_agent { label_width } else { 0 });
    let name = truncate_to_width(&name, identity_width);
    let mut spans = vec![
        Span::styled(" ", style),
        Span::styled(icon, style.fg(color)),
        Span::styled(format!(" {name}"), style),
    ];
    if show_agent {
        spans.push(Span::styled(label, style.fg(theme::TEXT_MUTED)));
    }
    spans.push(Span::styled(" ", style));
    spans
}

fn overflow_width(start: usize, end: usize, total: usize) -> usize {
    let left = if start > 0 {
        format!("+{start} ").len()
    } else {
        0
    };
    let right = if end + 1 < total {
        format!(" +{}", total - end - 1).len()
    } else {
        0
    };
    left + right
}

struct TitleWindow {
    current: usize,
    current_width: usize,
    peer_width: usize,
    worktree_width: usize,
}

impl TitleWindow {
    fn spans(
        &self,
        view: &TerminalTitleView<'_>,
        order: &[(usize, usize, &SessionInfo)],
        start: usize,
        end: usize,
    ) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        let mut previous = None;
        for (index, (wi, _, session)) in order.iter().enumerate().take(end + 1).skip(start) {
            let worktree = &view.project.worktrees[*wi];
            if previous != Some(*wi) {
                if previous.is_some() {
                    spans.push(Span::raw(" "));
                }
                let name = truncate_to_width(worktree.display_name(), self.worktree_width);
                spans.push(Span::styled(
                    format!(" {name} ›"),
                    theme::terminal_worktree(worktree.path == view.worktree.path),
                ));
                previous = Some(*wi);
            }
            spans.extend(chip(
                view,
                worktree,
                session,
                index == self.current,
                if index == self.current {
                    self.current_width
                } else {
                    self.peer_width
                },
            ));
        }
        spans
    }
}

fn title_line(view: &TerminalTitleView<'_>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    if width <= 3 {
        let (icon, color) = current_state(view);
        return Line::from(Span::styled(icon, theme::terminal_current().fg(color)));
    }
    let order: Vec<_> = session_state::context_sessions(view.project).collect();
    let mut spans = Vec::new();
    if width >= 38 {
        let limit = if order.len() <= 1 {
            width.saturating_sub(28).min(20)
        } else {
            (width / 5).min(20)
        };
        let project = truncate_to_width(&view.project.name, limit);
        spans.push(Span::styled(
            format!(" {project} "),
            theme::terminal_project(),
        ));
    }
    spans.push(Span::raw(" "));
    let available = width.saturating_sub(spans_width(&spans) + 1);
    let index = order
        .iter()
        .position(|(_, _, session)| session.session_id == view.session.session_id);
    if available < 36 || order.len() <= 1 || index.is_none() {
        let position = index
            .filter(|_| order.len() > 1)
            .map(|index| format!(" {}/{}", index + 1, order.len()))
            .filter(|position| position.len() + 12 <= available)
            .unwrap_or_default();
        let mut budget = available.saturating_sub(position.len());
        // At tiny widths, current identity and position outrank parent context.
        if budget >= 32 {
            let wt = truncate_to_width(view.worktree.display_name(), (budget / 5).min(16));
            let label = format!(" {wt} ›");
            budget = budget.saturating_sub(Line::from(label.as_str()).width());
            spans.push(Span::styled(label, theme::terminal_worktree(true)));
        }
        spans.extend(chip(view, view.worktree, view.session, true, budget));
        spans.push(Span::styled(position, theme::terminal_context()));
        return Line::from(spans);
    }
    let index = index.unwrap();
    let worktree_width = (available / 5).min(16);
    let parent_width = Line::from(truncate_to_width(
        view.worktree.display_name(),
        worktree_width,
    ))
    .width()
        + 3;
    let window = TitleWindow {
        current: index,
        current_width: (available / 2).clamp(24, 48).min(
            available.saturating_sub(parent_width + overflow_width(index, index, order.len())),
        ),
        peer_width: (available / 5).clamp(12, 24),
        worktree_width,
    };
    let (mut start, mut end) = (index, index);
    // ^ Candidate and final windows use identical labels and width allocations.
    loop {
        let mut grew = false;
        if start > 0
            && spans_width(&window.spans(view, &order, start - 1, end))
                + overflow_width(start - 1, end, order.len())
                <= available
        {
            start -= 1;
            grew = true;
        }
        if end + 1 < order.len()
            && spans_width(&window.spans(view, &order, start, end + 1))
                + overflow_width(start, end + 1, order.len())
                <= available
        {
            end += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    if start > 0 {
        spans.push(Span::styled(
            format!("+{start} "),
            theme::terminal_context(),
        ));
    }
    spans.extend(window.spans(view, &order, start, end));
    if end + 1 < order.len() {
        spans.push(Span::styled(
            format!(" +{}", order.len() - end - 1),
            theme::terminal_context(),
        ));
    }
    Line::from(spans)
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
    fn preview_keeps_workspace_order_across_live_state_changes() {
        let mut project = project();
        for acknowledged in [false, true, false] {
            project.worktrees[0].sessions[3].outcome_acknowledged = acknowledged;
            let text = line(&project, 160).to_string();
            assert!(
                text.starts_with(" demo   main › ● current (codex) "),
                "{text}"
            );
            let order = ["current", "shell", "worker", "finished", "fix › ◐ approval"]
                .map(|name| text.find(name).unwrap());
            assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
            assert_eq!(text.matches("current").count(), 1);
        }
    }
    #[test]
    fn chip_padding_keeps_user_supplied_brackets_in_names() {
        let mut project = project();
        project.worktrees[0].sessions[0].display_name = "[build]".into();
        let view = TerminalTitleView {
            project: &project,
            worktree: &project.worktrees[0],
            session: &project.worktrees[0].sessions[0],
            pane: None,
            animation_frame: 0,
        };
        for current in [false, true] {
            let spans = chip(&view, view.worktree, view.session, current, 72);
            assert_eq!(spans.first().unwrap().content, " ");
            assert_eq!(spans.last().unwrap().content, " ");
            assert!(Line::from(spans).to_string().contains("[build]"));
        }
    }

    #[test]
    fn cross_worktree_attribution_survives_long_unicode_labels() {
        let mut project = project();
        project.worktrees[0].sessions.truncate(1);
        project.worktrees[1].sessions[0].display_name = "开发👩‍💻e\u{301}".repeat(10);
        for width in 90..=160 {
            let title = line(&project, width);
            assert!(
                title.to_string().contains("fix ›"),
                "width {width}: {title}"
            );
            assert!(title.width() <= width);
        }
    }
    #[test]
    fn tiny_and_long_titles_preserve_width_identity_and_position() {
        let mut project = project();
        assert!(line(&project, 28).to_string().contains("current"));
        assert!(line(&project, 28).to_string().contains("1/5"));
        for name in ["current".into(), "开发👩‍💻e\u{301}".repeat(12)] {
            project.worktrees[0].sessions[0].display_name = name;
            for width in 0..=160 {
                let title = line(&project, width);
                assert!(title.width() <= width, "width {width}: {title}");
                if width >= 70 {
                    assert!(title.to_string().contains("(codex)"), "{title}");
                }
            }
        }
    }
    #[test]
    fn large_project_reserves_both_hidden_counts_before_current_identity() {
        let mut project = project();
        project.worktrees.truncate(1);
        project.worktrees[0].sessions = (0..2000)
            .map(|i| session(i + 1, &"x".repeat(120), AgentState::Idle))
            .collect();
        for width in 0..=160 {
            let title = title_line(
                &TerminalTitleView {
                    project: &project,
                    worktree: &project.worktrees[0],
                    session: &project.worktrees[0].sessions[1000],
                    pane: None,
                    animation_frame: 0,
                },
                width,
            );
            assert!(title.width() <= width, "width {width}: {title}");
        }
    }

    #[test]
    fn pane_state_and_chrome_remain_local_to_title_rectangle() {
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
        assert!(text.contains("current / logs"), "{text}");
        assert!(!text.contains("(codex)") && !text.contains(":3000"));
        assert!(!text.contains(['[', ']']), "{text}");
        for background in [theme::terminal_current().bg, theme::terminal_peer().bg] {
            assert_ne!(background, theme::terminal_titlebar().bg);
            assert!((3..87).any(|x| Some(buffer[(x, 2)].bg) == background));
        }
        for x in 3..87 {
            assert_ne!(buffer[(x, 2)].bg, Color::Reset);
            assert_eq!(buffer[(x, 1)].bg, Color::Reset);
            assert_eq!(buffer[(x, 3)].bg, Color::Reset);
        }
    }
}
