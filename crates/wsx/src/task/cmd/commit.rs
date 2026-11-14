//! `wsx task commit`: commit files of the main checkout on the default branch once the
//! project's light checks pass on that commit. The manager's one way to write main besides a
//! land: its boundary denies the checkout (docs/tasks.md "Manager", "Permissions").

use crate::task::host::{light, run_check};
use crate::task::{acting_as_manager, actor, owner_said, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::time::Duration;
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskConfig;
use wsx_core::ops;
use wsx_core::task::ledger::default_ref;
use wsx_core::task::view::short_commit;
use wsx_core::task::{copy, owner, store, CheckResult};

pub(crate) fn commit(
    files: &[String],
    message: &str,
    said: Option<u64>,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    if acting_as_manager() {
        let said = owner_said(said).context(
            "the manager commits in the main checkout only when the owner's message of this \
             turn asks: pass --owner-said <id>; nothing was committed",
        )?;
        owner::check_owner_said(&project_id, actor().chat(), said)?;
    }
    let refuse = |error: anyhow::Error| anyhow::anyhow!("{error:#}; nothing was committed");
    let default = default_ref(&main);
    let branch = default.trim_start_matches("refs/heads/");
    let (commit, tip) = copy::commit_files(&main, branch, files, message).map_err(refuse)?;
    let settings = load_task_config(&main).map_err(|error| refuse(anyhow::anyhow!(error)))?;
    let commands: Vec<&String> = settings.checks.iter().filter(|c| light(c)).collect();
    let checks = if commands.is_empty() {
        Vec::new()
    } else {
        check(&main, &project_id, &commit, &commands, &settings).map_err(refuse)?
    };
    if let Some(failed) = checks.iter().find(|check| check.exit != Some(0)) {
        bail!(
            "`{}` failed on the commit ({}); nothing was committed:\n{}",
            failed.command,
            failed
                .exit
                .map_or("no exit code".into(), |code| format!("exit {code}")),
            failed.output.trim_end()
        );
    }
    copy::take_commit(&main, branch, &tip, &commit, files).map_err(refuse)?;
    if json {
        return print_json(&json!({
            "commit": commit,
            "branch": branch,
            "files": files,
            "checks": checks.iter().map(|check| &check.command).collect::<Vec<_>>(),
        }));
    }
    let ran = match checks.len() {
        0 => "no light check is configured".to_string(),
        _ => format!(
            "{} passed",
            checks
                .iter()
                .map(|check| format!("`{}`", check.command))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    println!(
        "committed {} on {branch} as {}: {ran}",
        files.join(", "),
        short_commit(&commit)
    );
    Ok(())
}

/// Run `commands` on `commit` in a scratch copy beside the main checkout, which is removed
/// after; stops at the first that fails.
fn check(
    main: &std::path::Path,
    project_id: &str,
    commit: &str,
    commands: &[&String],
    settings: &TaskConfig,
) -> Result<Vec<CheckResult>> {
    copy::remove_orphans(main, &["commit"]);
    let scratch = copy::working_copy_path_named(main, &format!("commit-{}", std::process::id()))?;
    let made = copy::create_detached(main, commit, &scratch);
    let ran = made.map(|_| run(&scratch, project_id, commands, settings));
    if scratch.exists() {
        ops::cargo_clean(&scratch);
        copy::remove(main, &scratch, None)?;
    }
    ran?
}

fn run(
    scratch: &std::path::Path,
    project_id: &str,
    commands: &[&String],
    settings: &TaskConfig,
) -> Result<Vec<CheckResult>> {
    let timeout = Duration::from_secs(settings.check_timeout_secs);
    let beside = store::follow_up_log_path(project_id)?;
    let mut ran = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let log = beside.with_file_name(format!("commit-check-{}.log", index + 1));
        let check = run_check(scratch, command, &settings.env, timeout, &log);
        let failed = check.exit != Some(0);
        ran.push(check);
        if failed {
            break;
        }
    }
    Ok(ran)
}
