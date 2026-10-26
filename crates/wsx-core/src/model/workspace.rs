use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::runtime::{AgentState, PaneId, PaneLayout, SessionId, TerminalId};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceState {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub default_branch: String,
    pub last_agent_active_unix_ms: Option<u64>,
    pub last_terminal_active_unix_ms: Option<u64>,
    pub worktrees: Vec<WorktreeInfo>,
    #[serde(skip)]
    pub routines: Vec<asched_core::routine::ipc::RoutineView>,
    #[serde(skip)]
    pub routine_revision: u64,
    #[serde(skip)]
    pub routines_expanded: bool,
    #[serde(skip)]
    pub config: Option<ProjectConfig>,
    #[serde(skip)]
    pub expanded: bool,
    #[serde(skip)]
    pub missing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefaultSessionConfig {
    pub enabled: bool,
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeInitialSession {
    Disabled,
    Shell,
    Command(String),
}

#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    pub post_create: Option<String>,
    pub copy_includes: Vec<String>,
    pub copy_excludes: Vec<String>,
    /// Explicit Git subtree roots relative to the project worktree.
    pub git_subtrees: Vec<PathBuf>,
    pub branch_prefix: String,
    pub default_session: Option<DefaultSessionConfig>,
    pub tasks: TaskConfig,
    /// Migration or parse feedback for the TUI; never affects worktree behavior.
    pub notice: Option<String>,
}

pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 20 * 60;

/// The `tasks:` section of `wsx.config.yml` (docs/tasks.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskConfig {
    pub adapter: String,
    pub model: String,
    /// Tools beyond the fixed ones, never `Bash(...)`: the sandbox bounds Bash (docs/tasks.md "Permissions").
    pub tools: Vec<String>,
    pub sandbox: TaskSandbox,
    /// `tasks.skills: owner` lends workers the owner's skills, as the manager has them.
    pub owner_skills: bool,
    pub workflow: TaskWorkflow,
    pub checks: Vec<String>,
    /// `tasks.env`: set for the worker's commands and for the checks, the same in both.
    pub env: std::collections::BTreeMap<String, String>,
    /// Seconds one check may run before its process group is killed.
    pub check_timeout_secs: u64,
    /// Free disk a task needs before an agent turn or its checks; below it the task waits
    /// (docs/tasks.md "Checks, revisions and report").
    pub min_free_gb: u64,
    /// Whether the manager's land, deploy and release need the owner's approval (only a
    /// release does by default: docs/tasks.md "Approvals").
    pub land_approve: bool,
    /// Whether the host lands a task as soon as its checks pass (docs/tasks.md "Landing").
    pub land_auto: bool,
    pub deploy_approve: bool,
    /// Whether the deployed site is public: open "To check" items then stop a deploy.
    pub deploy_public: bool,
    /// Whether a land of the default branch deploys it (docs/tasks.md "Deploy and release").
    pub deploy_auto: bool,
    /// `tasks.preview`: how a task is tried on request (docs/tasks.md "Preview").
    pub preview: Option<TaskPreview>,
    pub release_approve: bool,
    /// Shell commands that deploy or release one default-branch commit.
    pub deploy_command: Option<String>,
    pub release_command: Option<String>,
    /// The project manager (`tasks.manager`).
    /// `tasks.manager.name`: the name the owner reads instead of "Manager", when set.
    pub manager_name: Option<String>,
    pub manager_model: String,
    /// The model of a turn with only wsx's events and no review due; `None` (`off`) keeps
    /// `manager_model`. ref: docs/tasks.md "Manager"
    pub manager_event_model: Option<String>,
    /// Context size after which the manager's next turn starts a fresh conversation.
    pub manager_rotate_tokens: u64,
    /// Quiet seconds after which the manager writes the owner a recap; 0 is off. The config
    /// says minutes (`recapMinutes`, a fraction allowed). ref: docs/tasks.md "Recap"
    pub manager_recap_secs: u64,
    /// Tools the manager gets beyond its read-only ones (`tasks.manager.tools`), never
    /// `Bash(...)` or a bare file tool.
    pub manager_tools: Vec<String>,
}

/// `tasks.sandbox`: what a worker's commands may reach beyond its working copy.
/// ref: docs/tasks.md "Permissions"
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(default)]
pub struct TaskSandbox {
    /// Hosts reachable without the classifier, such as `crates.io` or `*.npmjs.org`.
    pub allowed_domains: Vec<String>,
    /// Absolute or `~/` paths the worker's commands may write, such as `~/.cargo/registry`.
    pub allow_write: Vec<String>,
}

