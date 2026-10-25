//! The ledger: the bounded project state the manager reads before it acts, and the web
//! client's project brief. ref: docs/tasks.md "Ledger"

use super::view::{
    attention, check_outcomes, clip, overlaps, project_tasks, read_result, task_summary,
};
use super::{checks, copy, store, DeployKind, DeployRecord, TaskRecord, TaskState, WaitOn};
use crate::config::project::load_task_config;
use crate::model::workspace::TaskConfig;
use crate::{ops, runtime};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The whole ledger fits in this many bytes of JSON.
const MAX_LEDGER_BYTES: usize = 16 * 1024;
const RECENT_TASKS: usize = 10;
const SUMMARY_CHARS: usize = 240;
const TITLE_CHARS: usize = 80;
pub const MEMO_CHARS: usize = 600;
const STRAY_WORKTREES: usize = 20;

/// The ledger of one project. ref: docs/tasks.md "Ledger"
pub fn build(project_name: &str, main: &Path) -> Result<Value> {
    let settings = load_task_config(main).map_err(|error| anyhow::anyhow!(error))?;
    let project_id = super::store::project_id(project_name, main);
    let tasks = project_tasks(&project_id)?;
    let records: Vec<&TaskRecord> = tasks
        .iter()
        .filter_map(|(_, record)| record.as_ref().ok())
        .collect();
    let mut open = Vec::new();
    let mut recent = Vec::new();
    for (dir, record) in &tasks {
        let Ok(record) = record else { continue };
        let summary = task_summary(dir, record);
        let title = clip(summary["title"].as_str().unwrap_or_default(), TITLE_CHARS);
        if !record.state.is_open() {
            let mut row = json!({ "id": record.id, "title": title, "state": record.state, "revision": record.revision });
            // ^ The worker's memo, also in its land or close event (docs/tasks.md "Memo").
            if let Some(memo) = read_result(dir)
                .as_ref()
                .and_then(|r| {
                    r["result"]["memo"]
                        .as_str()
                        .map(str::trim)
                        .map(str::to_string)
                })
                .filter(|memo| !memo.is_empty())
            {
                row["memo"] = json!(clip_lines(&memo, MEMO_CHARS));
            }
            recent.push((record.updated_unix_ms, row));
            continue;
        }
        let result = read_result(dir);
        open.push((
            attention(record.state),
            record.id,
            json!({
                "id": record.id,
                "title": title,
                "state": record.state,
                "revision": record.revision,
                "overlaps": overlaps(record, &records, false),
                "human_verify": result.as_ref().map_or(0, |r| r["result"]["human_verify"].as_array().map_or(0, Vec::len)),
                "summary": result.as_ref().and_then(|r| r["result"]["summary"].as_str()).map(|s| clip(s, SUMMARY_CHARS)),
                // ^ What the host ran; it outranks the worker's summary (docs/tasks.md "Ledger").
                "checks": result.as_ref().map(check_outcomes).unwrap_or_default().into_iter()
                    .map(|(command, outcome)| json!({ "command": command, "outcome": outcome }))
                    .collect::<Vec<_>>(),
                // ^ What a waiting task waits for: the owner, a grant, disk or a person's hands.
                "waiting": record.waiting,
                // ^ Held for the owner to try before it lands (docs/tasks.md "Hold and trial").
                "hold": record.hold,
                // ^ What it asks to be granted; the manager may grant it (docs/tasks.md "Permissions").
                "asks": record.waits_on(WaitOn::Permission)
                    .then(|| result.as_ref().map(|r| r["asks"].clone()))
                    .flatten(),
                // ^ The main checkout's changed files this land would overwrite: pressing
                // Land refuses until they are committed or stashed (docs/tasks.md "Ledger").
                "land_blocked_by": (record.state == TaskState::Ready).then(|| land_blockers(record)),
            }),
        ));
    }
    open.sort_by_key(|(order, id, _)| (*order, *id));
    recent.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    recent.truncate(RECENT_TASKS);
    let copies: Vec<&Path> = records
        .iter()
        .filter(|record| record.state.is_open())
        .map(|record| record.working_copy.as_path())
        .collect();
    let (strays, stray_omitted) = stray_worktrees(main, &copies);
    let mut ledger = json!({
        "project": project_name,
        // ^ A ready task lands by itself unless this is false (docs/tasks.md "Landing").
        "auto_land": settings.land_auto,
        // ^ Open "To check" items: a release waits for them (docs/tasks.md "To check").
        "to_check": checks::open_items(&project_id).map_or(0, |items| items.len()),
        "main": main_checkout(main),
        "stray_worktrees": strays,
        "stray_worktrees_omitted": stray_omitted,
        "deploys": deploy_status(project_name, main, &settings).unwrap_or(Value::Null),
        "recent": recent.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "open": open.iter().map(|(_, _, row)| row.clone()).collect::<Vec<_>>(),
        "open_omitted": 0,
        "approvals": super::store::read_approvals(&project_id)
            .map(|lines| json!(super::manager::pending_approvals(&lines)))
            .unwrap_or(Value::Null),
    });
    // ^ Memos go first, then the least urgent open tasks, when the ledger is over its size.
    if serde_json::to_vec(&ledger)?.len() > MAX_LEDGER_BYTES {
        for row in ledger["recent"].as_array_mut().expect("recent is an array") {
            row.as_object_mut().map(|fields| fields.remove("memo"));
        }
    }
    let mut omitted = 0;
    while serde_json::to_vec(&ledger)?.len() > MAX_LEDGER_BYTES {
        let rows = ledger["open"].as_array_mut().expect("open is an array");
        if rows.pop().is_none() {
            ledger["recent"] = json!([]);
            break;
        }
        omitted += 1;
        ledger["open_omitted"] = json!(omitted);
    }
    Ok(ledger)
}

