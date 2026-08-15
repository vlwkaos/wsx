// tmux capture-pane for preview panel

use super::tmux_cmd;

/// Heuristic classification of a captured pane's tail. Best-effort: derived from
/// the last captured pane text, so it is only fresh for the previewed session.
/// A complement to the authoritative tmux bell, never a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureHint {
    WaitingForConfirm,
    WaitingForInput,
}

/// Sentinel embedded in wsx's status bar adjacent to the version badge.
/// When capture_pane finds this character in a pane's last two rows, it knows wsx is
/// running there and suppresses the preview to prevent nested-render loops.
/// U+214B TURNED AMPERSAND (⅋) — letterlike symbol, 1 display column, never appears
/// in normal terminal output, not stripped by compact_for_agent.
pub const WSX_SENTINEL: &str = "\u{214B}";

pub const NESTED_WSX_MSG: &str = " [wsx — nested instance, preview suppressed]";

/// Sanitize captured pane content.
/// PUA chars (U+E000-U+F8FF, powerline/Nerd Font) render as width-2 in terminals
/// but unicode-width reports 1. The per-cell drift this causes is handled by
/// force_redraw (terminal.clear) on every navigation via update_scroll.
fn sanitize_widths(raw: &str) -> String {
    raw.to_owned()
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

fn tail_lines_for_hint(raw: &str) -> Vec<String> {
    strip_ansi(raw)
        .lines()
        .rev()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_lowercase())
        })
        .take(3)
        .collect()
}

fn looks_like_confirm(line: &str) -> bool {
    let normalized = line.replace(' ', "");
    normalized.contains("[y/n]")
        || normalized.contains("(y/n)")
        || normalized.contains("[y/n]:")
        || normalized.contains("[y/n]?")
        || normalized.contains("[y/n]>")
        || normalized.contains("[y/n])")
        || line.contains("continue?")
        || line.contains("confirm")
        || line.contains("approve")
        || line.contains("press enter")
}

fn looks_like_input_wait(line: &str) -> bool {
    line.contains("input required")
        || line.contains("waiting for user")
        || line.contains("enter value")
        || line.contains("enter input")
}

pub fn detect_capture_hint(raw: &str) -> Option<CaptureHint> {
    for line in tail_lines_for_hint(raw) {
        if looks_like_confirm(&line) {
            return Some(CaptureHint::WaitingForConfirm);
        }
        if looks_like_input_wait(&line) {
            return Some(CaptureHint::WaitingForInput);
        }
    }
    None
}

pub fn capture_pane(session_name: &str) -> Option<String> {
    let raw = capture_pane_window(session_name, None, 0)?;
    // Sentinel fallback: catches renamed / wrapped wsx binaries not detected by is_running_wsx.
    if raw.lines().rev().take(2).any(|l| l.contains(WSX_SENTINEL)) {
        return Some(NESTED_WSX_MSG.to_string());
    }
    Some(raw)
}

/// Capture a window of scrollback.
/// `lines`  — how many lines to include (None = visible viewport only).
/// `offset` — how many lines from the bottom to skip (0 = current bottom).
/// Returns (`-S` value, `-E` value) for the given window parameters.
fn window_args(lines: Option<u32>, offset: u32) -> (Option<String>, Option<String>) {
    let start = lines
        .filter(|&n| n > 0)
        .map(|n| format!("-{}", n as i64 + offset as i64));
    let end = if start.is_some() && offset > 0 {
        Some(format!("-{}", offset))
    } else {
        None
    };
    (start, end)
}

pub fn capture_pane_window(session_name: &str, lines: Option<u32>, offset: u32) -> Option<String> {
    capture_pane_window_with_escape_mode(session_name, lines, offset, true)
}

/// Capture plain visible cells for semantic comparison. Unlike preview capture,
/// this excludes ANSI styling so identical redraws produce identical text.
pub fn capture_pane_plain_window(target: &str, lines: Option<u32>, offset: u32) -> Option<String> {
    capture_pane_window_with_escape_mode(target, lines, offset, false)
}

fn capture_pane_window_with_escape_mode(
    target: &str,
    lines: Option<u32>,
    offset: u32,
    include_escapes: bool,
) -> Option<String> {
    let mut args: Vec<&str> = vec!["capture-pane", "-t", target, "-p"];
    if include_escapes {
        args.push("-e");
    }

    let (start_buf, end_buf) = window_args(lines, offset);

    if let Some(ref s) = start_buf {
        args.push("-S");
        args.push(s);
    }
    if let Some(ref e) = end_buf {
        args.push("-E");
        args.push(e);
    }

    let output = tmux_cmd(&args).output().ok()?;
    if output.status.success() {
        let raw = String::from_utf8_lossy(&output.stdout).into_owned();
        Some(sanitize_widths(&raw))
    } else {
        None
    }
}

