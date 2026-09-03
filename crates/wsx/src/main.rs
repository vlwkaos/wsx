// wsx — workspace manager TUI
// Manages project worktrees and wsxd-owned terminals via ratatui.

mod action;
mod app;
mod cli;
mod event;
mod repo_scan;
mod session_state;
mod terminal_surface;
#[cfg(test)]
mod terminal_surface_tests;
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
        Some(
            cli::Command::Routine { .. }
                | cli::Command::Runtime { .. }
                | cli::Command::Daemon { .. }
                | cli::Command::Agent {
                    subcommand: cli::AgentCmd::Install { .. },
                }
        )
    ) {
        return cli::run(args.command.expect("matched Some"));
    }
    match args.command {
        Some(cmd) => {
            // ^ [[wsx Architecture]] CLI mutations require a ready adjacent daemon.
            let availability =
                wsx_core::runtime::ensure_available().context("wsxd is unavailable")?;
            match availability {
                wsx_core::runtime::Availability::Current => {}
                wsx_core::runtime::Availability::RecoveredFromBackup => eprintln!(
                    "wsx: wsxd recovered a corrupt primary state from its last-known-good backup"
                ),
                wsx_core::runtime::Availability::LegacyCompatible => {
                    eprintln!("wsx: a newer wsxd will be used after the current daemon stops")
                }
                wsx_core::runtime::Availability::ReplacementDeferred { live_runtimes } => {
                    eprintln!(
                        "wsx: wsxd update deferred while {live_runtimes} terminal runtime(s) remain open"
                    );
                }
            }
            cli::run(cmd)
        }
        // The TUI renders its config-backed shell before background runtime discovery completes.
        None => run_tui(args.mobile),
    }
}

fn run_tui(mobile: bool) -> Result<()> {
    // Restore terminal on panic so the shell isn't left in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stderr(),
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
        );
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    let mut terminal = tui::init().context("terminal init failed")?;
    let mut app = App::new(mobile)?;
    let result = app.run(&mut terminal);
    // Restore terminal before flush_cache so any eprintln is visible.
    let _ = tui::restore(&mut terminal);
    app.flush_cache();
    result
}