/// The main checkout's changed files that landing `record`'s revision would overwrite;
/// none when that cannot be told. ref: docs/tasks.md "Landing"
pub fn land_blockers(record: &TaskRecord) -> Vec<String> {
    copy::resolve_commit(&record.working_copy, "HEAD")
        .and_then(|head| copy::land_blockers(&record.project_path, &record.default_branch, &head))
        .unwrap_or_default()
}

/// At most `chars` characters of `text`, its lines kept.
fn clip_lines(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The main checkout's branch and how many files it has changed (tracked or new).
fn main_checkout(main: &Path) -> Value {
    let git = |args: &[&str]| {
        crate::git::git_cmd(main)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    json!({
        "path": main,
        "branch": git(&["rev-parse", "--abbrev-ref", "HEAD"]).map(|branch| branch.trim().to_string()),
        "changed_files": git(&["status", "--porcelain"]).map(|status| status.lines().count()),
    })
}

/// The project's worktrees, from git and wsxd, that are neither the main checkout nor an
/// open task's working copy: the owner's cleanup candidates. Nothing here deletes one.
/// ref: docs/tasks.md "Ledger"
fn stray_worktrees(main: &Path, copies: &[&Path]) -> (Vec<Value>, usize) {
    let same = |a: &Path, b: &Path| {
        a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
    };
    let mut found: Vec<(PathBuf, String)> = crate::git::worktree::list_worktrees(main)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| (entry.path, entry.branch))
        .collect();
    // ^ wsxd keeps a worktree it knows with its sessions, also after git forgot it.
    let snapshot = runtime::Client::new(runtime::default_socket_path())
        .call(&runtime::Request::Snapshot)
        .ok()
        .and_then(|response| match response {
            runtime::Response::Snapshot(snapshot) => Some(snapshot),
            _ => None,
        });
    if let Some(snapshot) = &snapshot {
        let project = snapshot
            .projects
            .iter()
            .find(|project| same(&project.path, main));
        for worktree in snapshot
            .worktrees
            .iter()
            .filter(|worktree| project.is_some_and(|project| project.id == worktree.project_id))
        {
            if !found.iter().any(|(path, _)| same(path, &worktree.path)) {
                found.push((worktree.path.clone(), worktree.branch.clone()));
            }
        }
    }
    found.retain(|(path, _)| !same(path, main) && !copies.iter().any(|copy| same(path, copy)));
    found.sort();
    let omitted = found.len().saturating_sub(STRAY_WORKTREES);
    let default = default_ref(main);
    let rows = found
        .into_iter()
        .take(STRAY_WORKTREES)
        .map(|(path, branch)| {
            let missing = !path.is_dir();
            let succeeds = |dir: &Path, args: &[&str]| {
                crate::git::git_cmd(dir)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .map(|output| output.stdout)
            };
            // ^ A detached worktree is judged by its own HEAD, so it needs its folder.
            let merged = if branch != "HEAD" {
                let tip = format!("refs/heads/{branch}");
                succeeds(main, &["merge-base", "--is-ancestor", &tip, &default]).is_some()
            } else {
                !missing
                    && succeeds(&path, &["merge-base", "--is-ancestor", "HEAD", &default]).is_some()
            };
            let dirty = !missing
                && succeeds(&path, &["status", "--porcelain"]).is_some_and(|out| !out.is_empty());
            let sessions = snapshot.as_ref().map_or(0, |snapshot| {
                snapshot
                    .worktrees
                    .iter()
                    .filter(|worktree| same(&worktree.path, &path))
                    .map(|worktree| {
                        snapshot
                            .sessions
                            .iter()
                            .filter(|session| session.worktree_id == worktree.id)
                            .count()
                    })
                    .sum::<usize>()
            });
            json!({
                "path": path,
                "branch": branch,
                "merged_into_main": merged,
                "dirty": dirty,
                "missing": missing,
                "sessions": sessions,
            })
        })
        .collect();
    (rows, omitted)
}

/// The ref a deploy defaults to and `behind` counts against: the local default branch.
pub fn default_ref(main: &Path) -> String {
    format!("refs/heads/{}", ops::detect_default_branch(main))
}

pub fn kind_name(kind: DeployKind) -> &'static str {
    match kind {
        DeployKind::Deploy => "deploy",
        DeployKind::Release => "release",
    }
}

