//! Shared application boundary for CLI and TUI clients.
//! ref: README.md#architecture

use anyhow::Result;
use asched_core::routine::ipc::{Action, Request, Response};
use asched_core::routine::RoutineClient;
use asched_core::{Project, RegistryStore};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

const TUI_REFRESH_TIMEOUT: Duration = Duration::from_millis(250);
const TUI_ACTION_TIMEOUT: Duration = Duration::from_secs(10);

pub fn registry() -> Result<RegistryStore> {
    Ok(RegistryStore::new(RegistryStore::default_root()?))
}

pub fn resolve_project(name: &str) -> Result<Project> {
    let mut projects = registry()?.select(&[name.to_string()], None)?;
    projects
        .pop()
        .ok_or_else(|| anyhow::anyhow!("project '{name}' not found"))
}

pub fn client() -> Result<RoutineClient> {
    Ok(RoutineClient::new(RegistryStore::default_root()?))
}

// ^ Shared CLI/TUI host boundary; lifecycle changes also touch asched-core routine client + daemon.
pub fn send(working_dir: &Path, action: Action) -> Result<Response> {
    let request = Request::new(working_dir.to_path_buf(), action);
    Ok(client()?.request_with_start(&request, daemon_command()?)?)
}

pub fn send_tui_observation(working_dir: &Path, action: Action) -> Result<Response> {
    send_tui(working_dir, action, TUI_REFRESH_TIMEOUT)
}

pub fn send_tui_action(working_dir: &Path, action: Action) -> Result<Response> {
    send_tui(working_dir, action, TUI_ACTION_TIMEOUT)
}

fn send_tui(working_dir: &Path, action: Action, request_timeout: Duration) -> Result<Response> {
    let request = Request::new(working_dir.to_path_buf(), action);
    let client = client()?
        .with_startup_timeout(TUI_REFRESH_TIMEOUT)
        .with_request_timeout(request_timeout);
    Ok(client.request_with_start(&request, daemon_command()?)?)
}

pub fn terminal_safe(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

pub fn daemon_command() -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("daemon-serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[cfg(test)]
#[path = "service_contract_tests.rs"]
mod service_contract_tests;
