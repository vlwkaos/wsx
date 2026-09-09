use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use wsx_core::runtime::{PluginSidecarDescriptor, PluginSidecarView, PluginTone, PluginViewRow};

use super::theme;

fn tone_color(tone: PluginTone) -> Color {
    match tone {
        PluginTone::Normal => theme::TEXT,
        PluginTone::Muted => theme::TEXT_MUTED,
        PluginTone::Accent => theme::ACCENT,
        PluginTone::Success => theme::SUCCESS,
        PluginTone::Warning => theme::WARNING,
        PluginTone::Error => theme::ERROR,
    }
}

fn truncate_cells(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if Line::from(value).width() <= width {
        return value.to_string();
    }
    let marker = "…";
    let content_width = width.saturating_sub(Line::from(marker).width());
    let mut output = String::new();
    for character in value.chars() {
        let candidate = format!("{output}{character}");
        if Line::from(candidate.as_str()).width() > content_width {
            break;
        }
        output.push(character);
    }
    output.push_str(marker);
    output
}

fn project_row(row: &PluginViewRow, width: usize) -> Line<'static> {
    let badge = truncate_cells(&row.badge, 2);
    let badge_width = Line::from(badge.as_str()).width();
    let value = truncate_cells(&row.value, width.saturating_sub(badge_width + 2));
    let value_width = Line::from(value.as_str()).width();
    let content_width = width.saturating_sub(badge_width + value_width + 2);
    let primary = truncate_cells(&row.primary, content_width);
    let primary_width = Line::from(primary.as_str()).width();
    let secondary_width = content_width.saturating_sub(primary_width + 1);
    let secondary = row
        .secondary
        .as_deref()
        .filter(|_| secondary_width >= 3)
        .map(|value| truncate_cells(value, secondary_width));
    let used = primary_width
        + secondary
            .as_deref()
            .map(|value| Line::from(value).width() + 1)
            .unwrap_or(0);
    let gap = content_width.saturating_sub(used) + 1;
    let mut spans = vec![
        Span::styled(badge, Style::default().fg(tone_color(row.tone)).bold()),
        Span::raw(" "),
        Span::styled(primary, Style::default().fg(theme::TEXT)),
    ];
    if let Some(secondary) = secondary {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            secondary,
            Style::default().fg(theme::TEXT_SUBTLE),
        ));
    }
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(
        value,
        Style::default().fg(tone_color(row.tone)),
    ));
    Line::from(spans)
}

pub fn render(
    frame: &mut Frame,
    area: Rect,
    descriptor: &PluginSidecarDescriptor,
    view: Option<&PluginSidecarView>,
) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(theme::DIVIDER))
        .title(Span::styled(
            format!(" {} ", descriptor.title),
            Style::default().fg(theme::ACCENT).bold(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let lines = match view {
        None => vec![Line::styled(
            "Loading",
            Style::default().fg(theme::TEXT_MUTED),
        )],
        Some(view) if view.payload.rows.is_empty() => vec![Line::styled(
            view.payload.empty.as_deref().unwrap_or("No items"),
            Style::default().fg(theme::TEXT_MUTED),
        )],
        Some(view) => {
            let mut lines = view
                .payload
                .rows
                .iter()
                .take(usize::from(inner.height))
                .map(|row| project_row(row, usize::from(inner.width)))
                .collect::<Vec<_>>();
            if view.payload.remaining > 0 && lines.len() < usize::from(inner.height) {
                lines.push(Line::styled(
                    format!("+{} more", view.payload.remaining),
                    Style::default().fg(theme::TEXT_MUTED),
                ));
            }
            lines
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_survives_before_parent_context_at_narrow_width() {
        let row = PluginViewRow {
            badge: " M".into(),
            primary: "important.rs".into(),
            secondary: Some("very/long/parent/path".into()),
            value: "+12 -3".into(),
            tone: PluginTone::Warning,
        };
        let line = project_row(&row, 24);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("important"));
        assert!(line.width() <= 24);
    }
}
