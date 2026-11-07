//! Text cut to terminal cells: shared by every view. Widths are display cells, not chars.
use ratatui::{
    style::Style,
    text::{Line, Span},
};

type Cell = (char, Option<usize>);

/// Display cells `text` takes.
pub(super) fn cells(text: &str) -> usize {
    Line::from(text).width()
}

pub(super) fn truncate_to_width(value: &str, max_width: usize) -> String {
    if cells(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let span = Span::raw(value);
    let mut truncated = String::new();
    let mut used = 0;
    for grapheme in span.styled_graphemes(Style::default()) {
        let width = cells(grapheme.symbol);
        if used + width > max_width - 1 {
            break;
        }
        truncated.push_str(grapheme.symbol);
        used += width;
    }
    truncated.push('…');
    truncated
}

/// `text` cut to `width` cells with an ellipsis, then padded with spaces to it.
pub(super) fn fit(text: &str, width: usize) -> String {
    let cut = truncate_to_width(text, width);
    let pad = width.saturating_sub(cells(&cut));
    cut + &" ".repeat(pad)
}

fn size_of(char: char) -> usize {
    cells(char.encode_utf8(&mut [0; 4]))
}

/// Appends `word` to `line`, starting a new line in `lines` whenever the next char would
/// pass `width`. `used` is the cells in `line`.
fn split_cells(
    lines: &mut Vec<Vec<Cell>>,
    line: &mut Vec<Cell>,
    used: &mut usize,
    word: &[Cell],
    width: usize,
) {
    for cell in word {
        let size = size_of(cell.0);
        if *used > 0 && *used + size > width {
            lines.push(std::mem::take(line));
            *used = 0;
        }
        line.push(*cell);
        *used += size;
    }
}

/// `cells` broken at spaces into lines of at most `width` cells; a longer word is split.
pub(super) fn wrap_cells(cells: &[Cell], width: usize) -> Vec<Vec<Cell>> {
    let (mut lines, mut line, mut used) = (Vec::new(), Vec::new(), 0);
    for word in cells.split(|(char, _)| *char == ' ') {
        let size: usize = word.iter().map(|(char, _)| size_of(*char)).sum();
        if used + usize::from(used > 0) + size <= width {
            if used > 0 {
                // ^ The space inside a link belongs to it, so its underline runs on.
                let inside = match (line.last(), word.first()) {
                    (Some((_, before)), Some((_, after))) if before == after => *after,
                    _ => None,
                };
                line.push((' ', inside));
                used += 1;
            }
            line.extend_from_slice(word);
            used += size;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        split_cells(&mut lines, &mut line, &mut used, word, width);
    }
    lines.push(line);
    lines
}

/// `text` cut into pieces of at most `width` cells, spaces kept.
pub(super) fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let cells: Vec<Cell> = text.chars().map(|char| (char, None)).collect();
    let (mut lines, mut line, mut used) = (Vec::new(), Vec::new(), 0);
    split_cells(&mut lines, &mut line, &mut used, &cells, width);
    lines.push(line);
    lines
        .into_iter()
        .map(|line| line.into_iter().map(|(char, _)| char).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_complete_graphemes_with_linear_prefix_selection() {
        assert_eq!(truncate_to_width("👩‍💻abcd", 3), "👩‍💻…");
        assert_eq!(truncate_to_width("e\u{301}abcd", 2), "e\u{301}…");
        assert_eq!(truncate_to_width("中文abcd", 3), "中…");
        for width in 0..32 {
            let text = truncate_to_width(&"👩‍💻e\u{301}中文".repeat(100), width);
            assert!(Line::from(text.as_str()).width() <= width);
            assert!(!text.ends_with("\u{200d}…"));
        }
    }
}
