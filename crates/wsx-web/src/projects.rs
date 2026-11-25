//! Add and remove a wsx project from the page (docs/web-client.md "Projects").
//! ^ The same config calls as the TUI's `p` and project delete (`ops::register_project`,
//! `ops::unregister_project`), then a sync of wsxd: the page lists wsxd's projects, and wsxd
//! learns of one only from a client's `SynchronizeProjects`.
use crate::{
    chat::{self, Fault, Outcome},
    site::config_or_fault,
    App,
};
use serde_json::{json, Value};
use std::sync::Mutex;
use wsx_core::{config::global::GlobalConfig, ops, project_search};

/// Load, change and save of the config run one at a time: two calls would lose one change.
static CONFIG: Mutex<()> = Mutex::new(());

/// Only a site that reaches every project and types into a terminal: a shell already gives
/// that power, so a host path adds none. A profile or path grant never manages projects.
fn gate(app: &App) -> Outcome<()> {
    app.site
        .manages_projects()
        .then_some(())
        .ok_or_else(|| Fault::forbidden("adding or removing projects"))
}

/// Config first, then wsxd, as every wsx client does.
fn save_and_sync(app: &App, config: &GlobalConfig) -> Outcome<()> {
    config
        .save()
        .map_err(|error| Fault::new(500, "config_save", format!("{error:#}")))?;
    let discovery = ops::discover_workspace(config)
        .map_err(|error| Fault::new(500, "project_scan", format!("{error:#}")))?;
    ops::synchronize_discovery(&app.client, &discovery)
        .map_err(|error| Fault::new(503, "daemon_unavailable", format!("{error:#}")))?;
    Ok(())
}

/// The repositories the background scan found under home (`project_search::scan_git_repos`),
/// kept for this server's life and scanned again when older than `RESCAN`.
struct Scan {
    repos: Vec<String>,
    done: bool,
    started: std::time::Instant,
}
static SCAN: Mutex<Option<Scan>> = Mutex::new(None);
const RESCAN: std::time::Duration = std::time::Duration::from_secs(600);
/// At most this many rows; the rest is a count ("12 more").
const SHOWN: usize = 40;

