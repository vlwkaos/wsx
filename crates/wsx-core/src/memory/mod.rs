//! The `memory` contract's built-in store: records in an append-only log, folded into the
//! current ones. ref: docs/memory.md, docs/design/memory-system.md

mod record;
mod store;

pub use record::{Hit, Kind, Record, Status};
pub use store::{known_scopes, validate_scope, Error, NewRecord, Patch, SearchOptions, Store};
