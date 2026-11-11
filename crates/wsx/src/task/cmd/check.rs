//! `wsx task check`: the owner's To check list (docs/tasks.md "To check").

use super::approve::owner_only;
use super::new::{create, NewTask, Steps};
use crate::task::args::{CheckAction, Chosen};
use crate::task::{acting_as_manager, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use wsx_core::task::checks::{self, CheckItem, CheckState};
use wsx_core::task::owner::{self};
use wsx_core::task::store::{self, TaskDir, REPORT_FILE};

/// `wsx task check`: list the open items, or pass, skip or fail them (docs/tasks.md "To check").
pub(crate) fn check(action: Option<CheckAction>, project: Option<&str>, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let (changed, verdict) = match action {
        None => {
            let items = checks::open_items(&project_id)?;
            if json {
                return print_json(&json!({ "project": name, "open": items }));
            }
            if items.is_empty() {
                println!("nothing to check in {name}");
            }
            for item in items {
                println!(
                    "{:<5} #{:<4} {} (pass if: {})",
                    item.id, item.task, item.check, item.pass_if
                );
            }
            return Ok(());
        }
        Some(CheckAction::Pass { chosen, said }) => {
            // ^ The manager passes only the items it names, on the owner's message of this turn.
            let owner_said = if acting_as_manager() {
                match crate::task::owner_said(said.owner_said) {
                    Some(said) if !chosen.all => {
                        owner::check_owner_said(&project_id, said)?;
                        Some(said)
                    }
                    _ => bail!(
                        "wsx task check pass is for the owner; the manager names each item with --owner-said <message>"
                    ),
                }
            } else {
                None
            };
            (
                close_checks(&project_id, chosen, CheckState::Passed, owner_said)?,
                "passed",
            )
        }
        Some(CheckAction::Skip(chosen)) => {
            owner_only("check skip")?;
            (
                close_checks(&project_id, chosen, CheckState::Skipped, None)?,
                "skipped",
            )
        }
        Some(CheckAction::Fail { id, note }) => {
            owner_only("check fail")?;
            let failed = match note {
                Some(note) => vec![fail_check(&name, &project_id, &id, &note)?],
                None => checks::close(&project_id, &[id], CheckState::Failed, None)?,
            };
            (failed, "failed")
        }
    };
    let open = checks::open_items(&project_id)?.len();
    let message = match changed.as_slice() {
        [item] if item.fix_task.is_some() => format!(
            "{} failed; #{} fixes it. {open} to check",
            item.id,
            item.fix_task.unwrap_or_default()
        ),
        items => format!("{} {verdict}. {open} to check", items.len()),
    };
    if json {
        return print_json(
            &json!({ "ok": true, "changed": changed, "open": open, "message": message }),
        );
    }
    println!("{message}");
    Ok(())
}

/// Pass or skip the chosen items; `owner_said` is the manager's pass on that owner message.
pub(crate) fn close_checks(
    project_id: &str,
    chosen: Chosen,
    state: CheckState,
    owner_said: Option<u64>,
) -> Result<Vec<CheckItem>> {
    let ids = if chosen.all { Vec::new() } else { chosen.ids };
    checks::close(project_id, &ids, state, owner_said)
}

/// A failed item starts a task that fixes it and links both.
pub(crate) fn fail_check(
    project: &str,
    project_id: &str,
    id: &str,
    note: &str,
) -> Result<CheckItem> {
    let note = note.trim();
    if note.is_empty() {
        bail!("a fail needs --note saying what is wrong; the new task gets it");
    }
    if note.len() > checks::MAX_NOTE_BYTES {
        bail!("the note is larger than {} bytes", checks::MAX_NOTE_BYTES);
    }
    let item = checks::find_open(&checks::read(project_id)?, id)?.clone();
    let report = TaskDir::open(project_id, item.task)?.file(REPORT_FILE);
    let prompt = format!(
        "Fix what the owner found when checking #{task} after it landed.\n\n\
         Check: {check}\nPass if: {pass_if}\nThe owner wrote: {note}\n\n\
         The report of #{task}: {report}\n\n\
         Find the cause, fix it, prove what a command or a capture can prove, and commit.",
        task = item.task,
        check = item.check,
        pass_if = item.pass_if,
        report = report.display(),
    );
    let (dir, ..) = create(NewTask {
        prompt,
        from: None,
        into: None,
        after: None,
        title: Some(fix_title(&item.check)),
        model: None,
        fixes: Some(item.id.clone()),
        hold: None,
        files: Vec::new(),
        skills: None,
        plan: false,
        steps: Steps::Skip(Vec::new()),
        project: Some(project.to_string()),
    })?;
    checks::fail(project_id, id, note, dir.id).with_context(|| {
        format!(
            "#{} was started to fix {id}, but the item was not marked failed",
            dir.id
        )
    })
}

/// "Fix: <check>" within the title limit, on one line.
pub(crate) fn fix_title(check: &str) -> String {
    let words: String = check
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    wsx_core::task::view::clip(
        &format!("Fix: {}", words.trim()),
        wsx_core::task::view::MAX_TITLE_CHARS,
    )
}
