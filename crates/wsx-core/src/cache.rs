//! Persistent wsx UI state and local mute flags.
//!
//! The wsx daemon is authoritative for sessions. Legacy backend/session fields
//! in older TOML files are ignored by serde and are never imported.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    config::global::{atomic_write_private, GroupKey},
    model::workspace::{FlatEntry, WorkspaceState},
};
use serde::{Deserialize, Deserializer, Serialize};

/// Stable cursor identity for projects, worktrees, terminal panes, and routines.
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_id: Option<String>,
        /// Legacy identity read once and migrated through the live snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
    },
    RoutinesHeader {
        project_path: String,
    },
    Routine {
        project_path: String,
        routine_name: String,
    },
}

#[derive(Serialize, Default, Clone)]
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
    /// Stable wsx terminal IDs muted in this local UI.
    #[serde(default)]
    pub muted_terminals: HashSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_group: Option<GroupKey>,
    /// wsx version for which the user dismissed the integration setup prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_prompt_version: Option<String>,
    #[serde(skip)]
    migration_needed: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WorkspaceCacheWire {
    written_at_unix_ms: Option<u64>,
    worktree_expanded: HashMap<String, bool>,
    project_expanded: HashMap<String, bool>,
    tree_selected: usize,
    cursor_identity: Option<CursorIdentity>,
    #[serde(alias = "muted_sessions")]
    muted_terminals: HashSet<String>,
    active_group: Option<String>,
    active_groups: Option<Vec<String>>,
    active_tab: Option<String>,
    integration_prompt_version: Option<String>,
}

impl<'de> Deserialize<'de> for WorkspaceCache {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceCacheWire::deserialize(deserializer)?;
        let migration_needed = wire.active_groups.is_some() || wire.active_tab.is_some();
        let raw_active = wire
            .active_group
            .or_else(|| wire.active_groups.into_iter().flatten().next())
            .or(wire.active_tab);
        let active_group = raw_active
            .map(|value| {
                if value.eq_ignore_ascii_case("default") {
                    Ok(GroupKey::Ungrouped)
                } else {
                    toml::Value::String(value)
                        .try_into()
                        .map_err(serde::de::Error::custom)
                }
            })
            .transpose()?;
        Ok(Self {
            written_at_unix_ms: wire.written_at_unix_ms,
            worktree_expanded: wire.worktree_expanded,
            project_expanded: wire.project_expanded,
            tree_selected: wire.tree_selected,
            cursor_identity: wire.cursor_identity,
            muted_terminals: wire.muted_terminals,
            active_group,
            integration_prompt_version: wire.integration_prompt_version,
            migration_needed,
        })
    }
}

impl WorkspaceCache {
    pub fn load() -> anyhow::Result<Self> {
        let Ok(content) = std::fs::read_to_string(cache_path()) else {
            return Ok(Self::default());
        };
        let mut cache: Self = toml::from_str(&content).unwrap_or_default();
        if cache.migration_needed {
            cache.save(false)?;
            cache.migration_needed = false;
        }
        Ok(cache)
    }

