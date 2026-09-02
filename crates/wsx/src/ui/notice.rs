use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, NoticeLevel, RuntimeHealth};

use super::theme;

struct NoticeView<'a> {
    level: NoticeLevel,
    title: &'a str,
    body: Option<&'a str>,
    sticky: bool,
}

fn notice_view(app: &App) -> Option<NoticeView<'_>> {
    if let Some(notice) = app.notice.as_ref() {
        return Some(NoticeView {
            level: notice.level,
            title: &notice.title,
            body: notice.body.as_deref(),
            sticky: false,
        });
    }
    match &app.runtime_health {
        RuntimeHealth::Reconnecting {
            last_success,
            error,
        } => Some(NoticeView {
            level: NoticeLevel::Error,
            title: "Runtime disconnected; retrying",
            body: Some(if last_success.is_some() {
                error.as_str()
            } else {
                "No current snapshot is available"
            }),
            sticky: true,
        }),
        RuntimeHealth::Connecting => Some(NoticeView {
            level: NoticeLevel::Info,
            title: "Connecting to Runtime",
            body: None,
            sticky: true,
        }),
        RuntimeHealth::Healthy { .. } => None,
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(notice) = notice_view(app) else {
        return;
    };
    if area.width < 8 || area.height < 3 {
        return;
    }

    let mobile = area.width < 60;
    let max_width = if mobile {
        area.width
    } else {
        area.width.saturating_sub(2).min(48)
    };
    let content_width = std::iter::once(notice.title)
        .chain(notice.body.into_iter().flat_map(str::lines))
        .map(|line| Line::from(line).width() as u16)
        .max()
        .unwrap_or(0);
    let width = if mobile {
        max_width
    } else {
        content_width.saturating_add(5).clamp(28, max_width)
    };
    let inner_width = width.saturating_sub(4).max(1) as usize;
    let wrapped_lines = std::iter::once(notice.title)
        .chain(notice.body.into_iter().flat_map(str::lines))
        .map(|line| Line::from(line).width().max(1).div_ceil(inner_width))
        .sum::<usize>();
    let height = (wrapped_lines as u16)
        .saturating_add(2)
        .clamp(2, 5.min(area.height.saturating_sub(1)));
    let x = if mobile {
        area.x
    } else {
        area.x + area.width.saturating_sub(width + 1)
    };
    let footer_y = area.bottom().saturating_sub(1);
    let y = footer_y.saturating_sub(height).max(area.y);
    let rect = Rect::new(x, y, width, height.min(footer_y.saturating_sub(y)));

    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(theme::toast_surface()), rect);
    let accent = match notice.level {
        NoticeLevel::Info => theme::ACCENT,
        NoticeLevel::Success => theme::SUCCESS,
        NoticeLevel::Warning => theme::WARNING,
        NoticeLevel::Error => theme::ERROR,
    };
    frame.render_widget(
        Block::default().style(theme::toast_accent(accent)),
        Rect::new(
            rect.x,
            rect.y,
            if notice.sticky { 2 } else { 1 }.min(rect.width),
            rect.height,
        ),
    );

    let content = Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(3),
        rect.height.saturating_sub(2),
    );
    let mut lines = vec![Line::from(Span::styled(
        notice.title,
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(body) = notice.body {
        lines
            .extend(body.lines().map(|line| {
                Line::from(Span::styled(line, Style::default().fg(theme::TEXT_MUTED)))
            }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme::toast_surface())
            .wrap(Wrap { trim: false }),
        content,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_error_target_fits_the_compact_notice_height() {
        let area = Rect::new(0, 0, 80, 16);
        let content_width = 44;
        let lines = [
            "Terminal busy: another writable controller",
            "Target: kgeditor › feature/#312 › ss › terminal",
        ];
        let wrapped = lines
            .iter()
            .map(|line| Line::from(*line).width().max(1).div_ceil(content_width))
            .sum::<usize>();

        assert_eq!(wrapped, 3);
        assert!(wrapped as u16 + 2 <= 5.min(area.height.saturating_sub(1)));
    }

    #[test]
    fn long_mixed_width_notice_stays_inside_frame() {
        let area = Rect::new(0, 0, 48, 12);
        let title = "Runtime 연결 실패 with a deliberately long explanation";
        let width = Line::from(title).width() as u16;
        assert!(width > 28);
        assert!(area.width >= 8);
    }
}
