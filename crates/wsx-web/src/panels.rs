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
use std::path::PathBuf;
use wsx_core::panel::{self, Action, PanelScope, Response};

fn allowed(app: &App) -> Outcome<()> {
    if !app.site.shows(Panel::Terminal) || !app.site.allows(Capability::TerminalInput) {
        return Err(Fault::forbidden("panels"));
    }
    Ok(())
}

fn project_path(app: &App, project: u64) -> Outcome<PathBuf> {
    let snapshot = chat::snapshot(&app.client)?;
    Ok(chat::project(&snapshot, &app.site.access, project)?
        .path
        .clone())
}

fn ask(app: &App, project: u64, action: Action) -> Outcome<Response> {
    allowed(app)?;
    blocking(|| {
        let path = project_path(app, project)?;
        panel::request(&app.client, &path, action)
            .map_err(|message| Fault::new(409, "panel_unavailable", message))
    })
}

/// `GET api/projects/<id>/panels`: the names and the config notice.
pub fn list(app: &App, project: u64) -> Outcome<Value> {
    match ask(app, project, Action::List)? {
        Response::Panels { panels, notice } => Ok(json!({ "panels": panels, "notice": notice })),
        _ => Err(Fault::new(502, "panel_unavailable", "unexpected answer")),
    }
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
    match ask(app, project, Action::Get { name, run })? {
        Response::Panel(view) => Ok(serde_json::to_value(view).unwrap_or(Value::Null)),
        _ => Err(Fault::new(502, "panel_unavailable", "unexpected answer")),
    }
}

/// `GET api/machine/panels`: the machine panels (`scope: machine`) of the granted projects,
/// for the Machine tab. A project whose list fails shows none; a project outside the grant
/// is never asked.
pub fn machine(app: &App) -> Outcome<Value> {
    allowed(app)?;
    blocking(|| {
        let snapshot = chat::snapshot(&app.client)?;
        let projects: Vec<Value> = chat::projects(&snapshot, &app.site.access)
            .into_iter()
            .filter_map(|project| {
                let Ok(Response::Panels { panels, .. }) =
                    panel::request(&app.client, &project.path, Action::List)
                else {
                    return None;
                };
                let panels: Vec<_> = panels
                    .into_iter()
                    .filter(|info| info.scope == PanelScope::Machine)
                    .collect();
                (!panels.is_empty())
                    .then(|| json!({ "id": project.id.0, "name": project.name, "panels": panels }))
            })
            .collect();
        Ok(json!({ "projects": projects }))
    })
}
