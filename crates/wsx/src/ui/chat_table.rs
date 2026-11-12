//! A GFM table in a chat message, drawn as aligned columns. The web's markdown shows a table
//! right after a list item too; this reads it the same way: a header row and a delimiter row of
//! as many cells start it, whatever line is above, and the first line without `|` ends it.
//! ref: docs/design/list-tables.md
use super::{
    text::{width_of, wrap_cells, Cell},
    theme,
};
use ratatui::style::{Modifier, Style};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Align {
    Left,
    Center,
    Right,
}

/// A line of cells with its style.
pub(super) type Shown = (Vec<Cell>, Style);

/// The table at the top of some lines: its rows (the header first, each with a cell per
/// column) and how many lines it took.
pub(super) struct Table {
    pub aligns: Vec<Align>,
    pub rows: Vec<Vec<String>>,
    pub len: usize,
}

/// Between two columns.
const GAP: &str = " │ ";
/// A column shrinks to this many cells before the table stacks.
const MIN_COLUMN: usize = 6;

/// The cells of a row: the outer pipes dropped, `\|` and a pipe inside backticks kept.
fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let (mut cells, mut cell, mut ticks, mut escaped) = (Vec::new(), String::new(), false, false);
    for char in line.chars() {
        match char {
            '|' if !escaped && !ticks => cells.push(std::mem::take(&mut cell)),
            '|' if escaped => {
                cell.pop();
                cell.push('|');
            }
            _ => {
                if char == '`' {
                    ticks = !ticks;
                }
                cell.push(char);
            }
        }
        escaped = char == '\\' && !escaped;
    }
    // ^ A closing pipe leaves no cell after it.
    if !cell.trim().is_empty() || cells.is_empty() {
        cells.push(cell);
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn align_of(cell: &str) -> Option<Align> {
    let dashes = cell.trim_matches(':');
    (!dashes.is_empty() && dashes.chars().all(|char| char == '-')).then(|| {
        match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        }
    })
}

/// The table that starts at `lines[0]`, none when it does not start one.
pub(super) fn read(lines: &[&str]) -> Option<Table> {
    let (head, rule) = (lines.first()?, lines.get(1)?);
    if !head.contains('|') || !rule.contains('|') {
        return None;
    }
    let head = split_row(head);
    let aligns: Vec<Align> = split_row(rule)
        .iter()
        .map(|cell| align_of(cell))
        .collect::<Option<_>>()?;
    if aligns.len() != head.len() {
        return None;
    }
    let mut rows = vec![head];
    for line in &lines[2..] {
        if !line.contains('|') || line.trim().is_empty() {
            break;
        }
        rows.push(split_row(line));
    }
    for row in &mut rows {
        row.resize(aligns.len(), String::new());
    }
    Some(Table {
        len: rows.len() + 1,
        aligns,
        rows,
    })
}

fn chars(text: &str) -> Vec<Cell> {
    text.chars().map(|char| (char, None)).collect()
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// `rows` (the header first) as lines of `width` cells at most, each with its style: columns
/// padded to their widest cell, shrunk and wrapped when that is too wide, and one record per row
/// (`header: value`) when even the shrunk columns do not fit.
pub(super) fn lines(rows: &[Vec<Vec<Cell>>], aligns: &[Align], width: usize) -> Vec<Shown> {
    let columns = aligns.len();
    let gaps = GAP.chars().count() * (columns - 1);
    let natural: Vec<usize> = (0..columns)
        .map(|at| {
            rows.iter()
                .map(|row| width_of(&row[at]))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let mut widths = natural.clone();
    if widths.iter().sum::<usize>() + gaps > width {
        let floor: Vec<usize> = natural.iter().map(|wide| (*wide).min(MIN_COLUMN)).collect();
        if floor.iter().sum::<usize>() + gaps > width {
            return stacked(rows, width);
        }
        while widths.iter().sum::<usize>() + gaps > width {
            // ^ The widest column above its floor gives one cell: the sum is over `width`, which
            // the floors fit, so one is above.
            let widest = (0..columns)
                .filter(|&at| widths[at] > floor[at])
                .max_by_key(|&at| widths[at])
                .expect("a column above its floor");
            widths[widest] -= 1;
        }
    }
    let wrapped: Vec<Vec<Vec<Vec<Cell>>>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(&widths)
                .map(|(cell, wide)| wrap_cells(cell, *wide))
                .collect()
        })
        .collect();
    let tall = wrapped[1..]
        .iter()
        .any(|row| row.iter().any(|cell| cell.len() > 1));
    let rule = {
        let mut line = Vec::new();
        for (at, wide) in widths.iter().enumerate() {
            if at > 0 {
                line.extend(chars("─┼─"));
            }
            line.extend(chars(&"─".repeat(*wide)));
        }
        (line, Style::default().fg(theme::TEXT_SUBTLE))
    };
    let mut out = Vec::new();
    for (index, row) in wrapped.iter().enumerate() {
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        for nth in 0..height {
            let mut line = Vec::new();
            for (at, cell) in row.iter().enumerate() {
                if at > 0 {
                    line.extend(chars(GAP));
                }
                let piece = cell.get(nth).map_or(&[][..], Vec::as_slice);
                let room = widths[at] - width_of(piece);
                let (before, after) = match aligns[at] {
                    Align::Left => (0, room),
                    Align::Right => (room, 0),
                    Align::Center => (room / 2, room - room / 2),
                };
                line.extend(chars(&" ".repeat(before)));
                line.extend_from_slice(piece);
                // ^ The last column ends at its words, not at its width.
                if at + 1 < columns {
                    line.extend(chars(&" ".repeat(after)));
                }
            }
            out.push((line, if index == 0 { bold() } else { Style::default() }));
        }
        // ^ A row that wraps is set off from the next, else two rows of lines blur.
        if index == 0 || (tall && index + 1 < wrapped.len()) {
            out.push(rule.clone());
        }
    }
    out
}

/// `cell` wrapped to `width`, each line in `style`.
fn wrapped_in(cell: &[Cell], width: usize, style: Style) -> impl Iterator<Item = Shown> {
    wrap_cells(cell, width)
        .into_iter()
        .map(move |line| (line, style))
}

/// Each row as a record: its first cell in bold, then a line `header: value` for each other cell.
fn stacked(rows: &[Vec<Vec<Cell>>], width: usize) -> Vec<Shown> {
    let mut out = Vec::new();
    if rows.len() == 1 {
        for cell in &rows[0] {
            out.extend(wrapped_in(cell, width, bold()));
        }
        return out;
    }
    for (index, row) in rows[1..].iter().enumerate() {
        if index > 0 {
            out.push((Vec::new(), Style::default()));
        }
        for (at, cell) in row.iter().enumerate() {
            if at == 0 {
                out.extend(wrapped_in(cell, width, bold()));
                continue;
            }
            let mut text = rows[0][at].clone();
            text.extend(chars(": "));
            text.extend_from_slice(cell);
            out.extend(wrapped_in(&text, width, Style::default()));
        }
    }
    out
}
