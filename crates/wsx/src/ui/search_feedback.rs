//! ^ docs/terminal-context.md: logical search hits decorate visible ancestors without changing folds.
use ratatui::{prelude::*, widgets::Paragraph};

use super::{theme, workspace_tree::truncate_to_width};

pub(super) fn search_status(query: &str, total: usize, width: usize) -> String {
    let suffix = format!(" ({total})");
    let suffix_width = Line::from(suffix.as_str()).width();
    let fixed = suffix_width + 3;
    if width < fixed {
        if width >= suffix_width {
            return suffix;
        }
        return truncate_to_width(&format!(" {total}"), width);
    }
    format!(" /{}_{}", truncate_to_width(query, width - fixed), suffix)
}

#[derive(Clone, Copy)]
pub struct SearchFeedback<'a> {
    pub query: &'a str,
    pub counts: &'a [usize],
}

impl SearchFeedback<'_> {
    pub fn content_area(self, area: Rect) -> Rect {
        let count = self.counts.iter().copied().max().unwrap_or(0);
        let reserved = if self.query.is_empty() || count == 0 {
            0
        } else {
            count.to_string().len() + 1
        };
        let reserved = if usize::from(area.width) >= reserved + 8 {
            reserved
        } else {
            0
        };
        Rect {
            width: area.width.saturating_sub(reserved as u16),
            ..area
        }
    }

    pub fn render(self, frame: &mut Frame, area: Rect, content: Rect, offset: usize) {
        if self.query.is_empty() || area.width == 0 {
            return;
        }
        let query = self.query.to_lowercase();
        for (row, count) in self
            .counts
            .iter()
            .enumerate()
            .skip(offset)
            .take(usize::from(area.height))
        {
            if *count == 0 {
                continue;
            }
            let y = area.y + (row - offset) as u16;
            decorate_row(frame.buffer_mut(), content, y, &query);
            let count_area = Rect::new(content.right(), y, area.width - content.width, 1);
            if count_area.width > 0 {
                frame.render_widget(
                    Paragraph::new(format!(" {count}"))
                        .alignment(Alignment::Right)
                        .style(Style::default().fg(theme::ACCENT).bold()),
                    count_area,
                );
            }
        }
    }
}

fn decorate_row(buffer: &mut Buffer, area: Rect, y: u16, query: &str) {
    let mut text = String::new();
    let mut cells = Vec::new();
    let mut x = area.x;
    while x < area.right() {
        let symbol = buffer[(x, y)].symbol();
        // Wide cells have continuation slots, not spaces within the searchable text.
        let width = Line::from(symbol)
            .width()
            .max(1)
            .min(usize::from(area.right() - x)) as u16;
        let start = text.len();
        text.push_str(&symbol.to_lowercase());
        cells.push((start..text.len(), x, width, !symbol.trim().is_empty()));
        x += width;
    }
    let matches: Vec<_> = text
        .match_indices(query)
        .map(|(start, _)| start..start + query.len())
        .collect();
    for (bytes, x, width, nonblank) in cells {
        if !nonblank {
            continue;
        }
        let direct = matches
            .iter()
            .any(|hit| hit.start < bytes.end && bytes.start < hit.end);
        for column in x..x + width {
            let cell = &mut buffer[(column, y)];
            // An underline also identifies folded ancestors whose matching child is not painted.
            cell.set_style(Style::default().add_modifier(Modifier::UNDERLINED));
            if direct {
                cell.set_style(theme::accent_selection().bold());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn search_decorates_unicode_and_folded_ancestors_without_bleeding_or_losing_state() {
        let mut terminal = Terminal::new(TestBackend::new(26, 4)).unwrap();
        let area = Rect::new(2, 1, 22, 2);
        let feedback = SearchFeedback {
            query: "开发e\u{301}",
            counts: &[1, 2],
        };
        terminal
            .draw(|frame| {
                let content = feedback.content_area(area);
                frame.render_widget(
                    Paragraph::new("开发e\u{301} text\n◐ parent")
                        .style(Style::default().fg(theme::BLOCKED)),
                    content,
                );
                feedback.render(frame, area, content, 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in [2, 4, 6] {
            assert_eq!(buffer[(x, 1)].bg, theme::ACCENT);
            assert!(buffer[(x, 1)].modifier.contains(Modifier::UNDERLINED));
        }
        assert_eq!(buffer[(2, 2)].fg, theme::BLOCKED);
        assert!(buffer[(2, 2)].modifier.contains(Modifier::UNDERLINED));
        assert_eq!(buffer[(23, 2)].symbol(), "2");
        assert_eq!(buffer[(2, 0)].bg, Color::Reset);
        assert_eq!(buffer[(24, 1)].bg, Color::Reset);
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("开发e\u{301} text\n◐ parent"), area);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(2, 1)].bg, Color::Reset);
        assert!(!terminal.backend().buffer()[(2, 2)]
            .modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn narrow_status_preserves_count_before_long_query_detail() {
        for width in 0..=60 {
            let status = search_status(&"开发👩‍💻e\u{301}".repeat(12), 1234, width);
            assert!(
                Line::from(status.as_str()).width() <= width,
                "{width}: {status}"
            );
            if width >= 9 {
                assert!(status.contains("(1234)"), "{status}");
            }
        }
    }
}
