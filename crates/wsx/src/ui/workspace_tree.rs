// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use crate::session_state::{self, AppSessionState};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState},
};
use std::collections::HashSet;
use wsx_core::model::workspace::{FlatEntry, SessionInfo, WorkspaceState};

use super::{
    compact_port_label_with_width, git_remote_status_color, theme,
    workspace_nav::{render_scrollbar, SidebarLayout},
};
// ref: ratatui Block title — title() accepts &str or String

fn sched_header_label(expanded: bool, count: usize) -> String {
    let icon = if expanded { "▾" } else { "▸" };
    format!(" {icon} ◈ sched [{count}]")
}

fn routine_tree_label(name: &str, status: &str) -> String {
    format!("  ◇ {name}{status}")
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if Line::from(value).width() <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut value = value.to_string();
    while !value.is_empty() && Line::from(format!("{value}…")).width() > max_width {
        value.pop();
    }
    if value.is_empty() {
        "…".into()
    } else {
        format!("{value}…")
    }
}

fn stale_project_label(icon: &str, name: &str, count: &str, width: usize) -> String {
    const SUFFIX: &str = "stale";
    let suffix_width = Line::from(SUFFIX).width();
    if width <= suffix_width {
        return truncate_to_width(SUFFIX, width);
    }
    let identity_width = width.saturating_sub(suffix_width + 1);
    let identity = truncate_to_width(&format!("{icon} {name}{count}"), identity_width);
    let gap = width.saturating_sub(Line::from(identity.as_str()).width() + suffix_width);
    format!("{identity}{}{SUFFIX}", " ".repeat(gap))
}

fn session_line(sess: &SessionInfo, width: usize) -> Line<'static> {
    let state = session_state::derive(sess).app_state();
    let (icon, icon_color) = session_icon(sess, state);
    let agent_label = session_state::agent_label(sess.agent.as_deref());
    let port_label =
        compact_port_label_with_width(&sess.listening_ports(), width.saturating_sub(12));
    let port_width = port_label
        .as_deref()
        .map_or(0, |label| Line::from(label).width());
    let prefix_width = 4;
    let identity_width =
        width.saturating_sub(prefix_width + port_width + usize::from(port_label.is_some()));
    let full_identity_width = Line::from(sess.display_name.as_str()).width()
        + agent_label
            .as_deref()
            .map_or(0, |label| Line::from(label).width());
    let show_agent = full_identity_width <= identity_width;
    let display_name = if show_agent {
        sess.display_name.clone()
    } else {
        truncate_to_width(&sess.display_name, identity_width)
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(icon, Style::default().fg(icon_color)),
        Span::styled(format!(" {display_name}"), Style::default().fg(theme::TEXT)),
    ];
    if show_agent {
        if let Some(agent_label) = agent_label {
            spans.push(Span::styled(agent_label, theme::agent_label()));
        }
    }
    if let Some(port_label) = port_label {
        let left_width = Line::from(spans.clone()).width();
        let gap = width.saturating_sub(left_width + port_width);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(port_label, Style::default().fg(theme::ACCENT)));
    }
    Line::from(spans)
}

pub struct TreeView<'a> {
    pub workspace: &'a WorkspaceState,
    pub flat: &'a [FlatEntry],
    pub stale_projects: &'a HashSet<usize>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub is_move_mode: bool,
}

