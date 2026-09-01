// Per-project wsx.config.yml editor overlay.

use crate::ui::{popup_block, popup_center, theme};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph, Wrap},
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

    let hints = Line::from(vec![
        Span::styled(" (e)dit wsx.config.yml", Style::default().fg(theme::TEXT)),
        Span::styled("  Esc close ", Style::default().fg(theme::TEXT_MUTED)),
    ]);
    let block = popup_block(
        Line::from(format!(" Config: {} ", project_name)),
        hints,
        Style::default().fg(theme::WARNING),
    );
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16) -> String {
        (0..terminal.backend().size().unwrap().width)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn config_actions_render_on_the_bottom_border_not_in_the_body() {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_config_modal(frame, frame.area(), &ProjectConfig::default(), "demo")
            })
            .unwrap();

        let bottom = row(&terminal, 11);
        assert!(bottom.contains("(e)dit wsx.config.yml"), "{bottom:?}");
        assert!(bottom.contains("Esc close"), "{bottom:?}");
        assert_eq!(terminal.backend().buffer()[(0, 11)].symbol(), "└");
        assert_eq!(terminal.backend().buffer()[(59, 11)].symbol(), "┘");
        for y in 1..11 {
            let interior = row(&terminal, y);
            assert!(!interior.contains("(e)dit"), "{interior:?}");
            assert!(!interior.contains("Esc close"), "{interior:?}");
        }
    }

    #[test]
    fn narrow_config_border_clips_without_panicking() {
        let backend = ratatui::backend::TestBackend::new(20, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                render_config_modal(
                    frame,
                    frame.area(),
                    &ProjectConfig::default(),
                    "long-project-name",
                )
            })
            .unwrap();

        assert_eq!(terminal.backend().buffer()[(0, 5)].symbol(), "└");
        assert_eq!(terminal.backend().buffer()[(19, 5)].symbol(), "┘");
    }
}
