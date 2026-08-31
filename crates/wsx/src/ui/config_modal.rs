// Per-project wsx.config.yml editor overlay.

use crate::ui::{popup_center, theme};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use wsx_core::model::workspace::ProjectConfig;

pub fn render_config_modal(
    frame: &mut Frame,
    area: Rect,
    config: &ProjectConfig,
    project_name: &str,
) {
    let width = area.width.clamp(40, 60);
    let height = area.height.clamp(8, 16);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("postCreate: ", Style::default().fg(theme::TEXT_MUTED)),
            Span::styled(
                config.post_create.as_deref().unwrap_or("(none)"),
                Style::default().fg(theme::TEXT),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "copy.include:",
            Style::default().fg(theme::TEXT_MUTED),
        )),
    ];

    for inc in &config.copy_includes {
        lines.push(Line::from(Span::styled(
            format!("  {}", inc),
            Style::default().fg(theme::SUCCESS),
        )));
    }
    if config.copy_includes.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none)",
            Style::default().fg(theme::TEXT_MUTED),
        )));
    }

    lines.push(Line::from(Span::styled(
        "copy.exclude:",
        Style::default().fg(theme::TEXT_MUTED),
    )));
    for exc in &config.copy_excludes {
        lines.push(Line::from(Span::styled(
            format!("  {}", exc),
            Style::default().fg(theme::ERROR),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "(e)dit wsx.config.yml  Esc: close",
        Style::default().fg(theme::TEXT_MUTED),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Config: {} ", project_name))
        .border_style(Style::default().fg(theme::WARNING));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}
