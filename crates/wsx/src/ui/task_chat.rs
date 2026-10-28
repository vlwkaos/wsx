//! The manager chat: the conversation with the newest at the bottom, the owner's open
//! items under it and the message line last. ref: docs/tasks.md "TUI"
use super::{tasks::muted, theme, workspace_tree::truncate_to_width};
use crate::task_chat::{Focus, Item, ManagerChat, Row, Who};
use ratatui::{
    layout::Rect,
    prelude::Stylize,
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
    Frame,
};

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
    let lines = chat_lines(&chat.rows, width);
    let max_scroll = (lines.len() as u16).saturating_sub(chat_height);
    chat.max_scroll.set(max_scroll);
    let scroll = chat.scroll.min(max_scroll);
    let first = (lines.len() as u16).saturating_sub(chat_height + scroll);
    let body = if lines.is_empty() {
        vec![muted(if !chat.loaded {
            "Loading the chat"
        } else {
            "No messages yet. Press i to write to the manager"
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
        Focus::Fail(_) => vec!["Enter: fail it".into(), "Esc: cancel".into()],
        Focus::Confirm { .. } => vec!["(y)es".into(), "(n)o".into()],
        Focus::Chat => {
            // ^ The footer drops hints from the end: scrolling goes first, then back.
            let mut hints = vec!["(i)message".into()];
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
            hints.extend(["Esc: back".into(), "(j/k)scroll".into()]);
            hints
        }
    }
}

/// `Manager <project>` on the left, the To check and approval counts on the right.
fn heading(chat: &ManagerChat, width: usize) -> Line<'static> {
    let checks = chat
        .items
        .iter()
        .filter(|item| matches!(item, Item::Check(_)))
        .count();
    let approvals = chat.items.len() - checks;
    let counts = format!(
        "{checks} to check  {approvals} approval{}",
        if approvals == 1 { "" } else { "s" }
    );
    let project = truncate_to_width(
        &format!(" {}", chat.project),
        width.saturating_sub("Manager".len() + counts.len() + 2),
    );
    let used = "Manager".len() + Line::from(project.as_str()).width() + counts.len();
    Line::from(vec![
        Span::styled("Manager", Style::default().fg(theme::ACCENT).bold()),
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
        (format!("Manager is working: {doing}"), theme::WORKING)
    } else if chat.loaded && !chat.running {
        (
            "The manager is not running; a message starts it".into(),
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
pub fn chat_lines(rows: &[Row], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        match row {
            Row::Said { who, unix_ms, text } => {
                if index > 0 {
                    lines.push(Line::raw(""));
                }
                let (name, color) = match who {
                    Who::Owner => ("You".to_string(), theme::ACCENT),
                    Who::Manager => ("Manager".to_string(), theme::DONE),
                    Who::Peer(project) => (project.clone(), theme::IDLE),
                };
                lines.push(Line::from(vec![
                    Span::styled(name, Style::default().fg(color).bold()),
                    Span::styled(
                        format!("  {}", clock(*unix_ms)),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    ),
                ]));
                for paragraph in text.lines() {
                    lines.extend(
                        wrap(paragraph, width.saturating_sub(2).max(1))
                            .into_iter()
                            .map(|part| Line::raw(format!("  {part}"))),
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
    match &chat.focus {
        Focus::Compose => typed(&chat.composer),
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
            (muted("> press i to write to the manager"), None)
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
