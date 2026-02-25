// Startup cache — persists last known sessions + expand state.
// Loaded before first refresh_all() so the tree is populated immediately.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use crate::model::workspace::{SessionInfo, WorkspaceState};

#[derive(Serialize, Deserialize, Default)]
pub struct WorkspaceCache {
    /// worktree path → session names
    pub sessions: HashMap<String, Vec<String>>,
    /// worktree path → expanded
    pub worktree_expanded: HashMap<String, bool>,
    /// project path → expanded
    pub project_expanded: HashMap<String, bool>,
    /// last cursor position in the flat tree
    pub tree_selected: usize,
}

impl WorkspaceCache {
    pub fn load() -> Self {
        let Ok(content) = std::fs::read_to_string(cache_path()) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = toml::to_string(self) {
            let _ = std::fs::write(path, s);
        }
    }
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace.toml")
}

/// Pre-populate workspace with cached state before first live sync.
/// Returns the last saved cursor position.
pub fn apply_cache(workspace: &mut WorkspaceState) -> usize {
    let cache = WorkspaceCache::load();
    for project in &mut workspace.projects {
        let proj_key = project.path.to_string_lossy().to_string();
        if let Some(&expanded) = cache.project_expanded.get(&proj_key) {
            project.expanded = expanded;
        }
        for wt in &mut project.worktrees {
            let key = wt.path.to_string_lossy().to_string();
            if let Some(&expanded) = cache.worktree_expanded.get(&key) {
                wt.expanded = expanded;
            }
            if let Some(names) = cache.sessions.get(&key) {
                wt.sessions = names.iter().map(|name| SessionInfo {
                    name: name.clone(),
                    display_name: name.clone(), // recomputed on next refresh_all()
                    has_activity: false,
                    pane_capture: None,
                    last_activity: None,
                    was_active: false,
                }).collect();
            }
        }
    }
    cache.tree_selected
}

/// Persist session names, expand states, and cursor position.
pub fn save_cache(workspace: &WorkspaceState, tree_selected: usize) {
    let mut cache = WorkspaceCache::default();
    cache.tree_selected = tree_selected;
    for project in &workspace.projects {
        let proj_key = project.path.to_string_lossy().to_string();
        cache.project_expanded.insert(proj_key, project.expanded);
        for wt in &project.worktrees {
            let key = wt.path.to_string_lossy().to_string();
            cache.sessions.insert(key.clone(), wt.sessions.iter().map(|s| s.name.clone()).collect());
            cache.worktree_expanded.insert(key, wt.expanded);
        }
    }
    cache.save();
}
