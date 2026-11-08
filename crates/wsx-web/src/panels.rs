//! A project's panels: a command wsxd runs and whose last output the page shows
//! (docs/panels.md). The right is the one for running commands, `terminal_input`.
use crate::{
    chat::{self, Fault, Outcome},
    http::blocking,
    site::{Capability, Panel},
    App,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use wsx_core::panel;

fn allowed(app: &App) -> Outcome<()> {
    if !app.site.shows(Panel::Terminal) || !app.site.allows(Capability::TerminalInput) {
        return Err(Fault::forbidden("panels"));
    }
    Ok(())
}

fn ask<T>(app: &App, project: u64, read: impl FnOnce(&Path) -> Result<T, String>) -> Outcome<T> {
    allowed(app)?;
    blocking(|| {
        let path = chat::project_path(app, project)?;
        read(&path).map_err(|message| Fault::new(409, "panel_unavailable", message))
    })
}

/// `GET api/projects/<id>/panels`: the names and the config notice.
pub fn list(app: &App, project: u64) -> Outcome<Value> {
    let (panels, notice) = ask(app, project, |path| panel::list(&app.client, Some(path)))?;
    Ok(json!({ "panels": panels, "notice": notice }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetQuery {
    /// `1`: start a run now (the page opened, or the refresh button).
    run: Option<u8>,
}

/// `GET api/projects/<id>/panels/<name>[?run=1]`: the last result, with its age and whether a
/// run is in flight.
pub fn get(app: &App, project: u64, name: String, query: GetQuery) -> Outcome<Value> {
    let run = query.run == Some(1);
    let view = ask(app, project, |path| {
        panel::get(&app.client, Some(path), name, run)
    })?;
    Ok(json!(view))
}

/// `GET api/machine[?run=1]`: the Machine tab's one card, wsxd's built-in panel with the sections of
/// the machine panels (`scope: machine`) of the granted projects (`panel::machine_view`). A project
/// outside the grant is never asked.
pub fn machine(app: &App, query: GetQuery) -> Outcome<Value> {
    allowed(app)?;
    let run = query.run == Some(1);
    let view = blocking(|| {
        let snapshot = chat::snapshot(&app.client)?;
        let projects: Vec<_> = chat::projects(&snapshot, &app.site.access)
            .into_iter()
            .map(|project| (project.name.clone(), project.path.clone()))
            .collect();
        panel::machine_view(&app.client, &projects, run)
            .map_err(|message| Fault::new(409, "panel_unavailable", message))
    })?;
    Ok(json!(view))
}
