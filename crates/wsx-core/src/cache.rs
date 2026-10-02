//! Persistent wsx UI state, local mute flags, and acknowledged outcomes.
//!
//! The wsx daemon is authoritative for sessions. Legacy backend/session fields
//! in older TOML files are ignored by serde and are never imported.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::{fs::MetadataExt, fs::OpenOptionsExt, io::AsRawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
    config::global::{
        atomic_write_private, GroupKey, ADAPTIVE_COLLAPSE_MAX_HOURS, ADAPTIVE_COLLAPSE_STEP_HOURS,
        MILLIS_PER_HOUR,
    },
    model::workspace::{FlatEntry, WorkspaceState},
};
use serde::{Deserialize, Deserializer, Serialize};

#[cfg(test)]
#[path = "cache_intent_tests.rs"]
mod intent_tests;

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

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdaptiveCollapseState {
    #[serde(default)]
    pub base_hours: u64,
    pub window_hours: u64,
    pub last_activity_unix_ms: u64,
    pub last_credit_unix_ms: u64,
}

impl AdaptiveCollapseState {
    pub fn new(base_hours: u64, activity_unix_ms: u64) -> Self {
        Self {
            base_hours,
            window_hours: base_hours.min(ADAPTIVE_COLLAPSE_MAX_HOURS),
            last_activity_unix_ms: activity_unix_ms,
            last_credit_unix_ms: activity_unix_ms,
        }
    }

    pub fn observe(&mut self, base_hours: u64, activity_unix_ms: u64) -> bool {
        let base_hours = base_hours.min(ADAPTIVE_COLLAPSE_MAX_HOURS);
        let previous = *self;
        if self.base_hours != base_hours {
            *self = Self::new(base_hours, activity_unix_ms);
            return *self != previous;
        }
        self.window_hours = self
            .window_hours
            .clamp(base_hours, ADAPTIVE_COLLAPSE_MAX_HOURS);
        if activity_unix_ms <= self.last_activity_unix_ms {
            return *self != previous;
        }
        let window_ms = self.window_hours.saturating_mul(MILLIS_PER_HOUR);
        if activity_unix_ms.saturating_sub(self.last_activity_unix_ms) > window_ms {
            *self = Self::new(base_hours, activity_unix_ms);
            return *self != previous;
        }
        self.last_activity_unix_ms = activity_unix_ms;
        let millis_per_day = 24 * MILLIS_PER_HOUR;
        if activity_unix_ms / millis_per_day > self.last_credit_unix_ms / millis_per_day {
            self.window_hours = self
                .window_hours
                .saturating_add(ADAPTIVE_COLLAPSE_STEP_HOURS)
                .min(ADAPTIVE_COLLAPSE_MAX_HOURS);
            self.last_credit_unix_ms = activity_unix_ms;
        }
        *self != previous
    }

    pub fn reset_after_expiry(&mut self, base_hours: u64) -> bool {
        let previous = *self;
        self.window_hours = base_hours.min(ADAPTIVE_COLLAPSE_MAX_HOURS);
        self.last_credit_unix_ms = self.last_activity_unix_ms;
        *self != previous
    }

    pub fn window_ms(self) -> u64 {
        self.window_hours.saturating_mul(MILLIS_PER_HOUR)
    }
}