    pub fn save(&self, sync: bool) -> anyhow::Result<()> {
        let mut cache = self.clone();
        cache.written_at_unix_ms = Some(now_unix_ms());
        let path = cache_path();
        let text = toml::to_string(&cache)?;
        atomic_write_private(&path, text.as_bytes(), sync)?;
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

pub type AppliedCache = (
    usize,
    Option<CursorIdentity>,
    Option<GroupKey>,
    HashSet<String>,
    Option<String>,
);

/// Apply only cached UI and local mute state. Sessions always come from wsxd.
pub fn apply_cache(workspace: &mut WorkspaceState) -> anyhow::Result<AppliedCache> {
    let cache = WorkspaceCache::load()?;
    let mut migrated_muted_terminals = HashSet::new();
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
                session.muted = cache
                    .muted_terminals
                    .contains(&session.terminal_id.to_string())
                    || cache.muted_terminals.contains(&session.pane_id.to_string());
                if session.muted {
                    migrated_muted_terminals.insert(session.terminal_id.to_string());
                }
            }
        }
    }
    Ok((
        cache.tree_selected,
        cache.cursor_identity,
        cache.active_group,
        migrated_muted_terminals,
        cache.integration_prompt_version,
    ))
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
        CursorIdentity::Session {
            worktree_path,
            terminal_id,
            pane_id,
        } => flat.iter().position(|entry| {
            let (project_idx, worktree_idx, session_idx, pane_idx) = match entry {
                FlatEntry::Session { project_idx, worktree_idx, session_idx } => {
                    (*project_idx, *worktree_idx, *session_idx, None)
                }
                FlatEntry::Pane { project_idx, worktree_idx, session_idx, pane_idx } => {
                    (*project_idx, *worktree_idx, *session_idx, Some(*pane_idx))
                }
                _ => return false,
            };
            let wt = &workspace.projects[project_idx].worktrees[worktree_idx];
            let session = &wt.sessions[session_idx];
            let (terminal, pane) = pane_idx
                .and_then(|idx| session.panes.get(idx))
                .map_or((session.terminal_id, session.pane_id), |pane| (pane.terminal_id, pane.pane_id));
            wt.path.to_string_lossy() == worktree_path.as_str()
                && terminal_id
                    .as_ref()
                    .map(|id| terminal.to_string() == *id)
                    .or_else(|| pane_id.as_ref().map(|id| pane.to_string() == *id))
                    .unwrap_or(false)
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
    active_group: Option<&GroupKey>,
    integration_prompt_version: Option<&str>,
    sync: bool,
) -> Option<String> {
    let mut cache = WorkspaceCache {
        written_at_unix_ms: Some(now_unix_ms()),
        tree_selected,
        cursor_identity: resolve_cursor_identity(workspace, flat, tree_selected),
        active_group: active_group.cloned(),
        integration_prompt_version: integration_prompt_version.map(str::to_owned),
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
            cache.muted_terminals.extend(
                worktree
                    .sessions
                    .iter()
                    .filter(|s| s.muted)
                    .map(|s| s.terminal_id.to_string()),
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
                terminal_id: Some(wt.sessions[*session_idx].terminal_id.to_string()),
                pane_id: None,
            })
        }
        FlatEntry::Pane {
            project_idx,
            worktree_idx,
            session_idx,
            pane_idx,
        } => {
            let wt = &workspace.projects[*project_idx].worktrees[*worktree_idx];
            Some(CursorIdentity::Session {
                worktree_path: wt.path.to_string_lossy().into_owned(),
                terminal_id: Some(
                    wt.sessions[*session_idx].panes[*pane_idx]
                        .terminal_id
                        .to_string(),
                ),
                pane_id: None,
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
    fn legacy_cache_defaults_missing_integration_prompt_version() {
        let cache: WorkspaceCache = toml::from_str("tree_selected = 2\n").unwrap();
        assert_eq!(cache.integration_prompt_version, None);
    }

    #[test]
    fn integration_prompt_version_round_trips() {
        let cache = WorkspaceCache {
            integration_prompt_version: Some("0.18.0".into()),
            ..Default::default()
        };
        let decoded: WorkspaceCache = toml::from_str(&toml::to_string(&cache).unwrap()).unwrap();
        assert_eq!(
            decoded.integration_prompt_version.as_deref(),
            Some("0.18.0")
        );
    }

    #[test]
    fn legacy_tmux_and_session_fields_are_ignored() {
        let cache: WorkspaceCache = toml::from_str(
            r#"tmux_server_pid = 123
sessions = { "/tmp/repo" = ["old-tmux-session"] }
muted_sessions = ["pane-1"]
"#,
        )
        .unwrap();
        assert_eq!(cache.muted_terminals, HashSet::from(["pane-1".to_string()]));
    }

    #[test]
    fn legacy_pane_cursor_identity_deserializes_for_live_migration() {
        let cache: WorkspaceCache = toml::from_str(
            r#"[cursor_identity.Session]
worktree_path = "/repo"
pane_id = "pane-1"
"#,
        )
        .unwrap();
        assert_eq!(
            cache.cursor_identity,
            Some(CursorIdentity::Session {
                worktree_path: "/repo".into(),
                terminal_id: None,
                pane_id: Some("pane-1".into()),
            })
        );
    }

    #[test]
    fn legacy_active_group_shapes_migrate_to_one_canonical_group() {
        for legacy in [
            "active_tab = \"work\"\n",
            "active_groups = [\"work\", \"other\"]\n",
        ] {
            let cache: WorkspaceCache = toml::from_str(legacy).unwrap();
            assert_eq!(cache.active_group, Some(GroupKey::Named("work".into())));
            assert!(cache.migration_needed);
            let encoded = toml::to_string(&cache).unwrap();
            assert!(encoded.contains("active_group = \"work\""));
            assert!(!encoded.contains("active_groups"));
            assert!(!encoded.contains("active_tab"));
        }
    }

    #[test]
    fn empty_multi_selection_cache_still_requests_scalar_rewrite() {
        let cache: WorkspaceCache = toml::from_str("active_groups = []\n").unwrap();
        assert_eq!(cache.active_group, None);
        assert!(cache.migration_needed);
        assert!(!toml::to_string(&cache).unwrap().contains("active_groups"));
    }

    #[test]
    fn cursor_identity_round_trips_through_stable_terminal_id() {
        use crate::{
            model::workspace::{flatten_tree, Project, SessionInfo, WorktreeInfo},
            runtime::{AgentState, PaneId, SessionId, TerminalId},
        };
        let workspace = WorkspaceState {
            projects: vec![Project {
                name: "repo".into(),
                path: "/repo".into(),
                default_branch: "main".into(),
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                worktrees: vec![WorktreeInfo {
                    name: "main".into(),
                    branch: "main".into(),
                    path: "/repo".into(),
                    is_main: true,
                    alias: None,
                    sessions: vec![SessionInfo {
                        session_id: SessionId(1),
                        pane_id: PaneId(1),
                        terminal_id: TerminalId(1),
                        agent: None,
                        display_name: "agent".into(),
                        agent_status: AgentState::Working,
                        revision: 1,
                        layout: crate::runtime::PaneLayout::Leaf { pane_id: PaneId(1) },
                        panes: vec![],
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
                terminal_id: Some("1".into()),
                pane_id: None,
            }
        );
        assert_eq!(find_cursor_index(&workspace, &flat, &identity), Some(2));
    }
}
