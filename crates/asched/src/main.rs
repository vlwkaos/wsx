mod cli;
mod service;
mod tui;

use anyhow::Result;
use clap::Parser;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {}", service::terminal_safe(&format!("{error:#}")));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    match cli.command {
        Some(command) => cli::run(command),
        None => tui::run(),
    }
}