pub fn render_tree(frame: &mut Frame, layout: SidebarLayout, view: TreeView<'_>) {
    let TreeView {
        workspace,
        flat,
        stale_projects,
        selected,
        scroll_offset,
        is_move_mode,
    } = view;
    let items: Vec<ListItem> = flat
        .iter()
        .map(|entry| match entry {
            FlatEntry::Project { idx } => {
                let p = &workspace.projects[*idx];
                let icon = if p.expanded { "▼" } else { "▶" };
                let count = if p.expanded {
                    String::new()
                } else {
                    format!(" [{}]", p.worktrees.len())
                };
                let (label, style) = if p.missing {
                    (
                        format!("{} {} (missing)", icon, p.name),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    )
                } else if stale_projects.contains(idx) {
                    (
                        stale_project_label(icon, &p.name, &count, usize::from(layout.list.width)),
                        theme::stale_project(),
                    )
                } else {
                    (
                        format!("{} {}{}", icon, p.name, count),
                        Style::default().fg(theme::ACCENT).bold(),
                    )
                };
                ListItem::new(label).style(style)
            }
            FlatEntry::Worktree {
                project_idx,
                worktree_idx,
            } => {
                let p = &workspace.projects[*project_idx];
                let wt = &p.worktrees[*worktree_idx];
                let main_mark = if wt.is_main { "~ " } else { "" };
                let expand_icon = if !wt.sessions.is_empty() {
                    if wt.expanded {
                        "▾"
                    } else {
                        "▸"
                    }
                } else {
                    " "
                };
                let sess_badge = if !wt.sessions.is_empty() && !wt.expanded {
                    format!(" [{}]", wt.sessions.len())
                } else {
                    String::new()
                };
                let proj_prefix = format!("{}-", p.name);
                let short_name = wt.name.strip_prefix(&proj_prefix).unwrap_or(&wt.name);
                let display = if let Some(alias) = &wt.alias {
                    format!("{} ({})", alias, short_name)
                } else if wt.is_main {
                    wt.branch.clone()
                } else {
                    short_name.to_string()
                };

                let dirty = wt
                    .git_info
                    .as_ref()
                    .map(|g| !g.modified_files.is_empty())
                    .unwrap_or(false);

                let mut spans = vec![Span::raw(format!(
                    " {} {}{}",
                    expand_icon, main_mark, display
                ))];

                // * directly after name (no space) if dirty
                if dirty {
                    spans.push(Span::styled("*", Style::default().fg(theme::WARNING)));
                }

                // remote tracking indicators
                if let Some(gi) = &wt.git_info {
                    match (gi.behind, gi.ahead) {
                        (b, a) if b > 0 && a > 0 => spans.push(Span::styled(
                            format!(" ↓{}↑{}", b, a),
                            Style::default().fg(git_remote_status_color(b, a)),
                        )),
                        (b, _) if b > 0 => spans.push(Span::styled(
                            format!(" ↓{}", b),
                            Style::default().fg(git_remote_status_color(b, 0)),
                        )),
                        (_, a) if a > 0 => spans.push(Span::styled(
                            format!(" ↑{}", a),
                            Style::default().fg(git_remote_status_color(0, a)),
                        )),
                        _ => {}
                    }
                }
                if !sess_badge.is_empty() {
                    spans.push(Span::raw(sess_badge));
                }

                ListItem::new(Line::from(spans)).style(Style::default().fg(theme::TEXT))
            }
            FlatEntry::Session {
                project_idx,
                worktree_idx,
                session_idx,
            } => {
                let sess = &workspace.projects[*project_idx].worktrees[*worktree_idx].sessions
                    [*session_idx];
                ListItem::new(session_line(sess, usize::from(layout.list.width)))
            }
            FlatEntry::Pane {
                project_idx,
                worktree_idx,
                session_idx,
                pane_idx,
            } => {
                let session = &workspace.projects[*project_idx].worktrees[*worktree_idx].sessions
                    [*session_idx];
                let pane = &session.panes[*pane_idx];
                let branch = if *pane_idx + 1 == session.panes.len() {
                    "└"
                } else {
                    "├"
                };
                let focused = if pane.pane_id == session.pane_id {
                    "●"
                } else {
                    "○"
                };
                let exited = if pane.exited { " · exited" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("    {branch} {focused} "),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    ),
                    Span::styled(&pane.label, Style::default().fg(theme::TEXT)),
                    Span::styled(exited, Style::default().fg(theme::BLOCKED)),
                ]))
            }
            FlatEntry::RoutinesHeader { project_idx } => {
                let project = &workspace.projects[*project_idx];
                ListItem::new(sched_header_label(
                    project.routines_expanded,
                    project.routines.len(),
                ))
                .style(Style::default().fg(theme::ACCENT).bold())
            }
            FlatEntry::Routine {
                project_idx,
                routine_idx,
            } => {
                let view = &workspace.projects[*project_idx].routines[*routine_idx];
                let status = view
                    .latest_run
                    .as_ref()
                    .map(|r| format!(" · {:?}", r.status))
                    .unwrap_or_default();
                ListItem::new(routine_tree_label(&view.routine.name, &status)).style(
                    Style::default().fg(if view.capabilities.can_cancel {
                        theme::WORKING
                    } else {
                        theme::ACCENT
                    }),
                )
            }
        })
        .collect();

    let mut list_state = ListState::default().with_offset(scroll_offset);
    if !flat.is_empty() {
        list_state.select(Some(selected.min(flat.len().saturating_sub(1))));
    }

    let list = List::new(items)
        .style(Style::default().fg(theme::TEXT))
        .highlight_style(theme::selected_row(is_move_mode))
        .highlight_symbol("");

    frame.render_stateful_widget(list, layout.list, &mut list_state);
    render_scrollbar(
        frame,
        layout.scrollbar,
        flat.len(),
        usize::from(layout.list.height),
        scroll_offset,
    );
}

