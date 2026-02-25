// Per-project .gtrconfig editor overlay.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use crate::model::workspace::ProjectConfig;
use crate::ui::popup_center;

pub fn render_config_modal(frame: &mut Frame, area: Rect, config: &ProjectConfig, project_name: &str) {
    let width = area.width.min(60).max(40);
    let height = area.height.min(16).max(8);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("postCreate: ", Style::default().fg(Color::Gray)),
            Span::styled(
                config.post_create.as_deref().unwrap_or("(none)"),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled("copy.include:", Style::default().fg(Color::Gray))),
    ];

    for inc in &config.copy_includes {
        lines.push(Line::from(Span::styled(format!("  {}", inc), Style::default().fg(Color::Green))));
    }
    if config.copy_includes.is_empty() {
        lines.push(Line::from(Span::styled("  (none)", Style::default().fg(Color::Gray))));
    }

    lines.push(Line::from(Span::styled("copy.exclude:", Style::default().fg(Color::Gray))));
    for exc in &config.copy_excludes {
        lines.push(Line::from(Span::styled(format!("  {}", exc), Style::default().fg(Color::Red))));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "e: edit .gtrignore  Esc: close",
        Style::default().fg(Color::Gray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Config: {} ", project_name))
        .border_style(Style::default().fg(Color::Yellow));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}