/// `tasks.workflow`: the project's own words for each fixed stage; wsx's defaults name no
/// skill. ref: docs/tasks.md "Workflow"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWorkflow {
    /// Added to every worker's instructions.
    pub build: String,
    /// Added to the manager's review of a ready task.
    pub review: String,
    /// The housekeeping task's prompt after a request's tasks land.
    pub batch: String,
    /// The prompt of the task the manager host starts when session notes are newer than the
    /// last one; empty is off.
    pub idle: String,
    /// The worker's turns in order, each one step; empty is one build turn. The project's
    /// steps, or else the owner's in `~/.config/wsx/wsx.config.yml`.
    pub steps: Vec<WorkflowStep>,
}

/// One step of `tasks.workflow.steps`: the host sends `prompt` as its own worker turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, Serialize)]
pub struct WorkflowStep {
    pub name: String,
    pub prompt: String,
}

impl Default for TaskWorkflow {
    fn default() -> Self {
        Self {
            build: "Keep the change lean: reuse what exists, remove what your change makes \
                    dead, and simplify the diff before you finish."
                .into(),
            review: String::new(),
            batch:
                "Consolidate the code these tasks changed: merge duplicates, remove dead \
                    code and layers that do nothing, keep behavior and public interfaces the \
                    same, and lower the net line count. The checks prove the behavior is unchanged."
                    .into(),
            idle: String::new(),
            steps: Vec::new(),
        }
    }
}

/// A command that serves one task on `$PORT`, the path that answers once it is up, and
/// the ports it may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPreview {
    pub command: String,
    pub ready: String,
    pub ports: (u16, u16),
}

impl TaskConfig {
    /// The manager's name as the owner reads it: `tasks.manager.name`, else "Manager"
    /// (docs/tasks.md "Config").
    pub fn manager_shown_name(&self) -> &str {
        self.manager_name.as_deref().unwrap_or("Manager")
    }
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            adapter: "claude".into(),
            model: "opus".into(),
            tools: Vec::new(),
            sandbox: TaskSandbox::default(),
            owner_skills: false,
            workflow: TaskWorkflow::default(),
            checks: Vec::new(),
            env: Default::default(),
            check_timeout_secs: DEFAULT_CHECK_TIMEOUT_SECS,
            min_free_gb: 5,
            land_approve: false,
            land_auto: true,
            deploy_approve: false,
            deploy_public: false,
            deploy_auto: false,
            preview: None,
            release_approve: true,
            deploy_command: None,
            release_command: None,
            manager_name: None,
            manager_model: "opus".into(),
            // ^ Off: a switch writes the conversation into the other model's cache, which cost
            // more than the turn saved (scripts/bench-manager.py, docs/tasks.md "Manager").
            manager_event_model: None,
            manager_rotate_tokens: 150_000,
            manager_recap_secs: 60 * 60,
            manager_tools: Vec::new(),
        }
    }
}