fn session_icon(
    sess: &wsx_core::model::workspace::SessionInfo,
    _state: AppSessionState,
) -> (&'static str, Color) {
    agent_state_icon(sess.agent_status, sess.muted)
}

pub(super) fn agent_state_icon(
    state: wsx_core::runtime::AgentState,
    muted: bool,
) -> (&'static str, Color) {
    use wsx_core::runtime::AgentState;
    if muted {
        return ("⊘", theme::TEXT_SUBTLE);
    }
    match state {
        AgentState::Blocked => ("×", theme::BLOCKED),
        AgentState::Done => ("✓", theme::SUCCESS),
        AgentState::Working => ("◐", theme::SUCCESS),
        AgentState::Idle => ("○", theme::WORKING),
        AgentState::Unknown => ("·", theme::UNKNOWN),
        AgentState::Error => ("!", theme::BLOCKED),
    }
}

/// Scroll a list viewport while retaining the selected item until it leaves the view.
pub fn scroll_viewport(
    current_offset: usize,
    selected: usize,
    visible_height: usize,
    content_len: usize,
    delta: isize,
) -> (usize, usize) {
    if content_len == 0 || visible_height == 0 {
        return (0, 0);
    }
    let max_offset = content_len.saturating_sub(visible_height);
    let offset = current_offset.saturating_add_signed(delta).min(max_offset);
    let last_visible = offset
        .saturating_add(visible_height.saturating_sub(1))
        .min(content_len - 1);
    (offset, selected.clamp(offset, last_visible))
}

/// Compute scroll offset to keep selected item visible.
pub fn compute_scroll(selected: usize, visible_height: usize, current_offset: usize) -> usize {
    let up_pad = (visible_height / 4).max(1); // scroll up when cursor within top 1/4
    let down_pad = (visible_height * 3 / 4).max(1); // scroll down when cursor past 3/4
    if selected < current_offset + up_pad {
        selected.saturating_sub(up_pad - 1)
    } else if selected >= current_offset + down_pad {
        selected.saturating_sub(down_pad - 1)
    } else {
        current_offset
    }
}

