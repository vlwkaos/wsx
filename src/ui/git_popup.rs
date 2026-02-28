use crate::ui::popup_center;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render_git_popup(frame: &mut Frame, area: Rect, default_branch: &str) {
    let popup = popup_center(area, 36, 9);
    frame.render_widget(Clear, popup);

    let def = if default_branch.len() > 10 {
        &default_branch[..10]
    } else {
        default_branch
    };

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  (p)", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Pull"),
        ]),
        Line::from(vec![
            Span::styled("  (P)", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Push"),
        ]),
        Line::from(vec![
            Span::styled("  (r)", Style::default().fg(Color::Yellow).bold()),
            Span::raw(format!(" Pull Rebase origin/{}…", def)),
        ]),
        Line::from(vec![
            Span::styled("  (m)", Style::default().fg(Color::Yellow).bold()),
            Span::raw(format!(" Merge {} here…", def)),
        ]),
        Line::from(vec![
            Span::styled("  (M)", Style::default().fg(Color::Yellow).bold()),
            Span::raw(format!(" Merge into {}…", def)),
        ]),
        Line::from(""),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Git ")
        .border_style(Style::default().fg(Color::Yellow));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup);
}
