//! Add and remove a wsx project from the page (docs/web-client.md "Projects").
//! ^ The same config calls as the TUI's `p` and project delete (`ops::register_project`,
//! `ops::unregister_project`), then a sync of wsxd: the page lists wsxd's projects, and wsxd
//! learns of one only from a client's `SynchronizeProjects`.
use crate::{
    chat::{self, Fault, Outcome},
    site::global_config,
    App,
};
use serde_json::{json, Value};
use std::path::Path;
use wsx_core::{
    config::global::GlobalConfig,
    ops,
    task::{store, view},
};

/// Only a site that reaches every project and types into a terminal: a shell already gives
/// that power, so a host path adds none. A profile or path grant never manages projects.
fn gate(app: &App) -> Outcome<()> {
    app.site
        .manages_projects()
        .then_some(())
        .ok_or_else(|| Fault::forbidden("adding or removing projects"))
}

fn config() -> Outcome<GlobalConfig> {
    global_config()
        .ok_or_else(|| Fault::new(409, "config_invalid", "the wsx config does not parse"))
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
    let mut config = config()?;
    let project = ops::register_project(path, &mut config)
        .map_err(|error| Fault::new(409, "project_refused", format!("{error:#}")))?;
    save_and_sync(app, &config)?;
    Ok(json!({ "added": project.name }))
}

pub fn remove(app: &App, id: u64) -> Outcome<Value> {
    gate(app)?;
    let snapshot = chat::snapshot(&app.client)?;
    let project = chat::project(&snapshot, &app.site.access, id)?;
    let open_tasks = view::project_tasks(&store::project_id(&project.name, &project.path))
        .unwrap_or_default()
        .iter()
        .any(|(_, record)| record.as_ref().is_ok_and(|record| record.state.is_open()));
    if open_tasks {
        return Err(Fault::new(
            409,
            "project_busy",
            format!("{} has open tasks", project.name),
        ));
    }
    // ^ wsxd keeps a project while a session stands on one of its worktrees, so the row would
    // stay. The TUI hides this by listing from the config; closing a session is the owner's call.
    let sessions = snapshot.sessions.iter().any(|session| {
        snapshot
            .worktrees
            .iter()
            .any(|worktree| worktree.id == session.worktree_id && worktree.project_id == project.id)
    });
    if sessions {
        return Err(Fault::new(
            409,
            "project_busy",
            format!("{} has open sessions", project.name),
        ));
    }
    let mut config = config()?;
    let path: &Path = &project.path;
    if !config.projects.iter().any(|entry| entry.path == path) {
        return Err(Fault::new(
            409,
            "project_refused",
            format!("{} is not in the wsx config", project.name),
        ));
    }
    ops::unregister_project(&project.path, &mut config);
    save_and_sync(app, &config)?;
    Ok(json!({ "removed": id }))
}
