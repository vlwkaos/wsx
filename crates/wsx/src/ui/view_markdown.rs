//! Markdown to terminal lines for the pane view. Pure: no I/O, one width in, lines out.
//! ^ docs/terminal-view.md: supported markdown and the renderer-plugin substitution.
use ratatui::prelude::*;
use wsx_core::runtime::{view_lines, PluginTone, ViewBlock, ViewLine};

use super::{plugin_sidecar::tone_color, theme, workspace_tree::truncate_to_width};

fn cells(value: &str) -> usize {
    Line::from(value).width()
}

#[derive(Clone)]
struct Cell {
    symbol: String,
    style: Style,
    width: usize,
}

fn push_cells(out: &mut Vec<Cell>, text: &str, style: Style) {
    let text = text.replace('\t', "    ");
    for grapheme in Span::raw(text.as_str()).styled_graphemes(Style::default()) {
        out.push(Cell {
            symbol: grapheme.symbol.to_string(),
            style,
            width: cells(grapheme.symbol),
        });
    }
}

fn find_close(chars: &[char], from: usize, delimiter: &[char]) -> Option<usize> {
    let mut index = from;
    while index + delimiter.len() <= chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index..index + delimiter.len()] == *delimiter
            && index > from
            && !chars[index - 1].is_whitespace()
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Inline emphasis, code spans and links. Unmatched delimiters stay literal.
fn inline(text: &str, base: Style, out: &mut Vec<Cell>) {
    let chars = text.chars().collect::<Vec<_>>();
    let mut plain = String::new();
    let mut index = 0;
    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                push_cells(out, &plain, base);
                plain.clear();
            }
        };
    }
    while index < chars.len() {
        let character = chars[index];
        let rest = &chars[index..];
        if character == '\\' && index + 1 < chars.len() && chars[index + 1].is_ascii_punctuation() {
            plain.push(chars[index + 1]);
            index += 2;
            continue;
        }
        if character == '`' {
            let run = rest.iter().take_while(|value| **value == '`').count();
            let close = (index + run..chars.len()).find(|start| {
                chars[*start..]
                    .iter()
                    .take_while(|value| **value == '`')
                    .count()
                    == run
                    && (*start == 0 || chars[*start - 1] != '`')
            });
            if let Some(close) = close {
                flush!();
                let code = chars[index + run..close].iter().collect::<String>();
                push_cells(out, code.trim(), base.patch(theme::view_code()));
                index = close + run;
                continue;
            }
        }
        let emphasis: Option<(&[char], Style)> = if rest.starts_with(&['*', '*']) {
            Some((&['*', '*'], base.bold()))
        } else if rest.starts_with(&['_', '_']) {
            Some((&['_', '_'], base.bold()))
        } else if rest.starts_with(&['~', '~']) {
            Some((&['~', '~'], base.crossed_out()))
        } else if character == '*' {
            Some((&['*'], base.italic()))
        } else if character == '_' && (index == 0 || !chars[index - 1].is_alphanumeric()) {
            Some((&['_'], base.italic()))
        } else {
            None
        };
        if let Some((delimiter, style)) = emphasis {
            let start = index + delimiter.len();
            let opens = chars.get(start).is_some_and(|next| !next.is_whitespace());
            if let Some(close) = opens
                .then(|| find_close(&chars, start, delimiter))
                .flatten()
            {
                flush!();
                let inner = chars[start..close].iter().collect::<String>();
                inline(&inner, style, out);
                index = close + delimiter.len();
                continue;
            }
        }
        if character == '[' || (character == '!' && chars.get(index + 1) == Some(&'[')) {
            let label_start = index + if character == '!' { 2 } else { 1 };
            let label_end = (label_start..chars.len()).find(|at| chars[*at] == ']');
            if let Some(label_end) = label_end.filter(|end| chars.get(end + 1) == Some(&'(')) {
                if let Some(url_end) = (label_end + 2..chars.len()).find(|at| chars[*at] == ')') {
                    flush!();
                    let label = chars[label_start..label_end].iter().collect::<String>();
                    inline(&label, base.underlined(), out);
                    index = url_end + 1;
                    continue;
                }
            }
        }
        plain.push(character);
        index += 1;
    }
    flush!();
}

