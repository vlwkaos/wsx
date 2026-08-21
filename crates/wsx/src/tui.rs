// Terminal init/restore wrapper
// ref: ratatui docs — https://ratatui.rs/concepts/backends/

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    Ok(())
}

pub fn restore(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Run a closure with raw mode disabled (for Herdr attach and external commands).
pub fn with_raw_mode_disabled<F, R>(terminal: &mut Tui, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    let result = f();
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()?;
    result
}
