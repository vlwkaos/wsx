//! The ledger: the bounded project state the manager reads before it acts, and the web
//! client's project brief. ref: docs/tasks.md "Ledger"

use super::view::{attention, clip, overlaps, project_tasks, read_result, task_summary};
use super::{brief, copy, store, verification, DeployKind, Phase, TaskRecord};
use crate::config::project::load_task_config;
use crate::model::workspace::{LandPolicy, TaskConfig};
use crate::ops;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

/// The whole ledger fits in this many bytes of JSON.
const MAX_LEDGER_BYTES: usize = 16 * 1024;
const RECENT_TASKS: usize = 10;
const SUMMARY_CHARS: usize = 240;
const TITLE_CHARS: usize = 80;

/// The ledger of one project. ref: docs/tasks.md "Ledger"
pub fn build(project_name: &str, main: &Path) -> Result<Value> {
    let settings = load_task_config(main).map_err(|error| anyhow::anyhow!(error))?;
    let tasks = project_tasks(&super::store::project_id(project_name, main))?;
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
            recent.push((
                record.updated_unix_ms,
                json!({ "id": record.id, "title": title, "state": record.state, "revision": record.revision }),
            ));
            continue;
        }
        let result = read_result(dir);
        let log = dir.read_log().unwrap_or_default();
        let verdict = verification(&log, record.revision).map(|entry| {
            let mut value = serde_json::to_value(&entry.event).unwrap_or_default();
            value["unix_ms"] = json!(entry.unix_ms);
            value.as_object_mut().map(|fields| fields.remove("note"));
            value
        });
        open.push((
            attention(record.state),
            record.id,
            json!({
                "id": record.id,
                "title": title,
                "state": record.state,
                "revision": record.revision,
                "overlaps": overlaps(record, &records, false),
                "verification": verdict,
                "human_verify": result.as_ref().map_or(0, |r| r["result"]["human_verify"].as_array().map_or(0, Vec::len)),
                "summary": result.as_ref().and_then(|r| r["result"]["summary"].as_str()).map(|s| clip(s, SUMMARY_CHARS)),
                "pr_url": record.pr_url,
                "phase": record.phase,
                // ^ What a blocked task asks for; the manager may grant it (docs/tasks.md "Permissions").
                "asks": (record.state == super::TaskState::Blocked)
                    .then(|| result.as_ref().map(|r| r["asks"].clone()))
                    .flatten()
                    .filter(|asks| asks.as_array().is_some_and(|asks| !asks.is_empty())),
                // ^ A brief that waits for the owner's start: its version and build prompt.
                "brief": (record.phase == Phase::Brief).then(|| brief::read(dir)).flatten().map(|file| json!({
                    "version": file.version,
                    "rewritten_prompt": clip(&file.brief.rewritten_prompt, SUMMARY_CHARS),
                })),
            }),
        ));
    }
    open.sort_by_key(|(order, id, _)| (*order, *id));
    recent.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    recent.truncate(RECENT_TASKS);
    let mut ledger = json!({
        "project": project_name,
        "main": main_checkout(main),
        "deploys": deploy_status(project_name, main, &settings, false).unwrap_or(Value::Null),
        "recent": recent.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "open": open.iter().map(|(_, _, row)| row.clone()).collect::<Vec<_>>(),
        "open_omitted": 0,
        "approvals": super::store::read_approvals(&super::store::project_id(project_name, main))
            .map(|lines| json!(super::manager::pending_approvals(&lines)))
            .unwrap_or(Value::Null),
    });
    // ^ Least urgent open tasks go first when the ledger is over its size.
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

/// The ref a deploy defaults to and `behind` counts against. Under the `pr` policy the
/// local default branch can lag the remote, so the remote one is used: fetched first when
/// `fetch`, else as last fetched.
pub fn default_ref(main: &Path, settings: &TaskConfig, fetch: bool) -> Result<String> {
    let default_branch = ops::detect_default_branch(main);
    match settings.land_policy {
        LandPolicy::LocalMerge => Ok(format!("refs/heads/{default_branch}")),
        LandPolicy::Pr if fetch => copy::fetch(main, &settings.land_remote, &default_branch),
        LandPolicy::Pr => Ok(format!(
            "refs/remotes/{}/{default_branch}",
            settings.land_remote
        )),
    }
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
    fetch: bool,
) -> Result<serde_json::Value> {
    let records = store::read_deploys(&store::project_id(project_name, main))?;
    let default = default_ref(main, settings, fetch)?;
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
    Ok(out)
}
