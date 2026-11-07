//! The Panels view: one panel's last output, read-only: as written, or for a `format: json`
//! panel with a good document, as boxed sections with bars. ref: docs/panels.md
use super::{
    text::{cells, fit, truncate_to_width},
    theme,
};
use crate::panel_view::PanelsView;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use wsx_core::panel::{Item, Level, PanelDoc, PanelStatus};

/// "12 s ago", "3 min ago".
fn age(ms: u64) -> String {
    match ms / 1000 {
        s if s < 60 => format!("{s} s ago"),
        s => format!("{} min ago", s / 60),
    }
}

/// The state line: "ok, 12 s ago · every 60 s".
fn state(view: &PanelsView) -> String {
    let Some(shown) = &view.shown else {
        return "Running…".into();
    };
    let how = match shown.run.status {
        PanelStatus::Pending => return "Running…".into(),
        PanelStatus::Ok => "ok".to_string(),
        PanelStatus::Failed => match shown.run.exit {
            Some(code) => format!("failed (exit {code})"),
            None => "failed (signal)".into(),
        },
        PanelStatus::TimedOut => "timed out".into(),
        PanelStatus::NotStarted => "did not start".into(),
    };
    let every = match shown.refresh {
        0 => "on open".to_string(),
        secs => format!("every {secs} s"),
    };
    let running = if shown.running { " · running" } else { "" };
    format!("{how}, updated {} · {every}{running}", age(view.age_ms()))
}

/// A level's word and the theme's state colour; no level is plain.
fn level_look(level: Option<Level>) -> (&'static str, Color) {
    match level {
        Some(Level::Ok) => ("ok", theme::SUCCESS),
        Some(Level::Warn) => ("warn", theme::WARNING),
        Some(Level::Bad) => ("bad", theme::ERROR),
        None => ("", theme::TEXT),
    }
}

