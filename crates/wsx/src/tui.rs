// Terminal init/restore wrapper
// ref: ratatui docs — https://ratatui.rs/concepts/backends/

use anyhow::Result;
use crossterm::{
    cursor::SetCursorStyle,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use wsx_core::runtime::Cursor;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Draw with synchronized output to prevent terminal from rendering partial frames.
/// Terminal clears are reserved for resize; preview clears flush captured terminal glyphs.
pub fn draw_sync<F>(
    terminal: &mut Tui,
    clear_terminal: bool,
    clear_preview: bool,
    cursor: Option<Cursor>,
    mut render: F,
) -> Result<()>
where
    F: FnMut(&mut ratatui::Frame, bool),
{
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    if clear_terminal {
        terminal.clear()?;
        // terminal.clear() resets the back buffer but NOT the current (front) buffer,
        // so the next diff skips cells that match the stale front buffer even though
        // the screen was cleared. Drawing an empty frame first flushes the front buffer
        // to blank state, ensuring the real draw below writes every cell unconditionally.
        terminal.draw(|_| {})?;
    }
    if clear_preview {
        terminal.draw(|frame| render(frame, true))?;
    }
    terminal.draw(|frame| render(frame, false))?;
    execute!(
        terminal.backend_mut(),
        cursor_style(cursor),
        EndSynchronizedUpdate
    )?;
    Ok(())
}

pub fn restore(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Temporarily restore the shell terminal while running an interactive external command.
pub fn with_raw_mode_disabled<F, R>(terminal: &mut Tui, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    let result = f();
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    terminal.clear()?;
    result
}

fn cursor_style(cursor: Option<Cursor>) -> SetCursorStyle {
    let Some(cursor) = cursor.filter(|cursor| cursor.visible) else {
        return SetCursorStyle::DefaultUserShape;
    };
    match (cursor.shape, cursor.blinking) {
        (0 | 3, true) => SetCursorStyle::BlinkingBlock,
        (0 | 3, false) => SetCursorStyle::SteadyBlock,
        (1, true) => SetCursorStyle::BlinkingUnderScore,
        (1, false) => SetCursorStyle::SteadyUnderScore,
        (2, true) => SetCursorStyle::BlinkingBar,
        (2, false) => SetCursorStyle::SteadyBar,
        _ => SetCursorStyle::DefaultUserShape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ghostty_cursor_shapes_and_blinking() {
        let cursor = |shape, blinking| Cursor {
            x: 0,
            y: 0,
            visible: true,
            blinking,
            shape,
        };
        assert_eq!(
            cursor_style(Some(cursor(0, false))).to_string(),
            "\u{1b}[2 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(1, true))).to_string(),
            "\u{1b}[3 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(2, false))).to_string(),
            "\u{1b}[6 q"
        );
        assert_eq!(
            cursor_style(Some(cursor(3, true))).to_string(),
            "\u{1b}[1 q"
        );
    }
}
