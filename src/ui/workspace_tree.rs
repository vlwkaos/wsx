// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use crate::model::workspace::{FlatEntry, WorkspaceState};
use crate::session_state::{self, AppSessionState};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
// ref: ratatui Block title — title() accepts &str or String

pub fn render_tree(
    frame: &mut Frame,
    area: Rect,
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    selected: usize,
    scroll_offset: usize,
    is_move_mode: bool,
    status_message: Option<&str>,
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
                        Style::default().fg(Color::DarkGray),
                    )
                } else {
                    (
                        format!("{} {}{}", icon, p.name, count),
                        Style::default().fg(Color::Cyan).bold(),
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
                    spans.push(Span::styled("*", Style::default().fg(Color::Yellow)));
                }

                // remote tracking indicators
                if let Some(gi) = &wt.git_info {
                    match (gi.behind, gi.ahead) {
                        (b, a) if b > 0 && a > 0 => spans.push(Span::styled(
                            format!(" ↓{}↑{}", b, a),
                            Style::default().fg(Color::Magenta),
                        )),
                        (b, _) if b > 0 => spans.push(Span::styled(
                            format!(" ↓{}", b),
                            Style::default().fg(Color::Red),
                        )),
                        (_, a) if a > 0 => spans.push(Span::styled(
                            format!(" ↑{}", a),
                            Style::default().fg(Color::Cyan),
                        )),
                        _ => {}
                    }
                }
                if !sess_badge.is_empty() {
                    spans.push(Span::raw(sess_badge));
                }

                ListItem::new(Line::from(spans)).style(Style::default().fg(Color::White))
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
                let idle_str = if state == AppSessionState::Idle {
                    sess.last_activity
                        .map(|e| format!("  {}", fmt_idle(e.elapsed())))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(icon, Style::default().fg(icon_color)),
                    Span::styled(
                        format!(" {}{}", sess.display_name, idle_str),
                        Style::default().fg(Color::Rgb(210, 200, 185)),
                    ),
                ]);
                ListItem::new(line)
            }
        })
        .collect();

    let mut list_state = ListState::default().with_offset(scroll_offset);
    if !flat.is_empty() {
        list_state.select(Some(selected.min(flat.len().saturating_sub(1))));
    }

    let highlight_bg = if is_move_mode {
        Color::Green
    } else {
        Color::Yellow
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
                    Style::default().fg(Color::Black).bg(Color::Yellow).bold()
                } else {
                    Style::default()
                };
                spans.push(Span::styled(display, style));
            }
            spans.push(Span::raw("]"));
        }
        spans.push(Span::raw(if is_move_mode { " — MOVE " } else { " " }));
        Line::from(spans)
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title_line))
        .highlight_style(Style::default().fg(Color::Black).bg(highlight_bg).bold())
        .highlight_symbol("");

    frame.render_stateful_widget(list, area, &mut list_state);

    // Overlay status message on the bottom row of the tree area (no layout shift)
    if let Some(msg) = status_message {
        let msg_y = area.y + area.height.saturating_sub(1);
        let inner_w = area.width.saturating_sub(2) as usize; // inside borders
        let truncated = if msg.len() > inner_w {
            &msg[..inner_w]
        } else {
            msg
        };
        let msg_rect = Rect::new(area.x + 1, msg_y, area.width.saturating_sub(2), 1);
        let para = Paragraph::new(Span::styled(
            format!(" {truncated}"),
            Style::default().fg(Color::Yellow).bg(Color::Reset),
        ));
        frame.render_widget(para, msg_rect);
    }
}

fn session_icon(
    sess: &crate::model::workspace::SessionInfo,
    state: AppSessionState,
) -> (&'static str, Color) {
    if sess.muted {
        ("⊘", Color::DarkGray)
    } else if state == AppSessionState::NeedsAttention {
        ("●", Color::Yellow)
    } else if state == AppSessionState::Active {
        ("◉", Color::Green)
    } else {
        ("○", Color::Gray)
    }
}

fn fmt_idle(d: std::time::Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s < 60 => format!("{}s", s),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
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
    use super::session_icon;
    use crate::model::workspace::{ForegroundKind, SessionInfo};
    use crate::session_state::AppSessionState;
    use ratatui::style::Color;

    fn session(muted: bool) -> SessionInfo {
        SessionInfo {
            name: "proj-wt-sess".to_string(),
            display_name: "sess".to_string(),
            has_activity: false,
            pane_capture: None,
            last_activity: None,
            foreground: ForegroundKind::Unknown,
            is_running_wsx: false,
            muted,
        }
    }

    // muted overrides app_state regardless of the state passed in.
    #[test]
    fn given_muted_session_with_needs_attention_when_icon_then_muted_glyph() {
        assert_eq!(
            session_icon(&session(true), AppSessionState::NeedsAttention),
            ("⊘", Color::DarkGray),
        );
    }
    #[test]
    fn given_muted_session_with_active_when_icon_then_muted_glyph() {
        assert_eq!(
            session_icon(&session(true), AppSessionState::Active),
            ("⊘", Color::DarkGray),
        );
    }

    // unmuted: each AppSessionState → distinct glyph + color.
    #[test]
    fn given_unmuted_needs_attention_when_icon_then_yellow_dot() {
        assert_eq!(
            session_icon(&session(false), AppSessionState::NeedsAttention),
            ("●", Color::Yellow),
        );
    }
    #[test]
    fn given_unmuted_active_when_icon_then_green_dot() {
        assert_eq!(
            session_icon(&session(false), AppSessionState::Active),
            ("◉", Color::Green),
        );
    }
    #[test]
    fn given_unmuted_idle_when_icon_then_gray_ring() {
        assert_eq!(
            session_icon(&session(false), AppSessionState::Idle),
            ("○", Color::Gray),
        );
    }
}
