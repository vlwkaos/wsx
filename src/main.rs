// wsx — workspace manager TUI
// Manages git worktrees + tmux sessions via ratatui interface.

mod action;
mod app;
mod cache;
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
        eprintln!("wsx requires tmux — https://github.com/tmux/tmux/wiki/Installing");
        std::process::exit(1);
    }

    let mut terminal = tui::init().context("terminal init failed")?;
    let mut app = App::new()?;
    let result = app.run(&mut terminal);
    // Restore terminal before flush_cache so any eprintln is visible.
    let _ = tui::restore(&mut terminal);
    app.flush_cache();
    result
}
