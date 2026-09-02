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

#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    pub post_create: Option<String>,
    pub copy_includes: Vec<String>,
    pub copy_excludes: Vec<String>,
    /// Explicit Git subtree roots relative to the project worktree.
    pub git_subtrees: Vec<PathBuf>,
    /// Migration or parse feedback for the TUI; never affects worktree behavior.
    pub notice: Option<String>,
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
    use super::canonical_session_slug;
    use std::path::Path;

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