/// 8.5, 16: one decimal, none when whole.
fn number(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

/// One section's rows (without the box), `width` cells wide at most.
fn item_rows(items: &[Item], width: usize) -> Vec<Vec<Span<'static>>> {
    let muted = Style::default().fg(theme::TEXT_MUTED);
    // ^ One label column for the section's gauges and stats, so their values line up.
    let label_width = items
        .iter()
        .filter_map(|item| match item {
            Item::Gauge { label, .. } | Item::Stat { label, .. } => Some(cells(label)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(width / 3);
    let mut rows = Vec::new();
    for item in items {
        match item {
            Item::Gauge {
                label,
                used,
                total,
                unit,
                level,
            } => {
                let (word, color) = level_look(*level);
                let share = used.0 / total.0;
                let unit = unit
                    .as_deref()
                    .map_or(String::new(), |unit| format!(" {unit}"));
                let tail = format!(
                    " {:>3}%  {} / {}{unit}{}{word}",
                    (share * 100.0).round().min(999.0),
                    number(used.0),
                    number(total.0),
                    if word.is_empty() { "" } else { "  " },
                );
                let bar = width.saturating_sub(label_width + 1 + cells(&tail));
                let mut row = vec![Span::raw(fit(label, label_width))];
                if bar >= 4 {
                    let filled = ((share.clamp(0.0, 1.0)) * bar as f64).round() as usize;
                    let bar_color = if level.is_some() {
                        color
                    } else {
                        theme::ACCENT
                    };
                    row.push(Span::raw(" "));
                    row.push(Span::styled(
                        "█".repeat(filled),
                        Style::default().fg(bar_color),
                    ));
                    row.push(Span::styled(
                        "░".repeat(bar - filled),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    ));
                }
                let (value, word_text) = tail.split_at(tail.len() - word.len());
                row.push(Span::raw(value.to_string()));
                row.push(Span::styled(
                    word_text.to_string(),
                    Style::default().fg(color),
                ));
                rows.push(row);
            }
            Item::Stat {
                label,
                value,
                note,
                level,
            } => {
                let (word, color) = level_look(*level);
                let mut row = vec![
                    Span::raw(fit(label, label_width)),
                    Span::raw("  "),
                    Span::styled(
                        value.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(note) = note {
                    row.push(Span::styled(format!("  {note}"), muted));
                }
                if !word.is_empty() {
                    row.push(Span::styled(
                        format!("  {word}"),
                        Style::default().fg(color),
                    ));
                }
                rows.push(row);
            }
            Item::Table {
                columns,
                rows: body,
            } => {
                let mut widths: Vec<usize> = (0..columns.len())
                    .map(|c| {
                        body.iter()
                            .map(|row| cells(&row[c]))
                            .chain([cells(&columns[c])])
                            .max()
                            .unwrap_or(0)
                    })
                    .collect();
                // ^ Too wide: the widest column gives a cell at a time (its cells end in `…`).
                while widths.iter().sum::<usize>() + 2 * (widths.len() - 1) > width {
                    let Some(widest) = widths.iter_mut().max().filter(|w| **w > 3) else {
                        break;
                    };
                    *widest -= 1;
                }
                let line = |texts: &[String], style: Style| {
                    let text: Vec<String> = texts
                        .iter()
                        .zip(&widths)
                        .map(|(text, w)| fit(text, *w))
                        .collect();
                    vec![Span::styled(text.join("  "), style)]
                };
                rows.push(line(columns, muted.add_modifier(Modifier::BOLD)));
                rows.extend(body.iter().map(|row| line(row, Style::default())));
            }
            Item::List {
                items: entries,
                level,
            } => {
                let (word, color) = level_look(*level);
                for (index, entry) in entries.iter().enumerate() {
                    let mut row = vec![
                        Span::styled("• ", Style::default().fg(color)),
                        Span::raw(entry.clone()),
                    ];
                    if index == 0 && !word.is_empty() {
                        row.push(Span::styled(
                            format!("  {word}"),
                            Style::default().fg(color),
                        ));
                    }
                    rows.push(row);
                }
            }
        }
    }
    rows
}

/// The document as lines: its title, then each section in a box that is `width` cells wide.
fn doc_lines(doc: &PanelDoc, width: usize) -> Vec<Line<'static>> {
    let frame = Style::default().fg(theme::DIVIDER);
    let inner = width.saturating_sub(4).max(8);
    let mut lines = Vec::new();
    if let Some(title) = &doc.title {
        let (word, color) = level_look(doc.level);
        // ^ `doc.level` is set only on the machine card.
        lines.push(Line::from(vec![
            Span::styled(
                truncate_to_width(title, width.saturating_sub(cells(word) + 2)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if word.is_empty() {
                    String::new()
                } else {
                    format!("  {word}")
                },
                Style::default().fg(color),
            ),
        ]));
    }
    for section in &doc.sections {
        let title = section
            .title
            .as_deref()
            .map(|title| format!(" {} ", truncate_to_width(title, inner.saturating_sub(2))))
            .unwrap_or_default();
        let dashes = "─".repeat((inner + 2).saturating_sub(cells(&title)));
        lines.push(Line::from(vec![
            Span::styled("┌", frame),
            Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(format!("{dashes}┐"), frame),
        ]));
        for row in item_rows(&section.items, inner) {
            let used: usize = row.iter().map(|span| span.width()).sum();
            let mut spans = vec![Span::styled("│ ", frame)];
            spans.extend(row);
            spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
            spans.push(Span::styled(" │", frame));
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled(format!("└{}┘", "─".repeat(inner + 2)), frame));
    }
    lines
}

pub fn render(frame: &mut Frame, area: Rect, view: &PanelsView) {
    frame.render_widget(Clear, area);
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = inner.width as usize;
    let mut head = vec![Span::styled(
        view.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (index, entry) in view.panels.iter().flatten().enumerate() {
        let style = if index == view.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        head.push(Span::raw("  "));
        head.push(Span::styled(entry.info.name.clone(), style));
    }
    let mut lines = vec![Line::from(head)];
    let muted = |text: &str| {
        Line::styled(
            truncate_to_width(text, width),
            Style::default().fg(theme::TEXT_MUTED),
        )
    };
    let error = |text: &str| {
        Line::styled(
            truncate_to_width(text, width),
            Style::default().fg(theme::ERROR),
        )
    };
    if let Some(notice) = &view.notice {
        lines.extend(notice.lines().map(&muted));
    }
    match (&view.panels, &view.shown, &view.error) {
        (None, _, None) => lines.push(muted("Loading panels")),
        (Some(panels), _, _) if panels.is_empty() => lines.push(muted("No panels")),
        _ => {}
    }
    if let Some(message) = &view.error {
        lines.push(error(&format!("Could not read the panel: {message}")));
    }
    if view.shown.is_some() {
        let failed = view.shown.as_ref().is_some_and(|shown| {
            !matches!(shown.run.status, PanelStatus::Ok | PanelStatus::Pending)
        });
        let text = state(view);
        lines.push(if failed { error(&text) } else { muted(&text) });
    }
    if let Some(notice) = view
        .shown
        .as_ref()
        .and_then(|shown| shown.run.notice.as_ref())
    {
        lines.extend(notice.lines().map(&muted));
    }
    lines.push(Line::raw(""));
    let chrome = lines.len() as u16;
    frame.render_widget(Paragraph::new(lines), inner);
    let Some(shown) = &view.shown else { return };
    let body = Rect {
        y: inner.y.saturating_add(chrome),
        height: inner.height.saturating_sub(chrome),
        ..inner
    };
    // ^ The output as written: no wrap, no reflow; a long line is cut by the width. A good
    // document is drawn to the width instead.
    let rows: Vec<Line> = match &shown.run.doc {
        Some(doc) => doc_lines(doc, body.width as usize),
        None => shown.run.output.lines().map(Line::raw).collect(),
    };
    view.max_scroll
        .set((rows.len() as u16).saturating_sub(body.height));
    frame.render_widget(Paragraph::new(rows).scroll((view.scroll, 0)), body);
}
