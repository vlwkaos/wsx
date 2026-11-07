//! The ledger: the bounded project state the manager reads before it acts, and the web
//! client's project brief. ref: docs/tasks.md "Ledger"

use super::view::{
    attention, check_outcomes, clip, overlaps, project_tasks, read_result, task_agent, task_summary,
};
use super::{checks, copy, status, store, usage, DeployKind, DeployRecord, TaskRecord, TaskState};
use crate::config::project::load_task_config;
use crate::model::workspace::TaskConfig;
use crate::routine::Trigger;
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
    let approvals = super::store::read_approvals(&project_id).unwrap_or_default();
    let pending = super::manager::pending_approvals(&approvals);
    let now = super::now_unix_ms();
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
        let waiters = status::waiters(records.iter().copied(), record.id);
        let facts = status::Facts {
            approvals: &pending,
            waiters: &waiters,
            ..Default::default()
        };
        let status = status::status(dir, record, &facts, now);
        let mut row = json!({
            "id": record.id,
            "title": title,
            "state": record.state,
            "revision": record.revision,
            "agent": task_agent(dir, record),
            "overlaps": overlaps(record, &records, false),
            "human_verify": result.as_ref().map_or(0, |r| r["result"]["human_verify"].as_array().map_or(0, Vec::len)),
            "summary": result.as_ref().and_then(|r| r["result"]["summary"].as_str()).map(|s| clip(s, SUMMARY_CHARS)),
            // ^ What the host ran; it outranks the worker's summary (docs/tasks.md "Ledger").
            // ^ With each check's run time and its run's wait for the check slot, in ms.
            "checks": result.as_ref().map(|result| check_outcomes(result).into_iter()
                .zip(result["checks"].as_array().into_iter().flatten())
                .map(|((command, outcome), check)| json!({
                    "command": command, "outcome": outcome,
                    "ms": check["ms"].as_u64().unwrap_or(0),
                    "queued_ms": check["queued_ms"].as_u64().unwrap_or(0),
                }))
                .collect::<Vec<_>>()).unwrap_or_default(),
            // ^ What a waiting task waits for: the owner, disk, the check slot or a person's hands.
            "waiting": record.waiting,
            // ^ The same words as every other view (docs/tasks.md "Status").
            "status": status,
            // ^ Held for the owner to try before it lands (docs/tasks.md "Hold and trial").
            "hold": record.hold,
            "after": record.after,
            // ^ The main checkout's changed files this land would overwrite: pressing
            // Land refuses until they are committed or stashed (docs/tasks.md "Ledger").
            "land_blocked_by": (record.state == TaskState::Ready).then(|| land_blockers(record)),
            "usage": usage::task(dir),
        });
        // ^ A parent task and its open children (docs/tasks.md "Parent tasks").
        if let Some(parent) = record.parent {
            row["parent"] = json!(parent);
        }
        let children = records
            .iter()
            .filter(|other| other.parent == Some(record.id) && other.state.is_open())
            .count();
        if children > 0 {
            row["children"] = json!(children);
        }
        open.push((attention(record.state), record.id, row));
    }
    open.sort_by_key(|(order, id, _)| (*order, *id));
    recent.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    recent.truncate(RECENT_TASKS);
    // ^ Read after the cut: a closed task's log is read only when its row stays.
    for (_, row) in &mut recent {
        if let Some((dir, _)) = tasks.iter().find(|(dir, _)| row["id"] == dir.id) {
            row["usage"] = json!(usage::task(dir));
        }
    }
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
        // ^ Spend today and in 7 days, workers and manager apart (docs/tasks.md "Usage").
        "usage": usage::project(&project_id, super::now_unix_ms()),
        "main": main_checkout(main),
        "stray_worktrees": strays,
        "stray_worktrees_omitted": stray_omitted,
        "deploys": deploy_status(project_name, main, &settings).unwrap_or(Value::Null),
        "routines": routines(main),
        // ^ Where the story of a closed task lives; the ledger keeps none (docs/tasks.md "Search").
        "history": "wsx task search --task <n> | <words>",
        "recent": recent.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        "open": open.iter().map(|(_, _, row)| row.clone()).collect::<Vec<_>>(),
        "open_omitted": 0,
        "approvals": pending,
        // ^ Open and asked roles; a closed one is history (docs/tasks.md "Roles").
        "roles": super::roles::rows(&project_id, &approvals)
            .into_iter()
            .filter(|row| row["state"] != "closed")
            .collect::<Vec<_>>(),
    });
    // ^ Memos go first, then the least urgent open tasks, when the ledger is over its size.
    if serde_json::to_vec(&ledger)?.len() > MAX_LEDGER_BYTES {
        for row in ledger["recent"].as_array_mut().into_iter().flatten() {
            row.as_object_mut().map(|fields| fields.remove("memo"));
        }
    }
    let mut omitted = 0;
    while serde_json::to_vec(&ledger)?.len() > MAX_LEDGER_BYTES {
        if ledger["open"].as_array_mut().and_then(Vec::pop).is_none() {
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

/// A routine's trigger as `wsx routine list`, the ledger and the routine editor show it: the
/// cron, `event:<kind>` or `at:<YYYY-MM-DD HH:MM>` in this machine's zone.
pub fn trigger_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron(cron) => cron.clone(),
        Trigger::Event { kind } => format!("event:{kind}"),
        Trigger::At { epoch_minute } => match local_tm(epoch_minute * 60) {
            Some(tm) => format!(
                "at:{:04}-{:02}-{:02} {:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            ),
            None => format!("at:{epoch_minute}"),
        },
    }
}

/// The trigger `trigger_text` shows, read back.
pub fn parse_trigger(text: &str, now_secs: i64) -> Result<Trigger, String> {
    let text = text.trim();
    if let Some(kind) = text.strip_prefix("event:") {
        return Ok(Trigger::Event { kind: kind.into() });
    }
    match text.strip_prefix("at:") {
        Some(at) => Ok(Trigger::At {
            epoch_minute: local_minute(at, now_secs)?,
        }),
        None => Ok(Trigger::Cron(text.into())),
    }
}

/// `YYYY-MM-DD HH:MM`, or `HH:MM` (its next time after `now_secs`), in this machine's zone, as
/// an epoch minute.
pub fn local_minute(text: &str, now_secs: i64) -> Result<i64, String> {
    let wrong = || format!("time must be HH:MM or YYYY-MM-DD HH:MM: {text}");
    let number = |part: &str| part.parse::<i32>().map_err(|_| wrong());
    let text = text.trim();
    let (date, clock) = match text.split_once(' ') {
        Some((date, clock)) => (Some(date), clock),
        None => (None, text),
    };
    let (hour, minute) = clock.split_once(':').ok_or_else(wrong)?;
    let (hour, minute) = (number(hour)?, number(minute)?);
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(wrong());
    }
    let mut tm = local_tm(now_secs).ok_or_else(wrong)?;
    if let Some(date) = date {
        let parts: Vec<&str> = date.split('-').collect();
        let [year, month, day] = parts[..] else {
            return Err(wrong());
        };
        (tm.tm_year, tm.tm_mon, tm.tm_mday) =
            (number(year)? - 1900, number(month)? - 1, number(day)?);
    }
    (tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_isdst) = (hour, minute, 0, -1);
    let day = (tm.tm_year, tm.tm_mon, tm.tm_mday);
    // SAFETY: tm is a valid, filled struct.
    let mut secs = unsafe { libc::mktime(&mut tm) };
    // ^ mktime moves a day that does not exist (02-31) into the next month: refuse it.
    if secs == -1 || (tm.tm_year, tm.tm_mon, tm.tm_mday) != day {
        return Err(wrong());
    }
    if date.is_none() && secs <= now_secs {
        secs += 24 * 3600;
    }
    Ok(secs / 60)
}

