// wsx — workspace manager TUI
// Manages project worktrees and wsxd-owned terminals via ratatui.

mod action;
mod app;
mod cli;
mod event;
mod repo_scan;
mod review;
mod search;
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
    // ^ Observation/report handlers must bypass this eager bootstrap AND use
    // a non-starting Client. See scripts/test-agent-context.py for the lifecycle oracle.
    if matches!(
        args.command,
        Some(
            cli::Command::Routine { .. }
                | cli::Command::Runtime { .. }
                | cli::Command::Daemon { .. }
                | cli::Command::Agent {
                    subcommand: cli::AgentCmd::Install { .. }
                        | cli::AgentCmd::Context { .. }
                        | cli::AgentCmd::Report { .. }
                        | cli::AgentCmd::ExchangeReceipt { .. },
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
            if let Some((_, message)) = app::runtime_availability_notice(&availability) {
                eprintln!("wsx: {message}");
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
    let (mut session, mut terminal) = tui::init().context("terminal init failed")?;
    let mut app = match App::new(mobile) {
        Ok(app) => app,
        Err(error) => {
            let _ = session.restore();
            session.record_failure("app_start_failed", &error);
            return Err(error);
        }
    };
    let result = app.run(&mut terminal, &session);
    // Restore before diagnostic I/O and cache flush; Drop retries a failed restoration.
    let restored = session.restore();
    if let Err(error) = &result {
        session.record_failure("app_run_failed", error);
    }
    let persisted = app.flush_cache();
    if let Err(error) = &persisted {
        session.record_failure("cache_flush_failed", error);
    }
    result?;
    restored?;
    persisted
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
