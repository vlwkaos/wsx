//! Host-owned review rendering using the existing preview theme.
use super::theme;
use crate::review::ReviewState;
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use wsx_core::runtime::ReviewLine;

pub fn render(frame: &mut Frame, area: Rect, mut context: Vec<Line<'static>>, state: &ReviewState) {
    frame.render_widget(Clear, area);
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(3),
        area.height.saturating_sub(2),
    );
    if context.len().saturating_add(12) > usize::from(inner.height) {
        context.truncate(2);
    }
    let mut lines = context;
    lines.push(Line::from(""));
    let small = inner.height < 14;
    if !small || !state.diff_focus {
        let count = state.files.as_ref().map_or(0, |list| list.files.len());
        lines.push(Line::styled(
            format!("Local:   {count} files"),
            Style::default().fg(theme::TEXT_SUBTLE),
        ));
        let visible = usize::from(inner.height.saturating_sub(6)).clamp(1, 6);
        let start = state.selected.saturating_sub(visible / 2);
        if let Some(list) = &state.files {
            for (index, file) in list.files.iter().enumerate().skip(start).take(visible) {
                let marker = if index == state.selected { "›" } else { " " };
                let path = file
                    .new_path
                    .as_ref()
                    .or(file.old_path.as_ref())
                    .map_or("", String::as_str);
                lines.push(Line::styled(
                    format!(
                        "{marker} {path}  +{} -{}",
                        file.additions.map_or("?".into(), |n| n.to_string()),
                        file.deletions.map_or("?".into(), |n| n.to_string())
                    ),
                    Style::default().fg(if index == state.selected {
                        theme::ACCENT
                    } else {
                        theme::WARNING
                    }),
                ));
            }
        }
        lines.push(Line::from(""));
    }
    if !state.message.is_empty() {
        lines.push(Line::styled(
            state.message.clone(),
            Style::default().fg(theme::WARNING),
        ));
    }
    if !small || state.diff_focus {
        if let Some(diff) = &state.diff {
            lines.push(Line::styled(
                format!("{} (working changes)", diff.file_id),
                Style::default().fg(theme::TEXT_SUBTLE),
            ));
            let mut body = Vec::new();
            for hunk in &diff.hunks {
                body.push(Line::styled(
                    format!(
                        "@@ -{},{} +{},{} @@ {}",
                        hunk.old_start,
                        hunk.old_count,
                        hunk.new_start,
                        hunk.new_count,
                        hunk.heading
                    ),
                    Style::default().fg(theme::TEXT_SUBTLE),
                ));
                for line in &hunk.lines {
                    let (prefix, text, color) = match line {
                        ReviewLine::Context(text) => (" ", text.as_str(), theme::TEXT),
                        ReviewLine::Addition(text) => ("+", text.as_str(), theme::SUCCESS),
                        ReviewLine::Deletion(text) => ("-", text.as_str(), theme::ERROR),
                        ReviewLine::NoNewline => {
                            ("", "No newline at end of file", theme::TEXT_SUBTLE)
                        }
                    };
                    body.push(Line::styled(
                        format!("{prefix}{}", text.replace('\t', "    ")),
                        Style::default().fg(color),
                    ));
                }
            }
            if body.is_empty() {
                body.push(Line::from(format!("{:?}", diff.content_kind)));
            }
            if diff.truncated {
                body.push(Line::from("Diff truncated"));
            }
            let scroll = state.scroll.min(body.len().saturating_sub(1));
            lines.extend(body.into_iter().skip(scroll));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
