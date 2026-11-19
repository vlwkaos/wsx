//! `wsx task ledger`: the bounded project state a manager reads before it acts.
//! ref: docs/tasks.md "Ledger"

use super::{print_json, project_entry};
use anyhow::Result;

pub(super) fn ledger(project: Option<&str>) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    print_json(&wsx_core::task::ledger::build(&project_name, &main)?)
}

/// `wsx task widths`: how often each scenario part failed at each lane width.
pub(super) fn widths(project: Option<&str>) -> Result<()> {
    let (name, main) = project_entry(project)?;
    println!(
        "{:<24} {:>5} {:>5} {:>6}",
        "part", "width", "runs", "failed"
    );
    for (part, width, runs, failed) in
        wsx_core::task::schedule::widths(&wsx_core::task::store::project_id(&name, &main))
    {
        println!("{part:<24} {width:>5} {runs:>5} {failed:>6}");
    }
    Ok(())
}
