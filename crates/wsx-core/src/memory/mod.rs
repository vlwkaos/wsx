//! The `memory` contract's built-in store: records in an append-only log, folded into the
//! current ones, and the index that searches them. ref: docs/memory.md,
//! docs/design/memory-system.md

pub mod backend;
pub mod index;
mod record;
mod store;

pub use backend::{Command, KindFilter, Query};
pub use record::{Kind, Record, Status};
pub use store::{known_scopes, validate_scope, Error, NewRecord, Patch, Store};
