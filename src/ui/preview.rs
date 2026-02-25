// Right preview pane — git info, session capture, project summary

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use crate::model::workspace::{Project, SessionInfo, WorktreeInfo};

pub fn render_worktree_preview(
    frame: &mut Frame,
    area: Rect,
    project: &Project,
    worktree: &WorktreeInfo,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", worktree.display_name()))
        .title_style(Style::default().bold());

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Branch:  ", Style::default().fg(Color::Gray)),
            Span::styled(worktree.branch.clone(), Style::default().fg(Color::Cyan).bold()),
        ]),
        Line::from(vec![
            Span::styled("Path:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                worktree.path.to_string_lossy().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    if let Some(info) = &worktree.git_info {
        if info.ahead > 0 || info.behind > 0 {
            lines.push(Line::from(Span::styled(
                format!("+{} -{} vs {}", info.ahead, info.behind, project.default_branch),
                Style::default().fg(Color::Yellow),
            )));
        }
        if !info.modified_files.is_empty() {
            lines.push(Line::from(Span::styled("Modified:", Style::default().fg(Color::Gray))));
            for f in info.modified_files.iter().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", f),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        if !info.recent_commits.is_empty() {
            lines.push(Line::from(Span::styled("Commits:", Style::default().fg(Color::Gray))));
            for c in &info.recent_commits {
                lines.push(Line::from(Span::styled(
                    format!("  {} {}", c.hash, c.message),
                    Style::default().fg(Color::White),
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(loading git info...)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if !worktree.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Sessions:", Style::default().fg(Color::Gray))));
        for s in &worktree.sessions {
            let dot = if s.has_activity { " ●" } else { "" };
            let owned = if s.is_wsx_owned { "" } else { " [external]" };
            lines.push(Line::from(Span::styled(
                format!("  {}{}{}", s.name, dot, owned),
                Style::default().fg(Color::Green),
            )));
        }
    }

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

pub fn render_session_preview(
    frame: &mut Frame,
    area: Rect,
    session: &SessionInfo,
) {
    let activity = if session.has_activity { " ●" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {}{} ", session.name, activity))
        .title_style(Style::default().bold());

    let content = session.pane_capture.as_deref().unwrap_or("(no capture)");
    let para = Paragraph::new(content)
        .block(block)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(para, area);
}

pub fn render_project_preview(frame: &mut Frame, area: Rect, project: &Project) {
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("Path:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                project.path.to_string_lossy().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("Branch: ", Style::default().fg(Color::Gray)),
            Span::styled(project.default_branch.clone(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled("Worktrees:", Style::default().fg(Color::Gray))),
    ];

    for wt in &project.worktrees {
        let main_mark = if wt.is_main { "* " } else { "  " };
        let sess_count = wt.sessions.len();
        let activity = if wt.sessions.iter().any(|s| s.has_activity) { " ●" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}{}", main_mark, wt.display_name()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("  ({} session{}){}", sess_count, if sess_count == 1 { "" } else { "s" }, activity),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    if project.worktrees.is_empty() {
        lines.push(Line::from(Span::styled("  (no worktrees)", Style::default().fg(Color::DarkGray))));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", project.name))
        .title_style(Style::default().bold());
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

pub fn render_empty_preview(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Preview ")
        .title_style(Style::default().fg(Color::DarkGray));
    let para = Paragraph::new("Select a project, worktree, or session")
        .style(Style::default().fg(Color::DarkGray))
        .block(block);
    frame.render_widget(para, area);
}
