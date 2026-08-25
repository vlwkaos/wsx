// Workspace operation functions — Git/config behavior plus the Herdr runtime mapping.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    config::global::GlobalConfig,
    git::{info as git_info, worktree as git_worktree},
    herdr, hooks,
    model::workspace::{
        FetchFailReason, GitInfo, Project, ProjectConfig, SessionInfo, WorkspaceState, WorktreeInfo,
    },
};

pub const WSX_WORKSPACE_PREFIX: &str = "wsx:";

struct HerdrMutationLock(File);

impl HerdrMutationLock {
    fn acquire() -> Result<Self> {
        let dir = dirs::cache_dir()
            .ok_or_else(|| anyhow!("could not resolve cache directory"))?
            .join("wsx");
        std::fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("herdr-mutation.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error).context("could not lock Herdr workspace mutation");
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for another wsx Herdr mutation");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for HerdrMutationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

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

fn is_git_repo(path: &Path) -> bool {
    path.exists() && path.join(".git").exists()
}

/// Reserved Herdr workspace label based on the existing human worktree identity.
pub fn herdr_workspace_label(project_name: &str, worktree_slug: &str) -> String {
    format!("{WSX_WORKSPACE_PREFIX}{project_name}-{worktree_slug}")
}

/// A workspace belongs to wsx only when its reserved label and an exact pane cwd agree.
// ^ [[Session Model]] Herdr ownership, identity, lifecycle, and local mute rules.
pub fn workspace_ids_for_worktree(snapshot: &herdr::Snapshot, path: &Path) -> Vec<String> {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| {
            workspace.label.starts_with(WSX_WORKSPACE_PREFIX)
                && snapshot.panes.iter().any(|pane| {
                    pane.workspace_id == workspace.workspace_id && pane.cwd.as_deref() == Some(path)
                })
        })
        .map(|workspace| workspace.workspace_id.clone())
        .collect()
}

/// Rebuild worktrees and sessions from Git and one authoritative Herdr snapshot.
pub fn refresh_workspace(workspace: &mut WorkspaceState, config: &GlobalConfig) -> Result<()> {
    let client = herdr::Client::local()?;
    let snapshot = herdr::snapshot_with(&client)?;
    let worktrees = workspace
        .projects
        .iter()
        .map(|project| {
            Ok((
                project.path.clone(),
                git_worktree::list_worktrees(&project.path)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    refresh_workspace_with_worktrees(workspace, config, &snapshot, worktrees)
}

/// Snapshot-fixture variant used by background callers and tests.
pub fn refresh_workspace_with_worktrees(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    snapshot: &herdr::Snapshot,
    worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
) -> Result<()> {
    let mut worktrees_map: HashMap<PathBuf, Vec<git_worktree::WorktreeEntry>> =
        worktrees.into_iter().collect();

    for project in &mut workspace.projects {
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
                let sessions = sessions_for_worktree(
                    snapshot,
                    &entry.path,
                    old.map(|state| state.sessions.as_slice())
                        .unwrap_or_default(),
                )?;
                Ok(WorktreeInfo {
                    name: entry.name,
                    branch: entry.branch.clone(),
                    path: entry.path,
                    is_main: entry.is_main,
                    alias: aliases.and_then(|map| map.get(&entry.branch)).cloned(),
                    sessions,
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

/// Refresh only Herdr-owned session state; Git worktree structure stays unchanged.
pub fn refresh_sessions_from_snapshot(
    workspace: &mut WorkspaceState,
    snapshot: &herdr::Snapshot,
) -> Result<()> {
    for worktree in workspace
        .projects
        .iter_mut()
        .flat_map(|project| &mut project.worktrees)
    {
        worktree.sessions = sessions_for_worktree(snapshot, &worktree.path, &worktree.sessions)?;
    }
    Ok(())
}

fn sessions_for_worktree(
    snapshot: &herdr::Snapshot,
    path: &Path,
    previous: &[SessionInfo],
) -> Result<Vec<SessionInfo>> {
    let workspace_ids = workspace_ids_for_worktree(snapshot, path);
    if workspace_ids.len() > 1 {
        bail!(
            "multiple wsx Herdr workspaces are associated with {}",
            path.display()
        );
    }
    let workspace_ids: HashSet<String> = workspace_ids.into_iter().collect();
    let previous: HashMap<&str, &SessionInfo> = previous
        .iter()
        .map(|session| (session.terminal_id.as_str(), session))
        .collect();
    Ok(snapshot
        .panes
        .iter()
        .filter(|pane| workspace_ids.contains(&pane.workspace_id))
        .map(|pane| {
            let old = previous.get(pane.terminal_id.as_str()).copied();
            SessionInfo {
                pane_id: pane.pane_id.clone(),
                terminal_id: pane.terminal_id.clone(),
                agent: pane.agent.clone(),
                workspace_id: pane.workspace_id.clone(),
                tab_id: pane.tab_id.clone(),
                display_name: pane.label.clone().unwrap_or_else(|| pane.pane_id.clone()),
                agent_status: pane.agent_status,
                revision: pane.revision,
                pane_capture: old.and_then(|session| session.pane_capture.clone()),
                muted: old.is_some_and(|session| session.muted),
            }
        })
        .collect())
}

pub fn load_workspace(config: &GlobalConfig) -> WorkspaceState {
    let projects = config
        .projects
        .iter()
        .filter(|entry| is_git_repo(&entry.path))
        .map(|entry| {
            let entries = git_worktree::list_worktrees(&entry.path).unwrap_or_default();
            Project {
                name: entry.name.clone(),
                path: entry.path.clone(),
                default_branch: detect_default_branch(&entry.path),
                worktrees: git_worktree::to_worktree_infos(
                    entries
                        .into_iter()
                        .filter(|wt| !config.is_worktree_excluded(&wt.path))
                        .collect(),
                    &entry.aliases,
                ),
                routines: Vec::new(),
                routine_revision: 0,
                routines_expanded: true,
                config: Some(crate::config::project::load_project_config(&entry.path)),
                expanded: true,
                missing: false,
            }
        })
        .collect();
    WorkspaceState { projects }
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

// ^ [[Worktree Model]] wsx owns Git lifecycle; Herdr workspaces close before deletion.
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

/// Close every associated wsx Herdr workspace before deleting the Git worktree.
pub fn delete_worktree(repo_path: &Path, wt_path: &Path, branch: &str) -> Result<()> {
    let _mutation_lock = HerdrMutationLock::acquire()?;
    let client = herdr::Client::local()?;
    let snapshot = herdr::snapshot_with(&client)?;
    for workspace_id in workspace_ids_for_worktree(&snapshot, wt_path) {
        herdr::close_workspace_with(&client, &workspace_id)?;
    }
    git_worktree::remove_worktree(repo_path, wt_path, branch)
}

/// Close Herdr workspaces before deleting every merged Git worktree.
pub fn clean_merged_worktrees(repo_path: &Path, default_branch: &str) -> Result<Vec<String>> {
    let candidates = git_worktree::merged_worktrees(repo_path, default_branch)?;
    let mut removed = Vec::new();
    for entry in candidates {
        delete_worktree(repo_path, &entry.path, &entry.branch)?;
        removed.push(entry.branch);
    }
    Ok(removed)
}

/// Create one Herdr tab/root pane and return its stable pane ID and display label.
pub fn create_session(
    project_name: &str,
    worktree_slug: &str,
    worktree_path: &Path,
    session_label: Option<String>,
    command: Option<String>,
) -> Result<(String, String)> {
    // ^ Serialize snapshot/create across wsx processes; Herdr has no atomic get-or-create command.
    let _mutation_lock = HerdrMutationLock::acquire()?;
    let client = herdr::Client::local()?;
    let snapshot = herdr::snapshot_with(&client)?;
    let workspace_ids = workspace_ids_for_worktree(&snapshot, worktree_path);
    if workspace_ids.len() > 1 {
        bail!(
            "multiple wsx Herdr workspaces are associated with {}",
            worktree_path.display()
        );
    }
    let base_label = session_label
        .filter(|label| !label.is_empty())
        .or_else(|| {
            command
                .as_ref()
                .and_then(|cmd| cmd.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_else(|| project_name.to_owned());
    let used: HashSet<&str> = workspace_ids
        .first()
        .into_iter()
        .flat_map(|id| {
            snapshot
                .panes
                .iter()
                .filter(move |pane| &pane.workspace_id == id)
        })
        .filter_map(|pane| pane.label.as_deref())
        .collect();
    let mut display_name = base_label.clone();
    let mut suffix = 2;
    while used.contains(display_name.as_str()) {
        display_name = format!("{base_label}-{suffix}");
        suffix += 1;
    }
    let (pane_id, created_workspace) = if let Some(workspace_id) = workspace_ids.first() {
        (
            herdr::create_tab_with(&client, workspace_id, worktree_path, &display_name)?
                .root_pane_id,
            None,
        )
    } else {
        let created = herdr::create_workspace_with(
            &client,
            worktree_path,
            &herdr_workspace_label(project_name, worktree_slug),
        )?;
        (created.root_pane_id, Some(created.workspace_id))
    };
    let setup = herdr::rename_pane_with(&client, &pane_id, &display_name).and_then(|()| {
        if let Some(command) = command {
            herdr::send_text_with(&client, &pane_id, &command, true)?;
        }
        Ok(())
    });
    if let Err(error) = setup {
        let cleanup = if let Some(workspace_id) = created_workspace {
            herdr::close_workspace_with(&client, &workspace_id)
        } else {
            herdr::close_pane_with(&client, &pane_id)
        };
        if let Err(cleanup_error) = cleanup {
            return Err(error).context(format!(
                "Herdr session setup failed and cleanup also failed: {cleanup_error}"
            ));
        }
        return Err(error);
    }
    Ok((pane_id, display_name))
}

/// Rename only the pane label; the stable pane identity does not change.
pub fn rename_session(pane_id: &str, new_label: &str) -> Result<()> {
    let client = herdr::Client::local()?;
    herdr::rename_pane_with(&client, pane_id, new_label)
}

#[derive(Debug, PartialEq, Eq)]
enum SessionCloseTarget {
    Pane(String),
    Workspace(String),
}

fn session_close_target(snapshot: &herdr::Snapshot, pane_id: &str) -> SessionCloseTarget {
    let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
        return SessionCloseTarget::Pane(pane_id.to_string());
    };
    let pane_count = snapshot
        .panes
        .iter()
        .filter(|candidate| candidate.workspace_id == pane.workspace_id)
        .count();
    if pane_count == 1 {
        SessionCloseTarget::Workspace(pane.workspace_id.clone())
    } else {
        SessionCloseTarget::Pane(pane_id.to_string())
    }
}

pub fn kill_session(pane_id: &str) -> Result<()> {
    let _mutation_lock = HerdrMutationLock::acquire()?;
    let client = herdr::Client::local()?;
    match session_close_target(&herdr::snapshot_with(&client)?, pane_id) {
        SessionCloseTarget::Pane(pane_id) => herdr::close_pane_with(&client, &pane_id),
        SessionCloseTarget::Workspace(workspace_id) => {
            herdr::close_workspace_with(&client, &workspace_id)
        }
    }
}

pub fn set_alias(config: &mut GlobalConfig, project_path: &PathBuf, branch: &str, alias: &str) {
    config.set_alias(project_path, branch, alias);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{AgentStatus, MetadataTokens, Pane, Snapshot, Tab, Workspace};

    fn fixture() -> Snapshot {
        Snapshot {
            version: "0.8.2".into(),
            protocol: 20,
            workspaces: vec![
                Workspace {
                    workspace_id: "owned".into(),
                    label: "wsx:repo-main".into(),
                    tokens: MetadataTokens::new(),
                },
                Workspace {
                    workspace_id: "foreign".into(),
                    label: "personal".into(),
                    tokens: MetadataTokens::new(),
                },
                Workspace {
                    workspace_id: "wrong-cwd".into(),
                    label: "wsx:repo-other".into(),
                    tokens: MetadataTokens::new(),
                },
            ],
            tabs: vec![Tab {
                tab_id: "tab".into(),
                workspace_id: "owned".into(),
                label: "agent".into(),
            }],
            panes: vec![
                Pane {
                    pane_id: "pane-1".into(),
                    terminal_id: "term-1".into(),
                    workspace_id: "owned".into(),
                    tab_id: "tab".into(),
                    cwd: Some("/repo".into()),
                    label: Some("agent".into()),
                    agent: Some("codex".into()),
                    agent_status: AgentStatus::Working,
                    revision: 7,
                    tokens: MetadataTokens::new(),
                },
                Pane {
                    pane_id: "pane-2".into(),
                    terminal_id: "term-2".into(),
                    workspace_id: "foreign".into(),
                    tab_id: "tab-2".into(),
                    cwd: Some("/repo".into()),
                    label: None,
                    agent: None,
                    agent_status: AgentStatus::Idle,
                    revision: 1,
                    tokens: MetadataTokens::new(),
                },
                Pane {
                    pane_id: "pane-3".into(),
                    terminal_id: "term-3".into(),
                    workspace_id: "wrong-cwd".into(),
                    tab_id: "tab-3".into(),
                    cwd: Some("/other".into()),
                    label: None,
                    agent: None,
                    agent_status: AgentStatus::Idle,
                    revision: 1,
                    tokens: MetadataTokens::new(),
                },
            ],
            layouts: vec![],
            agents: vec![],
        }
    }

    #[test]
    fn association_requires_reserved_label_and_exact_pane_cwd() {
        assert_eq!(
            workspace_ids_for_worktree(&fixture(), Path::new("/repo")),
            vec!["owned"]
        );
    }

    #[test]
    fn workspace_label_keeps_human_worktree_identity() {
        assert_eq!(
            herdr_workspace_label("repo", "feature-auth"),
            "wsx:repo-feature-auth"
        );
    }

    #[test]
    fn duplicate_owned_workspaces_are_rejected_during_projection() {
        let mut snapshot = fixture();
        let mut duplicate_workspace = snapshot.workspaces[0].clone();
        duplicate_workspace.workspace_id = "duplicate".into();
        snapshot.workspaces.push(duplicate_workspace);
        let mut duplicate_pane = snapshot.panes[0].clone();
        duplicate_pane.pane_id = "pane-duplicate".into();
        duplicate_pane.workspace_id = "duplicate".into();
        snapshot.panes.push(duplicate_pane);
        let mut workspace = WorkspaceState {
            projects: vec![Project {
                name: "repo".into(),
                path: "/repo".into(),
                default_branch: "main".into(),
                worktrees: vec![],
                routines: vec![],
                routine_revision: 0,
                routines_expanded: true,
                config: None,
                expanded: true,
                missing: false,
            }],
        };

        let error = refresh_workspace_with_worktrees(
            &mut workspace,
            &GlobalConfig::default(),
            &snapshot,
            vec![(
                "/repo".into(),
                vec![git_worktree::WorktreeEntry {
                    name: "main".into(),
                    path: "/repo".into(),
                    branch: "main".into(),
                    is_main: true,
                }],
            )],
        )
        .unwrap_err();
        assert!(error.to_string().contains("multiple wsx Herdr workspaces"));
    }

    #[test]
    fn final_pane_closes_workspace_while_nonfinal_pane_closes_only_itself() {
        let mut snapshot = fixture();
        assert_eq!(
            session_close_target(&snapshot, "pane-1"),
            SessionCloseTarget::Workspace("owned".into())
        );
        let mut second = snapshot.panes[0].clone();
        second.pane_id = "pane-4".into();
        snapshot.panes.push(second);
        assert_eq!(
            session_close_target(&snapshot, "pane-1"),
            SessionCloseTarget::Pane("pane-1".into())
        );
    }

    #[test]
    fn refresh_projects_authoritative_herdr_pane_state_without_foreign_panes() {
        let mut workspace = WorkspaceState {
            projects: vec![Project {
                name: "repo".into(),
                path: "/repo".into(),
                default_branch: "main".into(),
                worktrees: vec![],
                routines: vec![],
                routine_revision: 0,
                routines_expanded: true,
                config: None,
                expanded: true,
                missing: false,
            }],
        };
        refresh_workspace_with_worktrees(
            &mut workspace,
            &GlobalConfig::default(),
            &fixture(),
            vec![(
                "/repo".into(),
                vec![git_worktree::WorktreeEntry {
                    name: "main".into(),
                    path: "/repo".into(),
                    branch: "main".into(),
                    is_main: true,
                }],
            )],
        )
        .unwrap();

        let sessions = &workspace.projects[0].worktrees[0].sessions;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane_id, "pane-1");
        assert_eq!(sessions[0].terminal_id, "term-1");
        assert_eq!(sessions[0].agent.as_deref(), Some("codex"));
        assert_eq!(sessions[0].agent_status, AgentStatus::Working);
        assert_eq!(sessions[0].revision, 7);

        workspace.projects[0].worktrees[0].sessions[0].muted = true;
        workspace.projects[0].worktrees[0].sessions[0].pane_capture = Some("last output".into());
        let mut moved = fixture();
        moved.panes[0].pane_id = "pane-moved".into();
        moved.panes[0].revision = 8;
        refresh_sessions_from_snapshot(&mut workspace, &moved).unwrap();
        let session = &workspace.projects[0].worktrees[0].sessions[0];
        assert_eq!(session.pane_id, "pane-moved");
        assert_eq!(session.terminal_id, "term-1");
        assert!(session.muted);
        assert_eq!(session.pane_capture.as_deref(), Some("last output"));
    }

    fn test_root(label: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../target/ops-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_repo(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(path.join(".git")).unwrap();
        path
    }

    #[test]
    fn missing_project_is_marked_once_then_removed_on_next_refresh() {
        let root = test_root("missing-project");
        let live = make_repo(&root, "live");
        let missing = root.join("missing");
        let mut workspace = WorkspaceState {
            projects: vec![
                Project {
                    name: "live".into(),
                    path: live.clone(),
                    default_branch: "main".into(),
                    worktrees: vec![],
                    routines: vec![],
                    routine_revision: 0,
                    routines_expanded: true,
                    config: None,
                    expanded: true,
                    missing: false,
                },
                Project {
                    name: "missing".into(),
                    path: missing.clone(),
                    default_branch: "main".into(),
                    worktrees: vec![],
                    routines: vec![],
                    routine_revision: 0,
                    routines_expanded: true,
                    config: None,
                    expanded: true,
                    missing: false,
                },
            ],
        };
        let snapshot = Snapshot {
            version: "0.8.2".into(),
            protocol: 20,
            workspaces: vec![],
            tabs: vec![],
            panes: vec![],
            layouts: vec![],
            agents: vec![],
        };
        let worktrees = || vec![(live.clone(), vec![]), (missing.clone(), vec![])];

        refresh_workspace_with_worktrees(
            &mut workspace,
            &GlobalConfig::default(),
            &snapshot,
            worktrees(),
        )
        .unwrap();
        assert_eq!(workspace.projects.len(), 2);
        assert!(workspace.projects.iter().any(|project| project.missing));

        refresh_workspace_with_worktrees(
            &mut workspace,
            &GlobalConfig::default(),
            &snapshot,
            worktrees(),
        )
        .unwrap();
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.projects[0].path, live);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn register_project_normalizes_identity_and_rejects_duplicates() {
        let root = test_root("register");
        let repo = make_repo(&root, "repo");
        let with_slash = PathBuf::from(format!("{}/", repo.display()));
        let mut config = GlobalConfig::default();

        let project = register_project(with_slash.clone(), &mut config).unwrap();
        assert_eq!(project.name, "repo");
        assert_eq!(project.path, repo);
        assert_eq!(config.projects.len(), 1);
        assert!(register_project(with_slash, &mut config).is_err());
        assert_eq!(config.projects.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn register_project_rejects_empty_missing_and_non_git_paths() {
        let root = test_root("invalid-register");
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let mut config = GlobalConfig::default();

        assert!(register_project(PathBuf::new(), &mut config).is_err());
        assert!(register_project(root.join("missing"), &mut config).is_err());
        assert!(register_project(plain, &mut config).is_err());
        assert!(config.projects.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
