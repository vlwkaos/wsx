// Delete confirmation dialog.

use crate::ui::{popup_block, popup_upper, theme};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};

pub fn render_confirm(frame: &mut Frame, area: Rect, message: &str) {
    let width = 60_u16.min(area.width);
    let popup = popup_upper(area, width, 6);

    frame.render_widget(Clear, popup);

    let hints = Line::from(vec![
        Span::styled(
            " y/Enter confirm",
            Style::default().fg(theme::SUCCESS).bold(),
        ),
        Span::styled("  n/Esc cancel ", Style::default().fg(theme::TEXT_MUTED)),
    ]);
    let block = popup_block(
        Line::from(" Confirm "),
        hints,
        Style::default().fg(theme::ERROR),
    );

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let para = Paragraph::new(message).wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_actions_share_the_bottom_border() {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_confirm(frame, frame.area(), "Delete project?"))
            .unwrap();

        let rows = (0..12)
            .map(|y| {
                (0..60)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let bottom_y = rows
            .iter()
            .position(|row| row.contains("y/Enter confirm"))
            .expect("confirmation hints");
        let bottom = &rows[bottom_y];
        assert!(bottom.contains("n/Esc cancel"), "{bottom:?}");
        assert_eq!(
            terminal.backend().buffer()[(0, bottom_y as u16)].symbol(),
            "└"
        );
        assert_eq!(
            terminal.backend().buffer()[(59, bottom_y as u16)].symbol(),
            "┘"
        );
        for (y, row) in rows.iter().enumerate() {
            if y != bottom_y {
                assert!(!row.contains("y/Enter confirm"), "{row:?}");
                assert!(!row.contains("n/Esc cancel"), "{row:?}");
            }
        }
    }

    #[test]
    fn narrow_confirmation_render_is_safe() {
        let backend = ratatui::backend::TestBackend::new(20, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| render_confirm(frame, frame.area(), "Delete?"))
            .unwrap();
    }
}
