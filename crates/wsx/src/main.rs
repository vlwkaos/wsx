// wsx — workspace manager TUI
// Manages git worktrees + tmux sessions via ratatui interface.

mod action;
mod app;
mod cli;
mod event;
mod repo_scan;
mod session_state;
mod tui;
mod ui;
mod update;

use anyhow::{Context, Result};
use app::App;
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Args::parse();
    if matches!(
        args.command,
        Some(cli::Command::Routine { .. }) | Some(cli::Command::RoutineDaemonServe)
    ) {
        return cli::run(args.command.expect("matched Some"));
    }
    if !wsx_core::tmux::session::is_available() {
        eprintln!("wsx requires tmux — https://github.com/tmux/tmux/wiki/Installing");
        std::process::exit(1);
    }

    match args.command {
        Some(cmd) => cli::run(cmd),
        None => run_tui(args.mobile),
    }
}

fn run_tui(mobile: bool) -> Result<()> {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stderr(), crossterm::terminal::LeaveAlternateScreen,);
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    wsx_core::tmux::session::apply_server_defaults();
    let mut terminal = tui::init().context("terminal init failed")?;
    let mut app = App::new(mobile)?;
    let result = app.run(&mut terminal);
    // Restore terminal before flush_cache so any eprintln is visible.
    let _ = tui::restore(&mut terminal);
    app.flush_cache();
    result
}