fn to_line(cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for cell in cells {
        match spans.last_mut() {
            Some(last) if last.style == cell.style => last.content.to_mut().push_str(&cell.symbol),
            _ => spans.push(Span::styled(cell.symbol.clone(), cell.style)),
        }
    }
    Line::from(spans)
}

/// Greedy wrap on display cells. Breaks at spaces, between wide cells, or mid-word
/// when one word is wider than the line.
fn wrap(cells: &[Cell], width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        while start < cells.len() && cells[start].symbol == " " {
            start += 1;
        }
        let mut used = 0;
        let mut end = start;
        let mut last_break = None;
        while end < cells.len() && used + cells[end].width <= width {
            used += cells[end].width;
            if cells[end].symbol == " " {
                last_break = Some(end);
            } else if cells[end].width > 1 {
                last_break = Some(end + 1);
            }
            end += 1;
        }
        if end == start {
            if start >= cells.len() {
                break;
            }
            // One cell wider than the line: emit it alone instead of looping forever.
            end = start + 1;
        } else if end < cells.len() && cells[end].symbol != " " {
            if let Some(at) = last_break.filter(|at| *at > start) {
                end = at;
            }
        }
        let mut line = cells[start..end].to_vec();
        while line.last().is_some_and(|cell| cell.symbol == " ") {
            line.pop();
        }
        lines.push(line);
        start = end;
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn wrapped(
    text: &str,
    style: Style,
    first: &[Cell],
    rest: &[Cell],
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let mut content = Vec::new();
    inline(text, style, &mut content);
    let indent = first.iter().map(|cell| cell.width).sum::<usize>();
    for (index, line) in wrap(&content, width.saturating_sub(indent))
        .into_iter()
        .enumerate()
    {
        let mut cells = if index == 0 {
            first.to_vec()
        } else {
            rest.to_vec()
        };
        cells.extend(line);
        out.push(to_line(&cells));
    }
}

fn prefix(text: &str, style: Style) -> Vec<Cell> {
    let mut cells = Vec::new();
    push_cells(&mut cells, text, style);
    cells
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    let rest = &trimmed[level..];
    ((1..=6).contains(&level) && (rest.is_empty() || rest.starts_with(' ')))
        .then(|| (level, rest.trim().trim_end_matches('#').trim_end()))
}

fn rule(line: &str) -> bool {
    let compact = line
        .chars()
        .filter(|value| !value.is_whitespace())
        .collect::<String>();
    compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|marker| compact.chars().all(|value| value == *marker))
}

struct ListItem<'a> {
    indent: usize,
    marker: String,
    text: &'a str,
}

fn list_item(line: &str) -> Option<ListItem<'_>> {
    let indent = line.len() - line.trim_start().len();
    let rest = line.trim_start();
    let (marker, text) = if let Some(text) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|marker| rest.strip_prefix(marker))
    {
        ("•".to_string(), text)
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let after = rest.get(digits..)?;
        if !(1..=9).contains(&digits) || !(after.starts_with(". ") || after.starts_with(") ")) {
            return None;
        }
        (format!("{}.", &rest[..digits]), &after[2..])
    };
    let (marker, text) = match text.get(..4) {
        Some("[ ] ") => ("☐".to_string(), &text[4..]),
        Some("[x] ") | Some("[X] ") => ("☑".to_string(), &text[4..]),
        _ => (marker, text),
    };
    Some(ListItem {
        indent: (indent / 2).min(4),
        marker,
        text: text.trim(),
    })
}

fn preformatted(line: &str) -> bool {
    line.starts_with("    ")
        || line.starts_with('\t')
        || line
            .chars()
            .any(|value| ('\u{2500}'..='\u{259f}').contains(&value))
}

fn table_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed
        .strip_suffix('|')
        .filter(|rest| !rest.ends_with('\\'))
        .unwrap_or(trimmed);
    let mut cells = vec![String::new()];
    let mut chars = trimmed.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cells.last_mut().expect("one cell").push('|');
            }
            '|' => cells.push(String::new()),
            other => cells.last_mut().expect("one cell").push(other),
        }
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

