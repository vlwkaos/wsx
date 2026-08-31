//! Stable wsx daemon domain and local protocol.
//!
//! The types in this module are provider-neutral. Terminal emulators, agent
//! vendors, and executable plugins implement these contracts without becoming
//! the workspace model.

mod client;
mod domain;
mod protocol;

pub use client::{
    ensure_available, new_client_id, Client, EventMonitor, EventSignal, TerminalStream,
};
pub use domain::*;
pub use protocol::*;
