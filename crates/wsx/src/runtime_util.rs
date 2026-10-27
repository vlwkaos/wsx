//! Shared handling of daemon replies and JSON output for the CLI and the TUI.
//! ^ The error texts here are user-visible and tested by scripts; keep them exact.

use anyhow::Result;
use serde::Serialize;
use wsx_core::runtime::{ApiError, Response, Snapshot};

/// The daemon's refusal as text: `code: message`.
pub(crate) fn api_error_text(error: &ApiError) -> String {
    format!("{}: {}", error.code, error.message)
}

/// The daemon's refusal as an error: `code: message`.
pub(crate) fn api_error(error: ApiError) -> anyhow::Error {
    anyhow::anyhow!(api_error_text(&error))
}

/// Turns a reply the caller did not expect into an error. A daemon `Error`
/// reply keeps its `code: message` text; any other reply gets `unexpected`.
pub(crate) fn unexpected_response<T>(response: Response, unexpected: &str) -> Result<T> {
    match response {
        Response::Error(error) => Err(api_error(error)),
        _ => Err(anyhow::anyhow!("{unexpected}")),
    }
}

/// Accepts an `Ack` reply and returns its revision.
pub(crate) fn expect_ack(response: Response, unexpected: &str) -> Result<u64> {
    match response {
        Response::Ack { revision } => Ok(revision),
        other => unexpected_response(other, unexpected),
    }
}

/// Accepts a `Snapshot` reply.
pub(crate) fn expect_snapshot(response: Response, unexpected: &str) -> Result<Snapshot> {
    match response {
        Response::Snapshot(snapshot) => Ok(snapshot),
        other => unexpected_response(other, unexpected),
    }
}

/// Prints `value` as pretty JSON on its own line.
pub(crate) fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
