//! The manager chat: the conversation with the newest at the bottom, the owner's open
//! items under it and the message line last. ref: docs/tasks.md "TUI"
use super::{tasks::muted, theme, workspace_tree::truncate_to_width};
use crate::task_chat::{Focus, Item, ManagerChat, Row, Who};
use ratatui::{
    layout::Rect,
    prelude::Stylize,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};
use std::ops::Range;

/// Below this width a To check row drops where to look.
const WHERE_MIN_WIDTH: usize = 100;

pub fn render(frame: &mut Frame, area: Rect, chat: &ManagerChat) {
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
    let top = vec![heading(chat, width), status(chat, width), Line::raw("")];
    // ^ The items take at most a third of the height; the chat keeps the rest.
    let (items, item_selected) = item_lines(chat, width);
    let room = inner.height.saturating_sub(top.len() as u16 + 1);
    let items_height = (items.len() as u16).min(room / 3);
    let chat_height = room - items_height;
    chat.area.set((width, chat_height));
    let mut lines = chat_lines(&chat.rows, chat.who(), width);
    let matches = find_in(&lines, chat.find.value());
    // ^ Every match reversed, the current one also in a color (docs/tasks.md "TUI").
    let current = chat.found.min(matches.len().saturating_sub(1));
    for (index, (line, range)) in matches.into_iter().enumerate() {
        let mut style = Style::default().add_modifier(Modifier::REVERSED);
        if index == current {
            style = style.fg(theme::IDLE).bold();
        }
        lines[line] = mark(std::mem::take(&mut lines[line]), range, style);
    }
    let max_scroll = (lines.len() as u16).saturating_sub(chat_height);
    chat.max_scroll.set(max_scroll);
    let scroll = chat.scroll.min(max_scroll);
    let first = (lines.len() as u16).saturating_sub(chat_height + scroll);
    let body = if lines.is_empty() {
        vec![muted(if !chat.loaded {
            "Loading the chat"
        } else {
            "No messages yet. Press i to write a message"
        })]
    } else {
        lines
    };
    let row = |y: u16, height: u16| Rect {
        y: inner.y + y,
        height,
        ..inner
    };
    frame.render_widget(Paragraph::new(top), row(0, 3));
    // ^ Newest at the bottom: a short chat sits on the items, not under the heading.
    let shown = (body.len() as u16).min(chat_height);
    frame.render_widget(
        Paragraph::new(body).scroll((first, 0)),
        row(3 + chat_height - shown, shown),
    );
    let item_offset = item_selected
        .map(|line| (line + 2).saturating_sub(items_height as usize))
        .unwrap_or(0) as u16;
    frame.render_widget(
        Paragraph::new(items).scroll((item_offset, 0)),
        row(3 + chat_height, items_height),
    );
    let composer_y = inner.y + inner.height - 1;
    let (line, cursor) = composer(chat, width);
    frame.render_widget(Paragraph::new(line), row(inner.height - 1, 1));
    if let Some(column) = cursor {
        frame.set_cursor_position((inner.x + column.min(inner.width - 1), composer_y));
    }
}

/// The footer's hints: only the keys the focus and the selected item take.
pub fn hints(chat: &ManagerChat) -> Vec<String> {
    match &chat.focus {
        Focus::Compose => vec!["Enter: send".into(), "Esc: done".into()],
        Focus::Find => vec!["Enter: keep".into(), "Esc: clear".into()],
        Focus::Fail(_) => vec!["Enter: fail it".into(), "Esc: cancel".into()],
        Focus::Confirm { .. } => vec!["(y)es".into(), "(n)o".into()],
        Focus::Chat => {
            // ^ The footer drops hints from the end: scrolling goes first, then back.
            if !chat.find.value().is_empty() {
                return vec![
                    "(n)previous".into(),
                    "(N)next".into(),
                    "Esc: clear".into(),
                    "(/)find".into(),
                ];
            }
            let mut hints = vec!["(i)message".into(), "(/)find".into()];
            match chat.selected_item() {
                Some(Item::Check(_)) => {
                    hints.extend(["(p)ass".into(), "(s)kip".into(), "(f)ail".into()])
                }
                Some(Item::Approval { .. }) => {
                    hints.extend(["(y)approve".into(), "(n)deny".into()])
                }
                None => {}
            }
            if chat.items.len() > 1 {
                hints.push("(tab)next".into());
            }
            hints.extend(["(S)hell".into(), "Esc: back".into(), "(j/k)scroll".into()]);
            hints
        }
    }
}

