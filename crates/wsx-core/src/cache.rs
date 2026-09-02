//! Persistent wsx UI state, local mute flags, and acknowledged outcomes.
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
    /// Provider outcome revisions acknowledged by explicit interaction, keyed by terminal ID.
    #[serde(default)]
    pub acknowledged_outcomes: HashMap<String, u64>,
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
    acknowledged_outcomes: HashMap<String, u64>,
    active_group: Option<toml::Value>,
    active_groups: Option<toml::Value>,
    active_tab: Option<toml::Value>,
    integration_prompt_version: Option<String>,
}

impl<'de> Deserialize<'de> for WorkspaceCache {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceCacheWire::deserialize(deserializer)?;
        // ^ Group selection is process-local. Reading any historical selector requests a
        // canonical rewrite that strips it instead of restoring stale UI state.
        let migration_needed = wire.active_group.is_some()
            || wire.active_groups.is_some()
            || wire.active_tab.is_some();
        Ok(Self {
            written_at_unix_ms: wire.written_at_unix_ms,
            worktree_expanded: wire.worktree_expanded,
            project_expanded: wire.project_expanded,
            tree_selected: wire.tree_selected,
            cursor_identity: wire.cursor_identity,
            muted_terminals: wire.muted_terminals,
            acknowledged_outcomes: wire.acknowledged_outcomes,
            integration_prompt_version: wire.integration_prompt_version,
            migration_needed,
        })
    }
}

impl WorkspaceCache {
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from_paths(&cache_path(), &legacy_cache_path())
    }

    fn load_from_paths(
        canonical: &std::path::Path,
        legacy: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let (content, imported_legacy) = match std::fs::read_to_string(canonical) {
            Ok(content) => (content, false),
            Err(_) if !canonical.exists() => match std::fs::read_to_string(legacy) {
                Ok(content) => (content, true),
                Err(_) => return Ok(Self::default()),
            },
            Err(_) => return Ok(Self::default()),
        };
        let mut cache: Self = toml::from_str(&content).unwrap_or_default();
        if imported_legacy || cache.migration_needed {
            cache.save_to(canonical, false)?;
            cache.migration_needed = false;
        }
        Ok(cache)
    }

    pub fn save(&self, sync: bool) -> anyhow::Result<()> {
        self.save_to(&cache_path(), sync)
    }

    fn save_to(&self, path: &std::path::Path, sync: bool) -> anyhow::Result<()> {
        let mut cache = self.clone();
        cache.written_at_unix_ms = Some(now_unix_ms());
        let text = toml::to_string(&cache)?;
        atomic_write_private(path, text.as_bytes(), sync)?;
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
        .join("workspace-v2.toml")
}

fn legacy_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace.toml")
}

#[derive(Serialize, Deserialize)]
struct GroupSelection {
    selected: GroupKey,
}

fn group_selection_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("group-selection-v1.toml")
}

pub fn load_group_selection() -> anyhow::Result<Option<GroupKey>> {
    load_group_selection_from(&group_selection_path())
}

fn load_group_selection_from(path: &std::path::Path) -> anyhow::Result<Option<GroupKey>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(toml::from_str::<GroupSelection>(&content)
        .ok()
        .map(|selection| selection.selected))
}

pub fn save_group_selection(selected: &GroupKey) -> anyhow::Result<()> {
    save_group_selection_to(&group_selection_path(), selected)
}

fn save_group_selection_to(path: &std::path::Path, selected: &GroupKey) -> anyhow::Result<()> {
    let text = toml::to_string(&GroupSelection {
        selected: selected.clone(),
    })?;
    atomic_write_private(path, text.as_bytes(), true)?;
    Ok(())
}

