//! wsx-core — worktree / tmux / git / hooks / config / model primitives.
//!
//! Extracted from the wsx binary so external orchestrators (notably auwsx)
//! can drive the same workflow without subprocess overhead.
//!
//! Module layout:
//!   - [`ops`]       — workspace business logic (create/delete worktree+session)
//!   - [`tmux`]      — tmux CLI wrappers (session, capture, monitor)
//!   - [`git`]       — git CLI wrappers (worktree, info, ops)
//!   - [`hooks`]     — `.gtrconfig` post-create + gitignored-file copy
//!   - [`config`]    — global TOML + per-project INI
//!   - [`model`]     — shared data types (Project / WorktreeInfo / SessionInfo / GitInfo)
//!   - [`proc_tree`] — process tree snapshot (used by tmux::monitor)

pub mod cache;
pub mod config;
pub mod git;
pub mod hooks;
pub mod model;
pub mod ops;
pub mod proc_tree;
pub mod routine;
pub mod tmux;