/// `Manager <project>` (or the role's name) on the left, the To check and approval counts on
/// the right; a role's chat has none.
fn heading(chat: &ManagerChat, width: usize) -> Line<'static> {
    let checks = chat
        .items
        .iter()
        .filter(|item| matches!(item, Item::Check(_)))
        .count();
    let approvals = chat.items.len() - checks;
    let counts = match chat.role {
        Some(_) => String::new(),
        None => format!(
            "{checks} to check  {approvals} approval{}",
            if approvals == 1 { "" } else { "s" }
        ),
    };
    let who = truncate_to_width(chat.who(), width / 2);
    // ^ The agent in use now, after the project (docs/tasks.md "Agents").
    let agent = chat
        .agent
        .as_ref()
        .map_or(String::new(), |agent| format!(" · {agent}"));
    let project = truncate_to_width(
        &format!(" {}{agent}", chat.project),
        width.saturating_sub(Line::from(who.as_str()).width() + counts.len() + 2),
    );
    let used =
        Line::from(who.as_str()).width() + Line::from(project.as_str()).width() + counts.len();
    Line::from(vec![
        Span::styled(who, Style::default().fg(theme::ACCENT).bold()),
        Span::styled(project, Style::default().fg(theme::TEXT)),
        Span::raw(" ".repeat(width.saturating_sub(used).max(2))),
        Span::styled(counts, Style::default().fg(theme::TEXT_MUTED)),
    ])
}

/// A read failure, else what the last action said, else what runs, else whether the
/// manager works or runs at all.
fn status(chat: &ManagerChat, width: usize) -> Line<'static> {
    let (text, color) = if let Some(error) = &chat.error {
        (format!("Could not read the chat: {error}"), theme::ERROR)
    } else if let Some(doing) = &chat.acting {
        (format!("{doing}…"), theme::WORKING)
    } else if let Some((words, failed)) = &chat.notice {
        let color = if *failed {
            theme::ERROR
        } else {
            theme::TEXT_MUTED
        };
        (words.clone(), color)
    } else if chat.busy {
        let doing = chat.activity.as_deref().unwrap_or("thinking");
        (
            format!("{} is working: {doing}", chat.who()),
            theme::WORKING,
        )
    } else if chat.loaded && !chat.running {
        (
            format!("{} is not running; a message starts it", chat.who()),
            theme::TEXT_MUTED,
        )
    } else {
        (String::new(), theme::TEXT_MUTED)
    };
    Line::styled(truncate_to_width(&text, width), Style::default().fg(color))
}

/// "14:05" in this machine's zone.
fn clock(unix_ms: u64) -> String {
    crate::task::limit::local_time(unix_ms / 1000).map_or_else(String::new, |tm| {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    })
}

