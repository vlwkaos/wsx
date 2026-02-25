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
                    let after = &rest[2..];
                    if let Some(end) = after.find(|c: char| c.is_ascii_alphabetic()) {
                        if after.as_bytes()[end] == b'm' {
                            style = apply_sgr(style, &after[..end]);
                        }
                        rest = &after[end + 1..];
                    } else {
                        rest = &rest[1..];
                    }
                } else {
                    rest = &rest[1..];
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

fn push_text(text: &str, spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>, style: Style) {
    let mut s = text;
    loop {
        match s.find('\n') {
            Some(nl) => {
                if nl > 0 {
                    spans.push(Span::styled(s[..nl].to_owned(), style));
                }
                lines.push(Line::from(std::mem::take(spans)));
                s = &s[nl + 1..];
            }
            None => {
                if !s.is_empty() {
                    spans.push(Span::styled(s.to_owned(), style));
                }
                break;
            }
        }
    }
}

fn apply_sgr(mut style: Style, seq: &str) -> Style {
    let mut params: Vec<u8> = seq.split(';')
        .filter_map(|s| s.parse().ok())
        .collect();
    if params.is_empty() {
        params.push(0);
    }

    let mut idx = 0;
    while idx < params.len() {
        match params[idx] {
            0  => style = Style::default(),
            1  => style = style.add_modifier(Modifier::BOLD),
            2  => style = style.add_modifier(Modifier::DIM),
            3  => style = style.add_modifier(Modifier::ITALIC),
            4  => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            n @ 30..=37  => style = style.fg(ansi_color(n - 30, false)),
            39           => style = style.fg(Color::Reset),
            n @ 40..=47  => style = style.bg(ansi_color(n - 40, false)),
            49           => style = style.bg(Color::Reset),
            n @ 90..=97  => style = style.fg(ansi_color(n - 90, true)),
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
        (0, false) => Color::Black,     (0, true) => Color::DarkGray,
        (1, false) => Color::Red,       (1, true) => Color::LightRed,
        (2, false) => Color::Green,     (2, true) => Color::LightGreen,
        (3, false) => Color::Yellow,    (3, true) => Color::LightYellow,
        (4, false) => Color::Blue,      (4, true) => Color::LightBlue,
        (5, false) => Color::Magenta,   (5, true) => Color::LightMagenta,
        (6, false) => Color::Cyan,      (6, true) => Color::LightCyan,
        (7, false) => Color::White,     (7, true) => Color::Gray,
        _ => Color::Reset,
    }
}

fn color_256(n: u8) -> Color {
    match n {
        0..=7   => ansi_color(n, false),
        8..=15  => ansi_color(n - 8, true),
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
