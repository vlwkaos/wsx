//! Composable project, Git worktree, terminal-runtime, hook, and model primitives.
//!
//! [`runtime`] defines wsx-owned provider-neutral daemon contracts. Runtime
//! implementations and vendor adapters depend on these contracts, not vice versa.

pub mod cache;
pub mod config;
pub mod git;
pub mod hooks;
pub mod integration;
pub mod model;
pub mod ops;
pub mod runtime;