#[derive(Serialize, Default, Clone, PartialEq, Eq)]
pub struct WorkspaceCache {
    #[serde(default)]
    pub written_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub worktree_expanded: HashMap<String, bool>,
    #[serde(default)]
    pub project_expanded: HashMap<String, bool>,
    /// Latest explicit project interaction, keyed by stable project path.
    #[serde(default)]
    pub project_touched_unix_ms: HashMap<String, u64>,
    /// Projects whose last collapse was caused by the inactivity timer.
    #[serde(default)]
    pub stale_collapsed_projects: HashSet<String>,
    /// Adaptive inactivity windows keyed by stable project path.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub adaptive_collapse: HashMap<String, AdaptiveCollapseState>,
    #[serde(default)]
    pub routines_expanded: HashMap<String, bool>,
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
    /// Agent integrations whose demand-driven setup prompt the user declined.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub dismissed_integration_prompts: HashSet<crate::integration::IntegrationTarget>,
    #[serde(skip)]
    migration_needed: bool,
    #[serde(skip)]
    stale_provenance_missing: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WorkspaceCacheWire {
    written_at_unix_ms: Option<u64>,
    worktree_expanded: HashMap<String, bool>,
    project_expanded: HashMap<String, bool>,
    project_touched_unix_ms: HashMap<String, u64>,
    stale_collapsed_projects: Option<HashSet<String>>,
    adaptive_collapse: HashMap<String, AdaptiveCollapseState>,
    routines_expanded: HashMap<String, bool>,
    tree_selected: usize,
    cursor_identity: Option<CursorIdentity>,
    #[serde(alias = "muted_sessions")]
    muted_terminals: HashSet<String>,
    acknowledged_outcomes: HashMap<String, u64>,
    active_group: Option<toml::Value>,
    active_groups: Option<toml::Value>,
    active_tab: Option<toml::Value>,
    integration_prompt_version: Option<String>,
    dismissed_integration_prompts: HashSet<crate::integration::IntegrationTarget>,
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
            project_touched_unix_ms: wire.project_touched_unix_ms,
            stale_provenance_missing: wire.stale_collapsed_projects.is_none(),
            stale_collapsed_projects: wire.stale_collapsed_projects.unwrap_or_default(),
            adaptive_collapse: wire.adaptive_collapse,
            routines_expanded: wire.routines_expanded,
            tree_selected: wire.tree_selected,
            cursor_identity: wire.cursor_identity,
            muted_terminals: wire.muted_terminals,
            acknowledged_outcomes: wire.acknowledged_outcomes,
            dismissed_integration_prompts: wire.dismissed_integration_prompts,
            migration_needed: migration_needed || wire.integration_prompt_version.is_some(),
        })
    }
}

impl WorkspaceCache {
    pub fn load() -> anyhow::Result<Self> {
        let path = cache_path();
        let _lock = CacheLock::acquire(&path)?;
        Self::load_from_paths(&path, &legacy_cache_path())
    }

    fn load_from_paths(
        canonical: &std::path::Path,
        legacy: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let Some((content, imported_legacy)) = read_cache_files(canonical, legacy)? else {
            return Ok(Self::default());
        };
        let mut cache: Self = toml::from_str(&content)?;
        if imported_legacy || cache.migration_needed {
            cache.save_to(canonical, false)?;
            cache.migration_needed = false;
        }
        Ok(cache)
    }

    /// Explicit whole-cache replacement. Interactive clients use `WorkspaceCacheChanges`.
    pub fn save(&self, sync: bool) -> anyhow::Result<()> {
        let path = cache_path();
        let _lock = CacheLock::acquire(&path)?;
        self.save_to(&path, sync)
    }

    fn save_to(&self, path: &std::path::Path, sync: bool) -> anyhow::Result<()> {
        let mut cache = self.clone();
        cache.written_at_unix_ms = Some(now_unix_ms());
        let text = toml::to_string(&cache)?;
        atomic_write_private(path, text.as_bytes(), sync)?;
        Ok(())
    }
}