/// Each message as its author and time, then its words wrapped under them; each event or
/// quiet reply as one muted line.
pub fn chat_lines(rows: &[Row], other: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        match row {
            Row::Said {
                who,
                unix_ms,
                text,
                asks,
            } => {
                if index > 0 {
                    lines.push(Line::raw(""));
                }
                let (name, color) = match who {
                    Who::Owner => ("You".to_string(), theme::ACCENT),
                    Who::Manager => (other.to_string(), theme::DONE),
                    Who::Peer(project) => (project.clone(), theme::IDLE),
                };
                // ^ A reply that asks is set off like a thin border: a bar in the accent. Its name keeps the speaker's color, as the accent is the owner's.
                let heading = vec![
                    Span::styled(name, Style::default().fg(color).bold()),
                    Span::styled(
                        format!("  {}", clock(*unix_ms)),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    ),
                ];
                lines.push(Line::from(heading));
                for paragraph in text.lines() {
                    lines.extend(
                        wrap(paragraph, width.saturating_sub(2).max(1))
                            .into_iter()
                            .map(|part| {
                                if *asks {
                                    Line::from(vec![
                                        Span::styled("│ ", Style::default().fg(theme::ACCENT)),
                                        Span::raw(part),
                                    ])
                                } else {
                                    Line::raw(format!("  {part}"))
                                }
                            }),
                    );
                }
            }
            Row::Line(text) => {
                // ^ Lines after a message start apart from it, at the author's column.
                if matches!(
                    index.checked_sub(1).map(|at| &rows[at]),
                    Some(Row::Said { .. })
                ) {
                    lines.push(Line::raw(""));
                }
                lines.push(muted(&truncate_to_width(&text.replace('\n', " "), width)))
            }
        }
    }
    lines
}

/// Where `query` shows in `lines`, case-insensitive: each match's line and byte range in
/// the line's text, oldest first. An empty query finds nothing.
pub fn find_in(lines: &[Line], query: &str) -> Vec<(usize, Range<usize>)> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        // ^ A lower-case letter can take other bytes than its capital: map each byte of the
        // lower-case text back to the byte of its character in the line.
        let (mut lower, mut at) = (String::new(), Vec::new());
        for (byte, char) in text.char_indices() {
            for low in char.to_lowercase() {
                at.extend(std::iter::repeat_n(byte, low.len_utf8()));
                lower.push(low);
            }
        }
        at.push(text.len());
        let mut from = 0;
        while let Some(start) = lower[from..].find(&query) {
            let (start, end) = (from + start, from + start + query.len());
            // ^ The end is the next character's start, so a matched character stays whole.
            let stop = at[end..].iter().find(|&&byte| byte > at[end - 1]).copied();
            found.push((index, at[start]..stop.unwrap_or(text.len())));
            from = end;
        }
    }
    found
}

/// `line` with the bytes `range` of its text in `style` over their own.
fn mark(line: Line<'static>, range: Range<usize>, style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for span in line.spans {
        let (start, end) = (offset, offset + span.content.len());
        offset = end;
        let (from, to) = (range.start.clamp(start, end), range.end.clamp(start, end));
        if from == to {
            spans.push(span);
            continue;
        }
        let text = span.content.as_ref();
        for (part, lit) in [
            (&text[..from - start], false),
            (&text[from - start..to - start], true),
            (&text[to - start..], false),
        ] {
            if !part.is_empty() {
                let own = if lit {
                    span.style.patch(style)
                } else {
                    span.style
                };
                spans.push(Span::styled(part.to_string(), own));
            }
        }
    }
    Line { spans, ..line }
}

/// `text` broken at spaces into lines of at most `width` cells; a longer word is split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let cells = |text: &str| Line::from(text).width();
    let (mut lines, mut line, mut used) = (Vec::new(), String::new(), 0);
    for word in text.split(' ') {
        let size = cells(word);
        if used + usize::from(used > 0) + size <= width {
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += size;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for char in word.chars() {
            let size = cells(char.encode_utf8(&mut [0; 4]));
            if used > 0 && used + size > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(char);
            used += size;
        }
    }
    lines.push(line);
    lines
}