fn local_tm(secs: i64) -> Option<libc::tm> {
    let secs = libc::time_t::try_from(secs).ok()?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: localtime_r filled it.
    Some(unsafe { tm.assume_init() })
}

/// The ledger lists at most this many routines.
const ROUTINES: usize = 20;

/// A full row's command keeps this many words, each cut to this many characters.
const ARGV_WORDS: usize = 40;
const ARGV_CHARS: usize = 200;

/// The project's routines as wsxd's scheduler lists them, without starting it: name, trigger,
/// next run and the last run; `{error}` when the scheduler does not answer and the project
/// has routines. ref: docs/tasks.md "Routines"
pub fn routines(main: &Path) -> Value {
    routine_rows(main, ROUTINES, false)
}

/// `routines` for at most `limit` routines; `full` adds each row's `command` (clipped) and the
/// last run's `finished_unix_ms`. The web client's Routines screen reads it that way.
pub fn routine_rows(main: &Path, limit: usize, full: bool) -> Value {
    use crate::routine::{request, store, Action, Response};
    let listed = request(&crate::runtime::Client::local(), main, Action::List);
    let views = match listed {
        Ok(Response::Routines { routines, .. }) => routines,
        Ok(other) => return json!({ "error": format!("unexpected answer: {other:?}") }),
        Err(error) => {
            // ^ No routine file: nothing to schedule, so a stopped scheduler is not news.
            let defined = store::canonical_project(main)
                .is_ok_and(|project| store::project_file(&store::root(), &project).exists());
            return match defined {
                true => {
                    json!({ "error": format!("the routine scheduler does not answer: {error}") })
                }
                false => json!([]),
            };
        }
    };
    let rows: Vec<Value> = views
        .iter()
        .take(limit)
        .map(|view| {
            let last = view.latest_run.as_ref().map(|run| {
                let mut last = json!({
                    "status": run.status,
                    "exit": run.exit_code,
                    "unix_ms": run.started_epoch * 1000,
                    "duration_ms": run.finished_epoch.map(|end| (end - run.started_epoch) * 1000),
                });
                if full {
                    last["finished_unix_ms"] = json!(run.finished_epoch.map(|epoch| epoch * 1000));
                }
                last
            });
            let mut row = json!({
                "name": view.routine.name,
                "enabled": view.routine.enabled,
                "post": view.routine.post,
                "trigger": trigger_text(&view.routine.trigger),
                "next_run_unix_ms": view.next_run_epoch.map(|epoch| epoch * 1000),
                "running": view.running,
                "last": last,
            });
            if !view.routine.findings_exit.is_empty() {
                row["findings_exit"] = json!(view.routine.findings_exit);
            }
            if full {
                let words: Vec<String> = view
                    .routine
                    .command
                    .iter()
                    .take(ARGV_WORDS)
                    .map(|word| word.chars().take(ARGV_CHARS).collect())
                    .collect();
                row["command"] = json!(words);
            }
            row
        })
        .collect();
    json!(rows)
}

/// The ledger's `deploys` show this many last lines of a failed attempt's output.
const FAILED_LINES: usize = 40;

/// ... and at most this many bytes of them, so the ledger stays small.
const FAILED_BYTES: usize = 4 * 1024;

/// The last `lines` lines of `text`.
pub fn tail_lines(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The end of a failed attempt's output as the ledger shows it.
fn failed_tail(text: &str) -> String {
    let tail = tail_lines(text, FAILED_LINES);
    let mut start = tail.len().saturating_sub(FAILED_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    tail[start..].to_string()
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
        let (mut last, mut failed_since, mut failed) = (None, 0, None);
        for record in records.iter().rev().filter(|record| record.kind == kind) {
            if record.exit == Some(0) {
                last = Some(record);
                break;
            }
            failed_since += 1;
            failed = failed.or(Some(record));
        }
        let name = kind_name(kind);
        out[name] = match last {
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
        // ^ What the newest failed attempt ended with: the end of its output.
        if let Some(failed) = failed {
            out[name]["failed"] = json!({
                "commit": failed.commit,
                "exit": failed.exit,
                "unix_ms": failed.unix_ms,
                "output": failed.output.as_deref().map(failed_tail),
            });
        }
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
