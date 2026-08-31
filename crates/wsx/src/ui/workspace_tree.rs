// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use crate::session_state::{self, AppSessionState};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState},
};
use wsx_core::model::workspace::{FlatEntry, WorkspaceState};

use super::{
    compact_port_label, theme,
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

pub struct TreeView<'a> {
    pub workspace: &'a WorkspaceState,
    pub flat: &'a [FlatEntry],
    pub selected: usize,
    pub scroll_offset: usize,
    pub is_move_mode: bool,
}

pub fn render_tree(frame: &mut Frame, area: Rect, view: TreeView<'_>) {
    let TreeView {
        workspace,
        flat,
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
                            Style::default().fg(theme::WARNING),
                        )),
                        (b, _) if b > 0 => spans.push(Span::styled(
                            format!(" ↓{}", b),
                            Style::default().fg(theme::BLOCKED),
                        )),
                        (_, a) if a > 0 => spans.push(Span::styled(
                            format!(" ↑{}", a),
                            Style::default().fg(theme::ACCENT),
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
                let state = session_state::derive(sess).app_state();
                let (icon, icon_color) = session_icon(sess, state);
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(icon, Style::default().fg(icon_color)),
                    Span::styled(
                        format!(" {}", sess.display_name),
                        Style::default().fg(theme::TEXT),
                    ),
                ];
                if let Some(agent) = &sess.agent {
                    spans.push(Span::styled(
                        format!(" · {agent}"),
                        Style::default().fg(icon_color),
                    ));
                }
                let ports = sess.listening_ports();
                if let Some(label) = compact_port_label(&ports) {
                    spans.push(Span::styled(
                        format!(" · {label}"),
                        Style::default().fg(theme::ACCENT),
                    ));
                }
                let line = Line::from(spans);
                ListItem::new(line)
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

    let layout = SidebarLayout::new(area);
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
        AgentState::Done => ("✓", theme::DONE),
        AgentState::Working => ("◐", theme::WORKING),
        AgentState::Idle => ("○", theme::SUCCESS),
        AgentState::Unknown => ("·", theme::UNKNOWN),
        AgentState::Error => ("!", theme::BLOCKED),
    }
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
    use super::{routine_tree_label, sched_header_label, session_icon};
    use crate::{session_state::AppSessionState, ui::theme};
    use wsx_core::{
        model::workspace::SessionInfo,
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
            terminal_frame: None,
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
                ("✓", theme::DONE),
            ),
            (
                AgentState::Working,
                AppSessionState::Active,
                ("◐", theme::WORKING),
            ),
            (
                AgentState::Idle,
                AppSessionState::Idle,
                ("○", theme::SUCCESS),
            ),
            (
                AgentState::Unknown,
                AppSessionState::Idle,
                ("·", theme::UNKNOWN),
            ),
        ] {
            assert_eq!(session_icon(&session(false, status), app_state), expected);
        }
    }
}