fn table_separator(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    table_cells(line)
        .iter()
        .map(|cell| {
            let dashes = cell.trim_matches(':');
            (!dashes.is_empty() && dashes.bytes().all(|byte| byte == b'-')).then(|| {
                match (cell.starts_with(':'), cell.ends_with(':')) {
                    (true, true) => Align::Center,
                    (false, true) => Align::Right,
                    _ => Align::Left,
                }
            })
        })
        .collect()
}

const COLUMN_GAP: &str = " │ ";

fn table(rows: &[Vec<String>], aligns: &[Align], width: usize, out: &mut Vec<Line<'static>>) {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let parsed = rows
        .iter()
        .enumerate()
        .map(|(row, cells)| {
            (0..columns)
                .map(|column| {
                    let style = if row == 0 {
                        Style::default().fg(theme::TEXT).bold()
                    } else {
                        Style::default().fg(theme::TEXT)
                    };
                    let mut content = Vec::new();
                    inline(
                        cells.get(column).map_or("", String::as_str),
                        style,
                        &mut content,
                    );
                    content
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut widths = (0..columns)
        .map(|column| {
            parsed
                .iter()
                .map(|row| row[column].iter().map(|cell| cell.width).sum::<usize>())
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect::<Vec<_>>();
    let gaps = cells(COLUMN_GAP) * (columns - 1);
    // Shrink the widest column first; cells wrap inside their column.
    while widths.iter().sum::<usize>() + gaps > width {
        let (widest, value) = widths
            .iter()
            .copied()
            .enumerate()
            .max_by_key(|(_, value)| *value)
            .expect("at least one column");
        if value <= 3 {
            break;
        }
        widths[widest] = value - 1;
    }
    let divider = Style::default().fg(theme::DIVIDER);
    for (row_index, row) in parsed.iter().enumerate() {
        let wrapped = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| wrap(cell, *width))
            .collect::<Vec<_>>();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            let mut cells_out = Vec::new();
            for (column, lines) in wrapped.iter().enumerate() {
                if column > 0 {
                    push_cells(&mut cells_out, COLUMN_GAP, divider);
                }
                let content = lines.get(line).cloned().unwrap_or_default();
                let used = content.iter().map(|cell| cell.width).sum::<usize>();
                let pad = widths[column].saturating_sub(used);
                let left = match aligns.get(column).copied().unwrap_or(Align::Left) {
                    Align::Left => 0,
                    Align::Center => pad / 2,
                    Align::Right => pad,
                };
                push_cells(&mut cells_out, &" ".repeat(left), Style::default());
                cells_out.extend(content);
                if column + 1 < columns {
                    push_cells(&mut cells_out, &" ".repeat(pad - left), Style::default());
                }
            }
            out.push(clip(to_line(&cells_out), width));
        }
        if row_index == 0 {
            let rule = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("─┼─");
            out.push(clip(Line::styled(rule, divider), width));
        }
    }
}

/// Cut a line at `width` cells with a marker; styles are kept per span.
fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut remaining = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in line.spans {
        let span_width = span.width();
        if span_width <= remaining {
            remaining -= span_width;
            spans.push(span);
            continue;
        }
        let kept = truncate_to_width(&span.content, remaining + 1);
        spans.push(Span::styled(kept, span.style));
        return Line::from(spans);
    }
    spans.push(Span::styled("…", Style::default().fg(theme::TEXT_SUBTLE)));
    Line::from(spans)
}

fn block_start(line: &str, next: Option<&str>) -> bool {
    line.trim().is_empty()
        || heading(line).is_some()
        || rule(line)
        || list_item(line).is_some()
        || line.trim_start().starts_with('>')
        || preformatted(line)
        || (line.contains('|') && next.and_then(table_separator).is_some())
}

pub fn render(markdown: &str, blocks: &[ViewBlock], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let source = view_lines(markdown);
    let text = |index: usize| match source.get(index) {
        Some(ViewLine::Text(line)) => Some(*line),
        _ => None,
    };
    let body = Style::default().fg(theme::TEXT);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut index = 0;
    while index < source.len() {
        match source[index] {
            ViewLine::FenceOpen { index: block, .. } => {
                index += 1;
                let mut code = Vec::new();
                while let Some(ViewLine::Code(text)) = source.get(index) {
                    code.push(*text);
                    index += 1;
                }
                if matches!(source.get(index), Some(ViewLine::FenceClose)) {
                    index += 1;
                }
                match blocks.iter().find(|rendered| rendered.index == block) {
                    Some(rendered) => {
                        for line in &rendered.lines {
                            let spans = line
                                .iter()
                                .map(|span| {
                                    Span::styled(
                                        span.text.clone(),
                                        Style::default().fg(tone_color(span.tone)),
                                    )
                                })
                                .collect::<Vec<_>>();
                            out.push(clip(Line::from(spans), width));
                        }
                        if let Some(note) = &rendered.note {
                            out.push(clip(
                                Line::styled(
                                    note.clone(),
                                    Style::default().fg(tone_color(PluginTone::Muted)),
                                ),
                                width,
                            ));
                        }
                    }
                    None => {
                        for line in code {
                            out.push(clip(
                                Line::from(vec![
                                    Span::styled("│ ", Style::default().fg(theme::DIVIDER)),
                                    Span::styled(line.replace('\t', "    "), theme::view_code()),
                                ]),
                                width,
                            ));
                        }
                    }
                }
            }
            ViewLine::Code(_) | ViewLine::FenceClose => index += 1,
            ViewLine::Text(line) => {
                if line.trim().is_empty() {
                    if out.last().is_some_and(|last| last.width() > 0) {
                        out.push(Line::default());
                    }
                    index += 1;
                } else if let Some((level, title)) = heading(line) {
                    let style = match level {
                        1 => Style::default().fg(theme::ACCENT).bold(),
                        2 => Style::default().fg(theme::TEXT).bold(),
                        _ => Style::default().fg(theme::TEXT_MUTED).bold(),
                    };
                    wrapped(title, style, &[], &[], width, &mut out);
                    index += 1;
                } else if rule(line) {
                    out.push(Line::styled(
                        "─".repeat(width),
                        Style::default().fg(theme::DIVIDER),
                    ));
                    index += 1;
                } else if let Some(aligns) = line
                    .contains('|')
                    .then(|| text(index + 1).and_then(table_separator))
                    .flatten()
                {
                    let mut rows = vec![table_cells(line)];
                    index += 2;
                    while let Some(row) = text(index).filter(|row| row.contains('|')) {
                        rows.push(table_cells(row));
                        index += 1;
                    }
                    table(&rows, &aligns, width, &mut out);
                } else if let Some(item) = list_item(line) {
                    let mut content = item.text.to_string();
                    index += 1;
                    while let Some(more) =
                        text(index).filter(|more| !block_start(more, text(index + 1)))
                    {
                        content.push(' ');
                        content.push_str(more.trim());
                        index += 1;
                    }
                    let first = prefix(
                        &format!("{}{} ", "  ".repeat(item.indent), item.marker),
                        Style::default().fg(theme::TEXT_MUTED),
                    );
                    let hang = first.iter().map(|cell| cell.width).sum::<usize>();
                    let rest = prefix(&" ".repeat(hang), Style::default());
                    wrapped(&content, body, &first, &rest, width, &mut out);
                } else if let Some(quote) = line.trim_start().strip_prefix('>') {
                    let bar = prefix("▎ ", Style::default().fg(theme::TEXT_SUBTLE));
                    wrapped(
                        quote.trim(),
                        Style::default().fg(theme::TEXT_MUTED),
                        &bar,
                        &bar,
                        width,
                        &mut out,
                    );
                    index += 1;
                } else if preformatted(line) {
                    out.push(clip(Line::styled(line.replace('\t', "    "), body), width));
                    index += 1;
                } else {
                    let mut content = line.trim().to_string();
                    index += 1;
                    while let Some(more) =
                        text(index).filter(|more| !block_start(more, text(index + 1)))
                    {
                        content.push(' ');
                        content.push_str(more.trim());
                        index += 1;
                    }
                    wrapped(&content, body, &[], &[], width, &mut out);
                }
            }
        }
    }
    while out.last().is_some_and(|last| last.width() == 0) {
        out.pop();
    }
    out
}
