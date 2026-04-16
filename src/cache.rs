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

/// Atomic write: write to a `.toml.tmp` sibling then rename over the target.
fn write_atomic(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace.toml")
}

fn session_snapshot_path() -> Option<PathBuf> {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("wsx").join("sessions.toml"))
}

/// Collect session names per worktree path — shared by snapshot write and restore fallback.
pub fn collect_session_names(workspace: &WorkspaceState) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();
    for project in &workspace.projects {
        for wt in &project.worktrees {
            let names = wt.session_names();
            if !names.is_empty() {
                map.insert(wt.path.to_string_lossy().into_owned(), names);
            }
        }
    }
    map
}

/// Persist session names to Application Support — survives tmux crashes because
/// it's outside the cache and written whenever sessions change.
pub fn save_session_snapshot(workspace: &WorkspaceState) {
    let Some(path) = session_snapshot_path() else { return };
    save_snapshot_to(workspace, &path);
}

pub(crate) fn save_snapshot_to(workspace: &WorkspaceState, path: &std::path::Path) {
    let map = collect_session_names(workspace);
    let Ok(s) = toml::to_string(&map) else { return };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() { return; }
    }
    let _ = write_atomic(path, s.as_bytes());
}

/// Load the session snapshot written by `save_session_snapshot`.
pub fn load_session_snapshot() -> HashMap<String, Vec<String>> {
    let Some(path) = session_snapshot_path() else { return HashMap::new() };
    load_snapshot_from(&path)
}

pub(crate) fn load_snapshot_from(path: &std::path::Path) -> HashMap<String, Vec<String>> {
    let Ok(content) = std::fs::read_to_string(path) else { return HashMap::new() };
    toml::from_str(&content).unwrap_or_default()
}

/// Return type for `apply_cache`. Last two fields are for one-time tmux flag migration.
#[allow(clippy::type_complexity)]
type CacheResult = (usize, Option<CursorIdentity>, Vec<String>, Option<String>, Option<u32>, HashSet<String>, HashSet<String>);

/// Pre-populate workspace with cached state before first live sync.
pub fn apply_cache(workspace: &mut WorkspaceState) -> CacheResult {
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
    (cache.tree_selected, cache.cursor_identity, cache.command_history, cache.active_tab, cache.tmux_server_pid, cache.muted_sessions, cache.suppressed_sessions)
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
            // ^ muted_sessions and suppressed_sessions intentionally omitted:
            // these flags are stored as tmux user options (@wsx-muted, @wsx-suppressed)
            // so all instances share them without cache coordination.
        }
    }
    cache.command_history = command_history.to_vec();
    cache.tmux_server_pid = crate::tmux::session::server_pid();
    cache.save(sync).err().map(|e| format!("cache save failed: {e}"))
}

/// One-time migration: write cached muted/suppressed names as tmux user options so they
/// survive future cache writes and are visible to all instances. Idempotent and non-fatal.
pub fn migrate_flags_to_tmux(muted: &HashSet<String>, suppressed: &HashSet<String>) {
    use crate::tmux::session::{OPT_MUTED, OPT_SUPPRESSED, set_session_opt};
    for name in muted {
        set_session_opt(name, OPT_MUTED, "1");
    }
    for name in suppressed {
        set_session_opt(name, OPT_SUPPRESSED, "1");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::{Project, SessionInfo, WorktreeInfo};

    fn make_session(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            display_name: name.into(),
            has_activity: false,
            pane_capture: None,
            last_activity: None,
            has_running_app: false,
            running_app_suppressed: false,
            muted: false,
        }
    }

    fn make_worktree(path: &str, sessions: &[&str]) -> WorktreeInfo {
        WorktreeInfo {
            name: "main".into(),
            branch: "main".into(),
            path: std::path::PathBuf::from(path),
            is_main: true,
            alias: None,
            sessions: sessions.iter().map(|s| make_session(s)).collect(),
            expanded: true,
            git_info: None,
            fetch_failed: false,
            fetch_fail_count: 0,
            fetch_fail_reason: None,
            last_fetched: None,
            git_info_fetched_at: None,
        }
    }

    fn make_workspace(worktrees: &[(&str, &[&str])]) -> WorkspaceState {
        WorkspaceState {
            projects: vec![Project {
                name: "test".into(),
                path: std::path::PathBuf::from("/tmp/test"),
                default_branch: "main".into(),
                worktrees: worktrees
                    .iter()
                    .map(|(path, sessions)| make_worktree(path, sessions))
                    .collect(),
                config: None,
                expanded: true,
            }],
        }
    }

    // ── collect_session_names (regression + new) ─────────────────────────────

    #[test]
    fn collect_session_names_maps_by_path() {
        let ws = make_workspace(&[
            ("/tmp/proj", &["proj-main-claude", "proj-main-shell"]),
        ]);
        let map = collect_session_names(&ws);
        assert_eq!(map["/tmp/proj"], vec!["proj-main-claude", "proj-main-shell"]);
    }

    #[test]
    fn collect_session_names_skips_empty_worktrees() {
        let ws = make_workspace(&[
            ("/tmp/proj-a", &["proj-a-claude"]),
            ("/tmp/proj-b", &[]),
        ]);
        let map = collect_session_names(&ws);
        assert!(map.contains_key("/tmp/proj-a"));
        assert!(!map.contains_key("/tmp/proj-b"));
    }

    #[test]
    fn collect_session_names_empty_workspace_returns_empty_map() {
        let ws = make_workspace(&[]);
        assert!(collect_session_names(&ws).is_empty());
    }

    // ── snapshot roundtrip ───────────────────────────────────────────────────

    #[test]
    fn snapshot_roundtrip_via_path() {
        let dir = std::env::temp_dir().join("wsx_test_snapshot_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.toml");

        let ws = make_workspace(&[
            ("/tmp/proj-a", &["proj-a-claude"]),
            ("/tmp/proj-b", &["proj-b-shell", "proj-b-build"]),
        ]);

        save_snapshot_to(&ws, &path);
        let loaded = load_snapshot_from(&path);

        assert_eq!(loaded["/tmp/proj-a"], vec!["proj-a-claude"]);
        assert_eq!(loaded["/tmp/proj-b"], vec!["proj-b-shell", "proj-b-build"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_load_missing_file_returns_empty() {
        let path = std::path::Path::new("/tmp/wsx_nonexistent_snapshot.toml");
        assert!(load_snapshot_from(path).is_empty());
    }

    #[test]
    fn snapshot_empty_workspace_writes_and_loads_empty() {
        let dir = std::env::temp_dir().join("wsx_test_snapshot_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.toml");

        let ws = make_workspace(&[]);
        save_snapshot_to(&ws, &path);
        assert!(load_snapshot_from(&path).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
