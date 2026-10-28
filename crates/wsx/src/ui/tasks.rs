//! The Tasks view: the list in attention order and one task's detail, both read-only.
//! ref: docs/tasks.md "TUI"
use super::{theme, workspace_tree::truncate_to_width};
use crate::task_list::{waits, TaskList, TaskRow};
use ratatui::{
    layout::Rect,
    prelude::Stylize,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

const STATE_WIDTH: usize = 8;
/// Below this width a row drops its number; below `WAITS_MIN_WIDTH` what it waits for.
const NUMBER_MIN_WIDTH: usize = 40;
const WAITS_MIN_WIDTH: usize = 64;

pub fn render(frame: &mut Frame, area: Rect, list: &TaskList) {
    frame.render_widget(Clear, area);
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = inner.width as usize;
    let mut lines = vec![heading(list, width)];
    if let Some(error) = &list.error {
        lines.push(Line::styled(
            truncate_to_width(&format!("Could not read tasks: {error}"), width),
            Style::default().fg(theme::ERROR),
        ));
    }
    lines.push(Line::raw(""));
    let chrome = lines.len();
    if let Some(task) = list.detail_task() {
        let body = Paragraph::new(detail(task))
            .wrap(Wrap { trim: false })
            .scroll((list.scroll, 0));
        frame.render_widget(Paragraph::new(lines), inner);
        frame.render_widget(body, below(inner, chrome));
        return;
    }
    let (body, selected) = if list.ledger.is_none() {
        (
            vec![muted(if list.error.is_some() {
                ""
            } else {
                "Loading tasks"
            })],
            None,
        )
    } else if list.rows.is_empty() {
        (vec![muted("No tasks")], None)
    } else {
        task_lines(&list.rows, list.selected, width)
    };
    let area = below(inner, chrome);
    // ^ Keep the selected row in view; the list scrolls, the heading stays.
    let offset = selected
        .map(|line| (line + 1).saturating_sub(area.height as usize))
        .unwrap_or(0);
    frame.render_widget(Paragraph::new(lines), inner);
    frame.render_widget(Paragraph::new(body).scroll((offset as u16, 0)), area);
}

fn below(area: Rect, rows: usize) -> Rect {
    let rows = (rows as u16).min(area.height);
    Rect {
        y: area.y + rows,
        height: area.height - rows,
        ..area
    }
}

pub(super) fn muted(text: &str) -> Line<'static> {
    Line::styled(text.to_string(), Style::default().fg(theme::TEXT_MUTED))
}

/// `Tasks <project>` on the left, the To check and approval counts on the right; the
/// project name goes first when both do not fit.
fn heading(list: &TaskList, width: usize) -> Line<'static> {
    let ledger = list.ledger.as_ref();
    let count = |key: &str| {
        ledger.map_or(0, |ledger| {
            ledger[key]
                .as_u64()
                .or_else(|| ledger[key].as_array().map(|items| items.len() as u64))
                .unwrap_or(0)
        })
    };
    let approvals = count("approvals");
    let counts = format!(
        "{} to check  {} approval{}",
        count("to_check"),
        approvals,
        if approvals == 1 { "" } else { "s" }
    );
    let title = "Tasks";
    let project = format!(" {}", list.project);
    let used = title.len() + Line::from(project.as_str()).width() + counts.len();
    let mut spans = vec![Span::styled(
        title,
        Style::default().fg(theme::ACCENT).bold(),
    )];
    if used + 2 <= width {
        spans.push(Span::styled(project, Style::default().fg(theme::TEXT)));
        spans.push(Span::raw(" ".repeat(width - used)));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(counts, Style::default().fg(theme::TEXT_MUTED)));
    Line::from(spans)
}

fn state_color(state: &str) -> Color {
    match state {
        "waiting" | "failed" => theme::BLOCKED,
        "ready" => theme::DONE,
        "checking" | "working" => theme::WORKING,
        _ => theme::TEXT_MUTED,
    }
}

/// `text` cut to `width` cells with an ellipsis, then padded to it.
fn cell(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let pad = width.saturating_sub(Line::from(text.as_str()).width());
    text + &" ".repeat(pad)
}

