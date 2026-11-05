//! The Panels view: one panel's last output as written, read-only. ref: docs/panels.md
use super::{theme, workspace_tree::truncate_to_width};
use crate::panel_view::PanelsView;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use wsx_core::panel::PanelStatus;

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

pub fn render(frame: &mut Frame, area: Rect, view: &PanelsView) {
    frame.render_widget(Clear, area);
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = inner.width as usize;
    let mut head = vec![Span::styled(
        format!("{} panels", view.project),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for (index, info) in view.panels.iter().flatten().enumerate() {
        let style = if index == view.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        head.push(Span::raw("  "));
        head.push(Span::styled(info.name.clone(), style));
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
        lines.extend(notice.lines().map(|line| muted(line)));
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
    lines.push(Line::raw(""));
    let chrome = lines.len() as u16;
    frame.render_widget(Paragraph::new(lines), inner);
    let Some(shown) = &view.shown else { return };
    let body = Rect {
        y: inner.y.saturating_add(chrome),
        height: inner.height.saturating_sub(chrome),
        ..inner
    };
    // ^ The output as written: no wrap, no reflow; a long line is cut by the width.
    let rows: Vec<Line> = shown.run.output.lines().map(Line::raw).collect();
    view.max_scroll
        .set((rows.len() as u16).saturating_sub(body.height));
    frame.render_widget(Paragraph::new(rows).scroll((view.scroll, 0)), body);
}
