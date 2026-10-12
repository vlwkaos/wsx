//! Draws the pane view band and reader over the terminal viewport.
//! ^ docs/terminal-view.md: covered terminal rows are hidden; nothing below them moves.
use ratatui::{prelude::*, widgets::Clear};

use super::{fit_hints, theme, workspace_tree::truncate_to_width};
use crate::view_panel::ViewOpen;

pub struct ViewPanelView<'a> {
    pub open: ViewOpen,
    pub title: &'a str,
    /// One-based page position and page count.
    pub page: (usize, usize),
    /// `None` while the document loads.
    pub lines: Option<&'a [Line<'static>]>,
    pub failed: bool,
    pub scroll: usize,
    /// Prefix label such as `Ctrl+A`.
    pub prefix: &'a str,
}

/// Content rows of a panel that occupies `area`.
pub fn visible_rows(open: ViewOpen, area: Rect) -> usize {
    usize::from(area.height.saturating_sub(match open {
        ViewOpen::Closed => area.height,
        ViewOpen::Band => 1,
        ViewOpen::Reader => 2,
    }))
}

fn status_row(frame: &mut Frame, area: Rect, left: Vec<Span<'static>>, right: String) {
    frame.buffer_mut().set_style(area, theme::view_status());
    let width = usize::from(area.width);
    let left = Line::from(left);
    let right_width = Line::from(right.as_str()).width();
    frame
        .buffer_mut()
        .set_line(area.x, area.y, &left, area.width);
    if right_width > 0 && left.width() + right_width < width {
        let x = area
            .right()
            .saturating_sub(u16::try_from(right_width).unwrap_or(area.width));
        frame
            .buffer_mut()
            .set_stringn(x, area.y, right, right_width, theme::view_status());
    }
}

pub fn render(frame: &mut Frame, area: Rect, view: ViewPanelView<'_>) {
    if area.is_empty() || view.open == ViewOpen::Closed {
        return;
    }
    frame.render_widget(Clear, area);
    frame.buffer_mut().set_style(area, theme::view_surface());
    let rows = visible_rows(view.open, area);
    let content = Rect::new(
        area.x.saturating_add(1),
        area.y + u16::from(view.open == ViewOpen::Reader),
        area.width.saturating_sub(2),
        u16::try_from(rows).unwrap_or(0),
    );
    let total = view.lines.map_or(0, <[Line<'static>]>::len);
    match view.lines {
        Some(lines) => {
            for (row, line) in lines.iter().skip(view.scroll).take(rows).enumerate() {
                frame.buffer_mut().set_line(
                    content.x,
                    content.y + u16::try_from(row).unwrap_or(0),
                    line,
                    content.width,
                );
            }
        }
        None if content.height > 0 => {
            let message = if view.failed {
                "View is unavailable"
            } else {
                "Loading view…"
            };
            frame.buffer_mut().set_stringn(
                content.x,
                content.y,
                message,
                usize::from(content.width),
                theme::view_surface().fg(theme::TEXT_MUTED),
            );
        }
        None => {}
    }

    let width = usize::from(area.width);
    let page = (view.page.1 > 1).then(|| format!(" page {}/{} ", view.page.0, view.page.1));
    let title_budget = width.saturating_sub(page.as_deref().map_or(0, |page| page.len()) + 4);
    let mut identity = vec![Span::styled(
        format!(
            " ▤ {} ",
            truncate_to_width(view.title, title_budget.min(48))
        ),
        theme::view_title(),
    )];
    if let Some(page) = page {
        identity.push(Span::styled(page, theme::view_status()));
    }
    let last = (view.scroll + rows).min(total);
    let position = if total == 0 {
        String::new()
    } else {
        format!("{last}/{total}")
    };
    let prefix = view.prefix.to_ascii_lowercase();
    let bottom = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    let identity_width = Line::from(identity.clone()).width();
    match view.open {
        ViewOpen::Band => {
            let budget = width.saturating_sub(identity_width + position.len() + 3);
            let hints = fit_hints(
                &[
                    format!("({prefix} V)close"),
                    format!("({prefix} v)read"),
                    "(wheel)scroll".to_string(),
                ],
                budget,
            );
            status_row(frame, bottom, identity, format!("{position}  {hints} "));
        }
        ViewOpen::Reader => {
            let top = Rect::new(area.x, area.y, area.width, 1);
            status_row(frame, top, identity, String::new());
            let mut keys = vec![
                "(q)back".to_string(),
                "(j/k)line".to_string(),
                "(space)page".to_string(),
                "(g/G)ends".to_string(),
            ];
            if view.page.1 > 1 {
                keys.insert(1, "([ ])page".to_string());
            }
            let budget = width.saturating_sub(position.len() + 4);
            status_row(
                frame,
                bottom,
                vec![Span::styled(format!(" {position} "), theme::view_status())],
                format!("{} ", fit_hints(&keys, budget)),
            );
        }
        ViewOpen::Closed => {}
    }
}
