//! Persistent wsx UI state and local mute flags.
//!
//! Herdr is authoritative for sessions. Legacy tmux/session fields in older TOML
//! files are ignored by serde and are never imported.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::workspace::{FlatEntry, WorkspaceState};
use serde::{Deserialize, Serialize};

/// Stable cursor identity for projects, worktrees, Herdr panes, and routines.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CursorIdentity {
    Project {
        path: String,
    },
    Worktree {
        path: String,
    },
    Session {
        worktree_path: String,
        pane_id: String,
    },
    RoutinesHeader {
        project_path: String,
    },
    Routine {
        project_path: String,
        routine_name: String,
    },
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct WorkspaceCache {
    #[serde(default)]
    pub written_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub worktree_expanded: HashMap<String, bool>,
    #[serde(default)]
    pub project_expanded: HashMap<String, bool>,
    #[serde(default)]
    pub tree_selected: usize,
    #[serde(default)]
    pub cursor_identity: Option<CursorIdentity>,
    /// Stable Herdr pane IDs muted in this local wsx UI.
    #[serde(default)]
    pub muted_sessions: HashSet<String>,
    #[serde(default)]
    pub command_history: Vec<String>,
    #[serde(default)]
    pub active_tab: Option<String>,
}

impl WorkspaceCache {
    pub fn load() -> Self {
        let Ok(content) = std::fs::read_to_string(cache_path()) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self, sync: bool) -> anyhow::Result<()> {
        let mut cache = self.clone();
        cache.written_at_unix_ms = Some(now_unix_ms());
        let path = cache_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(toml::to_string(&cache)?.as_bytes())?;
        if sync {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace.toml")
}

/// Apply only cached UI and local mute state. Sessions always come from Herdr.
pub fn apply_cache(
    workspace: &mut WorkspaceState,
) -> (
    usize,
    Option<CursorIdentity>,
    Vec<String>,
    Option<String>,
    HashSet<String>,
) {
    let cache = WorkspaceCache::load();
    for project in &mut workspace.projects {
        let project_key = project.path.to_string_lossy().to_string();
        if let Some(expanded) = cache.project_expanded.get(&project_key) {
            project.expanded = *expanded;
        }
        for worktree in &mut project.worktrees {
            let key = worktree.path.to_string_lossy().to_string();
            if let Some(expanded) = cache.worktree_expanded.get(&key) {
                worktree.expanded = *expanded;
            }
            for session in &mut worktree.sessions {
                session.muted = cache.muted_sessions.contains(&session.pane_id);
            }
        }
    }
    (
        cache.tree_selected,
        cache.cursor_identity,
        cache.command_history,
        cache.active_tab,
        cache.muted_sessions,
    )
}

pub fn find_cursor_index(
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    id: &CursorIdentity,
) -> Option<usize> {
    match id {
        CursorIdentity::Project { path } => flat.iter().position(|entry| {
            matches!(entry, FlatEntry::Project { idx } if workspace.projects[*idx].path.to_string_lossy() == path.as_str())
        }),
        CursorIdentity::Worktree { path } => flat.iter().position(|entry| {
            matches!(entry, FlatEntry::Worktree { project_idx, worktree_idx } if workspace.projects[*project_idx].worktrees[*worktree_idx].path.to_string_lossy() == path.as_str())
        }),
        CursorIdentity::Session { worktree_path, pane_id } => flat.iter().position(|entry| {
            if let FlatEntry::Session { project_idx, worktree_idx, session_idx } = entry {
                let wt = &workspace.projects[*project_idx].worktrees[*worktree_idx];
                wt.path.to_string_lossy() == worktree_path.as_str() && wt.sessions[*session_idx].pane_id == *pane_id
            } else { false }
        }),
        CursorIdentity::RoutinesHeader { project_path } => flat.iter().position(|entry| {
            matches!(entry, FlatEntry::RoutinesHeader { project_idx } if workspace.projects[*project_idx].path.to_string_lossy() == project_path.as_str())
        }),
        CursorIdentity::Routine { project_path, routine_name } => flat.iter().position(|entry| {
            matches!(entry, FlatEntry::Routine { project_idx, routine_idx } if workspace.projects[*project_idx].path.to_string_lossy() == project_path.as_str() && workspace.projects[*project_idx].routines[*routine_idx].routine.name == *routine_name)
        }),
    }
}

pub fn save_cache(
    workspace: &WorkspaceState,
    tree_selected: usize,
    flat: &[FlatEntry],
    command_history: &[String],
    active_tab: Option<&str>,
    sync: bool,
) -> Option<String> {
    let mut cache = WorkspaceCache {
        written_at_unix_ms: Some(now_unix_ms()),
        tree_selected,
        cursor_identity: resolve_cursor_identity(workspace, flat, tree_selected),
        command_history: command_history.to_vec(),
        active_tab: active_tab.map(str::to_owned),
        ..Default::default()
    };
    for project in &workspace.projects {
        cache.project_expanded.insert(
            project.path.to_string_lossy().into_owned(),
            project.expanded,
        );
        for worktree in &project.worktrees {
            cache.worktree_expanded.insert(
                worktree.path.to_string_lossy().into_owned(),
                worktree.expanded,
            );
            cache.muted_sessions.extend(
                worktree
                    .sessions
                    .iter()
                    .filter(|s| s.muted)
                    .map(|s| s.pane_id.clone()),
            );
        }
    }
    cache
        .save(sync)
        .err()
        .map(|e| format!("cache save failed: {e}"))
}

pub fn resolve_cursor_identity(
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    idx: usize,
) -> Option<CursorIdentity> {
    match flat.get(idx)? {
        FlatEntry::Project { idx } => Some(CursorIdentity::Project {
            path: workspace.projects[*idx].path.to_string_lossy().into_owned(),
        }),
        FlatEntry::Worktree {
            project_idx,
            worktree_idx,
        } => Some(CursorIdentity::Worktree {
            path: workspace.projects[*project_idx].worktrees[*worktree_idx]
                .path
                .to_string_lossy()
                .into_owned(),
        }),
        FlatEntry::Session {
            project_idx,
            worktree_idx,
            session_idx,
        } => {
            let wt = &workspace.projects[*project_idx].worktrees[*worktree_idx];
            Some(CursorIdentity::Session {
                worktree_path: wt.path.to_string_lossy().into_owned(),
                pane_id: wt.sessions[*session_idx].pane_id.clone(),
            })
        }
        FlatEntry::RoutinesHeader { project_idx } => Some(CursorIdentity::RoutinesHeader {
            project_path: workspace.projects[*project_idx]
                .path
                .to_string_lossy()
                .into_owned(),
        }),
        FlatEntry::Routine {
            project_idx,
            routine_idx,
        } => Some(CursorIdentity::Routine {
            project_path: workspace.projects[*project_idx]
                .path
                .to_string_lossy()
                .into_owned(),
            routine_name: workspace.projects[*project_idx].routines[*routine_idx]
                .routine
                .name
                .clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tmux_and_session_fields_are_ignored() {
        let cache: WorkspaceCache = toml::from_str(
            r#"tmux_server_pid = 123
sessions = { "/tmp/repo" = ["old-tmux-session"] }
muted_sessions = ["pane-1"]
"#,
        )
        .unwrap();
        assert_eq!(cache.muted_sessions, HashSet::from(["pane-1".to_string()]));
    }

    #[test]
    fn cursor_identity_round_trips_through_stable_pane_id() {
        use crate::{
            herdr::AgentStatus,
            model::workspace::{flatten_tree, Project, SessionInfo, WorktreeInfo},
        };
        let workspace = WorkspaceState {
            projects: vec![Project {
                name: "repo".into(),
                path: "/repo".into(),
                default_branch: "main".into(),
                worktrees: vec![WorktreeInfo {
                    name: "main".into(),
                    branch: "main".into(),
                    path: "/repo".into(),
                    is_main: true,
                    alias: None,
                    sessions: vec![SessionInfo {
                        pane_id: "pane-1".into(),
                        terminal_id: "terminal-1".into(),
                        workspace_id: "workspace-1".into(),
                        tab_id: "tab-1".into(),
                        display_name: "agent".into(),
                        agent_status: AgentStatus::Working,
                        revision: 1,
                        pane_capture: None,
                        muted: false,
                    }],
                    expanded: true,
                    git_info: None,
                    fetch_failed: false,
                    fetch_fail_count: 0,
                    fetch_fail_reason: None,
                    last_fetched: None,
                    git_info_fetched_at: None,
                }],
                routines: vec![],
                routine_revision: 0,
                routines_expanded: true,
                config: None,
                expanded: true,
                missing: false,
            }],
        };
        let flat = flatten_tree(&workspace);
        let identity = resolve_cursor_identity(&workspace, &flat, 2).unwrap();
        assert_eq!(
            identity,
            CursorIdentity::Session {
                worktree_path: "/repo".into(),
                pane_id: "pane-1".into(),
            }
        );
        assert_eq!(find_cursor_index(&workspace, &flat, &identity), Some(2));
    }
}