/// The rows under `Open` and `Recent` headings, and the line of the selected row.
/// Title first, then the number, the state word and what it waits for; narrow widths keep
/// the title and the state.
fn task_lines(
    rows: &[TaskRow],
    selected: usize,
    width: usize,
) -> (Vec<Line<'static>>, Option<usize>) {
    let number_width = if width >= NUMBER_MIN_WIDTH {
        rows.iter()
            .map(|row| row.id.to_string().len() + 2)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let waits_width = if width >= WAITS_MIN_WIDTH {
        width * 2 / 5
    } else {
        0
    };
    let title_width = width.saturating_sub(STATE_WIDTH + 1 + number_width + waits_width);
    let mut lines = Vec::new();
    let mut selected_line = None;
    for (index, row) in rows.iter().enumerate() {
        if index == 0 || rows[index - 1].open != row.open {
            if index > 0 {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                if row.open { "Open" } else { "Recent" },
                Style::default().fg(theme::TEXT_SUBTLE).bold(),
            ));
        }
        let mut spans = vec![Span::styled(
            cell(&row.title, title_width.saturating_sub(1)) + " ",
            Style::default().fg(theme::TEXT),
        )];
        if number_width > 0 {
            spans.push(Span::styled(
                cell(&format!("#{}", row.id), number_width),
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }
        spans.push(Span::styled(
            cell(&row.state, STATE_WIDTH) + " ",
            Style::default().fg(state_color(&row.state)),
        ));
        if waits_width > 0 {
            spans.push(Span::styled(
                cell(&row.waits, waits_width),
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }
        let mut line = Line::from(spans);
        if index == selected {
            selected_line = Some(lines.len());
            line = line.patch_style(theme::selected_row(false));
        }
        lines.push(line);
    }
    (lines, selected_line)
}

/// One task's ledger entry as labeled rows: state, what it waits for, its checks, its
/// To check items, then the worker's summary or memo.
fn detail(task: &Value) -> Vec<Line<'static>> {
    let label = |text: &str| {
        Span::styled(
            format!("{text:<10}"),
            Style::default().fg(theme::TEXT_SUBTLE),
        )
    };
    let state = task["state"].as_str().unwrap_or_default().to_string();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                task["title"].as_str().unwrap_or_default().to_string(),
                Style::default().fg(theme::TEXT).bold(),
            ),
            Span::styled(
                format!("  #{}", task["id"].as_u64().unwrap_or_default()),
                Style::default().fg(theme::TEXT_MUTED),
            ),
        ]),
        Line::from(vec![
            label("State"),
            Span::styled(state.clone(), Style::default().fg(state_color(&state))),
        ]),
    ];
    let waiting = waits(task);
    if !waiting.is_empty() {
        lines.push(Line::from(vec![label("Waits"), Span::raw(waiting)]));
    }
    if let Some(checks) = task["checks"]
        .as_array()
        .filter(|checks| !checks.is_empty())
    {
        let outcome = |check: &Value| check["outcome"].as_str().unwrap_or_default().to_string();
        let passed = checks
            .iter()
            .filter(|check| outcome(check) == "passed")
            .count();
        lines.push(Line::from(vec![
            label("Checks"),
            Span::raw(format!("{passed} passed, {} failed", checks.len() - passed)),
        ]));
        for check in checks {
            let outcome = outcome(check);
            let color = if outcome == "passed" {
                theme::SUCCESS
            } else {
                theme::ERROR
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{outcome:<10}"), Style::default().fg(color)),
                Span::raw(check["command"].as_str().unwrap_or_default().to_string()),
            ]));
        }
    }
    if let Some(items) = task["human_verify"].as_u64().filter(|items| *items > 0) {
        lines.push(Line::from(vec![
            label("To check"),
            Span::raw(format!("{items} item{}", if items == 1 { "" } else { "s" })),
        ]));
    }
    for (key, heading) in [("summary", "Summary"), ("memo", "Memo")] {
        if let Some(text) = task[key].as_str().filter(|text| !text.trim().is_empty()) {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                heading,
                Style::default().fg(theme::ACCENT).bold(),
            ));
            lines.extend(text.lines().map(|line| Line::raw(line.to_string())));
        }
    }
    lines
}
