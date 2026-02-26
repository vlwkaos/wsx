// Right preview pane — git info, session capture, project summary

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use crate::model::workspace::{Project, SessionInfo, WorktreeInfo};
use crate::ui::ansi;

pub fn render_worktree_preview(
    frame: &mut Frame,
    area: Rect,
    project: &Project,
    worktree: &WorktreeInfo,
    title: &str,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title))
        .title_style(Style::default().bold());

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Branch:  ", Style::default().fg(Color::Rgb(120, 120, 140))),
            Span::styled(worktree.branch.clone(), Style::default().fg(Color::Rgb(100, 200, 255)).bold()),
        ]),
        Line::from(vec![
            Span::styled("Path:    ", Style::default().fg(Color::Rgb(120, 120, 140))),
            Span::styled(
                worktree.path.to_string_lossy().to_string(),
                Style::default().fg(Color::Rgb(200, 200, 210)),
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
            lines.push(Line::from(Span::styled("Modified:", Style::default().fg(Color::Rgb(120, 120, 140)))));
            for f in info.modified_files.iter().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", f),
                    Style::default().fg(Color::Rgb(255, 100, 100)),
                )));
            }
        }
        if !info.recent_commits.is_empty() {
            lines.push(Line::from(Span::styled("Commits:", Style::default().fg(Color::Rgb(120, 120, 140)))));
            for c in &info.recent_commits {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", c.hash), Style::default().fg(Color::Rgb(255, 180, 80))),
                    Span::styled(c.message.clone(), Style::default().fg(Color::Rgb(210, 210, 220))),
                ]));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            "(loading git info...)",
            Style::default().fg(Color::Gray),
        )));
    }

    if !worktree.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Sessions:", Style::default().fg(Color::Rgb(120, 120, 140)))));
        for s in &worktree.sessions {
            let dot = if s.has_activity { " ●" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("  {}{}", s.display_name, dot),
                Style::default().fg(Color::Rgb(100, 220, 130)),
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
    title: &str,
) {
    let activity = if session.has_activity { " ●" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {}{} ", title, activity))
        .title_style(Style::default().bold());

    let text = session.pane_capture.as_deref()
        .map(ansi::parse)
        .unwrap_or_else(|| "(no capture)".into());
    let inner_h = area.height.saturating_sub(2) as usize; // minus borders
    let scroll = text.lines.len().saturating_sub(inner_h) as u16;
    let para = Paragraph::new(text).block(block).scroll((scroll, 0));
    frame.render_widget(para, area);
}

pub fn render_project_preview(frame: &mut Frame, area: Rect, project: &Project) {
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("Path:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                project.path.to_string_lossy().to_string(),
                Style::default().fg(Color::White),
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
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    if project.worktrees.is_empty() {
        lines.push(Line::from(Span::styled("  (no worktrees)", Style::default().fg(Color::Gray))));
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
        .title_style(Style::default().fg(Color::Gray));
    let para = Paragraph::new("Select a project, worktree, or session")
        .style(Style::default().fg(Color::Gray))
        .block(block);
    frame.render_widget(para, area);
}
