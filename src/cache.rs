// Startup cache — persists last known sessions + expand state.
// Loaded before first refresh_all() so the tree is populated immediately.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::model::workspace::{
    session_display_name_from_tmux, FlatEntry, SessionInfo, WorkspaceState,
};
use serde::{Deserialize, Serialize};

/// Stable cursor identity — survives session appear/disappear and expand-state changes.
#[derive(Serialize, Deserialize, Clone)]
pub enum CursorIdentity {
    Project {
        path: String,
    },
    Worktree {
        path: String,
    },
    Session {
        worktree_path: String,
        session_name: String,
    },
}

#[derive(Serialize, Deserialize, Default)]
pub struct WorkspaceCache {
    /// worktree path → session names
    pub sessions: HashMap<String, Vec<String>>,
    /// worktree path → expanded
    pub worktree_expanded: HashMap<String, bool>,
    /// project path → expanded
    pub project_expanded: HashMap<String, bool>,
    /// last cursor position in the flat tree (raw fallback)
    pub tree_selected: usize,
    /// stable cursor identity (preferred over raw index)
    #[serde(default)]
    pub cursor_identity: Option<CursorIdentity>,
    /// session names where the user dismissed the running-app notification
    #[serde(default)]
    pub suppressed_sessions: HashSet<String>,
    /// session names the user has muted (no activity updates, shown as ⊘)
    #[serde(default)]
    pub muted_sessions: HashSet<String>,
    /// global send-command history (Shift+S), newest last, capped at 50
    #[serde(default)]
    pub command_history: Vec<String>,
    /// last active tab name (None = default tab)
    #[serde(default)]
    pub active_tab: Option<String>,
    /// tmux server PID at last save — used to detect server restart on next launch
    #[serde(default)]
    pub tmux_server_pid: Option<u32>,
}

impl WorkspaceCache {
    pub fn load() -> Self {
        let Ok(content) = std::fs::read_to_string(cache_path()) else {
            return Self::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self, sync: bool) -> anyhow::Result<()> {
        let path = cache_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = toml::to_string(self)?;
        // Atomic write: temp file + rename so a crash mid-write leaves the original intact.
        let tmp = path.with_extension("toml.tmp");
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, s.as_bytes())?;
        if sync {
            f.sync_all()?;
        }
        drop(f);
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace.toml")
}

/// Pre-populate workspace with cached state before first live sync.
/// Returns (raw_index, identity, command_history, active_tab, tmux_server_pid).
pub fn apply_cache(workspace: &mut WorkspaceState) -> (usize, Option<CursorIdentity>, Vec<String>, Option<String>, Option<u32>) {
    let cache = WorkspaceCache::load();
    for project in &mut workspace.projects {
        let proj_key = project.path.to_string_lossy().to_string();
        let cached = cache.project_expanded.get(&proj_key).copied();
        if let Some(expanded) = cached {
            project.expanded = expanded;
        }
        for wt in &mut project.worktrees {
            let key = wt.path.to_string_lossy().to_string();
            if let Some(&expanded) = cache.worktree_expanded.get(&key) {
                wt.expanded = expanded;
            }
            if let Some(names) = cache.sessions.get(&key) {
                wt.sessions = names
                    .iter()
                    .map(|name| {
                        let display_name = session_display_name_from_tmux(
                            name,
                            &project.name,
                            &wt.path,
                            &wt.branch,
                            wt.alias.as_deref(),
                        );
                        SessionInfo {
                            name: name.clone(),
                            display_name,
                            has_activity: false,
                            pane_capture: None,
                            last_activity: None,
                            has_running_app: false,
                            running_app_suppressed: cache.suppressed_sessions.contains(name),
                            muted: cache.muted_sessions.contains(name),
                        }
                    })
                    .collect();
            }
        }
    }
    (cache.tree_selected, cache.cursor_identity, cache.command_history, cache.active_tab, cache.tmux_server_pid)
}

/// Resolve a saved CursorIdentity back to a flat-tree index.
pub fn find_cursor_index(
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    id: &CursorIdentity,
) -> Option<usize> {
    match id {
        CursorIdentity::Project { path } => flat.iter().position(|e| {
            if let FlatEntry::Project { idx } = e {
                workspace.projects[*idx].path.to_string_lossy() == path.as_str()
            } else {
                false
            }
        }),
        CursorIdentity::Worktree { path } => flat.iter().position(|e| {
            if let FlatEntry::Worktree {
                project_idx: pi,
                worktree_idx: wi,
            } = e
            {
                workspace.projects[*pi].worktrees[*wi]
                    .path
                    .to_string_lossy()
                    == path.as_str()
            } else {
                false
            }
        }),
        CursorIdentity::Session {
            worktree_path,
            session_name,
        } => flat.iter().position(|e| {
            if let FlatEntry::Session {
                project_idx: pi,
                worktree_idx: wi,
                session_idx: si,
            } = e
            {
                let wt = &workspace.projects[*pi].worktrees[*wi];
                wt.path.to_string_lossy() == worktree_path.as_str()
                    && wt.sessions[*si].name == *session_name
            } else {
                false
            }
        }),
    }
}

/// Persist session names, expand states, cursor position, active_tab, and command history.
/// Returns an error string if the save fails (caller should surface it in TUI).
pub fn save_cache(
    workspace: &WorkspaceState,
    tree_selected: usize,
    flat: &[FlatEntry],
    command_history: &[String],
    active_tab: Option<&str>,
    sync: bool,
) -> Option<String> {
    let mut cache = WorkspaceCache::default();
    cache.tree_selected = tree_selected;
    cache.cursor_identity = resolve_cursor_identity(workspace, flat, tree_selected);
    cache.active_tab = active_tab.map(|s| s.to_string());
    for project in &workspace.projects {
        let proj_key = project.path.to_string_lossy().to_string();
        cache.project_expanded.insert(proj_key, project.expanded);
        for wt in &project.worktrees {
            let key = wt.path.to_string_lossy().to_string();
            cache.sessions.insert(
                key.clone(),
                wt.sessions.iter().map(|s| s.name.clone()).collect(),
            );
            cache.worktree_expanded.insert(key, wt.expanded);
            for s in &wt.sessions {
                if s.running_app_suppressed {
                    cache.suppressed_sessions.insert(s.name.clone());
                }
                if s.muted {
                    cache.muted_sessions.insert(s.name.clone());
                }
            }
        }
    }
    cache.command_history = command_history.to_vec();
    cache.tmux_server_pid = crate::tmux::session::server_pid();
    cache.save(sync).err().map(|e| format!("cache save failed: {e}"))
}

fn resolve_cursor_identity(
    workspace: &WorkspaceState,
    flat: &[FlatEntry],
    idx: usize,
) -> Option<CursorIdentity> {
    match flat.get(idx)? {
        FlatEntry::Project { idx: pi } => Some(CursorIdentity::Project {
            path: workspace.projects[*pi].path.to_string_lossy().to_string(),
        }),
        FlatEntry::Worktree {
            project_idx: pi,
            worktree_idx: wi,
        } => {
            let wt = &workspace.projects[*pi].worktrees[*wi];
            Some(CursorIdentity::Worktree {
                path: wt.path.to_string_lossy().to_string(),
            })
        }
        FlatEntry::Session {
            project_idx: pi,
            worktree_idx: wi,
            session_idx: si,
        } => {
            let wt = &workspace.projects[*pi].worktrees[*wi];
            Some(CursorIdentity::Session {
                worktree_path: wt.path.to_string_lossy().to_string(),
                session_name: wt.sessions[*si].name.clone(),
            })
        }
    }
}
