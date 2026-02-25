// wsx — workspace manager TUI
// Manages git worktrees + tmux sessions via ratatui interface.

mod action;
mod app;
mod config;
mod event;
mod git;
mod hooks;
mod model;
mod ops;
mod tmux;
mod tui;
mod ui;

use anyhow::{Context, Result};
use app::App;

fn main() -> Result<()> {
    // Require tmux
    if !tmux::session::is_available() {
        eprintln!("wsx requires tmux. Install it with: brew install tmux");
        std::process::exit(1);
    }

    let mut terminal = tui::init().context("terminal init failed")?;

    let result = run(&mut terminal);

    // Always restore terminal, even on error
    let _ = tui::restore(&mut terminal);

    result
}

fn run(terminal: &mut tui::Tui) -> Result<()> {
    let mut app = App::new()?;
    app.run(terminal)
}
