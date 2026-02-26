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
            let count = if p.expanded { String::new() } else { format!(" [{}]", p.worktrees.len()) };
            let label = format!("{} {}{}", icon, p.name, count);
            ListItem::new(label).style(Style::default().fg(Color::Cyan).bold())
        }
        FlatEntry::Worktree { project_idx, worktree_idx } => {
            let p = &workspace.projects[*project_idx];
            let wt = &p.worktrees[*worktree_idx];
            let main_mark = if wt.is_main { "*" } else { "" };
            let expand_icon = if !wt.sessions.is_empty() {
                if wt.expanded { "▾" } else { "▸" }
            } else { " " };
            let has_activity = wt.sessions.iter().any(|s| s.has_activity);
            let activity = if has_activity { " ●" } else { "" };
            let dirty = wt.git_info.as_ref().map(|g| !g.modified_files.is_empty()).unwrap_or(false);
            let dirty_mark = if dirty { " ✎" } else { "" };
            let sess_badge = if !wt.sessions.is_empty() && !wt.expanded {
                format!(" [{}]", wt.sessions.len())
            } else { String::new() };
            let proj_prefix = format!("{}-", p.name);
            let short_name = wt.name.strip_prefix(&proj_prefix).unwrap_or(&wt.name);
            let display = if let Some(alias) = &wt.alias {
                format!("{} ({})", alias, short_name)
            } else if wt.is_main {
                wt.branch.clone()
            } else {
                short_name.to_string()
            };
            let label = format!(" {} {}{}{}{}{}", expand_icon, main_mark, display, dirty_mark, activity, sess_badge);
            ListItem::new(label).style(Style::default().fg(Color::White))
        }
        FlatEntry::Session { project_idx, worktree_idx, session_idx } => {
            let sess = &workspace.projects[*project_idx].worktrees[*worktree_idx].sessions[*session_idx];
            let elapsed = sess.last_activity.map(|t| t.elapsed());
            let active = elapsed.map(|e| e.as_secs() < IDLE_SECS).unwrap_or(false);
            let (icon, icon_color) = if sess.muted {
                ("⊘", Color::DarkGray)             // muted — no activity tracking
            } else if sess.has_activity {
                ("●", Color::Yellow)               // tmux bell — needs attention
            } else if active {
                ("◉", Color::Green)                // actively outputting
            } else if sess.has_running_app && !sess.running_app_suppressed {
                ("●", Color::Yellow)               // app open but quiet — needs attention
            } else {
                ("○", Color::Gray)                 // truly idle
            };
            let idle_str = match elapsed {
                Some(e) if e.as_secs() >= IDLE_SECS => format!("  {}", fmt_idle(e)),
                _ => String::new(),
            };
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::styled(format!(" {}{}", sess.display_name, idle_str), Style::default().fg(Color::Rgb(210, 200, 185))),
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
    match s {
        s if s < 60   => format!("{}s", s),
        s if s < 3600 => format!("{}m", s / 60),
        s             => format!("{}h", s / 3600),
    }
}

/// Compute scroll offset to keep selected item visible.
pub fn compute_scroll(selected: usize, visible_height: usize, current_offset: usize) -> usize {
    let lookahead = (visible_height * 2 / 3).max(1);
    if selected < current_offset {
        selected
    } else if selected >= current_offset + lookahead {
        selected.saturating_sub(lookahead - 1)
    } else {
        current_offset
    }
}
