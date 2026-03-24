// wsx — workspace manager TUI
// Manages git worktrees + tmux sessions via ratatui interface.

mod action;
mod app;
mod cache;
mod cli;
mod config;
mod event;
mod git;
mod hooks;
mod model;
mod ops;
mod tmux;
mod tui;
mod ui;
mod update;

use anyhow::{Context, Result};
use app::App;
use clap::Parser;

fn main() -> Result<()> {
    // Require tmux
    if !tmux::session::is_available() {
        eprintln!("wsx requires tmux — https://github.com/tmux/tmux/wiki/Installing");
        std::process::exit(1);
    }

    let args = cli::Args::parse();
    match args.command {
        Some(cmd) => cli::run(cmd),
        None => run_tui(),
    }
}

fn run_tui() -> Result<()> {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    let mut terminal = tui::init().context("terminal init failed")?;
    let mut app = App::new()?;
    let result = app.run(&mut terminal);
    // Restore terminal before flush_cache so any eprintln is visible.
    let _ = tui::restore(&mut terminal);
    app.flush_cache();
    result
}