/// The owner's open items: a check as its id, task and words, then when it passes (and
/// where to look on a wide screen); an approval as one line. Also the selected item's
/// first line.
fn item_lines(chat: &ManagerChat, width: usize) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines = Vec::new();
    if chat.items.is_empty() {
        return (lines, None);
    }
    // ^ A blank line sets the items apart from the chat above them.
    lines.push(Line::raw(""));
    let mut selected = None;
    for (index, item) in chat.items.iter().enumerate() {
        let start = lines.len();
        let style = if index == chat.selected {
            selected = Some(start);
            theme::selected_row(false)
        } else {
            Style::default()
        };
        let marker = if index == chat.selected { "▸ " } else { "  " };
        match item {
            Item::Check(item) => {
                lines.push(
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(
                            format!("{:<5}#{:<4}", item.id, item.task),
                            Style::default().fg(theme::TEXT_MUTED),
                        ),
                        Span::styled(
                            truncate_to_width(&item.check, width.saturating_sub(12)),
                            Style::default().fg(theme::TEXT),
                        ),
                    ])
                    .patch_style(style),
                );
                let mut detail = format!("pass if: {}", item.pass_if);
                if width >= WHERE_MIN_WIDTH && !item.place.is_empty() {
                    detail = format!("{detail}  where: {}", item.place);
                }
                lines.push(
                    Line::styled(
                        truncate_to_width(&format!("{:12}{detail}", ""), width),
                        Style::default().fg(theme::TEXT_MUTED),
                    )
                    .patch_style(style),
                );
            }
            Item::Approval { id, label } => lines.push(
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{id:<10}"), Style::default().fg(theme::TEXT_MUTED)),
                    Span::styled(label.clone(), Style::default().fg(theme::BLOCKED)),
                ])
                .patch_style(style),
            ),
        }
    }
    // ^ The selected item's rows are lit across the width, as a selected task row is.
    if let Some(start) = selected {
        let rows = match chat.items.get(chat.selected) {
            Some(Item::Check(_)) => 2,
            _ => 1,
        };
        for line in &mut lines[start..start + rows] {
            let pad = width.saturating_sub(line.width());
            line.spans.push(Span::raw(" ".repeat(pad)));
        }
    }
    (lines, selected)
}

/// The bottom line by focus, and the cursor's column when it takes typing.
fn composer(chat: &ManagerChat, width: usize) -> (Line<'static>, Option<u16>) {
    let typed = |state: &crate::ui::input::InputState| {
        let cells = |text: &str| Line::from(text).width();
        let mut before = format!("{}{}", state.prompt, &state.buffer[..state.cursor]);
        // ^ A long line shows its end, where the cursor is.
        while cells(&before) >= width && !before.is_empty() {
            before.remove(0);
        }
        let column = cells(&before) as u16;
        let shown = before + &state.buffer[state.cursor..];
        (Line::raw(truncate_to_width(&shown, width)), Some(column))
    };
    // ^ The find's count after its text: `2/5`, from the oldest match (docs/tasks.md "TUI").
    let count = || {
        let found = chat.matches().len();
        match found {
            0 => " 0/0".to_string(),
            _ => format!(" {}/{found}", chat.found.min(found - 1) + 1),
        }
    };
    match &chat.focus {
        Focus::Compose => typed(&chat.composer),
        Focus::Find => {
            let (line, cursor) = typed(&chat.find);
            let mut spans = line.spans;
            if !chat.find.value().is_empty() {
                spans.push(Span::styled(
                    count(),
                    Style::default().fg(theme::TEXT_MUTED),
                ));
            }
            (Line::from(spans), cursor)
        }
        Focus::Chat if !chat.find.value().is_empty() => (
            Line::from(vec![
                Span::raw(truncate_to_width(
                    &format!("/{}", chat.find.value()),
                    width / 2,
                )),
                Span::styled(count(), Style::default().fg(theme::TEXT_MUTED)),
            ]),
            None,
        ),
        Focus::Fail(_) => typed(&chat.note),
        Focus::Confirm { label, approve, .. } => {
            let words = if *approve {
                label.clone()
            } else {
                label.replacen("Approve", "Deny", 1)
            };
            (
                Line::styled(
                    truncate_to_width(&format!("{words}? y/n"), width),
                    Style::default().fg(theme::WARNING).bold(),
                ),
                None,
            )
        }
        Focus::Chat if chat.composer.value().is_empty() => {
            (muted("> press i to write a message"), None)
        }
        Focus::Chat => (
            muted(&truncate_to_width(
                &format!("> {}", chat.composer.value()),
                width,
            )),
            None,
        ),
    }
}