pub type AppliedCache = (
    usize,
    Option<CursorIdentity>,
    HashSet<String>,
    HashMap<String, u64>,
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
                for pane in &mut session.panes {
                    pane.outcome_acknowledged = pane.agent_status
                        == crate::runtime::AgentState::Done
                        && cache
                            .acknowledged_outcomes
                            .get(&pane.terminal_id.to_string())
                            == Some(&pane.revision);
                }
                session.outcome_acknowledged = session
                    .panes
                    .iter()
                    .find(|pane| pane.pane_id == session.pane_id)
                    .is_some_and(|pane| pane.outcome_acknowledged);
            }
        }
    }
    Ok((
        cache.tree_selected,
        cache.cursor_identity,
        migrated_muted_terminals,
        cache.acknowledged_outcomes,
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
    integration_prompt_version: Option<&str>,
    sync: bool,
) -> Option<String> {
    let mut cache = WorkspaceCache {
        written_at_unix_ms: Some(now_unix_ms()),
        tree_selected,
        cursor_identity: resolve_cursor_identity(workspace, flat, tree_selected),
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
            for session in &worktree.sessions {
                for pane in &session.panes {
                    if pane.outcome_acknowledged {
                        cache
                            .acknowledged_outcomes
                            .insert(pane.terminal_id.to_string(), pane.revision);
                    }
                }
            }
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
    fn acknowledged_outcome_revisions_round_trip() {
        let cache = WorkspaceCache {
            acknowledged_outcomes: HashMap::from([("42".into(), 7)]),
            ..Default::default()
        };

        let decoded: WorkspaceCache = toml::from_str(&toml::to_string(&cache).unwrap()).unwrap();

        assert_eq!(decoded.acknowledged_outcomes.get("42"), Some(&7));
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
    fn historical_active_group_shapes_are_discarded_on_rewrite() {
        for historical in [
            "active_group = \"work\"\n",
            "active_tab = \"work\"\n",
            "active_groups = [\"work\", \"other\"]\n",
            "active_groups = []\n",
        ] {
            let cache: WorkspaceCache = toml::from_str(historical).unwrap();
            assert!(cache.migration_needed);
            let encoded = toml::to_string(&cache).unwrap();
            assert!(!encoded.contains("active_group"));
            assert!(!encoded.contains("active_groups"));
            assert!(!encoded.contains("active_tab"));
        }
    }

    #[test]
    fn group_selection_is_independent_and_malformed_data_defaults_absent() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::current_dir()
            .unwrap()
            .join(".work/group-selection-tests")
            .join(format!("{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("group-selection-v1.toml");

        assert_eq!(load_group_selection_from(&path).unwrap(), None);
        save_group_selection_to(&path, &GroupKey::Named("work".into())).unwrap();
        assert_eq!(
            load_group_selection_from(&path).unwrap(),
            Some(GroupKey::Named("work".into()))
        );
        std::fs::write(&path, "selected = [\n").unwrap();
        assert_eq!(load_group_selection_from(&path).unwrap(), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_cache_serialization_never_carries_group_selection() {
        let encoded = toml::to_string(&WorkspaceCache::default()).unwrap();
        assert!(!encoded.contains("selected_group"));
        assert!(!encoded.contains("active_group"));
    }

    #[test]
    fn first_v2_cache_load_imports_active_tab_without_rewriting_legacy() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::current_dir()
            .unwrap()
            .join(".work/cache-v2-tests")
            .join(format!("{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let canonical = directory.join("workspace-v2.toml");
        let legacy = directory.join("workspace.toml");
        let legacy_text = "active_tab = \"personal\"\ntree_selected = 3\n";
        std::fs::write(&legacy, legacy_text).unwrap();

        let cache = WorkspaceCache::load_from_paths(&canonical, &legacy).unwrap();

        assert_eq!(cache.tree_selected, 3);
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), legacy_text);
        let canonical_text = std::fs::read_to_string(&canonical).unwrap();
        assert!(!canonical_text.contains("active_group"));
        assert!(!canonical_text.contains("active_tab"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_v2_cache_wins_without_falling_back_to_legacy() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::current_dir()
            .unwrap()
            .join(".work/cache-v2-tests")
            .join(format!("malformed-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let canonical = directory.join("workspace-v2.toml");
        let legacy = directory.join("workspace.toml");
        std::fs::write(&canonical, "active_group = [\n").unwrap();
        std::fs::write(&legacy, "active_tab = \"personal\"\n").unwrap();

        let cache = WorkspaceCache::load_from_paths(&canonical, &legacy).unwrap();

        assert_eq!(cache.tree_selected, 0);
        assert_eq!(
            std::fs::read_to_string(&canonical).unwrap(),
            "active_group = [\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
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
                        outcome_acknowledged: false,
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
