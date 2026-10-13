// Startup screen while wsxd is upgraded. See docs/daemon-live-upgrade.md.

use crate::app::{replacement_wait_reason, UpgradeGate, UpgradeStep};
use crate::ui::theme;
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Wrap},
};
use wsx_core::runtime;

struct UpgradeView {
    body: Vec<Line<'static>>,
    // What the user can do about the step; wraps under the step label.
    advice: Vec<Line<'static>>,
    keys: Vec<Line<'static>>,
}

const ADVICE_INSET: u16 = 3;

fn key_line(key: &'static str, effect: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key:<6}"), Style::default().fg(theme::ACCENT)),
        Span::styled(effect, Style::default().fg(theme::TEXT_MUTED)),
    ])
}

// ^ Pure projection: the same state gives the same rows at every size.
fn project(gate: &UpgradeGate, spacious: bool) -> UpgradeView {
    let text = Style::default().fg(theme::TEXT);
    let muted = Style::default().fg(theme::TEXT_MUTED);
    let terminals = match gate.live_runtimes {
        0 => " No terminal is running".to_string(),
        1 => " 1 running terminal keeps running".to_string(),
        count => format!(" {count} running terminals keep running"),
    };
    let mut body = vec![
        Line::styled(" wsx upgrade", text.bold()),
        Line::from(vec![
            Span::styled(" wsxd  ", muted),
            Span::styled(
                format!("{} r{}", gate.daemon_version, gate.daemon_revision),
                text,
            ),
            Span::styled("  →  ", muted),
            Span::styled(
                format!("{} r{}", runtime::WSX_VERSION, runtime::DAEMON_REVISION),
                text.bold(),
            ),
        ]),
        Line::styled(terminals, text),
    ];
    let mut advice = Vec::new();
    let mut keys = Vec::new();
    match &gate.step {
        UpgradeStep::Upgrading => body.push(Line::styled(
            " ● Upgrading wsxd",
            Style::default().fg(theme::WORKING).bold(),
        )),
        UpgradeStep::Waiting(blockers) => {
            body.push(Line::styled(
                " ◐ Waiting",
                Style::default().fg(theme::WARNING).bold(),
            ));
            advice.push(Line::styled(replacement_wait_reason(blockers), text));
            advice.push(Line::styled("The upgrade then continues by itself.", muted));
            keys.push(key_line(
                "Enter",
                format!("open on wsxd {} now", gate.daemon_version),
            ));
        }
    }
    keys.push(key_line("q", "quit".into()));
    if spacious {
        // Group identity, state and actions when rows allow it.
        body.insert(1, Line::default());
        body.insert(4, Line::default());
    }
    UpgradeView { body, advice, keys }
}

pub fn render(frame: &mut Frame, area: Rect, gate: &UpgradeGate) {
    let view = project(gate, area.height >= 16);
    let keys_height = (view.keys.len() as u16).min(area.height);
    let [body_area, advice_area, keys_area] = Layout::vertical([
        Constraint::Length(view.body.len() as u16),
        Constraint::Min(0),
        Constraint::Length(keys_height),
    ])
    .areas(area);
    let inset = ADVICE_INSET.min(advice_area.width);
    let advice_area = Rect {
        x: advice_area.x + inset,
        width: advice_area.width - inset,
        ..advice_area
    };
    frame.render_widget(Paragraph::new(view.body), body_area);
    frame.render_widget(
        Paragraph::new(view.advice).wrap(Wrap { trim: true }),
        advice_area,
    );
    frame.render_widget(Paragraph::new(view.keys), keys_area);
}
