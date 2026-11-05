//! The Tasks view: the list in attention order and one task's detail, both read-only.
//! ref: docs/tasks.md "TUI"
use super::{task_chat::wrap_cells, theme, workspace_tree::truncate_to_width};
use crate::task_list::{Overview, Section, TaskList, TaskRow};
use ratatui::{
    layout::Rect,
    prelude::Stylize,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};
use serde::Deserialize;
use serde_json::Value;
use wsx_core::task::usage::{self, Usage};

const STATE_WIDTH: usize = 8;
/// Below this width a row drops its number; below `STATUS_MIN_WIDTH` its status label.
const NUMBER_MIN_WIDTH: usize = 40;
const STATUS_MIN_WIDTH: usize = 64;

pub fn render(frame: &mut Frame, area: Rect, list: &TaskList) {
    frame.render_widget(Clear, area);
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let width = inner.width as usize;
    let mut lines = vec![heading(list, width)];
    if let Some(ledger) = &list.ledger {
        // ^ The agent `a` picked, named once on the first line; both lines count it.
        let agent = list
            .agent
            .as_deref()
            .map_or(String::new(), |agent| format!("{agent}: "));
        for spent in project_usage(&ledger["usage"], &agent)
            .into_iter()
            .chain(project_usage(&ledger["usage_all"], "All projects "))
        {
            lines.push(muted(&truncate_to_width(&spent, width)));
        }
        lines.extend(limit_line(&ledger["usage_all"]["limits"], width));
    }
    // ^ A short window keeps its height for the tasks. ref: docs/tasks.md "TUI"
    if shows_days(list, area.height) {
        lines.extend(
            list.ledger
                .as_ref()
                .map_or(Vec::new(), |ledger| usage_rows(ledger, width, list.tokens)),
        );
    }
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

/// The overview as written: headings bold, a fenced block (mermaid too) muted with its
/// fences subtle, long lines wrapped. `Overview <project>` and the lines shown, as `3-30/64`,
/// head it.
pub fn render_overview(frame: &mut Frame, area: Rect, project: &str, overview: &Overview) {
    frame.render_widget(Clear, area);
    let inner = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    if inner.height < 3 || inner.width < 4 {
        return;
    }
    let width = inner.width as usize;
    let lines = match &overview.text {
        Ok(Some(text)) => overview_lines(text, width),
        Ok(None) => vec![muted("No overview yet")],
        Err(error) => vec![Line::styled(
            truncate_to_width(&format!("Could not read the overview: {error}"), width),
            Style::default().fg(theme::ERROR),
        )],
    };
    let height = inner.height - 2;
    let max_scroll = (lines.len() as u16).saturating_sub(height);
    overview.max_scroll.set(max_scroll);
    overview.height.set(height);
    let first = overview.scroll.min(max_scroll);
    let last = (first + height).min(lines.len() as u16);
    let position = format!("{}-{}/{}", first + 1, last, lines.len());
    let mut head = vec![Span::styled(
        "Overview",
        Style::default().fg(theme::ACCENT).bold(),
    )];
    let name = format!(" {project}");
    let used = "Overview".len() + Line::from(name.as_str()).width() + position.len();
    if used < width {
        head.push(Span::styled(name, Style::default().fg(theme::TEXT)));
        head.push(Span::raw(" ".repeat(width - used)));
    } else {
        head.push(Span::raw("  "));
    }
    head.push(Span::styled(
        position,
        Style::default().fg(theme::TEXT_MUTED),
    ));
    frame.render_widget(Paragraph::new(Line::from(head)), inner);
    frame.render_widget(Paragraph::new(lines).scroll((first, 0)), below(inner, 2));
}

/// The text's lines for `width` cells. A fenced block keeps its spaces and breaks at the
/// edge; other lines wrap at spaces.
fn overview_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut fenced = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        let fence = line.trim_start().starts_with("```");
        let (style, pieces) = if fence {
            fenced = !fenced;
            (
                Style::default().fg(theme::TEXT_SUBTLE),
                vec![line.to_string()],
            )
        } else if fenced {
            (
                Style::default().fg(theme::TEXT_MUTED),
                hard_wrap(line, width),
            )
        } else if line.starts_with('#') {
            (Style::default().fg(theme::ACCENT).bold(), wrap(line, width))
        } else {
            (Style::default().fg(theme::TEXT), wrap(line, width))
        };
        lines.extend(
            pieces
                .into_iter()
                .map(|piece| Line::styled(truncate_to_width(&piece, width), style)),
        );
    }
    lines
}

/// `text` broken at spaces into lines of at most `width` cells.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let cells: Vec<_> = text.chars().map(|char| (char, None)).collect();
    wrap_cells(&cells, width)
        .into_iter()
        .map(|line| line.into_iter().map(|(char, _)| char).collect())
        .collect()
}

