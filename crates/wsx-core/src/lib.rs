//! wsx-core — Git worktree, required Herdr runtime, hooks, config, and model primitives.
//!
//! [`herdr`] is the sole terminal runtime adapter. [`ops`] maps its protocol-20
//! snapshots to Git worktrees and implements workspace/session lifecycle.

pub mod cache;
pub mod config;
pub mod git;
pub mod herdr;
pub mod hooks;
pub mod model;
pub mod ops;
