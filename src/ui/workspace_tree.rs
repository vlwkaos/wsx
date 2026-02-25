// Left sidebar — 3-level tree (Project -> Worktree -> Session) using ratatui List.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState},
};
use crate::model::workspace::{FlatEntry, WorkspaceState, flatten_tree};

pub fn render_tree(
    frame: &mut Frame,
    area: Rect,
    workspace: &WorkspaceState,
    selected: usize,
    scroll_offset: usize,
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
            let label = if let Some(alias) = &wt.alias {
                format!("  {} {}{}{}{} ({})", expand_icon, main_mark, alias, activity, sess_badge, wt.name)
            } else {
                format!("  {} {}{}{}{}", expand_icon, main_mark, wt.name, activity, sess_badge)
            };
            ListItem::new(label).style(Style::default().fg(Color::White))
        }
        FlatEntry::Session { project_idx, worktree_idx, session_idx } => {
            let sess = &workspace.projects[*project_idx].worktrees[*worktree_idx].sessions[*session_idx];
            let dot = if sess.has_activity { " ●" } else { "" };
            let ext = if !sess.is_wsx_owned { " ~" } else { "" };
            let label = format!("    ○ {}{}{}", sess.name, dot, ext);
            ListItem::new(label).style(Style::default().fg(Color::DarkGray))
        }
    }).collect();

    let mut list_state = ListState::default().with_offset(scroll_offset);
    if !flat.is_empty() {
        list_state.select(Some(selected.min(flat.len().saturating_sub(1))));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Workspaces ")
                .title_style(Style::default().bold()),
        )
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow).bold())
        .highlight_symbol("► ");

    frame.render_stateful_widget(list, area, &mut list_state);
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
