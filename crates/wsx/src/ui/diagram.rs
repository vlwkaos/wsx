//! A mermaid block drawn as a Unicode diagram for the chat and the overview (`mermaid-text`),
//! as Pi's TUI draws one: only when it fits the width, else the caller shows the source.
//! ref: docs/tasks.md "TUI"
use super::text::cells;

/// `source` drawn in at most `width` cells, one string per row; none when the diagram type is
/// not drawn, the source does not parse, or the drawing is wider than `width`.
pub(super) fn draw(source: &str, width: usize) -> Option<Vec<String>> {
    if width == 0 {
        return None;
    }
    // ^ The crate is a layout engine run on a message's text, and the message stays in the log:
    // a panic on the UI thread would restore the terminal (tui.rs panic hook) at every open of
    // the chat. On a worker thread the hook keeps the modes and the drawing shows as its source.
    let drawn = std::thread::scope(|scope| {
        scope
            .spawn(|| mermaid_text::render_with_width(source, Some(width)))
            .join()
    });
    let rows: Vec<String> = drawn.ok()?.ok()?.lines().map(str::to_string).collect();
    (!rows.is_empty() && rows.iter().all(|row| cells(row) <= width)).then_some(rows)
}
