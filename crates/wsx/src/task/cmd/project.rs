//! `wsx task project add`: clone and register a project (docs/tasks.md "Approvals").

use super::approve::{ask_covering, request_of};
use crate::cli::load_config;
use crate::task::args::ProjectCmd;
use crate::task::{acting_as_manager, need, print_json, project_entry};
use anyhow::Result;
use serde_json::json;
use wsx_core::task::manager::{ApprovalAction, ApprovalLine};
use wsx_core::task::policy::Capability;
use wsx_core::task::store;
use wsx_core::{ops, runtime};

pub(crate) fn project(action: ProjectCmd, project: Option<&str>, json: bool) -> Result<()> {
    let ProjectCmd::Add { path, clone, said } = action;
    need(Capability::Deploy, "project add")?;
    let mut config = load_config()?;
    // ^ The refusals come first, so the manager hears them and the owner is never asked for
    // what would be refused.
    let folder = ops::check_add_project(&config, &path, clone.as_deref())?;
    if acting_as_manager() {
        let (name, main) = project_entry(project)?;
        let mut request = request_of(ApprovalAction::Project, None, None, None, None);
        if let ApprovalLine::Request { path, url, .. } = &mut request {
            *path = Some(folder.display().to_string());
            *url = clone.clone();
        }
        if ask_covering(
            &main,
            &store::project_id(&name, &main),
            request,
            said.owner_said,
            json,
        )? {
            return Ok(());
        }
    }
    let added = ops::add_project(&mut config, &path, clone.as_deref())?;
    // ^ The config is saved: a daemon that does not answer learns of the project from the
    // next client that synchronizes.
    let told = ops::discover_workspace(&config)
        .and_then(|found| ops::synchronize_discovery(&runtime::Client::local(), &found));
    let mut message = format!("project {} added at {}", added.name, added.path.display());
    if let Err(error) = told {
        message += &format!("; wsxd was not told: {error:#}");
    }
    if json {
        return print_json(&json!({
            "project": added.name,
            "path": added.path,
            "state": "added",
            "message": message,
        }));
    }
    println!("{message}");
    Ok(())
}