fn read_cache_files(canonical: &Path, legacy: &Path) -> io::Result<Option<(String, bool)>> {
    let oldest = legacy.with_file_name("workspace.toml");
    for (path, imported) in [(canonical, false), (legacy, true), (oldest.as_path(), true)] {
        match std::fs::read_to_string(path) {
            Ok(content) => return Ok(Some((content, imported))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn cached_project_touch_unix_ms(
    cache: &WorkspaceCache,
    project_key: &str,
    loaded_at_unix_ms: u64,
) -> u64 {
    cache
        .project_touched_unix_ms
        .get(project_key)
        .copied()
        .or(cache.written_at_unix_ms)
        .unwrap_or(loaded_at_unix_ms)
}

fn legacy_seeded_touch_cohort(cache: &WorkspaceCache) -> Option<u64> {
    if !cache.stale_provenance_missing {
        return None;
    }
    let mut counts = HashMap::<u64, usize>::new();
    for timestamp in cache.project_touched_unix_ms.values() {
        *counts.entry(*timestamp).or_default() += 1;
    }
    let cohort_size = cache.project_touched_unix_ms.len();
    // ^ 0.26.1 seeded one timestamp across the large untouched-project cohort.
    // Do not infer collapse provenance from a small or non-majority timestamp collision.
    counts
        .into_iter()
        .filter(|(_, count)| *count >= 3 && *count > cohort_size / 2)
        .max_by_key(|(timestamp, count)| (*count, *timestamp))
        .map(|(timestamp, _)| timestamp)
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace-v3.toml")
}

fn legacy_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("wsx")
        .join("workspace-v2.toml")
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
    HashMap<PathBuf, u64>,
    HashSet<PathBuf>,
    HashMap<PathBuf, AdaptiveCollapseState>,
    HashSet<String>,
    HashMap<String, u64>,
    HashSet<crate::integration::IntegrationTarget>,
    HashMap<String, bool>,
);

/// Apply only cached UI and local mute state. Sessions always come from wsxd.
pub fn apply_cache(workspace: &mut WorkspaceState) -> anyhow::Result<AppliedCache> {
    let path = cache_path();
    let _lock = CacheLock::acquire(&path)?;
    let cache = WorkspaceCache::load_from_paths(&path, &legacy_cache_path())?;
    apply_workspace_cache(workspace, cache, |cache| cache.save_to(&path, false))
}

fn apply_workspace_cache(
    workspace: &mut WorkspaceState,
    mut cache: WorkspaceCache,
    persist_migration: impl FnOnce(&WorkspaceCache) -> anyhow::Result<()>,
) -> anyhow::Result<AppliedCache> {
    let mut migrated_muted_terminals = HashSet::new();
    let mut project_touched_unix_ms = HashMap::new();
    let mut stale_collapsed_projects = HashSet::new();
    let mut adaptive_collapse = HashMap::new();
    let loaded_at_unix_ms = now_unix_ms();
    let legacy_seeded_touch = legacy_seeded_touch_cohort(&cache);
    let mut seeded_touches = false;
    for project in &mut workspace.projects {
        let project_key = project.path.to_string_lossy().to_string();
        let touched_unix_ms = cached_project_touch_unix_ms(&cache, &project_key, loaded_at_unix_ms);
        project_touched_unix_ms.insert(project.path.clone(), touched_unix_ms);
        if !cache.project_touched_unix_ms.contains_key(&project_key) {
            cache
                .project_touched_unix_ms
                .insert(project_key.clone(), touched_unix_ms);
            seeded_touches = true;
        }
        if let Some(expanded) = cache.project_expanded.get(&project_key) {
            project.expanded = *expanded;
        }
        if cache.stale_collapsed_projects.contains(&project_key)
            || (!project.expanded && legacy_seeded_touch == Some(touched_unix_ms))
        {
            stale_collapsed_projects.insert(project.path.clone());
        }
        if let Some(state) = cache.adaptive_collapse.get(&project_key) {
            adaptive_collapse.insert(project.path.clone(), *state);
        }
        if let Some(expanded) = cache.routines_expanded.get(&project_key) {
            project.routines_expanded = *expanded;
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
    if cache.stale_provenance_missing {
        cache.project_touched_unix_ms = project_touched_unix_ms
            .iter()
            .map(|(path, timestamp)| (path.to_string_lossy().into_owned(), *timestamp))
            .collect();
        cache.stale_collapsed_projects = stale_collapsed_projects
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        cache.stale_provenance_missing = false;
        seeded_touches = true;
    }
    if seeded_touches {
        persist_migration(&cache)?;
    }
    migrated_muted_terminals.extend(cache.muted_terminals);
    Ok((
        cache.tree_selected,
        cache.cursor_identity,
        project_touched_unix_ms,
        stale_collapsed_projects,
        adaptive_collapse,
        migrated_muted_terminals,
        cache.acknowledged_outcomes,
        cache.dismissed_integration_prompts,
        cache.worktree_expanded,
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

/// Adaptive activity input, never a caller's derived credit/window snapshot.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveCacheActivity {
    pub base_hours: u64,
    pub activity_unix_ms: u64,
}

impl From<AdaptiveCollapseState> for AdaptiveCacheActivity {
    fn from(state: AdaptiveCollapseState) -> Self {
        Self {
            base_hours: state.base_hours,
            activity_unix_ms: state.last_activity_unix_ms,
        }
    }
}

/// One project's pending intent. Automatic decisions carry their observed interaction
/// time so an old window cannot undo a newer interaction. See docs/ui-state-ownership.md.
#[derive(Default, Debug)]
pub struct ProjectCacheChange {
    pub observed_touch_unix_ms: u64,
    pub touched_unix_ms: Option<u64>,
    pub expanded: Option<bool>,
    pub routines_expanded: Option<bool>,
    pub stale: Option<bool>,
    pub adaptive: Option<Option<AdaptiveCacheActivity>>,
}

/// Committed state for one submitted project, including rejected automatic decisions.
#[derive(Debug)]
pub struct ProjectCacheState {
    pub touched_unix_ms: Option<u64>,
    pub expanded: Option<bool>,
    pub routines_expanded: Option<bool>,
    pub stale: bool,
    pub adaptive: Option<AdaptiveCollapseState>,
}

/// Only commands, never a complete presentation snapshot, may update shared UI intent.
#[derive(Default, Debug)]
pub struct WorkspaceCacheChanges {
    pub projects: HashMap<PathBuf, ProjectCacheChange>,
    pub worktree_expanded: HashMap<PathBuf, bool>,
    pub muted_terminals: HashMap<String, bool>,
    pub acknowledged_outcomes: HashMap<String, u64>,
    pub dismissed_integration_prompts: HashMap<crate::integration::IntegrationTarget, bool>,
    pub cursor: Option<(usize, Option<CursorIdentity>)>,
}

impl WorkspaceCacheChanges {
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
            && self.worktree_expanded.is_empty()
            && self.muted_terminals.is_empty()
            && self.acknowledged_outcomes.is_empty()
            && self.dismissed_integration_prompts.is_empty()
            && self.cursor.is_none()
    }

    pub fn save(&self, sync: bool) -> anyhow::Result<HashMap<PathBuf, ProjectCacheState>> {
        self.save_to(&cache_path(), &legacy_cache_path(), sync)
    }

    fn save_to(
        &self,
        path: &Path,
        legacy: &Path,
        sync: bool,
    ) -> anyhow::Result<HashMap<PathBuf, ProjectCacheState>> {
        if self.is_empty() && !sync {
            return Ok(HashMap::new());
        }
        let _lock = CacheLock::acquire(path)?;
        if self.is_empty() {
            sync_cache(path)?;
            return Ok(HashMap::new());
        }
        // A failed read/parse must retain pending commands, not replace the file with defaults.
        let (mut cache, imported) = match read_cache_files(path, legacy)? {
            Some((content, imported)) => (toml::from_str::<WorkspaceCache>(&content)?, imported),
            None => (WorkspaceCache::default(), false),
        };
        let previous = cache.clone();
        self.apply_to(&mut cache);
        if cache == previous && !imported && !cache.migration_needed {
            if sync {
                sync_cache(path)?;
            }
        } else {
            cache.save_to(path, sync)?;
        }
        Ok(self
            .projects
            .keys()
            .map(|path| {
                let key = path.to_string_lossy();
                (
                    path.clone(),
                    ProjectCacheState {
                        touched_unix_ms: cache.project_touched_unix_ms.get(key.as_ref()).copied(),
                        expanded: cache.project_expanded.get(key.as_ref()).copied(),
                        routines_expanded: cache.routines_expanded.get(key.as_ref()).copied(),
                        stale: cache.stale_collapsed_projects.contains(key.as_ref()),
                        adaptive: cache.adaptive_collapse.get(key.as_ref()).copied(),
                    },
                )
            })
            .collect())
    }

    fn apply_to(&self, cache: &mut WorkspaceCache) {
        for (path, change) in &self.projects {
            let key = path.to_string_lossy().into_owned();
            let current_touch = cache
                .project_touched_unix_ms
                .get(&key)
                .copied()
                .unwrap_or(0);
            let proposed_touch = change
                .touched_unix_ms
                .unwrap_or(change.observed_touch_unix_ms);
            if current_touch > proposed_touch {
                continue;
            }
            // ^ Adaptive credit and expiry use the latest durable state, not a caller's window snapshot.
            let adaptive = match change.adaptive {
                Some(Some(proposed)) => {
                    let activity = proposed
                        .activity_unix_ms
                        .max(current_touch)
                        .max(proposed_touch);
                    let mut state =
                        cache
                            .adaptive_collapse
                            .get(&key)
                            .copied()
                            .unwrap_or_else(|| {
                                AdaptiveCollapseState::new(proposed.base_hours, activity)
                            });
                    state.observe(proposed.base_hours, activity);
                    if change.stale == Some(true) {
                        if now_unix_ms().saturating_sub(activity.max(state.last_activity_unix_ms))
                            <= state.window_ms()
                        {
                            continue;
                        }
                        state.reset_after_expiry(proposed.base_hours);
                    }
                    Some(Some(state))
                }
                Some(None) => Some(None),
                None => None,
            };
            if let Some(touched) = change.touched_unix_ms {
                cache.project_touched_unix_ms.insert(key.clone(), touched);
            }
            if let Some(expanded) = change.expanded {
                cache.project_expanded.insert(key.clone(), expanded);
            }
            if let Some(expanded) = change.routines_expanded {
                cache.routines_expanded.insert(key.clone(), expanded);
            }
            if let Some(stale) = change.stale {
                set_membership(&mut cache.stale_collapsed_projects, key.clone(), stale);
            }
            if let Some(adaptive) = adaptive {
                match adaptive {
                    Some(state) => {
                        cache.adaptive_collapse.insert(key, state);
                    }
                    None => {
                        cache.adaptive_collapse.remove(&key);
                    }
                }
            }
        }
        for (path, expanded) in &self.worktree_expanded {
            cache
                .worktree_expanded
                .insert(path.to_string_lossy().into_owned(), *expanded);
        }
        for (terminal, muted) in &self.muted_terminals {
            set_membership(&mut cache.muted_terminals, terminal.clone(), *muted);
        }
        for (terminal, revision) in &self.acknowledged_outcomes {
            let acknowledged = cache
                .acknowledged_outcomes
                .entry(terminal.clone())
                .or_default();
            *acknowledged = (*acknowledged).max(*revision);
        }
        for (target, dismissed) in &self.dismissed_integration_prompts {
            set_membership(
                &mut cache.dismissed_integration_prompts,
                *target,
                *dismissed,
            );
        }
        if let Some((selected, identity)) = &self.cursor {
            cache.tree_selected = *selected;
            cache.cursor_identity = identity.clone();
        }
    }
}

fn sync_cache(path: &Path) -> io::Result<()> {
    match std::fs::File::open(path) {
        Ok(file) => {
            file.sync_all()?;
            std::fs::File::open(
                path.parent()
                    .ok_or_else(|| io::Error::other("cache has no parent"))?,
            )?
            .sync_all()
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn set_membership<T: Eq + std::hash::Hash>(set: &mut HashSet<T>, key: T, present: bool) {
    if present {
        set.insert(key);
    } else {
        set.remove(&key);
    }
}

struct CacheLock(std::fs::File);

impl CacheLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("lock"))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe workspace cache lock",
            ));
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "workspace cache is busy; retry pending intent",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
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
    fn adaptive_collapse_credits_one_active_day_without_rapid_inflation() {
        let day_ms = 24 * MILLIS_PER_HOUR;
        let mut state = AdaptiveCollapseState::new(24, 1_000);

        assert!(state.observe(24, 1_000 + day_ms));
        assert_eq!(state.window_hours, 36);
        assert!(state.observe(24, 1_000 + day_ms + 1));
        assert_eq!(state.window_hours, 36);
        assert!(state.observe(24, 1_000 + 2 * day_ms));
        assert_eq!(state.window_hours, 48);
    }

    #[test]
    fn adaptive_collapse_credits_the_first_activity_on_a_new_utc_day() {
        let day_ms = 24 * MILLIS_PER_HOUR;
        let mut state = AdaptiveCollapseState::new(24, day_ms - 1);

        assert!(state.observe(24, day_ms));
        assert_eq!(state.window_hours, 36);
        assert!(state.observe(24, day_ms + 1));
        assert_eq!(state.window_hours, 36);
    }

    #[test]
    fn adaptive_collapse_caps_at_four_weeks_and_resets_after_an_expired_gap() {
        let day_ms = 24 * MILLIS_PER_HOUR;
        let mut state = AdaptiveCollapseState::new(24, 1_000);
        for day in 1..=100 {
            state.observe(24, 1_000 + day * day_ms);
        }
        assert_eq!(state.window_hours, ADAPTIVE_COLLAPSE_MAX_HOURS);

        let after_expiry = state
            .last_activity_unix_ms
            .saturating_add(state.window_ms())
            .saturating_add(1);
        assert!(state.observe(24, after_expiry));
        assert_eq!(state.window_hours, 24);
        assert_eq!(state.last_activity_unix_ms, after_expiry);
        assert_eq!(state.last_credit_unix_ms, after_expiry);
    }

    #[test]
    fn adaptive_collapse_accepts_the_exact_window_boundary_and_resets_on_base_change() {
        let mut state = AdaptiveCollapseState::new(24, 1_000);
        let boundary = 1_000 + state.window_ms();
        assert!(state.observe(24, boundary));
        assert_eq!(state.window_hours, 36);

        assert!(state.observe(48, boundary + 1));
        assert_eq!(state.base_hours, 48);
        assert_eq!(state.window_hours, 48);
        assert_eq!(state.last_credit_unix_ms, boundary + 1);
    }

    #[test]
    fn legacy_cache_defaults_missing_dismissed_integration_prompts() {
        let cache: WorkspaceCache = toml::from_str("tree_selected = 2\n").unwrap();
        assert!(cache.dismissed_integration_prompts.is_empty());
    }

    #[test]
    fn dismissed_integration_prompts_round_trip_per_agent() {
        let cache = WorkspaceCache {
            dismissed_integration_prompts: [crate::integration::IntegrationTarget::Pi]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let decoded: WorkspaceCache = toml::from_str(&toml::to_string(&cache).unwrap()).unwrap();
        assert_eq!(
            decoded.dismissed_integration_prompts,
            cache.dismissed_integration_prompts
        );
    }

    #[test]
    fn legacy_blanket_dismissal_is_migrated_away() {
        let cache: WorkspaceCache =
            toml::from_str("tree_selected = 2\nintegration_prompt_version = \"0.21.0\"\n").unwrap();
        assert!(cache.dismissed_integration_prompts.is_empty());
        assert!(cache.migration_needed);
    }

    #[test]
    fn expansion_maps_round_trip_by_stable_path() {
        let cache = WorkspaceCache {
            project_expanded: HashMap::from([("/projects/app".into(), true)]),
            project_touched_unix_ms: HashMap::from([("/projects/app".into(), 42)]),
            stale_collapsed_projects: HashSet::from(["/projects/old".into()]),
            adaptive_collapse: HashMap::from([(
                "/projects/app".into(),
                AdaptiveCollapseState::new(24, 42),
            )]),
            worktree_expanded: HashMap::from([("/projects/app/feature".into(), false)]),
            routines_expanded: HashMap::from([("/projects/app".into(), false)]),
            ..Default::default()
        };

        let decoded: WorkspaceCache = toml::from_str(&toml::to_string(&cache).unwrap()).unwrap();

        assert_eq!(decoded.project_expanded, cache.project_expanded);
        assert_eq!(
            decoded.project_touched_unix_ms,
            cache.project_touched_unix_ms
        );
        assert_eq!(
            decoded.stale_collapsed_projects,
            cache.stale_collapsed_projects
        );
        assert_eq!(decoded.adaptive_collapse, cache.adaptive_collapse);
        assert_eq!(decoded.worktree_expanded, cache.worktree_expanded);
        assert_eq!(decoded.routines_expanded, cache.routines_expanded);
        let empty = toml::to_string(&WorkspaceCache::default()).unwrap();
        assert!(empty.contains("stale_collapsed_projects = []"));
        assert!(
            !toml::from_str::<WorkspaceCache>(&empty)
                .unwrap()
                .stale_provenance_missing
        );
    }

    #[test]
    fn legacy_cache_defaults_missing_routines_expanded_map() {
        let cache: WorkspaceCache = toml::from_str(
            r#"[project_expanded]
"/projects/app" = true

[worktree_expanded]
"/projects/app/main" = false
"#,
        )
        .unwrap();

        assert!(cache.routines_expanded.is_empty());
        assert!(cache.project_touched_unix_ms.is_empty());
        assert!(cache.stale_collapsed_projects.is_empty());
        assert!(cache.stale_provenance_missing);
    }

    #[test]
    fn missing_project_touch_uses_cache_write_time_once() {
        let legacy = WorkspaceCache {
            written_at_unix_ms: Some(41),
            ..Default::default()
        };
        assert_eq!(
            cached_project_touch_unix_ms(&legacy, "/projects/app", 99),
            41
        );

        let current = WorkspaceCache {
            written_at_unix_ms: Some(41),
            project_touched_unix_ms: HashMap::from([("/projects/app".into(), 42)]),
            ..Default::default()
        };
        assert_eq!(
            cached_project_touch_unix_ms(&current, "/projects/app", 99),
            42
        );

        assert_eq!(
            cached_project_touch_unix_ms(&WorkspaceCache::default(), "/projects/app", 99),
            99
        );
    }

    #[test]
    fn repeated_legacy_migration_timestamp_identifies_one_stale_cohort() {
        let cache: WorkspaceCache = toml::from_str(
            r#"written_at_unix_ms = 99

[project_touched_unix_ms]
"/projects/a" = 41
"/projects/b" = 41
"/projects/c" = 41
"/projects/touched" = 72
"#,
        )
        .unwrap();

        assert_eq!(legacy_seeded_touch_cohort(&cache), Some(41));

        let ambiguous: WorkspaceCache = toml::from_str(
            r#"[project_touched_unix_ms]
"/projects/a" = 41
"/projects/b" = 41
"#,
        )
        .unwrap();
        assert_eq!(legacy_seeded_touch_cohort(&ambiguous), None);

        let current: WorkspaceCache = toml::from_str(
            r#"stale_collapsed_projects = []

[project_touched_unix_ms]
"/projects/a" = 41
"/projects/b" = 41
"#,
        )
        .unwrap();
        assert_eq!(legacy_seeded_touch_cohort(&current), None);
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
    fn malformed_canonical_cache_is_rejected_without_fallback_or_replacement() {
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

        assert!(WorkspaceCache::load_from_paths(&canonical, &legacy).is_err());
        assert_eq!(
            std::fs::read_to_string(&legacy).unwrap(),
            "active_tab = \"personal\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(&canonical).unwrap(),
            "active_group = [\n"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn applying_legacy_touch_cohort_persists_stale_provenance_immediately() {
        use crate::model::workspace::Project;

        fn project(path: &str) -> Project {
            Project {
                name: path.into(),
                path: path.into(),
                default_branch: "main".into(),
                last_agent_active_unix_ms: None,
                last_terminal_active_unix_ms: None,
                worktrees: vec![],
                routines: vec![],
                routine_revision: 0,
                routines_expanded: true,
                config: None,
                expanded: true,
                missing: false,
            }
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::current_dir()
            .unwrap()
            .join(".work/cache-v2-tests")
            .join(format!("stale-migration-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let canonical = directory.join("workspace-v2.toml");
        let legacy = directory.join("workspace.toml");
        let cache = WorkspaceCache {
            project_expanded: HashMap::from([
                ("/closed".into(), false),
                ("/closed-two".into(), false),
                ("/open".into(), true),
            ]),
            project_touched_unix_ms: HashMap::from([
                ("/closed".into(), 100),
                ("/closed-two".into(), 100),
                ("/open".into(), 100),
            ]),
            stale_provenance_missing: true,
            ..Default::default()
        };
        let mut workspace = WorkspaceState {
            projects: vec![project("/closed"), project("/closed-two"), project("/open")],
        };

        let (_, _, touches, stale, _, _, _, _, _) =
            apply_workspace_cache(&mut workspace, cache, |cache| {
                cache.save_to(&canonical, false)
            })
            .unwrap();

        assert!(!workspace.projects[0].expanded);
        assert!(!workspace.projects[1].expanded);
        assert!(workspace.projects[2].expanded);
        assert_eq!(touches.get(&PathBuf::from("/closed")), Some(&100));
        assert_eq!(
            stale,
            HashSet::from([PathBuf::from("/closed"), PathBuf::from("/closed-two")])
        );
        let persisted = WorkspaceCache::load_from_paths(&canonical, &legacy).unwrap();
        assert!(!persisted.stale_provenance_missing);
        assert_eq!(
            persisted.stale_collapsed_projects,
            HashSet::from(["/closed".into(), "/closed-two".into()])
        );
        assert_eq!(
            persisted.project_touched_unix_ms,
            HashMap::from([
                ("/closed".into(), 100),
                ("/closed-two".into(), 100),
                ("/open".into(), 100),
            ])
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
