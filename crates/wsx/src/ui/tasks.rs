//! The Tasks view: the list in attention order and one task's detail, both read-only.
//! ref: docs/tasks.md "TUI"
use super::{
    text::{fit, hard_wrap, truncate_to_width, wrap_cells},
    theme,
};
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

/// The New chat form over the list: a line each for the name, the goal and, when the config
/// has entries, the agent; a refusal under them. ref: docs/tasks.md "TUI"
pub fn render_new_chat(frame: &mut Frame, area: Rect, form: &crate::task_new_chat::NewChat) {
    let rows = form.agents_rows() as u16 + 1;
    let popup = super::popup_upper(area, area.width.min(64), 4 + rows);
    frame.render_widget(Clear, popup);
    let hints = Line::from(vec![
        Span::styled(" Enter create", Style::default().fg(theme::TEXT)),
        Span::styled(
            "  Tab next  Esc cancel ",
            Style::default().fg(theme::TEXT_MUTED),
        ),
    ]);
    frame.render_widget(
        super::popup_block(
            Line::from(" New chat "),
            hints,
            Style::default().fg(theme::ACCENT),
        ),
        popup,
    );
    if popup.width < 4 || popup.height < 4 {
        return;
    }
    let width = popup.width as usize - 2;
    let field = |at: usize, text: String| {
        let style = if form.focus == at {
            Style::default().fg(theme::ACCENT)
        } else {
            Style::default().fg(theme::TEXT)
        };
        Line::styled(truncate_to_width(&text, width), style)
    };
    let mut lines = vec![
        field(0, format!("{}{}", form.name.prompt, form.name.buffer)),
        field(1, format!("{}{}", form.goal.prompt, form.goal.buffer)),
    ];
    if form.agents_rows() == 3 {
        lines.push(field(2, format!("agent: {}", form.agent_words())));
    }
    let (note, refused) = form.note();
    lines.push(Line::styled(
        truncate_to_width(&note, width),
        Style::default().fg(if refused {
            theme::ERROR
        } else {
            theme::TEXT_MUTED
        }),
    ));
    let body = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
    frame.render_widget(Paragraph::new(lines), body);
    let (text, row) = match form.focus {
        0 => (Some(&form.name), 0),
        1 => (Some(&form.goal), 1),
        _ => (None, 0),
    };
    if let Some(text) = text {
        let col = text.prompt.len() + text.display_cursor();
        frame.set_cursor_position((
            body.x + (col as u16).min(body.width.saturating_sub(1)),
            body.y + row,
        ));
    }
}

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
        // ^ The limit state first, as the web's Usage page: what Claude may still spend comes before what was spent.
        lines.extend(limit_line(&ledger["usage_all"]["limits"], width));
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
    // ^ What the ledger could not read, such as a git that did not answer. ref: docs/tasks.md "Ledger"
    for note in list
        .ledger
        .as_ref()
        .and_then(|ledger| ledger["notes"].as_array())
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        lines.push(Line::styled(
            truncate_to_width(note, width),
            Style::default().fg(theme::WARNING),
        ));
    }
    // ^ A land or a hand waiting for `y`, what runs, or how the last action ended.
    if let Some(id) = list.detail {
        let (words, color) = match (&list.confirm, &list.acting, &list.notice) {
            (Some(what), _, _) => (
                format!("{} #{id}? (y)es  (n)o", confirm_words(what)),
                theme::WARNING,
            ),
            (_, Some(doing), _) => (doing.clone(), theme::TEXT_MUTED),
            (_, _, Some((words, failed))) => (
                words.clone(),
                if *failed {
                    theme::ERROR
                } else {
                    theme::SUCCESS
                },
            ),
            _ => (String::new(), theme::TEXT),
        };
        if !words.is_empty() {
            lines.push(Line::styled(
                truncate_to_width(&words, width),
                Style::default().fg(color),
            ));
        }
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
        // ^ 2/5 of the width, more (up to 11/20) when a status with its wait reason needs it.
        let longest = rows
            .iter()
            .map(|row| row.status.chars().count() + 1)
            .max()
            .unwrap_or(0);
        (width * 2 / 5).max(longest.min(width * 11 / 20))
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
        // ^ A chat's pill counts follow its name, inside the title's width.
        let marks = super::workspace_tree::tray_marks(
            row.counts.running,
            row.counts.needs,
            row.counts.check,
        );
        let marks_width: usize = marks.iter().map(|mark| mark.content.chars().count()).sum();
        let mut spans = vec![Span::styled(
            fit(&row.title, title_width.saturating_sub(1 + marks_width)),
            Style::default().fg(theme::TEXT),
        )];
        spans.extend(marks);
        spans.push(Span::raw(" "));
        if number_width > 0 {
            // ^ A chat has no number: its column stays empty.
            let number = match row.section {
                Section::Chats => String::new(),
                _ => format!("#{}", row.id),
            };
            spans.push(Span::styled(
                fit(&number, number_width),
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }
        spans.push(Span::styled(
            fit(&row.state, STATE_WIDTH) + " ",
            Style::default().fg(state_color(&row.state)),
        ));
        if status_width > 0 {
            spans.push(Span::styled(
                fit(&row.status, status_width),
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
        let date =
            wsx_core::routine::local::local_tm((day["day_ms"].as_u64().unwrap_or(0) / 1000) as i64)
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
                wsx_core::task::status::resume_time(resets, now)
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

/// One task's ledger entry: first what the owner needs at a glance, one labeled row each
/// (state and status, what it waits on, where it goes, its parent, children and the tasks that
/// wait for it, the agent, the checks in one line, To check, the summary and the status
/// sentence), then under "Details" the rest: usage, each check and a closed task's memo.
/// ref: docs/tasks.md "TUI"
fn detail(task: &Value) -> Vec<Line<'static>> {
    let label = |text: &str| {
        Span::styled(
            format!("{text:<10}"),
            Style::default().fg(theme::TEXT_SUBTLE),
        )
    };
    let row = |name: &str, text: String| Line::from(vec![label(name), Span::raw(text)]);
    let numbers = |rows: &[Value], key: &str| {
        rows.iter()
            .map(|row| format!("#{}", row[key].as_u64().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let state = crate::task_list::state_word(task);
    let flow = &task["flow"];
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
            Span::styled(
                task["status"]["label"]
                    .as_str()
                    .map(|status| format!("  {status}"))
                    .unwrap_or_default(),
                Style::default().fg(theme::TEXT_MUTED),
            ),
        ]),
    ];
    // ^ Held, after, step, parent and children: wsx-core `view::flow`, as the web's sheet.
    let waits: Vec<String> = [
        flow["after"]["task"].as_u64().map(|after| {
            format!(
                "#{after} to land, {}",
                flow["after"]["because"].as_str().unwrap_or_default()
            )
        }),
        flow["held"].as_str().map(|held| format!("held: {held}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !waits.is_empty() {
        lines.push(row("Waits on", waits.join(", ")));
    }
    if let Some(into) = flow["into"].as_str() {
        let step = flow["step"]["name"].as_str().map(|name| {
            format!(
                "{name} {}/{}, then ",
                flow["step"]["at"], flow["step"]["of"]
            )
        });
        lines.push(row(
            "Goes",
            format!("{}into {into}", step.unwrap_or_default()),
        ));
    }
    if let Some(children) = flow["children"].as_array().filter(|rows| !rows.is_empty()) {
        let words: Vec<String> = children
            .iter()
            .map(|child| {
                format!(
                    "#{} {}",
                    child["id"].as_u64().unwrap_or_default(),
                    child["state"].as_str().unwrap_or_default()
                )
            })
            .collect();
        lines.push(row("Children", words.join(", ")));
    }
    // ^ The open tasks that wait for this one (`wsx task after`), by number.
    if let Some(waiters) = task["status"]["waited_by"]
        .as_array()
        .filter(|waiters| !waiters.is_empty())
    {
        lines.push(row("Blocks", numbers(waiters, "task")));
    }
    if let Some(agent) = wsx_core::task::view::agent_label(&task["agent"]) {
        lines.push(row("Agent", agent));
    }
    let checks = task["checks"].as_array().cloned().unwrap_or_default();
    let outcome = |check: &Value| check["outcome"].as_str().unwrap_or_default().to_string();
    if !checks.is_empty() {
        let failed: Vec<String> = checks
            .iter()
            .filter(|check| outcome(check) != "passed")
            .map(|check| check["command"].as_str().unwrap_or_default().to_string())
            .collect();
        let mut words = format!("{} passed", checks.len() - failed.len());
        if !failed.is_empty() {
            words += &format!(", {} failed: {}", failed.len(), failed.join(", "));
        }
        lines.push(row("Checks", words));
    }
    if let Some(items) = task["human_verify"].as_u64().filter(|items| *items > 0) {
        lines.push(row(
            "To check",
            format!("{items} item{}", if items == 1 { "" } else { "s" }),
        ));
    }
    // ^ The ledger's summary is its first line, cut (`ledger::open_row`).
    if let Some(summary) = task["summary"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        lines.push(row("Result", summary.to_string()));
    }
    if let Some(status) = task["status"]["sentence"].as_str() {
        lines.push(row("Next", status.to_string()));
    }
    // ^ One scroll deeper: what the rows above sum up.
    let mut more: Vec<Line<'static>> = Vec::new();
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
        more.push(row("Usage", line));
    }
    for check in &checks {
        let outcome = outcome(check);
        let color = if outcome == "passed" {
            theme::SUCCESS
        } else {
            theme::ERROR
        };
        more.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{outcome:<10}"), Style::default().fg(color)),
            Span::raw(check["command"].as_str().unwrap_or_default().to_string()),
        ]));
    }
    if let Some(memo) = task["memo"].as_str().filter(|text| !text.trim().is_empty()) {
        more.push(Line::styled(
            "Memo",
            Style::default().fg(theme::ACCENT).bold(),
        ));
        more.extend(memo.lines().map(|line| Line::raw(line.to_string())));
    }
    if !more.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Details",
            Style::default().fg(theme::TEXT_SUBTLE).bold(),
        ));
        lines.extend(more);
    }
    lines
}

/// The question's verb for an action that asks first.
fn confirm_words(what: &str) -> &'static str {
    match what {
        "pass" => "Pass and land",
        "land" => "Land",
        "handback" => "Hand back",
        _ => "Take over",
    }
}
