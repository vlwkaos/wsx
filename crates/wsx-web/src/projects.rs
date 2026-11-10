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
use wsx_core::{config::global::GlobalConfig, ops};

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
/// The same rule as the TUI's `d` (`wsx_core::stale`, docs/tasks.md "Stale worktrees").
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
    let copy = wsx_core::task::view::project_tasks(&store)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, record)| record.ok())
        .any(|record| record.state.is_open() && record.working_copy == worktree.path);
    let task_session = snapshot
        .sessions
        .iter()
        .filter(|session| session.worktree_id == worktree.id)
        .any(|session| {
            let provider = snapshot
                .panes
                .iter()
                .filter(|pane| pane.session_id == session.id)
                .find_map(|pane| pane.agent.as_ref())
                .map(|agent| agent.provider.as_str());
            wsx_core::task::session_news::is_task_session(&session.label, provider)
        });
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
