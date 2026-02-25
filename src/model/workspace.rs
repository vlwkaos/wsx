use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub default_branch: String,
    pub worktrees: Vec<WorktreeInfo>,
    pub config: Option<ProjectConfig>,
    pub expanded: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    pub post_create: Option<String>,
    pub copy_includes: Vec<String>,
    pub copy_excludes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,         // full tmux session name
    pub display_name: String, // shown in UI (strips wt_slug prefix)
    pub has_activity: bool,
    pub pane_capture: Option<String>,
    pub last_activity: Option<std::time::Instant>,
    pub was_active: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub name: String,
    pub branch: String,
    pub path: PathBuf,
    pub is_main: bool,
    pub alias: Option<String>,
    pub sessions: Vec<SessionInfo>,
    pub expanded: bool,
    pub git_info: Option<GitInfo>,
}

impl WorktreeInfo {
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    pub fn session_slug(&self) -> String {
        self.alias.as_deref()
            .map(|a| a.to_string())
            .unwrap_or_else(|| self.branch.replace('/', "-"))
    }
}

#[derive(Debug, Clone)]
pub struct GitInfo {
    pub recent_commits: Vec<CommitSummary>,
    pub modified_files: Vec<String>,
    pub ahead: usize,
    pub behind: usize,
}

#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub hash: String,
    pub message: String,
}

/// Flat tree entry for rendering and 3-level navigation.
#[derive(Debug, Clone, PartialEq)]
pub enum FlatEntry {
    Project { idx: usize },
    Worktree { project_idx: usize, worktree_idx: usize },
    Session { project_idx: usize, worktree_idx: usize, session_idx: usize },
}

/// Flatten workspace into visible tree entries based on expand state.
pub fn flatten_tree(workspace: &WorkspaceState) -> Vec<FlatEntry> {
    let mut result = Vec::new();
    for (pi, project) in workspace.projects.iter().enumerate() {
        result.push(FlatEntry::Project { idx: pi });
        if project.expanded {
            for (wi, wt) in project.worktrees.iter().enumerate() {
                result.push(FlatEntry::Worktree { project_idx: pi, worktree_idx: wi });
                if wt.expanded {
                    for (si, _) in wt.sessions.iter().enumerate() {
                        result.push(FlatEntry::Session {
                            project_idx: pi,
                            worktree_idx: wi,
                            session_idx: si,
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
    None,
}

impl WorkspaceState {
    pub fn empty() -> Self {
        Self { projects: Vec::new() }
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
        self.projects.get_mut(pi)?.worktrees.get_mut(wi)?.sessions.get_mut(si)
    }

    /// Resolve flat index to Selection using a pre-computed flat slice.
    pub fn get_selection(&self, flat_idx: usize, flat: &[FlatEntry]) -> Selection {
        match flat.get(flat_idx) {
            Some(FlatEntry::Project { idx }) => Selection::Project(*idx),
            Some(FlatEntry::Worktree { project_idx, worktree_idx }) => {
                Selection::Worktree(*project_idx, *worktree_idx)
            }
            Some(FlatEntry::Session { project_idx, worktree_idx, session_idx }) => {
                Selection::Session(*project_idx, *worktree_idx, *session_idx)
            }
            None => Selection::None,
        }
    }
}
