//! Project panels inside wsxd: the one runner and the one cache of the last result, so the web
//! client and the TUI show the same output. A panel runs only when a client asks: on open, on a
//! manual refresh, and when its result is older than its `refresh`; one run at a time. With no
//! project, wsxd's built-in machine panel (`machine.rs`) shares the same rules.
//! ref: docs/panels.md

use super::*;
use crate::machine;
use std::collections::HashMap;
use std::sync::OnceLock;
use wsx_core::config::project::load_project_config;
use wsx_core::panel::{self, Action, PanelInfo, PanelRun, PanelStatus, PanelView, Response};

#[derive(Default)]
struct Entry {
    last: Option<PanelRun>,
    running: bool,
}

/// Keyed by the project, none for wsxd's built-in machine panel, and the panel's name.
type Cache = Mutex<HashMap<(Option<PathBuf>, String), Entry>>;

fn runs() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(crate) fn handle(daemon: &Daemon, project: Option<&Path>, action: Action) -> Response {
    let Some(project) = project else {
        return builtin(action);
    };
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
                path.as_ref() != Some(&project)
                    || config.panels.iter().any(|panel| panel.name == *name)
            });
            Response::Panels {
                panels: config
                    .panels
                    .iter()
                    .map(|panel| PanelInfo {
                        name: panel.name.clone(),
                        refresh: panel.refresh,
                        scope: panel.scope,
                    })
                    .collect(),
                notice: config.notice,
            }
        }
        Action::Get { name, run } => {
            let Some(panel) = config.panels.into_iter().find(|panel| panel.name == name) else {
                return Response::Error(format!("the project has no panel {name:?}"));
            };
            let refresh = panel.refresh;
            get(Some(project.clone()), name, refresh, run, move || {
                panel::run(&panel, &project)
            })
        }
    }
}

/// wsxd's own machine panels: the one built in (`machine.rs`).
fn builtin(action: Action) -> Response {
    match action {
        Action::List => Response::Panels {
            panels: vec![machine::info()],
            notice: None,
        },
        Action::Get { name, run } if name == machine::NAME => {
            get(None, name, machine::REFRESH_SECS, run, machine::read)
        }
        Action::Get { name, .. } => Response::Error(format!("wsxd has no panel {name:?}")),
    }
}

/// The cached result of a panel; a run of `read` starts on `run` or when the result is older
/// than `refresh`, one at a time.
fn get(
    project: Option<PathBuf>,
    name: String,
    refresh: u64,
    run: bool,
    read: impl FnOnce() -> PanelRun + Send + 'static,
) -> Response {
    let key = (project, name);
    let mut table = lock(runs());
    let entry = table.entry(key.clone()).or_default();
    let stale = entry.last.as_ref().is_none_or(|last| {
        refresh > 0 && unix_time_millis().saturating_sub(last.finished_unix_ms) >= refresh * 1000
    });
    if (run || stale) && !entry.running {
        entry.running = true;
        let key = key.clone();
        thread::spawn(move || {
            let result = read();
            let mut table = lock(runs());
            let entry = table.entry(key).or_default();
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
        name: key.1,
        refresh,
        age_ms: match run.status {
            PanelStatus::Pending => 0,
            _ => unix_time_millis().saturating_sub(run.finished_unix_ms),
        },
        run,
        running: entry.running,
    })
}
