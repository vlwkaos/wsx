// Minimal ANSI SGR parser → ratatui Text
// Handles: reset, bold/dim/italic/underline, fg/bg (4-bit, 8-bit, 24-bit)

use ratatui::prelude::*;

pub fn parse(input: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut rest = input;

    while !rest.is_empty() {
        match rest.find('\x1b') {
            Some(0) => {
                if rest.starts_with("\x1b[") {
                    // CSI sequence: \x1b[ ... final_byte (alphabetic)
                    let after = &rest[2..];
                    if let Some(end) = after.find(|c: char| c.is_ascii_alphabetic()) {
                        if after.as_bytes()[end] == b'm' {
                            style = apply_sgr(style, &after[..end]);
                        }
                        rest = &after[end + 1..];
                    } else {
                        rest = &rest[1..];
                    }
                } else if rest.starts_with("\x1b]") {
                    // OSC sequence: \x1b] ... ST  where ST = \x07 or \x1b\\
                    let after = &rest[2..];
                    if let Some(pos) = after.find('\x07') {
                        rest = &after[pos + 1..];
                    } else if let Some(pos) = after.find("\x1b\\") {
                        rest = &after[pos + 2..];
                    } else {
                        rest = "";
                    }
                } else if rest.len() >= 3 && matches!(rest.as_bytes()[1], b'(' | b')' | b'*' | b'+')
                {
                    // Charset designation: \x1b ( X  — skip ESC + designator + charset byte
                    rest = &rest[3..];
                } else {
                    // Other 2-byte sequences (\x1bM, \x1b7, \x1b8, \x1b=, \x1b>, …)
                    rest = if rest.len() >= 2 { &rest[2..] } else { "" };
                }
            }
            Some(pos) => {
                push_text(&rest[..pos], &mut spans, &mut lines, style);
                rest = &rest[pos..];
            }
            None => {
                push_text(rest, &mut spans, &mut lines, style);
                break;
            }
        }
    }

    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn push_text(
    text: &str,
    spans: &mut Vec<Span<'static>>,
    lines: &mut Vec<Line<'static>>,
    style: Style,
) {
    let mut s = text;
    loop {
        match s.find('\n') {
            Some(nl) => {
                if nl > 0 {
                    let chunk = strip_controls(&s[..nl]);
                    if !chunk.is_empty() {
                        spans.push(Span::styled(chunk, style));
                    }
                }
                lines.push(Line::from(std::mem::take(spans)));
                s = &s[nl + 1..];
            }
            None => {
                let chunk = strip_controls(s);
                if !chunk.is_empty() {
                    spans.push(Span::styled(chunk, style));
                }
                break;
            }
        }
    }
}

/// Remove control characters (< 0x20, except \t) and DEL (0x7F) that have no
/// printable representation but would confuse ratatui's cell-width accounting.
fn strip_controls(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\t' || (c >= ' ' && c != '\x7f'))
        .collect()
}

fn apply_sgr(mut style: Style, seq: &str) -> Style {
    let mut params: Vec<u8> = seq.split(';').filter_map(|s| s.parse().ok()).collect();
    if params.is_empty() {
        params.push(0);
    }

    let mut idx = 0;
    while idx < params.len() {
        match params[idx] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            n @ 30..=37 => style = style.fg(ansi_color(n - 30, false)),
            39 => style = style.fg(Color::Reset),
            n @ 40..=47 => style = style.bg(ansi_color(n - 40, false)),
            49 => style = style.bg(Color::Reset),
            n @ 90..=97 => style = style.fg(ansi_color(n - 90, true)),
            n @ 100..=107 => style = style.bg(ansi_color(n - 100, true)),
            38 | 48 => {
                let is_fg = params[idx] == 38;
                if params.get(idx + 1) == Some(&5) && idx + 2 < params.len() {
                    let c = color_256(params[idx + 2]);
                    style = if is_fg { style.fg(c) } else { style.bg(c) };
                    idx += 2;
                } else if params.get(idx + 1) == Some(&2) && idx + 4 < params.len() {
                    let c = Color::Rgb(params[idx + 2], params[idx + 3], params[idx + 4]);
                    style = if is_fg { style.fg(c) } else { style.bg(c) };
                    idx += 4;
                }
            }
            _ => {}
        }
        idx += 1;
    }
    style
}

fn ansi_color(n: u8, bright: bool) -> Color {
    match (n, bright) {
        (0, false) => Color::Black,
        (0, true) => Color::DarkGray,
        (1, false) => Color::Red,
        (1, true) => Color::LightRed,
        (2, false) => Color::Green,
        (2, true) => Color::LightGreen,
        (3, false) => Color::Yellow,
        (3, true) => Color::LightYellow,
        (4, false) => Color::Blue,
        (4, true) => Color::LightBlue,
        (5, false) => Color::Magenta,
        (5, true) => Color::LightMagenta,
        (6, false) => Color::Cyan,
        (6, true) => Color::LightCyan,
        (7, false) => Color::White,
        (7, true) => Color::Gray,
        _ => Color::Reset,
    }
}

fn color_256(n: u8) -> Color {
    match n {
        0..=7 => ansi_color(n, false),
        8..=15 => ansi_color(n - 8, true),
        16..=231 => {
            let n = n - 16;
            let b = (n % 6) * 51;
            let g = ((n / 6) % 6) * 51;
            let r = (n / 36) * 51;
            Color::Rgb(r, g, b)
        }
        232..=255 => {
            let v = 8 + (n - 232) * 10;
            Color::Rgb(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &Text<'static>) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn sgr_colors_preserved() {
        let t = parse("\x1b[31mhello\x1b[0m world");
        assert_eq!(plain(&t), "hello world");
        assert_eq!(t.lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(t.lines[0].spans[1].style.fg, None);
    }

    #[test]
    fn osc_title_sequence_stripped() {
        // Shell PS1 typically sets terminal title: \x1b]0;title\x07
        let t = parse("\x1b]0;user@host: ~/project\x07prompt$ ");
        assert_eq!(plain(&t), "prompt$ ");
    }

    #[test]
    fn osc_with_string_terminator_stripped() {
        let t = parse("\x1b]0;title\x1b\\prompt$ ");
        assert_eq!(plain(&t), "prompt$ ");
    }

    #[test]
    fn charset_designation_stripped() {
        // \x1b(B resets G0 to ASCII — common in terminal output
        let t = parse("hello\x1b(Bworld");
        assert_eq!(plain(&t), "helloworld");
    }

    #[test]
    fn charset_designation_g1_stripped() {
        let t = parse("a\x1b)0b");
        assert_eq!(plain(&t), "ab");
    }

    #[test]
    fn two_byte_escape_stripped() {
        // \x1bM = reverse index
        let t = parse("a\x1bMb");
        assert_eq!(plain(&t), "ab");
    }

    #[test]
    fn control_chars_stripped_from_spans() {
        // \x07 BEL and other controls must not reach ratatui cells
        let t = parse("hello\x07world");
        assert_eq!(plain(&t), "helloworld");
    }

    #[test]
    fn newlines_split_lines() {
        let t = parse("line1\nline2");
        assert_eq!(t.lines.len(), 2);
        assert_eq!(plain(&t), "line1\nline2");
    }

    #[test]
    fn osc_unterminated_consumed_to_end() {
        // No \x07 or ST — consume the rest rather than leaking it as text
        let t = parse("\x1b]0;orphan");
        assert_eq!(plain(&t), "");
    }
}
