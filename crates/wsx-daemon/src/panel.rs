//! Project panels inside wsxd: the one runner and the one cache of the last result, so the web
//! client and the TUI show the same output. A panel runs only when a client asks: on open, on a
//! manual refresh, and when its result is older than its `refresh`; one run at a time.
//! ref: docs/panels.md

use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;
use wsx_core::config::project::load_project_config;
use wsx_core::panel::{self, Action, PanelInfo, PanelRun, PanelStatus, PanelView, Response};

#[derive(Default)]
struct Entry {
    last: Option<PanelRun>,
    running: bool,
}

type Cache = Mutex<HashMap<(PathBuf, String), Entry>>;

fn runs() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(crate) fn handle(daemon: &Daemon, project: &Path, action: Action) -> Response {
    let Ok(project) = fs::canonicalize(project) else {
        return Response::Error(format!("{} is not a folder", project.display()));
    };
    // ^ A request runs a command of the project's file: only a project wsxd manages.
    if !lock(&daemon.state)
        .persisted
        .projects
        .iter()
        .any(|known| known.path == project)
    {
        return Response::Error(format!("{} is not a wsx project", project.display()));
    }
    let config = load_project_config(&project);
    match action {
        Action::List => {
            lock(runs()).retain(|(path, name), _| {
                *path != project || config.panels.iter().any(|panel| panel.name == *name)
            });
            Response::Panels {
                panels: config
                    .panels
                    .iter()
                    .map(|panel| PanelInfo {
                        name: panel.name.clone(),
                        refresh: panel.refresh,
                    })
                    .collect(),
                notice: config.notice,
            }
        }
        Action::Get { name, run } => {
            let Some(panel) = config.panels.into_iter().find(|panel| panel.name == name) else {
                return Response::Error(format!("the project has no panel {name:?}"));
            };
            let mut table = lock(runs());
            let entry = table.entry((project.clone(), name)).or_default();
            let stale = entry.last.as_ref().is_none_or(|last| {
                panel.refresh > 0
                    && unix_time_millis().saturating_sub(last.finished_unix_ms)
                        >= panel.refresh * 1000
            });
            if (run || stale) && !entry.running {
                entry.running = true;
                let config = panel.clone();
                thread::spawn(move || {
                    let result = panel::run(&config, &project);
                    let mut table = lock(runs());
                    let entry = table.entry((project, config.name)).or_default();
                    entry.last = Some(result);
                    entry.running = false;
                });
            }
            let run = entry.last.clone().unwrap_or(PanelRun {
                status: PanelStatus::Pending,
                output: String::new(),
                truncated: false,
                exit: None,
                finished_unix_ms: 0,
                doc: None,
                notice: None,
            });
            Response::Panel(PanelView {
                name: panel.name,
                refresh: panel.refresh,
                age_ms: match run.status {
                    PanelStatus::Pending => 0,
                    _ => unix_time_millis().saturating_sub(run.finished_unix_ms),
                },
                run,
                running: entry.running,
            })
        }
    }
}