/// Strip ANSI escapes, box-drawing/block-element chars (U+2500-U+28FF, U+2B00-U+2BFF),
/// and collapse consecutive blank lines to one.
pub fn compact_for_agent(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut out = String::with_capacity(stripped.len());
    let mut blank_run = 0usize;

    for line in stripped.lines() {
        let line_start = out.len();
        let mut last_non_ws = line_start;
        for c in line.chars() {
            let cp = c as u32;
            // box-drawing, block elements, braille, misc symbols
            if (0x2500..=0x28FF).contains(&cp) || (0x2B00..=0x2BFF).contains(&cp) {
                continue;
            }
            out.push(c);
            if !c.is_whitespace() {
                last_non_ws = out.len();
            }
        }
        out.truncate(last_non_ws);

        if out.len() == line_start {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push('\n');
        }
    }

    out.trim_matches('\n').to_owned()
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

    // ── sentinel detection ───────────────────────────────────────────────────

    #[test]
    fn compact_preserves_sentinel_char() {
        // U+214B is not in the stripped box-drawing ranges; must pass through
        let input = format!("hello{WSX_SENTINEL}world");
        assert!(compact_for_agent(&input).contains(WSX_SENTINEL));
    }

    // ── trim_capture (regression) ────────────────────────────────────────────

    #[test]
    fn ansi_only_trailing_lines_are_trimmed() {
        let raw = "hello\n\x1b[0m\x1b[32m\n\x1b[0m";
        assert_eq!(trim_capture(raw), "hello");
    }

    #[test]
    fn visible_content_after_ansi_preserved() {
        let raw = "\x1b[32mgreen\x1b[0m text";
        assert_eq!(trim_capture(raw), raw);
    }

    #[test]
    fn pua_chars_passed_through() {
        let input = "foo\u{E0B0}bar";
        assert_eq!(sanitize_widths(input), "foo\u{E0B0}bar");
    }

    #[test]
    fn trim_capture_all_blank_returns_empty() {
        assert_eq!(trim_capture("\n\n\n"), "");
        assert_eq!(trim_capture(""), "");
    }

    #[test]
    fn trim_capture_preserves_leading_blank_lines() {
        assert_eq!(trim_capture("\n\nhello"), "\n\nhello");
    }

    // ── window_args ──────────────────────────────────────────────────────────

    #[test]
    fn window_args_no_lines_no_flags() {
        assert_eq!(window_args(None, 0), (None, None));
    }

    #[test]
    fn window_args_lines_only_sets_start() {
        assert_eq!(window_args(Some(100), 0), (Some("-100".into()), None));
    }

    #[test]
    fn window_args_lines_and_offset() {
        // start = -(50+10) = -60, end = -10
        assert_eq!(
            window_args(Some(50), 10),
            (Some("-60".into()), Some("-10".into()))
        );
    }

    #[test]
    fn window_args_zero_lines_treated_as_no_window() {
        assert_eq!(window_args(Some(0), 0), (None, None));
        // offset without lines produces no -E either
        assert_eq!(window_args(Some(0), 5), (None, None));
    }

    // ── compact_for_agent ────────────────────────────────────────────────────

    #[test]
    fn compact_strips_ansi_sequences() {
        assert_eq!(compact_for_agent("\x1b[32mhello\x1b[0m"), "hello");
    }

    #[test]
    fn compact_strips_box_drawing_chars() {
        // trailing space after │ is removed by per-line trim
        assert_eq!(compact_for_agent("│ hello │"), " hello");
        assert_eq!(compact_for_agent("────────"), "");
    }

    #[test]
    fn compact_strips_block_elements() {
        // U+2588 █ (full block), U+2580 ▀ (upper half block)
        assert_eq!(compact_for_agent("█progress▀"), "progress");
    }

    #[test]
    fn compact_collapses_multiple_blank_lines_to_one() {
        assert_eq!(compact_for_agent("a\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn compact_trims_leading_and_trailing_blank_lines() {
        assert_eq!(compact_for_agent("\n\nhello\n\n"), "hello");
    }

    #[test]
    fn compact_trims_trailing_whitespace_per_line() {
        assert_eq!(compact_for_agent("hello   \nworld  "), "hello\nworld");
    }

    #[test]
    fn compact_preserves_leading_indent() {
        assert_eq!(
            compact_for_agent("  indented\n    deeper"),
            "  indented\n    deeper"
        );
    }

    #[test]
    fn compact_box_drawing_only_line_becomes_blank() {
        // separator line of box chars → blank → collapsed with surrounding blanks
        assert_eq!(
            compact_for_agent("above\n────────\nbelow"),
            "above\n\nbelow"
        );
    }

    #[test]
    fn capture_hint_detects_yes_no_prompt() {
        assert_eq!(
            detect_capture_hint("build finished\nContinue? [y/n]"),
            Some(CaptureHint::WaitingForConfirm)
        );
    }

    #[test]
    fn capture_hint_detects_input_wait() {
        assert_eq!(
            detect_capture_hint("agent paused\nwaiting for user"),
            Some(CaptureHint::WaitingForInput)
        );
    }

    #[test]
    fn compact_empty_input_returns_empty() {
        assert_eq!(compact_for_agent(""), "");
    }

    #[test]
    fn compact_plain_text_unchanged() {
        let input = "hello\nworld";
        assert_eq!(compact_for_agent(input), input);
    }
}
