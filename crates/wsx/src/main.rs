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

use anyhow::{bail, Context, Result};
use app::App;
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Args::parse();
    reject_nested_tui(
        args.command.as_ref(),
        std::env::var_os(wsx_core::runtime::WSX_PANE_ID_ENV).as_deref(),
    )?;
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

// ^ [[wsx Architecture]] wsxd marks managed PTYs; only interactive TUI startup
// is rejected because explicit CLI commands are valid from managed terminals.
fn reject_nested_tui(
    command: Option<&cli::Command>,
    pane_marker: Option<&std::ffi::OsStr>,
) -> Result<()> {
    if command.is_none() && pane_marker.is_some_and(|marker| !marker.is_empty()) {
        bail!(
            "cannot start a nested wsx TUI inside a wsx-managed terminal; use the outer wsx workspace instead"
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn managed_terminal_rejects_plain_and_mobile_tui_startup() {
        for args in [
            cli::Args::try_parse_from(["wsx"]).unwrap(),
            cli::Args::try_parse_from(["wsx", "--mobile"]).unwrap(),
        ] {
            let error =
                reject_nested_tui(args.command.as_ref(), Some(OsStr::new("42"))).unwrap_err();
            assert_eq!(
                error.to_string(),
                "cannot start a nested wsx TUI inside a wsx-managed terminal; use the outer wsx workspace instead"
            );
        }
    }

    #[test]
    fn managed_terminal_allows_explicit_subcommands() {
        let args = cli::Args::try_parse_from(["wsx", "runtime", "status"]).unwrap();
        assert!(reject_nested_tui(args.command.as_ref(), Some(OsStr::new("42"))).is_ok());
    }

    #[test]
    fn unmanaged_or_empty_marker_allows_tui_startup() {
        assert!(reject_nested_tui(None, None).is_ok());
        assert!(reject_nested_tui(None, Some(OsStr::new(""))).is_ok());
    }
}