/// The scanned repositories so far, and whether the scan still runs. The first call starts it.
fn scanned() -> (Vec<String>, bool) {
    let mut scan = crate::locked(&SCAN);
    let stale = scan
        .as_ref()
        .is_none_or(|scan| scan.done && scan.started.elapsed() > RESCAN);
    if stale {
        // ^ A rescan keeps listing the last scan's repositories until it has them all; the first
        // scan lists them as they come.
        let first = scan.is_none();
        let last = scan.take().map(|scan| scan.repos).unwrap_or_default();
        *scan = Some(Scan {
            repos: last,
            done: false,
            started: std::time::Instant::now(),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || project_search::scan_git_repos(tx));
        std::thread::spawn(move || {
            let mut found = Vec::new();
            for repo in rx {
                if first {
                    if let Some(scan) = crate::locked(&SCAN).as_mut() {
                        scan.repos.push(repo.clone());
                    }
                }
                found.push(repo);
            }
            if let Some(scan) = crate::locked(&SCAN).as_mut() {
                scan.repos = found;
                scan.done = true;
            }
        });
    }
    let scan = scan.as_ref().expect("set above");
    (scan.repos.clone(), !scan.done)
}

/// What the Add project sheet lists for `query` (the TUI's `p` list, `project_search::suggestions`):
/// each row's path, name, whether it is a git repository and the project that already has it;
/// and, when the text names a folder, what that folder is.
pub fn suggest(app: &App, query: &str) -> Outcome<Value> {
    gate(app)?;
    let config = config_or_fault()?;
    let (repos, scanning) = scanned();
    let all = project_search::suggestions(query.trim_start(), &repos);
    let rows: Vec<Value> = all
        .iter()
        .take(SHOWN)
        .map(|shown| {
            let path = project_search::expand(shown);
            json!({
                "path": shown,
                "name": path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
                "git": path.join(".git").exists(),
                "added": project_search::added(&config, &path),
            })
        })
        .collect();
    Ok(json!({
        "rows": rows,
        "more": all.len().saturating_sub(SHOWN),
        "scanning": scanning,
        "folder": project_search::folder(query, &config),
    }))
}

pub fn add(app: &App, path: &str) -> Outcome<Value> {
    gate(app)?;
    let path = ops::expand_path(path.trim());
    if !path.is_absolute() {
        return Err(Fault::new(
            400,
            "invalid_path",
            "enter an absolute path, or one that starts with ~/",
        ));
    }
    let _one = crate::locked(&CONFIG);
    let mut config = config_or_fault()?;
    let project = ops::register_project(path, &mut config)
        .map_err(|error| Fault::new(409, "project_refused", format!("{error:#}")))?;
    save_and_sync(app, &config)?;
    Ok(json!({ "added": project.name }))
}

/// Only unregisters, as the TUI's `d` does: no task, worktree, branch or file is closed or
/// deleted, and nothing refuses it. Sessions stay unless `halt`, which closes them first with
/// `ops::halt_project_sessions`, as the TUI's confirmation does. wsxd keeps the project while a
/// session stands on it, so `chat::projects` lists from the config and the row leaves Home at once.
pub fn remove(app: &App, id: u64, halt: bool) -> Outcome<Value> {
    gate(app)?;
    let snapshot = chat::snapshot(&app.client)?;
    let project = chat::project(&snapshot, &app.site.access, id)?;
    let halted = if halt {
        ops::halt_project_sessions(&app.client, &project.path)
            .map_err(|error| Fault::new(500, "halt_failed", format!("{error:#}")))?
    } else {
        0
    };
    let _one = crate::locked(&CONFIG);
    let mut config = config_or_fault()?;
    ops::unregister_project(&project.path, &mut config);
    save_and_sync(app, &config)?;
    Ok(json!({ "removed": id, "halted": halted }))
}

/// The granted project and its worktree `id`, refused when the delete is not the page's to
/// make: the main checkout, a task's working copy or a worktree that holds a task's session.
/// ref: `wsx_core::stale`, docs/tasks.md "Stale worktrees"
fn deletable<'a>(
    app: &App,
    snapshot: &'a wsx_core::runtime::Snapshot,
    id: u64,
) -> Outcome<(
    &'a wsx_core::runtime::Project,
    &'a wsx_core::runtime::Worktree,
)> {
    let worktree = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id.0 == id)
        .ok_or_else(Fault::not_found)?;
    let project = chat::project(snapshot, &app.site.access, worktree.project_id.0)?;
    let blocked = |code: &'static str, message: &str| Err(Fault::new(409, code, message));
    if worktree.path == project.path {
        return blocked("main_checkout", "the main checkout cannot be deleted");
    }
    let store = wsx_core::task::store::project_id(&project.name, &project.path);
    let copy = wsx_core::stale::open_copies(&store).contains(&worktree.path);
    let task_session = wsx_core::stale::holds_task_session(snapshot, worktree.id);
    if copy || task_session {
        return blocked(
            "task_session",
            "this worktree belongs to a task; close the task instead",
        );
    }
    Ok((project, worktree))
}

fn loss_of(
    project: &wsx_core::runtime::Project,
    worktree: &wsx_core::runtime::Worktree,
) -> Outcome<wsx_core::stale::Loss> {
    wsx_core::stale::loss(&project.path, &worktree.path, &worktree.branch)
        .ok_or_else(|| Fault::new(503, "git_unavailable", "git did not answer; try again"))
}

/// What deleting the worktree closes and loses, for the page's confirmation.
pub fn worktree_loss(app: &App, id: u64) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    let (project, worktree) = deletable(app, &snapshot, id)?;
    let loss = loss_of(project, worktree)?;
    let sessions = snapshot
        .sessions
        .iter()
        .filter(|session| session.worktree_id == worktree.id)
        .count();
    Ok(json!({
        "branch": worktree.branch, "sessions": sessions, "dirty": loss.dirty,
        "dirty_total": loss.dirty_total, "unmerged": loss.unmerged, "lost": loss.words(),
    }))
}

/// Delete the worktree and its sessions. A loss (uncommitted work, commits neither merged nor
/// pushed) needs `confirm`, which the page sets only after it named the loss; the server
/// reads the loss again, so a confirmation of an older state does not widen.
pub fn worktree_remove(app: &App, id: u64, confirm: bool) -> Outcome<Value> {
    let snapshot = chat::snapshot(&app.client)?;
    let (project, worktree) = deletable(app, &snapshot, id)?;
    let loss = loss_of(project, worktree)?;
    if !loss.is_empty() && !confirm {
        return Err(Fault::new(
            409,
            "needs_confirm",
            format!("deleting it loses {}", loss.words()),
        ));
    }
    let kept = ops::delete_worktree(
        &app.client,
        &project.path,
        &worktree.path,
        &worktree.branch,
        !loss.is_empty(),
    )
    .map_err(|error| Fault::new(409, "delete_failed", format!("{error:#}")))?;
    Ok(json!({ "removed": id, "branch_deleted": kept }))
}