impl ProjectConfig {
    pub fn initial_session(&self, fallback: WorktreeInitialSession) -> WorktreeInitialSession {
        match &self.default_session {
            None => fallback,
            Some(session) if !session.enabled => WorktreeInitialSession::Disabled,
            Some(session) => session
                .command
                .as_ref()
                .map_or(WorktreeInitialSession::Shell, |command| {
                    WorktreeInitialSession::Command(command.clone())
                }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PaneInfo {
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    pub label: String,
    pub agent: Option<String>,
    pub agent_status: AgentState,
    pub revision: u64,
    pub exited: bool,
    pub listening_ports: Vec<u16>,
    pub foreground_job: bool,
    /// This exact provider outcome revision was acknowledged by explicit UI interaction.
    #[serde(skip)]
    pub outcome_acknowledged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub pane_id: PaneId,
    pub terminal_id: TerminalId,
    /// Provider label reported by the pane's normalized agent adapter.
    pub agent: Option<String>,
    pub display_name: String,
    pub agent_status: AgentState,
    pub revision: u64,
    pub layout: PaneLayout,
    pub panes: Vec<PaneInfo>,
    #[serde(skip)]
    pub muted: bool,
    /// This exact provider outcome revision was acknowledged by explicit UI interaction.
    #[serde(skip)]
    pub outcome_acknowledged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum FetchFailReason {
    Auth,    // "Authentication failed", "Permission denied", "could not read Username"
    Timeout, // killed after 10s
    Network, // generic / other failure
}

impl SessionInfo {
    pub fn has_foreground_job(&self) -> bool {
        self.panes.iter().any(|pane| pane.foreground_job)
    }

    pub fn is_agentic(&self) -> bool {
        self.agent.is_some() || self.panes.iter().any(|pane| pane.agent.is_some())
    }

    pub fn listening_ports(&self) -> Vec<u16> {
        let mut ports = self
            .panes
            .iter()
            .flat_map(|pane| pane.listening_ports.iter().copied())
            .collect::<Vec<_>>();
        ports.sort_unstable();
        ports.dedup();
        ports
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeInfo {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub is_main: bool,
    pub alias: Option<String>,
    pub sessions: Vec<SessionInfo>,
    #[serde(skip)]
    pub expanded: bool,
    pub git_info: Option<GitInfo>,
    pub fetch_failed: bool,
    pub fetch_fail_count: u32,
    pub fetch_fail_reason: Option<FetchFailReason>,
    #[serde(skip)]
    pub last_fetched: Option<std::time::Instant>,
    #[serde(skip)]
    pub git_info_fetched_at: Option<std::time::Instant>,
}

impl WorktreeInfo {
    pub fn listening_ports(&self) -> Vec<u16> {
        let mut ports = self
            .sessions
            .iter()
            .flat_map(SessionInfo::listening_ports)
            .collect::<Vec<_>>();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    pub fn session_slug(&self, project_name: &str) -> String {
        canonical_session_slug(project_name, &self.path)
    }
}

fn sanitize_slug(raw: &str) -> String {
    raw.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "-")
}

pub fn canonical_session_slug(project_name: &str, worktree_path: &Path) -> String {
    let dir_name = worktree_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_name.to_string());
    let proj_prefix = format!("{}-", project_name);
    let short_name = dir_name.strip_prefix(&proj_prefix).unwrap_or(&dir_name);
    sanitize_slug(short_name)
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_session_slug, DefaultSessionConfig, ProjectConfig, WorktreeInitialSession,
    };
    use std::path::Path;

    #[test]
    fn initial_session_preserves_surface_defaults_until_explicitly_configured() {
        let mut config = ProjectConfig::default();
        assert_eq!(
            config.initial_session(WorktreeInitialSession::Disabled),
            WorktreeInitialSession::Disabled
        );
        assert_eq!(
            config.initial_session(WorktreeInitialSession::Shell),
            WorktreeInitialSession::Shell
        );

        config.default_session = Some(DefaultSessionConfig {
            enabled: false,
            command: Some("cargo watch".into()),
        });
        assert_eq!(
            config.initial_session(WorktreeInitialSession::Shell),
            WorktreeInitialSession::Disabled
        );

        config.default_session = Some(DefaultSessionConfig {
            enabled: true,
            command: None,
        });
        assert_eq!(
            config.initial_session(WorktreeInitialSession::Disabled),
            WorktreeInitialSession::Shell
        );

        config.default_session = Some(DefaultSessionConfig {
            enabled: true,
            command: Some("cargo watch".into()),
        });
        assert_eq!(
            config.initial_session(WorktreeInitialSession::Disabled),
            WorktreeInitialSession::Command("cargo watch".into())
        );
    }

    #[test]
    fn canonical_slug_uses_human_worktree_identity() {
        assert_eq!(canonical_session_slug("wsx", Path::new("/tmp/wsx")), "wsx");
        assert_eq!(
            canonical_session_slug("wsx", Path::new("/tmp/wsx-feature-auth")),
            "feature-auth"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GitInfo {
    pub recent_commits: Vec<CommitSummary>,
    pub modified_files: Vec<String>,
    /// `None` means Git could not inspect configured submodules.
    pub submodules: Option<Vec<SubmoduleInfo>>,
    pub subtrees: Vec<SubtreeInfo>,
    pub ahead: usize,
    pub behind: usize,
    pub remote_branch: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmoduleCommitState {
    InSync,
    CommitChanged,
    Uninitialized,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubmoduleInfo {
    pub path: String,
    pub commit_state: SubmoduleCommitState,
    pub modified_content: bool,
    pub untracked_content: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubtreeInfo {
    pub path: String,
    pub modified_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommitSummary {
    pub hash: String,
    pub message: String,
}

/// Flat tree entry for rendering and 3-level navigation.
#[derive(Debug, Clone, PartialEq)]
pub enum FlatEntry {
    Project {
        idx: usize,
    },
    Worktree {
        project_idx: usize,
        worktree_idx: usize,
    },
    Session {
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
    },
    Pane {
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
        pane_idx: usize,
    },
    RoutinesHeader {
        project_idx: usize,
    },
    Routine {
        project_idx: usize,
        routine_idx: usize,
    },
}

/// Flatten workspace into visible tree entries based on expand state.
#[allow(dead_code)]
pub fn flatten_tree(workspace: &WorkspaceState) -> Vec<FlatEntry> {
    let mut result = Vec::new();
    for (pi, project) in workspace.projects.iter().enumerate() {
        result.push(FlatEntry::Project { idx: pi });
        if project.expanded {
            for (wi, wt) in project.worktrees.iter().enumerate() {
                result.push(FlatEntry::Worktree {
                    project_idx: pi,
                    worktree_idx: wi,
                });
                if wt.expanded {
                    for (si, session) in wt.sessions.iter().enumerate() {
                        result.push(FlatEntry::Session {
                            project_idx: pi,
                            worktree_idx: wi,
                            session_idx: si,
                        });
                        if session.panes.len() > 1 {
                            for (pane_idx, _) in session.panes.iter().enumerate() {
                                result.push(FlatEntry::Pane {
                                    project_idx: pi,
                                    worktree_idx: wi,
                                    session_idx: si,
                                    pane_idx,
                                });
                            }
                        }
                    }
                }
            }
            if !project.routines.is_empty() {
                result.push(FlatEntry::RoutinesHeader { project_idx: pi });
                if project.routines_expanded {
                    for (ri, _) in project.routines.iter().enumerate() {
                        result.push(FlatEntry::Routine {
                            project_idx: pi,
                            routine_idx: ri,
                        });
                    }
                }
            }
        }
    }
    result
}

/// Like `flatten_tree` but skips projects whose index is not in `visible`.
pub fn flatten_tree_filtered(
    workspace: &WorkspaceState,
    visible: &HashSet<usize>,
) -> Vec<FlatEntry> {
    let mut result = Vec::new();
    for (pi, project) in workspace.projects.iter().enumerate() {
        if !visible.contains(&pi) {
            continue;
        }
        result.push(FlatEntry::Project { idx: pi });
        if project.expanded {
            for (wi, wt) in project.worktrees.iter().enumerate() {
                result.push(FlatEntry::Worktree {
                    project_idx: pi,
                    worktree_idx: wi,
                });
                if wt.expanded {
                    for (si, session) in wt.sessions.iter().enumerate() {
                        result.push(FlatEntry::Session {
                            project_idx: pi,
                            worktree_idx: wi,
                            session_idx: si,
                        });
                        if session.panes.len() > 1 {
                            for (pane_idx, _) in session.panes.iter().enumerate() {
                                result.push(FlatEntry::Pane {
                                    project_idx: pi,
                                    worktree_idx: wi,
                                    session_idx: si,
                                    pane_idx,
                                });
                            }
                        }
                    }
                }
            }
            if !project.routines.is_empty() {
                result.push(FlatEntry::RoutinesHeader { project_idx: pi });
                if project.routines_expanded {
                    for (ri, _) in project.routines.iter().enumerate() {
                        result.push(FlatEntry::Routine {
                            project_idx: pi,
                            routine_idx: ri,
                        });
                    }
                }
            }
        }
    }
    result
}

/// What is currently focused.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Project(usize),
    Worktree(usize, usize),
    Session(usize, usize, usize),
    Pane(usize, usize, usize, usize),
    RoutinesHeader(usize),
    Routine(usize, usize),
    None,
}

impl WorkspaceState {
    pub fn empty() -> Self {
        Self {
            projects: Vec::new(),
        }
    }

    pub fn worktree(&self, pi: usize, wi: usize) -> Option<&WorktreeInfo> {
        self.projects.get(pi)?.worktrees.get(wi)
    }

    pub fn worktree_mut(&mut self, pi: usize, wi: usize) -> Option<&mut WorktreeInfo> {
        self.projects.get_mut(pi)?.worktrees.get_mut(wi)
    }

    pub fn session(&self, pi: usize, wi: usize, si: usize) -> Option<&SessionInfo> {
        self.projects.get(pi)?.worktrees.get(wi)?.sessions.get(si)
    }

    pub fn session_mut(&mut self, pi: usize, wi: usize, si: usize) -> Option<&mut SessionInfo> {
        self.projects
            .get_mut(pi)?
            .worktrees
            .get_mut(wi)?
            .sessions
            .get_mut(si)
    }

    /// Resolve flat index to Selection using a pre-computed flat slice.
    pub fn get_selection(&self, flat_idx: usize, flat: &[FlatEntry]) -> Selection {
        match flat.get(flat_idx) {
            Some(FlatEntry::Project { idx }) => Selection::Project(*idx),
            Some(FlatEntry::Worktree {
                project_idx,
                worktree_idx,
            }) => Selection::Worktree(*project_idx, *worktree_idx),
            Some(FlatEntry::Session {
                project_idx,
                worktree_idx,
                session_idx,
            }) => Selection::Session(*project_idx, *worktree_idx, *session_idx),
            Some(FlatEntry::Pane {
                project_idx,
                worktree_idx,
                session_idx,
                pane_idx,
            }) => Selection::Pane(*project_idx, *worktree_idx, *session_idx, *pane_idx),
            Some(FlatEntry::RoutinesHeader { project_idx }) => {
                Selection::RoutinesHeader(*project_idx)
            }
            Some(FlatEntry::Routine {
                project_idx,
                routine_idx,
            }) => Selection::Routine(*project_idx, *routine_idx),
            None => Selection::None,
        }
    }
}
