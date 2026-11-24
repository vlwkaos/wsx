//! `wsx chat ledger`: the bounded project state a manager reads before it acts.
//! ref: docs/tasks.md "Ledger"

use super::project_entry;
use crate::runtime_util::print_json;
use anyhow::Result;

pub(super) fn ledger(project: Option<&str>) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    print_json(&wsx_core::task::ledger::build(&project_name, &main)?)
}
