//! `wsx task ledger`: the bounded project state a manager reads before it acts.
//! ref: docs/tasks.md "Ledger"

use super::{print_json, project_entry};
use anyhow::Result;

pub(super) fn ledger(project: Option<&str>) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    print_json(&wsx_core::task::ledger::build(&project_name, &main)?)
}