/// `text` cut into pieces of at most `width` cells, spaces kept.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let (mut pieces, mut piece, mut used) = (Vec::new(), String::new(), 0);
    for char in text.chars() {
        let size = Line::from(char.encode_utf8(&mut [0; 4]) as &str).width();
        if used > 0 && used + size > width {
            pieces.push(std::mem::take(&mut piece));
            used = 0;
        }
        piece.push(char);
        used += size;
    }
    pieces.push(piece);
    pieces
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
        "ready" | "open" => theme::DONE,
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
/// Title first, then the number, the state word and the status label; narrow widths keep
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
    let status_width = if width >= STATUS_MIN_WIDTH {
        width * 2 / 5
    } else {
        0
    };
    let title_width = width.saturating_sub(STATE_WIDTH + 1 + number_width + status_width);
    let mut lines = Vec::new();
    let mut selected_line = None;
    for (index, row) in rows.iter().enumerate() {
        if index == 0 || rows[index - 1].section != row.section {
            if index > 0 {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                row.section.heading(),
                Style::default().fg(theme::TEXT_SUBTLE).bold(),
            ));
        }
        let mut spans = vec![Span::styled(
            cell(&row.title, title_width.saturating_sub(1)) + " ",
            Style::default().fg(theme::TEXT),
        )];
        if number_width > 0 {
            // ^ A role has no number: its column stays empty.
            let number = match row.section {
                Section::Roles => String::new(),
                _ => format!("#{}", row.id),
            };
            spans.push(Span::styled(
                cell(&number, number_width),
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }
        spans.push(Span::styled(
            cell(&row.state, STATE_WIDTH) + " ",
            Style::default().fg(state_color(&row.state)),
        ));
        if status_width > 0 {
            spans.push(Span::styled(
                cell(&row.status, status_width),
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

/// The window height from which the task list also shows the spend per day.
const USAGE_ROWS_HEIGHT: u16 = 30;

/// True when the list, in a content area `height` rows high, shows the day rows: the
/// footer offers `u` only then.
pub fn shows_days(list: &TaskList, height: u16) -> bool {
    height >= USAGE_ROWS_HEIGHT
        && list.detail_task().is_none()
        && list
            .ledger
            .as_ref()
            .is_some_and(|ledger| ledger["usage"]["days"].is_array())
}

/// One kind of a day's amount: its legend word, its key in the ledger's day (`parts` for
/// cost, `usage` for tokens) and its fixed color.
type Kind = (&'static str, &'static str, ratatui::style::Color);

/// The parts of a day's spend, as `usage.rs` names them.
const USAGE_PARTS: [Kind; 7] = [
    ("build", "build", theme::ACCENT),
    ("plan", "plan", theme::WARNING),
    ("context", "context", theme::DONE),
    ("review", "review", theme::IDLE),
    ("memo", "memo", theme::BLOCKED),
    ("other", "other", theme::SUCCESS),
    ("manager", "manager", theme::TEXT_MUTED),
];
/// The kinds of token of a day, as the web's Tokens chart (`usage.js` KINDS) takes them.
const USAGE_TOKENS: [Kind; 4] = [
    ("input", "input_tokens", theme::ACCENT),
    ("cache read", "cache_read_tokens", theme::WARNING),
    ("cache write", "cache_creation_tokens", theme::DONE),
    ("output", "output_tokens", theme::IDLE),
];
/// The width of a day row's date, its amount and, for tokens, the cache hit rate.
const DATE_WIDTH: usize = 7;
const AMOUNT_WIDTH: usize = 9;
const RATE_WIDTH: usize = 5;

/// A legend, then one row per day, newest first: its date, a stacked bar on one scale, its
/// cost; with `tokens` its kinds of token, their sum and the cache hit rate (cache read /
/// (input + cache read + cache write)), as the web's Usage sheet shows them.
fn usage_rows(ledger: &Value, width: usize, tokens: bool) -> Vec<Line<'static>> {
    let Some(days) = ledger["usage"]["days"].as_array() else {
        return Vec::new();
    };
    let kinds: &[Kind] = if tokens { &USAGE_TOKENS } else { &USAGE_PARTS };
    let group = if tokens { "usage" } else { "parts" };
    let amount_of = |day: &Value, key: &str| day[group][key].as_u64().unwrap_or(0);
    let total = |day: &Value| -> u64 { kinds.iter().map(|(_, key, _)| amount_of(day, key)).sum() };
    let format = |amount: u64| {
        if tokens {
            usage::tokens(amount)
        } else {
            usage::usd(amount)
        }
    };
    let hit_rate = |day: &Value| {
        let [input, read, write] = [0, 1, 2].map(|kind| amount_of(day, USAGE_TOKENS[kind].1));
        match input + read + write {
            0 => "-".to_string(),
            all => format!("{}%", (100.0 * read as f64 / all as f64).round()),
        }
    };
    let most = days.iter().map(total).max().unwrap_or(0).max(1);
    let rate_width = if tokens { RATE_WIDTH } else { 0 };
    let track = width
        .saturating_sub(DATE_WIDTH + AMOUNT_WIDTH + rate_width + 1)
        .max(1);
    // ^ Only the words that fit: a wrapped legend would push the tasks down.
    let mut room = width;
    let mut legend: Vec<Span> = kinds
        .iter()
        .map_while(|(word, _, color)| {
            let text = format!("■ {word} ");
            room = room.checked_sub(text.chars().count())?;
            Some(Span::styled(text, Style::default().fg(*color)))
        })
        .collect();
    if tokens && room >= "hit rate".len() {
        legend.push(Span::styled(
            "hit rate",
            Style::default().fg(theme::TEXT_MUTED),
        ));
    }
    let mut lines = vec![Line::from(legend)];
    for day in days {
        let date = crate::task::limit::local_time(day["day_ms"].as_u64().unwrap_or(0) / 1000)
            .map_or_else(String::new, |tm| {
                let name = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
                format!("{} {:>2}", name[tm.tm_wday as usize % 7], tm.tm_mday)
            });
        let mut spans = vec![Span::styled(
            format!("{date:<DATE_WIDTH$}"),
            Style::default().fg(theme::TEXT_SUBTLE),
        )];
        let mut used = 0;
        for (_, key, color) in kinds {
            let amount = amount_of(day, key);
            if amount > 0 {
                let cells = ((amount as usize * track) / most as usize)
                    .max(1)
                    .min(track - used);
                used += cells;
                spans.push(Span::styled("█".repeat(cells), Style::default().fg(*color)));
            }
        }
        spans.push(Span::raw(" ".repeat(track.saturating_sub(used) + 1)));
        let mut amount = format!("{:>AMOUNT_WIDTH$}", format(total(day)));
        if tokens {
            amount += &format!("{:>RATE_WIDTH$}", hit_rate(day));
        }
        spans.push(Span::styled(amount, Style::default().fg(theme::TEXT_MUTED)));
        lines.push(Line::from(spans));
    }
    lines
}

/// A spend (`usage::project`'s or `usage::all`'s): "Today $3.42 (manager $0.80) · 7 days
/// $21.10 (manager $4.02)", after `prefix`.
fn project_usage(spend: &Value, prefix: &str) -> Option<String> {
    let spent = |window: &str| {
        let cost = |who: &str| {
            Usage::deserialize(&spend[window][who]).map_or(0, |usage| usage.cost_micro_usd)
        };
        let manager = cost("manager");
        format!(
            "{} (manager {})",
            usage::usd(cost("workers") + manager),
            usage::usd(manager)
        )
    };
    spend.is_object().then(|| {
        format!(
            "{prefix}Today {} · 7 days {}",
            spent("today"),
            spent("week")
        )
    })
}

/// Claude's limit state (`usage::limits`): "Claude limit 5 hours 25%, resets 14:00 · 7 days
/// 47%, resets Nov 8 10:00", a window's status after its share unless it is `allowed`; in
/// the warning color while one warns, the error color while one refuses.
fn limit_line(limits: &Value, width: usize) -> Option<Line<'static>> {
    let limit = limits.as_array()?.first()?;
    let now = wsx_core::task::now_unix_ms();
    let mut worst = "allowed";
    let windows: Vec<String> = limit["windows"]
        .as_array()?
        .iter()
        .map(|window| {
            let kind = window["kind"].as_str().unwrap_or("");
            let status = window["status"].as_str().unwrap_or("");
            if status == "rejected" || (status == "warning" && worst == "allowed") {
                worst = status;
            }
            let mut words = match kind {
                "five_hour" => "5 hours".to_string(),
                "seven_day" => "7 days".to_string(),
                kind => kind.replace('_', " "),
            };
            if let Some(used) = window["utilization"].as_f64() {
                words += &format!(" {}%", (used * 100.0).round());
            }
            if !status.is_empty() && status != "allowed" {
                words += &format!(" {status}");
            }
            let resets = window["resets_ms"].as_u64().unwrap_or(0);
            format!(
                "{words}, resets {}",
                crate::task::limit::resume_time(resets, now)
            )
        })
        .collect();
    let color = match worst {
        "rejected" => theme::ERROR,
        "warning" => theme::WARNING,
        _ => theme::TEXT_MUTED,
    };
    Some(Line::styled(
        truncate_to_width(&format!("Claude limit {}", windows.join(" · ")), width),
        Style::default().fg(color),
    ))
}

/// One task's ledger entry as labeled rows: state, its status sentence, its checks, its
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
    if let Some(agent) = wsx_core::task::view::agent_label(&task["agent"]) {
        lines.push(Line::from(vec![label("Agent"), Span::raw(agent)]));
    }
    if let Some(spent) = Usage::deserialize(&task["usage"])
        .ok()
        .filter(|usage| usage.turns > 0)
    {
        let mut line = spent.line();
        // ^ The keep-warm turns that the turn count holds. ref: docs/tasks.md "Context rotation"
        if spent.pings > 0 {
            line += &format!(
                " · {} ping{}",
                spent.pings,
                if spent.pings == 1 { "" } else { "s" }
            );
        }
        lines.push(Line::from(vec![label("Usage"), Span::raw(line)]));
    }
    if let Some(status) = task["status"]["sentence"].as_str() {
        lines.push(Line::from(vec![
            label("Status"),
            Span::raw(status.to_string()),
        ]));
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
