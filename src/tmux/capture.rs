// tmux capture-pane for preview panel

use super::tmux_cmd;

/// Replace Private Use Area characters (U+E000–U+F8FF) with space.
/// Powerline/Nerd Font symbols: terminals render them as width 2 but
/// unicode-width reports 1, causing ratatui cell-shift and stale bleed.
fn sanitize_widths(raw: &str) -> String {
    raw.chars()
        .map(|c| if ('\u{E000}'..='\u{F8FF}').contains(&c) { ' ' } else { c })
        .collect()
}

/// Strip ANSI escape sequences, returning plain text for whitespace checks.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        match rest.find('\x1b') {
            Some(0) => {
                if rest.starts_with("\x1b[") {
                    let after = &rest[2..];
                    if let Some(end) = after.find(|c: char| c.is_ascii_alphabetic()) {
                        rest = &after[end + 1..];
                    } else {
                        rest = &rest[1..];
                    }
                } else if rest.starts_with("\x1b]") {
                    let after = &rest[2..];
                    if let Some(pos) = after.find('\x07') {
                        rest = &after[pos + 1..];
                    } else if let Some(pos) = after.find("\x1b\\") {
                        rest = &after[pos + 2..];
                    } else {
                        rest = &rest[1..];
                    }
                } else {
                    out.push('\x1b');
                    rest = &rest[1..];
                }
            }
            Some(n) => {
                out.push_str(&rest[..n]);
                rest = &rest[n..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

pub fn capture_pane(session_name: &str) -> Option<String> {
    let output = tmux_cmd(&["capture-pane", "-t", session_name, "-p", "-e"])
        .output()
        .ok()?;
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout).into_owned();
        Some(sanitize_widths(&raw))
    } else {
        None
    }
}

pub fn trim_capture(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let last_nonempty = lines.iter().rposition(|l| !strip_ansi(l).trim().is_empty());
    match last_nonempty {
        Some(i) => lines[..=i].join("\n"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_only_trailing_lines_are_trimmed() {
        let raw = "hello\n\x1b[0m\x1b[32m\n\x1b[0m";
        assert_eq!(trim_capture(raw), "hello");
    }

    #[test]
    fn pua_chars_replaced_with_space() {
        let input = "foo\u{E0B0}bar";
        let result = sanitize_widths(input);
        assert_eq!(result, "foo bar");
    }

    #[test]
    fn visible_content_after_ansi_preserved() {
        let raw = "\x1b[32mgreen\x1b[0m text";
        assert_eq!(trim_capture(raw), raw);
    }
}
