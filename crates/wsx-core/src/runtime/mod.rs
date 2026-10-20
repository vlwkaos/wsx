//! Stable wsx daemon domain and local protocol.
//!
//! The types in this module are provider-neutral. Terminal emulators, agent
//! vendors, and executable plugins implement these contracts without becoming
//! the workspace model.

mod client;
mod domain;
mod protocol;
mod review;

pub use client::{
    ensure_available, ensure_background_available, new_client_id, pending_upgrade, recover_daemon,
    restart_daemon, routine_daemon_command, upgrade_failure, Availability, Client, DaemonRestart,
    DaemonStatus, EventMonitor, EventSignal, PendingUpgrade, RawTerminalStream, TerminalStream,
};
pub use domain::*;
pub use protocol::*;
pub use review::*;
