// Delete confirmation dialog.

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use crate::ui::popup_upper;

pub fn render_confirm(frame: &mut Frame, area: Rect, message: &str) {
    let width = 60_u16.min(area.width);
    let popup = popup_upper(area, width, 6);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm ")
        .border_style(Style::default().fg(Color::Red));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Message (may wrap)
    let msg_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
    let para = Paragraph::new(message)
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(para, msg_area);

    // Action bar pinned to bottom
    render_confirm_actions(frame, Rect::new(inner.x, inner.y + inner.height.saturating_sub(1), inner.width, 1));
}

/// Reusable confirm/cancel action bar: `[y/Enter] Confirm  [n/Esc] Cancel`
pub fn render_confirm_actions(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("[y/Enter]", Style::default().fg(Color::Green).bold()),
        Span::raw(" Confirm  "),
        Span::styled("[n/Esc]", Style::default().fg(Color::Red).bold()),
        Span::raw(" Cancel"),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}
