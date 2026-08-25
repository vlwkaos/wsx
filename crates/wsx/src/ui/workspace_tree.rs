// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use crate::session_state::{self, AppSessionState};
use ratatui::{
    prelude::*,
    widgets::{List, ListItem, ListState, Paragraph},
};
use wsx_core::model::workspace::{FlatEntry, WorkspaceState};

use super::theme;
// ref: ratatui Block title — title() accepts &str or String

fn sched_header_label(expanded: bool, count: usize) -> String {
    let icon = if expanded { "▾" } else { "▸" };
    format!(" {icon} ◈ sched [{count}]")
}

fn routine_tree_label(name: &str, status: &str) -> String {
    format!("  ◇ {name}{status}")
}

pub fn render_tree(
    frame: &mut Frame,
    area: Rect,
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    selected: usize,
    scroll_offset: usize,
    is_move_mode: bool,
    active_tab: Option<&str>,
    tab_names: &[String],
) {
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
                let status = match sess.agent_status {
                    wsx_core::herdr::AgentStatus::Idle => "idle",
                    wsx_core::herdr::AgentStatus::Working => "working",
                    wsx_core::herdr::AgentStatus::Blocked => "blocked",
                    wsx_core::herdr::AgentStatus::Done => "done",
                    wsx_core::herdr::AgentStatus::Unknown => "unknown",
                };
                let muted = if sess.muted { " · muted" } else { "" };
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(icon, Style::default().fg(icon_color)),
                    Span::styled(
                        format!(" {}", sess.display_name),
                        Style::default().fg(theme::TEXT),
                    ),
                    Span::styled(
                        format!(" · {status}{muted}"),
                        Style::default().fg(icon_color),
                    ),
                ]);
                ListItem::new(line)
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

    let highlight_bg = if is_move_mode {
        theme::ROW_MOVE
    } else {
        theme::ROW_SELECTED
    };
    let title_line: Line<'_> = {
        let mut spans: Vec<Span<'_>> = vec![Span::styled(" Workspaces ", Style::default().bold())];
        if !tab_names.is_empty() {
            spans.push(Span::raw("["));
            for i in 0..=tab_names.len() {
                if i > 0 {
                    spans.push(Span::raw("|"));
                }
                let (name, is_active) = if i == 0 {
                    ("default", active_tab.is_none())
                } else {
                    let t = tab_names[i - 1].as_str();
                    (t, active_tab == Some(t))
                };
                let display: String = if is_active {
                    name.to_string()
                } else {
                    name.chars().take(2).collect()
                };
                let style = if is_active {
                    Style::default()
                        .fg(theme::BACKGROUND)
                        .bg(theme::ACCENT)
                        .bold()
                } else {
                    Style::default().fg(theme::TEXT_MUTED)
                };
                spans.push(Span::styled(display, style));
            }
            spans.push(Span::raw("]"));
        }
        spans.push(Span::raw(if is_move_mode { " — MOVE " } else { " " }));
        Line::from(spans)
    };
    frame
        .buffer_mut()
        .set_style(area, Style::default().bg(theme::PANEL));
    frame.render_widget(
        Paragraph::new(title_line).style(Style::default().bg(theme::PANEL)),
        Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            1,
        ),
    );
    let list_area = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(2),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let list = List::new(items)
        .style(Style::default().fg(theme::TEXT).bg(theme::PANEL))
        .highlight_style(Style::default().fg(theme::TEXT).bg(highlight_bg).bold())
        .highlight_symbol("");

    frame.render_stateful_widget(list, list_area, &mut list_state);
}

fn session_icon(
    sess: &wsx_core::model::workspace::SessionInfo,
    _state: AppSessionState,
) -> (&'static str, Color) {
    use wsx_core::herdr::AgentStatus;
    if sess.muted {
        ("⊘", theme::TEXT_SUBTLE)
    } else {
        match sess.agent_status {
            AgentStatus::Blocked => ("×", theme::BLOCKED),
            AgentStatus::Done => ("✓", theme::DONE),
            AgentStatus::Working => ("◐", theme::WORKING),
            AgentStatus::Idle => ("○", theme::SUCCESS),
            AgentStatus::Unknown => ("·", theme::UNKNOWN),
        }
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
    use wsx_core::{herdr::AgentStatus, model::workspace::SessionInfo};

    fn session(muted: bool, agent_status: AgentStatus) -> SessionInfo {
        SessionInfo {
            pane_id: "pane-1".to_string(),
            terminal_id: "terminal-1".to_string(),
            agent: Some("codex".into()),
            workspace_id: "workspace-1".to_string(),
            tab_id: "tab-1".to_string(),
            display_name: "sess".to_string(),
            agent_status,
            revision: 1,
            pane_capture: None,
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
                &session(true, AgentStatus::Blocked),
                AppSessionState::NeedsAttention
            ),
            ("⊘", theme::TEXT_SUBTLE),
        );
    }

    #[test]
    fn authoritative_statuses_have_distinct_symbols_and_semantic_colors() {
        for (status, app_state, expected) in [
            (
                AgentStatus::Blocked,
                AppSessionState::NeedsAttention,
                ("×", theme::BLOCKED),
            ),
            (
                AgentStatus::Done,
                AppSessionState::NeedsAttention,
                ("✓", theme::DONE),
            ),
            (
                AgentStatus::Working,
                AppSessionState::Active,
                ("◐", theme::WORKING),
            ),
            (
                AgentStatus::Idle,
                AppSessionState::Idle,
                ("○", theme::SUCCESS),
            ),
            (
                AgentStatus::Unknown,
                AppSessionState::Idle,
                ("·", theme::UNKNOWN),
            ),
        ] {
            assert_eq!(session_icon(&session(false, status), app_state), expected);
        }
    }
}