#[cfg(test)]
mod tests {
    use super::{
        render_tree, routine_tree_label, sched_header_label, session_icon, session_line, TreeView,
    };
    use crate::{
        session_state::AppSessionState,
        ui::{theme, workspace_nav::SidebarLayout},
    };
    use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};
    use std::{collections::HashSet, path::PathBuf};
    use wsx_core::{
        model::workspace::{FlatEntry, Project, SessionInfo, WorkspaceState},
        runtime::{AgentState, PaneId, SessionId, TerminalId},
    };

    fn session(muted: bool, agent_status: AgentState) -> SessionInfo {
        SessionInfo {
            session_id: SessionId(1),
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            agent: Some("codex".into()),
            display_name: "sess".to_string(),
            agent_status,
            revision: 1,
            layout: wsx_core::runtime::PaneLayout::Leaf { pane_id: PaneId(1) },
            panes: vec![],
            muted,
        }
    }

    #[test]
    fn given_project_sched_when_labelled_then_aligns_with_worktree_level() {
        assert_eq!(sched_header_label(true, 2), " ▾ ◈ sched [2]");
    }

    #[test]
    fn given_routine_when_labelled_then_aligns_with_session_level() {
        assert_eq!(routine_tree_label("nightly", ""), "  ◇ nightly");
    }

    #[test]
    fn mute_preserves_one_distinct_suppression_glyph() {
        assert_eq!(
            session_icon(
                &session(true, AgentState::Blocked),
                AppSessionState::NeedsAttention
            ),
            ("⊘", theme::TEXT_SUBTLE),
        );
    }

    #[test]
    fn stale_and_manual_collapse_rows_have_distinct_readable_projection() {
        let project = |name: &str| Project {
            name: name.into(),
            path: PathBuf::from(format!("/{name}")),
            default_branch: "main".into(),
            last_agent_active_unix_ms: None,
            last_terminal_active_unix_ms: None,
            worktrees: vec![],
            routines: vec![],
            routine_revision: 0,
            routines_expanded: false,
            config: None,
            expanded: false,
            missing: false,
        };
        let mut missing = project("missing");
        missing.missing = true;
        let workspace = WorkspaceState {
            projects: vec![
                project("stale-one"),
                project("manual"),
                project("selected"),
                missing,
            ],
        };
        let flat = vec![
            FlatEntry::Project { idx: 0 },
            FlatEntry::Project { idx: 1 },
            FlatEntry::Project { idx: 2 },
            FlatEntry::Project { idx: 3 },
        ];
        let stale_projects = HashSet::from([0, 3]);
        let backend = TestBackend::new(40, 4);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_tree(
                    frame,
                    SidebarLayout::new(frame.area()),
                    TreeView {
                        workspace: &workspace,
                        flat: &flat,
                        stale_projects: &stale_projects,
                        selected: 2,
                        scroll_offset: 0,
                        is_move_mode: false,
                    },
                );
            })
            .unwrap();

        let row = |y| {
            (0..40)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        };
        let stale_row = row(0);
        assert!(stale_row.contains("stale-one [0]"));
        assert!(stale_row.ends_with("  stale  "), "{stale_row:?}");
        assert!(!stale_row.contains('·'));
        assert_eq!(terminal.backend().buffer()[(1, 0)].fg, theme::TEXT_MUTED);
        assert!(!row(1).contains("stale"));
        assert_eq!(terminal.backend().buffer()[(1, 1)].fg, theme::ACCENT);
        assert!(row(3).contains("missing (missing)"));
        assert!(!row(3).contains("stale"));
        assert_eq!(terminal.backend().buffer()[(1, 3)].fg, theme::TEXT_SUBTLE);

        let backend = TestBackend::new(18, 1);
        let mut narrow = Terminal::new(backend).unwrap();
        narrow
            .draw(|frame| {
                render_tree(
                    frame,
                    SidebarLayout::new(frame.area()),
                    TreeView {
                        workspace: &workspace,
                        flat: &flat,
                        stale_projects: &stale_projects,
                        selected: 0,
                        scroll_offset: 0,
                        is_move_mode: false,
                    },
                );
            })
            .unwrap();
        let selected = (0..18)
            .map(|x| narrow.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let list = SidebarLayout::new(narrow.backend().buffer().area).list;
        let stale_start = list.right().saturating_sub(5);
        let stale_suffix = (stale_start..list.right())
            .map(|x| narrow.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert_eq!(stale_suffix, "stale", "{selected:?}");
        assert!(!selected.contains('·'));
        let selected_cell = &narrow.backend().buffer()[(1, 0)];
        assert_eq!(selected_cell.fg, theme::TEXT);
        assert_eq!(selected_cell.bg, theme::selected_row(false).bg.unwrap());
    }

    #[test]
    fn session_row_without_ports_keeps_its_identity_when_rendered() {
        let mut sess = session(false, AgentState::Idle);
        sess.display_name = "plain-session".into();
        let backend = TestBackend::new(30, 1);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(session_line(&sess, 30)), frame.area());
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(rendered.trim_end(), "  ○ plain-session (codex)");
    }

    #[test]
    fn authoritative_statuses_have_distinct_symbols_and_semantic_colors() {
        for (status, app_state, expected) in [
            (
                AgentState::Blocked,
                AppSessionState::NeedsAttention,
                ("×", theme::BLOCKED),
            ),
            (
                AgentState::Done,
                AppSessionState::NeedsAttention,
                ("✓", theme::SUCCESS),
            ),
            (
                AgentState::Working,
                AppSessionState::Active,
                ("◐", theme::SUCCESS),
            ),
            (
                AgentState::Idle,
                AppSessionState::Idle,
                ("○", theme::WORKING),
            ),
            (
                AgentState::Unknown,
                AppSessionState::Idle,
                ("·", theme::UNKNOWN),
            ),
            (
                AgentState::Error,
                AppSessionState::NeedsAttention,
                ("!", theme::BLOCKED),
            ),
        ] {
            assert_eq!(session_icon(&session(false, status), app_state), expected);
        }
    }
}
