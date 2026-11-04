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
    let _one = crate::locked(&CONFIG);
    let mut config = config()?;
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
    let mut config = config()?;
    ops::unregister_project(&project.path, &mut config);
    save_and_sync(app, &config)?;
    Ok(json!({ "removed": id, "halted": halted }))
}