/// The last successful deploy and release, how far each is behind the default branch,
/// and how many attempts failed since.
pub fn deploy_status(
    project_name: &str,
    main: &Path,
    settings: &TaskConfig,
) -> Result<serde_json::Value> {
    let project_id = store::project_id(project_name, main);
    let records = store::read_deploys(&project_id)?;
    let default = default_ref(main);
    let tip = copy::resolve_commit(main, &default).ok();
    let mut out = json!({ "default_branch": default, "tip": tip });
    for kind in [DeployKind::Deploy, DeployKind::Release] {
        let configured = match kind {
            DeployKind::Deploy => settings.deploy_command.is_some(),
            DeployKind::Release => settings.release_command.is_some(),
        };
        // ^ Newest first: attempts that failed until the last success of this kind.
        let (mut last, mut failed_since) = (None, 0);
        for record in records.iter().rev().filter(|record| record.kind == kind) {
            if record.exit == Some(0) {
                last = Some(record);
                break;
            }
            failed_since += 1;
        }
        out[kind_name(kind)] = match last {
            None => {
                json!({ "commit": null, "failed_since": failed_since, "configured": configured })
            }
            Some(record) => json!({
                "commit": record.commit,
                "unix_ms": record.unix_ms,
                "behind": tip
                    .as_deref()
                    .and_then(|tip| copy::unlanded_commits(main, tip, &record.commit).ok()),
                "failed_since": failed_since,
                "configured": configured,
            }),
        };
    }
    out["trial"] = trial_status(&project_id, &records);
    Ok(out)
}

/// The project's trial: `null`, or `{task, revision, commit, by, unix_ms, stage}` with
/// `stage` `preparing` (rebase and checks), `deploying` or `live` (its deploy succeeded).
/// ref: docs/tasks.md "Hold and trial"
pub fn trial_status(project_id: &str, records: &[DeployRecord]) -> Value {
    let Ok(Some(trial)) = store::read_trial(project_id) else {
        return Value::Null;
    };
    let live = records.iter().any(|record| {
        record.trial == Some(trial.task)
            && record.exit == Some(0)
            && Some(&record.commit) == trial.commit.as_ref()
            && record.unix_ms >= trial.unix_ms
    });
    let stage = match (&trial.commit, live) {
        (None, _) => "preparing",
        (Some(_), false) => "deploying",
        (Some(_), true) => "live",
    };
    let mut value = json!(trial);
    value["stage"] = json!(stage);
    value
}
