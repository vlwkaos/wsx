//! Git/project operations projected against the wsx-owned runtime.
// ^ [[wsx Architecture]] Git owns worktree discovery; the daemon owns sessions and panes.

use crate::{
    config::global::GlobalConfig,
    git::{info as git_info, worktree as git_worktree},
    hooks,
    model::workspace::{
        FetchFailReason, GitInfo, PaneInfo, Project, ProjectConfig, SessionInfo, WorkspaceState,
        WorktreeInfo,
    },
    runtime::{
        AgentState, Client, ProjectSpec, Request, Response, SessionId, Snapshot, WorktreeSpec,
    },
};
use anyhow::{anyhow, bail, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

struct WorktreeState {
    git_info: Option<GitInfo>,
    git_info_fetched_at: Option<std::time::Instant>,
    expanded: bool,
    sessions: Vec<SessionInfo>,
    last_fetched: Option<std::time::Instant>,
    fetch_failed: bool,
    fetch_fail_count: u32,
    fetch_fail_reason: Option<FetchFailReason>,
}

#[derive(Debug, Clone)]
struct DiscoveredProject {
    name: String,
    path: PathBuf,
    worktrees: Vec<git_worktree::WorktreeEntry>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceDiscovery {
    projects: Vec<DiscoveredProject>,
}

impl WorkspaceDiscovery {
    pub fn into_worktrees(self) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
        self.projects
            .into_iter()
            .map(|project| (project.path, project.worktrees))
            .collect()
    }
}

fn is_git_repo(path: &Path) -> bool {
    path.exists() && path.join(".git").exists()
}

pub fn runtime_snapshot() -> Result<Snapshot> {
    match Client::local().call(&Request::Snapshot)? {
        Response::Snapshot(snapshot) => Ok(snapshot),
        Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsx daemon returned an unexpected snapshot response"),
    }
}

fn synchronize(client: &Client, projects: Vec<ProjectSpec>) -> Result<Snapshot> {
    match client.call(&Request::SynchronizeProjects { projects })? {
        Response::Ack { .. } => {}
        Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsx daemon returned an unexpected synchronization response"),
    }
    match client.call(&Request::Snapshot)? {
        Response::Snapshot(snapshot) => Ok(snapshot),
        Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsx daemon returned an unexpected snapshot response"),
    }
}

pub fn workspace_from_config(config: &GlobalConfig) -> WorkspaceState {
    WorkspaceState {
        projects: config
            .projects
            .iter()
            .filter(|entry| is_git_repo(&entry.path))
            .map(|entry| Project {
                name: entry.name.clone(),
                path: entry.path.clone(),
                default_branch: "main".into(),
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                worktrees: Vec::new(),
                routines: Vec::new(),
                routine_revision: 0,
                routines_expanded: true,
                config: Some(crate::config::project::load_project_config(&entry.path)),
                expanded: true,
                missing: false,
            })
            .collect(),
    }
}

pub fn discover_workspace(config: &GlobalConfig) -> Result<WorkspaceDiscovery> {
    discover_workspace_with(config, git_worktree::list_worktrees)
}

fn discover_workspace_with<F>(
    config: &GlobalConfig,
    mut list_worktrees: F,
) -> Result<WorkspaceDiscovery>
where
    F: FnMut(&Path) -> Result<Vec<git_worktree::WorktreeEntry>>,
{
    let projects = config
        .projects
        .iter()
        .filter(|entry| is_git_repo(&entry.path))
        .map(|entry| {
            let worktrees = list_worktrees(&entry.path)?;
            Ok(DiscoveredProject {
                name: entry.name.clone(),
                path: entry.path.clone(),
                worktrees,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(WorkspaceDiscovery { projects })
}

pub fn synchronize_discovery(discovery: &WorkspaceDiscovery) -> Result<Snapshot> {
    let projects = discovery
        .projects
        .iter()
        .map(|project| ProjectSpec {
            path: project.path.clone(),
            name: project.name.clone(),
            worktrees: project
                .worktrees
                .iter()
                .map(|worktree| WorktreeSpec {
                    path: worktree.path.clone(),
                    branch: worktree.branch.clone(),
                })
                .collect(),
        })
        .collect();
    synchronize(&Client::local(), projects)
}

fn apply_discovery(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    snapshot: &Snapshot,
    discovery: WorkspaceDiscovery,
) -> Result<()> {
    let worktrees = discovery
        .projects
        .into_iter()
        .map(|project| (project.path, project.worktrees))
        .collect();
    refresh_workspace_with_worktrees(workspace, config, snapshot, worktrees)
}

pub fn load_full_workspace(config: &GlobalConfig) -> Result<WorkspaceState> {
    let discovery = discover_workspace(config)?;
    let snapshot = synchronize_discovery(&discovery)?;
    let mut workspace = workspace_from_config(config);
    apply_discovery(&mut workspace, config, &snapshot, discovery)?;
    Ok(workspace)
}

pub fn refresh_workspace_with_worktrees(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    snapshot: &Snapshot,
    worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
) -> Result<()> {
    let mut worktrees_map: HashMap<PathBuf, Vec<git_worktree::WorktreeEntry>> =
        worktrees.into_iter().collect();
    update_project_activity(workspace, snapshot);
    for project in &mut workspace.projects {
        if let Some(default_branch) = worktrees_map
            .get(&project.path)
            .and_then(|entries| entries.iter().find(|entry| entry.is_main))
            .filter(|entry| entry.branch != "HEAD")
            .map(|entry| entry.branch.clone())
        {
            project.default_branch = default_branch;
        }
        let previous: HashMap<PathBuf, WorktreeState> = project
            .worktrees
            .iter()
            .map(|worktree| {
                (
                    worktree.path.clone(),
                    WorktreeState {
                        git_info: worktree.git_info.clone(),
                        git_info_fetched_at: worktree.git_info_fetched_at,
                        expanded: worktree.expanded,
                        sessions: worktree.sessions.clone(),
                        last_fetched: worktree.last_fetched,
                        fetch_failed: worktree.fetch_failed,
                        fetch_fail_count: worktree.fetch_fail_count,
                        fetch_fail_reason: worktree.fetch_fail_reason.clone(),
                    },
                )
            })
            .collect();
        let aliases = config
            .projects
            .iter()
            .find(|entry| entry.path == project.path)
            .map(|entry| &entry.aliases);
        let entries = worktrees_map.remove(&project.path).unwrap_or_default();
        project.worktrees = entries
            .into_iter()
            .filter(|entry| !config.is_worktree_excluded(&entry.path))
            .map(|entry| {
                let old = previous.get(&entry.path);
                Ok(WorktreeInfo {
                    name: entry.name,
                    branch: entry.branch.clone(),
                    path: entry.path.clone(),
                    is_main: entry.is_main,
                    alias: aliases.and_then(|map| map.get(&entry.branch)).cloned(),
                    sessions: sessions_for_worktree(
                        snapshot,
                        &entry.path,
                        old.map(|state| state.sessions.as_slice())
                            .unwrap_or_default(),
                    )?,
                    expanded: old.map(|state| state.expanded).unwrap_or(true),
                    git_info: old.and_then(|state| state.git_info.clone()),
                    fetch_failed: old.map(|state| state.fetch_failed).unwrap_or(false),
                    fetch_fail_count: old.map(|state| state.fetch_fail_count).unwrap_or(0),
                    fetch_fail_reason: old.and_then(|state| state.fetch_fail_reason.clone()),
                    last_fetched: old.and_then(|state| state.last_fetched),
                    git_info_fetched_at: old.and_then(|state| state.git_info_fetched_at),
                })
            })
            .collect::<Result<Vec<_>>>()?;
    }
    workspace.projects.retain(|project| !project.missing);
    for project in &mut workspace.projects {
        project.missing = !is_git_repo(&project.path);
    }
    Ok(())
}

pub fn refresh_sessions_from_snapshot(
    workspace: &mut WorkspaceState,
    snapshot: &Snapshot,
) -> Result<()> {
    update_project_activity(workspace, snapshot);
    for worktree in workspace
        .projects
        .iter_mut()
        .flat_map(|project| &mut project.worktrees)
    {
        worktree.sessions = sessions_for_worktree(snapshot, &worktree.path, &worktree.sessions)?;
    }
    Ok(())
}

fn update_project_activity(workspace: &mut WorkspaceState, snapshot: &Snapshot) {
    for project in &mut workspace.projects {
        let runtime_project = snapshot
            .projects
            .iter()
            .find(|candidate| candidate.path == project.path)
            .or_else(|| {
                let project_id = snapshot.worktrees.iter().find_map(|runtime_worktree| {
                    project
                        .worktrees
                        .iter()
                        .any(|worktree| worktree.path == runtime_worktree.path)
                        .then_some(runtime_worktree.project_id)
                })?;
                snapshot
                    .projects
                    .iter()
                    .find(|candidate| candidate.id == project_id)
            });
        if let Some(runtime_project) = runtime_project {
            project.last_agent_active_unix_ms = runtime_project.last_agent_active_unix_ms;
            project.last_terminal_active_unix_ms = runtime_project.last_terminal_active_unix_ms;
        }
    }
}

fn sessions_for_worktree(
    snapshot: &Snapshot,
    path: &Path,
    previous: &[SessionInfo],
) -> Result<Vec<SessionInfo>> {
    let Some(worktree) = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.path == path)
    else {
        return Ok(Vec::new());
    };
    let previous = previous
        .iter()
        .map(|session| (session.session_id, session))
        .collect::<HashMap<_, _>>();
    let listening_ports = snapshot
        .listening_ports
        .iter()
        .map(|ports| (ports.pane_id, ports.tcp.as_slice()))
        .collect::<HashMap<_, _>>();
    snapshot
        .sessions
        .iter()
        .filter(|session| session.worktree_id == worktree.id)
        .map(|session| {
            let focused = snapshot
                .panes
                .iter()
                .find(|pane| pane.id == session.focused_pane)
                .ok_or_else(|| anyhow!("session {} has no focused pane", session.id))?;
            let old = previous.get(&session.id).copied();
            let panes = session
                .panes
                .iter()
                .map(|pane_id| {
                    let pane = snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.id == *pane_id)
                        .ok_or_else(|| {
                            anyhow!("session {} references missing pane {}", session.id, pane_id)
                        })?;
                    Ok(PaneInfo {
                        pane_id: pane.id,
                        terminal_id: pane.terminal_id,
                        label: pane.label.clone(),
                        agent: pane.agent.as_ref().map(|agent| agent.provider.clone()),
                        agent_status: pane
                            .agent
                            .as_ref()
                            .map_or(AgentState::Unknown, |agent| agent.state),
                        revision: pane.revision,
                        exited: pane.exited,
                        listening_ports: listening_ports
                            .get(&pane.id)
                            .copied()
                            .unwrap_or_default()
                            .to_vec(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let revision = session.revision.max(focused.revision);
            Ok(SessionInfo {
                session_id: session.id,
                pane_id: focused.id,
                terminal_id: focused.terminal_id,
                agent: focused.agent.as_ref().map(|agent| agent.provider.clone()),
                display_name: session.label.clone(),
                agent_status: focused
                    .agent
                    .as_ref()
                    .map_or(AgentState::Unknown, |agent| agent.state),
                revision,
                layout: session.layout.clone(),
                panes,
                muted: old.is_some_and(|session| session.muted),
            })
        })
        .collect()
}

pub fn expand_path(value: &str) -> PathBuf {
    value
        .strip_prefix("~/")
        .and_then(|tail| dirs::home_dir().map(|home| home.join(tail)))
        .unwrap_or_else(|| PathBuf::from(value))
}
pub fn detect_default_branch(path: &Path) -> String {
    git_info::current_branch(path).unwrap_or_else(|| "main".into())
}

pub fn register_project(path: PathBuf, config: &mut GlobalConfig) -> Result<Project> {
    if path.as_os_str().is_empty() {
        bail!("empty path");
    }
    let path = crate::config::global::normalize_project_path(&path);
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }
    if !is_git_repo(&path) {
        bail!("not a git repository: {}", path.display());
    }
    if config.projects.iter().any(|entry| entry.path == path) {
        bail!("project already registered: {}", path.display());
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into());
    let project = Project {
        name: name.clone(),
        path: path.clone(),
        default_branch: detect_default_branch(&path),
        last_agent_active_unix_ms: None,
        last_terminal_active_unix_ms: None,
        worktrees: git_worktree::to_worktree_infos(
            git_worktree::list_worktrees(&path).unwrap_or_default(),
            &HashMap::new(),
        ),
        routines: Vec::new(),
        routine_revision: 0,
        routines_expanded: true,
        config: Some(crate::config::project::load_project_config(&path)),
        expanded: true,
        missing: false,
    };
    config.add_project(name, path);
    Ok(project)
}
pub fn unregister_project(path: &PathBuf, config: &mut GlobalConfig) {
    config.remove_project(path);
}

pub fn create_worktree(
    repo_path: &Path,
    default_branch: &str,
    project_config: &ProjectConfig,
    branch: &str,
) -> Result<(PathBuf, Option<String>)> {
    let path = git_worktree::create_worktree(repo_path, branch, default_branch)?;
    let mut warning = hooks::copy_env_files(repo_path, &path, project_config)
        .err()
        .map(|error| format!("Warning: .env copy: {error}"));
    if let Some(command) = &project_config.post_create {
        if let Err(error) = hooks::run_post_create(&path, command) {
            warning = Some(format!("Warning: postCreate: {error}"));
        }
    }
    Ok((path, warning))
}

pub fn delete_worktree(repo_path: &Path, wt_path: &Path, branch: &str) -> Result<()> {
    let client = Client::local();
    let snapshot = runtime_snapshot()?;
    if let Some(worktree) = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.path == wt_path)
    {
        for session in snapshot
            .sessions
            .iter()
            .filter(|session| session.worktree_id == worktree.id)
        {
            expect_ack(client.call(&Request::SessionClose {
                session_id: session.id,
                expected_revision: session.revision,
            })?)?;
        }
    }
    git_worktree::remove_worktree(repo_path, wt_path, branch)
}
pub fn clean_merged_worktrees(repo_path: &Path, default_branch: &str) -> Result<Vec<String>> {
    let candidates = git_worktree::merged_worktrees(repo_path, default_branch)?;
    let mut removed = Vec::new();
    for entry in candidates {
        delete_worktree(repo_path, &entry.path, &entry.branch)?;
        removed.push(entry.branch);
    }
    Ok(removed)
}

pub fn create_session(
    project_name: &str,
    _worktree_slug: &str,
    worktree_path: &Path,
    session_label: Option<String>,
    command: Option<String>,
) -> Result<(SessionId, String)> {
    let client = Client::local();
    let snapshot = runtime_snapshot()?;
    let worktree = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.path == worktree_path)
        .ok_or_else(|| anyhow!("worktree is not synchronized with wsx daemon"))?;
    let base = session_label
        .filter(|label| !label.trim().is_empty())
        .or_else(|| {
            command
                .as_ref()
                .and_then(|command| command.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_else(|| project_name.to_owned());
    let used = snapshot
        .sessions
        .iter()
        .filter(|session| session.worktree_id == worktree.id)
        .map(|session| session.label.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut label = base.clone();
    let mut suffix = 2;
    while used.contains(label.as_str()) {
        label = format!("{base}-{suffix}");
        suffix += 1;
    }
    let response = client.call(&Request::SessionCreate {
        worktree_id: worktree.id,
        label: label.clone(),
        command: Vec::new(),
        initial_input: command,
        rows: 24,
        cols: 80,
    })?;
    let session_id = match response {
        Response::Created { id, .. } => SessionId(id),
        Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsx daemon returned an unexpected create response"),
    };
    Ok((session_id, label))
}

pub fn rename_session(session_id: SessionId, new_label: &str) -> Result<()> {
    let snapshot = runtime_snapshot()?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| anyhow!("session not found"))?;
    expect_ack(Client::local().call(&Request::SessionRename {
        session_id,
        label: new_label.into(),
        expected_revision: session.revision,
    })?)
}
pub fn kill_session(session_id: SessionId) -> Result<()> {
    let snapshot = runtime_snapshot()?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .ok_or_else(|| anyhow!("session not found"))?;
    expect_ack(Client::local().call(&Request::SessionClose {
        session_id,
        expected_revision: session.revision,
    })?)
}

fn expect_ack(response: Response) -> Result<()> {
    match response {
        Response::Ack { .. } => Ok(()),
        Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsx daemon returned an unexpected mutation response"),
    }
}
pub fn set_alias(config: &mut GlobalConfig, project_path: &PathBuf, branch: &str, alias: &str) {
    config.set_alias(project_path, branch, alias);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        self, Capabilities, Pane, PaneId, PaneLayout, Project as RuntimeProject, ProjectId,
        Session, TerminalId, Worktree, WorktreeId,
    };

    #[test]
    fn projection_lists_sessions_directly_under_their_worktree() {
        let snapshot = Snapshot {
            protocol: runtime::PROTOCOL_VERSION,
            epoch: 1,
            revision: 4,
            projects: vec![RuntimeProject {
                id: ProjectId(1),
                path: "/repo".into(),
                name: "repo".into(),
                revision: 1,
                last_agent_active_unix_ms: Some(42),
                last_terminal_active_unix_ms: Some(43),
            }],
            worktrees: vec![Worktree {
                id: WorktreeId(2),
                project_id: ProjectId(1),
                path: "/repo".into(),
                branch: "main".into(),
                revision: 1,
            }],
            sessions: vec![Session {
                id: SessionId(3),
                worktree_id: WorktreeId(2),
                label: "shell".into(),
                primary_pane: PaneId(4),
                focused_pane: PaneId(6),
                panes: vec![PaneId(4), PaneId(6)],
                layout: PaneLayout::Split {
                    axis: runtime::SplitAxis::Vertical,
                    ratio_millis: 500,
                    first: Box::new(PaneLayout::Leaf { pane_id: PaneId(4) }),
                    second: Box::new(PaneLayout::Leaf { pane_id: PaneId(6) }),
                },
                revision: 4,
            }],
            panes: vec![
                Pane {
                    id: PaneId(4),
                    terminal_id: TerminalId(5),
                    session_id: SessionId(3),
                    label: "primary".into(),
                    agent: None,
                    exited: false,
                    revision: 4,
                },
                Pane {
                    id: PaneId(6),
                    terminal_id: TerminalId(7),
                    session_id: SessionId(3),
                    label: "split".into(),
                    agent: None,
                    exited: false,
                    revision: 4,
                },
            ],
            listening_ports: vec![
                runtime::PanePorts {
                    pane_id: PaneId(4),
                    tcp: vec![5173],
                },
                runtime::PanePorts {
                    pane_id: PaneId(6),
                    tcp: vec![3000, 5173],
                },
            ],
            capabilities: Capabilities::default(),
        };
        let sessions = sessions_for_worktree(&snapshot, Path::new("/repo"), &[]).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, SessionId(3));
        assert_eq!(sessions[0].display_name, "shell");
        assert_eq!(sessions[0].pane_id, PaneId(6));
        assert_eq!(sessions[0].panes.len(), 2);
        assert_eq!(sessions[0].panes[0].label, "primary");
        assert_eq!(sessions[0].panes[1].label, "split");
        assert_eq!(sessions[0].listening_ports(), vec![3000, 5173]);

        let mut workspace = WorkspaceState {
            projects: vec![Project {
                name: "repo".into(),
                path: "/repo".into(),
                default_branch: "main".into(),
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                worktrees: Vec::new(),
                routines: Vec::new(),
                routine_revision: 0,
                routines_expanded: true,
                config: None,
                expanded: true,
                missing: false,
            }],
        };
        refresh_sessions_from_snapshot(&mut workspace, &snapshot).unwrap();
        assert_eq!(workspace.projects[0].last_agent_active_unix_ms, Some(42));
        assert_eq!(workspace.projects[0].last_terminal_active_unix_ms, Some(43));
    }

    #[test]
    fn discovery_lists_each_registered_project_once() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work")
            .join(format!("discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = [root.join("one"), root.join("two")];
        for path in &paths {
            std::fs::create_dir_all(path.join(".git")).unwrap();
        }
        let config = GlobalConfig {
            projects: paths
                .iter()
                .map(|path| crate::config::global::ProjectEntry {
                    name: path.file_name().unwrap().to_string_lossy().into_owned(),
                    path: path.clone(),
                    groups: Vec::new(),
                    aliases: HashMap::new(),
                })
                .collect(),
            ..GlobalConfig::default()
        };
        let shell = workspace_from_config(&config);
        assert_eq!(shell.projects.len(), paths.len());
        assert!(shell
            .projects
            .iter()
            .all(|project| project.worktrees.is_empty()));
        let calls = std::cell::Cell::new(0usize);

        let discovery = discover_workspace_with(&config, |path| {
            calls.set(calls.get() + 1);
            Ok(vec![git_worktree::WorktreeEntry {
                name: "main".into(),
                path: path.to_path_buf(),
                branch: "trunk".into(),
                is_main: true,
            }])
        })
        .unwrap();

        assert_eq!(calls.get(), paths.len());
        assert_eq!(discovery.into_worktrees().len(), paths.len());
        let failed =
            discover_workspace_with(&config, |_| Err(anyhow!("worktree discovery failed")));
        assert!(failed.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn register_project_rejects_empty_paths() {
        let mut config = GlobalConfig::default();
        assert!(register_project(PathBuf::new(), &mut config).is_err());
    }
}
