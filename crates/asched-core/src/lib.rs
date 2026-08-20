//! Reusable local scheduler core.
//!
//! Applications own presentation. This crate owns project registration,
//! routine persistence, scheduling, execution, daemon IPC, and lifecycle.

// ^ [[asched Architecture]]
pub mod migration;
pub mod registry;
pub mod routine;

pub use registry::{Project, ProjectRegistry, RegistryError, RegistryStore};
