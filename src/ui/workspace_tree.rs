// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState},
};
use crate::app::IDLE_SECS;
use crate::model::workspace::{FlatEntry, WorkspaceState, flatten_tree};

pub fn render_tree(
    frame: &mut Frame,
    area: Rect,
    workspace: &WorkspaceState,
    selected: usize,
    scroll_offset: usize,
    is_move_mode: bool,
) {
    let flat = flatten_tree(workspace);

    let items: Vec<ListItem> = flat.iter().map(|entry| match entry {
        FlatEntry::Project { idx } => {
            let p = &workspace.projects[*idx];
            let icon = if p.expanded { "▼" } else { "▶" };
            let wt_count = p.worktrees.len();
            let label = format!("{} {} ({})", icon, p.name, wt_count);
            ListItem::new(label).style(Style::default().fg(Color::Cyan).bold())
        }
        FlatEntry::Worktree { project_idx, worktree_idx } => {
            let wt = &workspace.projects[*project_idx].worktrees[*worktree_idx];
            let main_mark = if wt.is_main { "*" } else { "" };
            let expand_icon = if !wt.sessions.is_empty() {
                if wt.expanded { "▾" } else { "▸" }
            } else { " " };
            let has_activity = wt.sessions.iter().any(|s| s.has_activity);
            let activity = if has_activity { " ●" } else { "" };
            let sess_badge = if !wt.sessions.is_empty() {
                format!(" [{}s]", wt.sessions.len())
            } else { String::new() };
            let display = if let Some(alias) = &wt.alias {
                format!("{} ({})", alias, wt.name)
            } else if wt.is_main {
                wt.branch.clone()
            } else {
                wt.name.clone()
            };
            let label = format!(" {} {}{}{}{}", expand_icon, main_mark, display, activity, sess_badge);
            ListItem::new(label).style(Style::default().fg(Color::White))
        }
        FlatEntry::Session { project_idx, worktree_idx, session_idx } => {
            let sess = &workspace.projects[*project_idx].worktrees[*worktree_idx].sessions[*session_idx];
            let elapsed = sess.last_activity.map(|t| t.elapsed());
            let active = elapsed.map(|e| e.as_secs() < IDLE_SECS).unwrap_or(false);
            let (icon, icon_color) = if sess.has_activity {
                ("●", Color::Yellow)
            } else if active {
                ("◉", Color::Green)
            } else if sess.was_active {
                ("⊙", Color::Rgb(255, 160, 60))   // orange — finished, needs attention
            } else {
                ("○", Color::Gray)                 // never seen active, neutral
            };
            let idle_str = match elapsed {
                Some(e) if e.as_secs() >= IDLE_SECS => format!("  {}", fmt_idle(e)),
                _ => String::new(),
            };
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(format!(" {}{}", sess.display_name, idle_str)),
            ]);
            ListItem::new(line)
        }
    }).collect();

    let mut list_state = ListState::default().with_offset(scroll_offset);
    if !flat.is_empty() {
        list_state.select(Some(selected.min(flat.len().saturating_sub(1))));
    }

    let (block_title, highlight_bg) = if is_move_mode {
        (" Workspaces — MOVE ", Color::Green)
    } else {
        (" Workspaces ", Color::Yellow)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(block_title)
                .title_style(Style::default().bold()),
        )
        .highlight_style(Style::default().fg(Color::Black).bg(highlight_bg).bold())
        .highlight_symbol("");

    frame.render_stateful_widget(list, area, &mut list_state);
}

fn fmt_idle(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 { format!("{}s", s) }
    else if s < 3600 { format!("{}m", s / 60) }
    else { format!("{}h", s / 3600) }
}

/// Compute scroll offset to keep selected item visible.
pub fn compute_scroll(selected: usize, visible_height: usize, current_offset: usize) -> usize {
    if selected < current_offset {
        selected
    } else if selected >= current_offset + visible_height {
        selected.saturating_sub(visible_height - 1)
    } else {
        current_offset
    }
}
