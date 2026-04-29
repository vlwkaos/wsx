// Workspace operation functions — pure business logic, no App state.
// These take explicit arguments rather than &mut App so they can be
// tested and reasoned about independently of the TUI state machine.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::{
    config::global::GlobalConfig,
    git::{info as git_info, worktree as git_worktree},
    hooks,
    model::workspace::{
        session_display_name_from_tmux, FetchFailReason, GitInfo, Project, ProjectConfig,
        SessionInfo, WorkspaceState, WorktreeInfo,
    },
    tmux::{monitor::SessionStatus, session},
};

// (pane_capture, running_app_suppressed, muted)
type PaneSnap = HashMap<String, (Option<String>, bool, bool)>;
// session_order preserves user-defined sort across refresh
type WorktreeSnap = HashMap<PathBuf, WorktreeSnapEntry>;

struct WorktreeSnapEntry {
    git_info: Option<GitInfo>,
    git_info_fetched_at: Option<Instant>,
    expanded: bool,
    panes: PaneSnap,
    session_order: Vec<String>,
    last_fetched: Option<Instant>,
    fetch_failed: bool,
    fetch_fail_count: u32,
    fetch_fail_reason: Option<FetchFailReason>,
}

pub const IDLE_SECS: u64 = 3;

fn is_git_repo(path: &std::path::Path) -> bool {
    path.exists() && path.join(".git").exists()
}

// ── Refresh helpers ───────────────────────────────────────────────────────────

fn unix_ts_to_instant(unix_ts: u64) -> Option<Instant> {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs_ago = now_unix.saturating_sub(unix_ts);
    Instant::now().checked_sub(Duration::from_secs(secs_ago))
}

/// Rebuild all worktrees + sessions for every project from live data.
/// Calls `list_worktrees` per project synchronously — use for user-triggered refreshes.
pub fn refresh_workspace(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    sessions_with_paths: &[(String, PathBuf)],
    activity: &HashMap<String, SessionStatus>,
) {
    let worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> = workspace
        .projects
        .iter()
        .map(|p| {
            let entries = git_worktree::list_worktrees(&p.path).unwrap_or_default();
            (p.path.clone(), entries)
        })
        .collect();
    refresh_workspace_with_worktrees(workspace, config, sessions_with_paths, activity, worktrees);
}

/// Like `refresh_workspace` but with pre-computed worktree entries — avoids subprocess calls
/// on the caller's thread. Used by the periodic background refresh path.
pub fn refresh_workspace_with_worktrees(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    sessions_with_paths: &[(String, PathBuf)],
    activity: &HashMap<String, SessionStatus>,
    worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
) {
    // Pre-index sessions by worktree path for O(1) lookup per worktree
    let mut sessions_by_path: HashMap<&PathBuf, Vec<&str>> = HashMap::new();
    for (name, path) in sessions_with_paths {
        sessions_by_path.entry(path).or_default().push(name.as_str());
    }

    let aliases_by_path: Vec<(PathBuf, HashMap<String, String>)> = config
        .projects
        .iter()
        .map(|e| (e.path.clone(), e.aliases.clone()))
        .collect();

    let mut worktrees_map: HashMap<PathBuf, Vec<git_worktree::WorktreeEntry>> =
        worktrees.into_iter().collect();

    for i in 0..workspace.projects.len() {
        let path = workspace.projects[i].path.clone();
        let proj_name = workspace.projects[i].name.clone();
        let aliases = aliases_by_path
            .iter()
            .find(|(p, _)| p == &path)
            .map(|(_, a)| a.clone())
            .unwrap_or_default();

        let snapshot: WorktreeSnap = workspace.projects[i]
            .worktrees
            .iter()
            .map(|w| {
                let panes = w
                    .sessions
                    .iter()
                    .map(|s| {
                        (
                            s.name.clone(),
                            (s.pane_capture.clone(), s.running_app_suppressed, s.muted),
                        )
                    })
                    .collect();
                let order = w.sessions.iter().map(|s| s.name.clone()).collect();
                (
                    w.path.clone(),
                    WorktreeSnapEntry {
                        git_info: w.git_info.clone(),
                        git_info_fetched_at: w.git_info_fetched_at,
                        expanded: w.expanded,
                        panes,
                        session_order: order,
                        last_fetched: w.last_fetched,
                        fetch_failed: w.fetch_failed,
                        fetch_fail_count: w.fetch_fail_count,
                        fetch_fail_reason: w.fetch_fail_reason.clone(),
                    },
                )
            })
            .collect();

        let entries = worktrees_map.remove(&path).unwrap_or_default();
        let mut new_worktrees = Vec::new();
        for entry in entries
            .into_iter()
            .filter(|e| !config.is_worktree_excluded(&e.path))
        {
            let alias = aliases.get(&entry.branch).cloned();
            let wt_path = entry.path.clone();
            let prev = snapshot.get(&entry.path);

            let prev_order: &[String] = prev
                .map(|snap| snap.session_order.as_slice())
                .unwrap_or(&[]);
            // Index prev_order for O(1) sort-key lookup
            let order_index: HashMap<&str, usize> = prev_order
                .iter()
                .enumerate()
                .map(|(i, n)| (n.as_str(), i))
                .collect();

            let empty_names: Vec<&str> = Vec::new();
            let session_names = sessions_by_path.get(&wt_path).unwrap_or(&empty_names);
            let mut sessions: Vec<SessionInfo> = session_names
                .iter()
                .map(|&name| {
                    let display_name = session_display_name_from_tmux(
                        name,
                        &proj_name,
                        &wt_path,
                        &entry.branch,
                        alias.as_deref(),
                    );
                    let prev_pane = prev.and_then(|snap| snap.panes.get(name));
                    let (pane_capture, prev_suppressed, prev_muted) = prev_pane
                        .map(|(p, s, m)| (p.clone(), *s, *m))
                        .unwrap_or((None, false, false));
                    let status = activity.get(name);
                    // Prefer tmux-sourced muted/suppressed flags over snapshot so all instances agree.
                    // ^ @wsx-muted and @wsx-suppressed are written to tmux on toggle; snapshot is fallback.
                    let mut muted = status.map(|s| s.wsx_muted).unwrap_or(prev_muted);
                    let tmux_suppressed = status.map(|s| s.wsx_suppressed).unwrap_or(false);
                    let last_activity = status
                        .filter(|s| s.last_activity_ts > 0)
                        .and_then(|s| unix_ts_to_instant(s.last_activity_ts));
                    let currently_active = last_activity
                        .map(|t| t.elapsed().as_secs() < IDLE_SECS)
                        .unwrap_or(false);
                    // Auto-unmute when new output arrives — mute is "suppress for now", not permanent.
                    if muted && currently_active {
                        session::set_session_opt(name, session::OPT_MUTED, "0");
                        muted = false;
                    }
                    // Muted sessions skip all activity tracking.
                    let (has_activity, has_running_app, last_activity, running_app_suppressed) =
                        if muted {
                            (false, false, None, false)
                        } else {
                            let has_activity = status.map(|s| s.has_bell).unwrap_or(false);
                            let has_running_app =
                                status.map(|s| s.has_running_app).unwrap_or(false);
                            // Reset suppressed when new activity arrives; prefer tmux value.
                            let running_app_suppressed = if currently_active {
                                false
                            } else {
                                tmux_suppressed || prev_suppressed
                            };
                            (
                                has_activity,
                                has_running_app,
                                last_activity,
                                running_app_suppressed,
                            )
                        };
                    SessionInfo {
                        name: name.to_string(),
                        display_name,
                        has_activity,
                        pane_capture,
                        last_activity,
                        has_running_app,
                        is_running_wsx: status.map(|s| s.is_running_wsx).unwrap_or(false),
                        running_app_suppressed,
                        muted,
                    }
                })
                .collect();
            sessions.sort_by_key(|s| *order_index.get(s.name.as_str()).unwrap_or(&usize::MAX));

            let (git_info, git_info_fetched_at, expanded, last_fetched, fetch_failed, fetch_fail_count, fetch_fail_reason) = prev
                .map(|snap| {
                    (
                        snap.git_info.clone(),
                        snap.git_info_fetched_at,
                        snap.expanded,
                        snap.last_fetched,
                        snap.fetch_failed,
                        snap.fetch_fail_count,
                        snap.fetch_fail_reason.clone(),
                    )
                })
                .unwrap_or((None, None, true, None, false, 0, None));

            new_worktrees.push(WorktreeInfo {
                name: entry.name,
                branch: entry.branch,
                path: entry.path,
                is_main: entry.is_main,
                alias,
                sessions,
                expanded,
                git_info,
                git_info_fetched_at,
                fetch_failed,
                fetch_fail_count,
                fetch_fail_reason,
                last_fetched,
            });
        }
        workspace.projects[i].worktrees = new_worktrees;
    }
    // Drop projects that were already marked missing last cycle; mark newly-gone ones.
    // This gives one refresh cycle (~3 s) of visual "(missing)" indication before removal.
    workspace.projects.retain(|p| !p.missing);
    for p in &mut workspace.projects {
        p.missing = !is_git_repo(&p.path);
    }
}

/// Update session activity state from live tmux data. Returns true if any field changed.
pub fn update_activity(
    workspace: &mut WorkspaceState,
    activity: &HashMap<String, SessionStatus>,
) -> bool {
    let mut changed = false;
    for project in &mut workspace.projects {
        for wt in &mut project.worktrees {
            for sess in &mut wt.sessions {
                if sess.muted {
                    continue;
                }
                let old_bell = sess.has_activity;
                let old_running = sess.has_running_app;
                if let Some(status) = activity.get(&sess.name) {
                    sess.has_activity = status.has_bell;
                    sess.has_running_app = status.has_running_app;
                    sess.is_running_wsx = status.is_running_wsx;
                    sess.last_activity = Some(status.last_activity_ts)
                        .filter(|&ts| ts > 0)
                        .and_then(|ts| unix_ts_to_instant(ts));
                    let currently_active = sess
                        .last_activity
                        .map(|t| t.elapsed().as_secs() < IDLE_SECS)
                        .unwrap_or(false);
                    if currently_active {
                        sess.running_app_suppressed = false;
                    }
                } else {
                    sess.has_activity = false;
                    sess.has_running_app = false;
                    sess.is_running_wsx = false;
                }
                if sess.has_activity != old_bell || sess.has_running_app != old_running {
                    changed = true;
                }
            }
        }
    }
    changed
}

// ── Workspace loading ─────────────────────────────────────────────────────────

pub fn load_workspace(config: &GlobalConfig) -> WorkspaceState {
    if config.projects.is_empty() {
        return WorkspaceState::empty();
    }

    let projects = config
        .projects
        .iter()
        .filter_map(|entry| {
            let path = &entry.path;
            if !is_git_repo(path) {
                return None;
            }

            let default_branch = detect_default_branch(path);
            let proj_config = crate::config::project::load_project_config(path);
            let entries = git_worktree::list_worktrees(path).unwrap_or_default();
            let entries = entries
                .into_iter()
                .filter(|e| !config.is_worktree_excluded(&e.path))
                .collect();
            let worktrees = git_worktree::to_worktree_infos(entries, &entry.aliases);

            Some(Project {
                name: entry.name.clone(),
                path: path.clone(),
                default_branch,
                worktrees,
                config: Some(proj_config),
                expanded: true,
                missing: false,
            })
        })
        .collect();

    WorkspaceState { projects }
}

pub fn expand_path(s: &str) -> PathBuf {
    if s.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&s[2..]);
        }
    }
    PathBuf::from(s)
}

pub fn detect_default_branch(path: &std::path::Path) -> String {
    git_info::current_branch(path).unwrap_or_else(|| "main".into())
}

// ── Project registration ──────────────────────────────────────────────────────

/// Register a new project at `path`. Returns the constructed `Project` and
/// mutates `config` (caller must call `config.save()`).
pub fn register_project(path: PathBuf, config: &mut GlobalConfig) -> Result<Project> {
    if path.as_os_str().is_empty() {
        bail!("empty path");
    }
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }
    if !is_git_repo(&path) {
        bail!("not a git repository: {}", path.display());
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let default_branch = detect_default_branch(&path);
    let proj_config = crate::config::project::load_project_config(&path);
    let entries = git_worktree::list_worktrees(&path).unwrap_or_default();
    let aliases = config
        .projects
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.aliases.clone())
        .unwrap_or_default();
    let worktrees = git_worktree::to_worktree_infos(entries, &aliases);

    config.add_project(name.clone(), path.clone());

    Ok(Project {
        name,
        path,
        default_branch,
        worktrees,
        config: Some(proj_config),
        expanded: true,
        missing: false,
    })
}

/// Remove a project by path from config. Caller must call `config.save()`.
pub fn unregister_project(path: &PathBuf, config: &mut GlobalConfig) {
    config.remove_project(path);
}

// ── Worktree operations ───────────────────────────────────────────────────────

/// Create a new git worktree under `repo_path` for `branch`.
/// Runs hooks (env copy, post_create) and returns the new worktree path.
/// Returns a warning string if a hook failed (non-fatal).
pub fn create_worktree(
    repo_path: &PathBuf,
    default_branch: &str,
    proj_config: &ProjectConfig,
    branch: &str,
) -> Result<(PathBuf, Option<String>)> {
    let wt_path = git_worktree::create_worktree(repo_path, branch, default_branch)?;

    let mut warning: Option<String> = None;

    if let Err(e) = hooks::copy_env_files(repo_path, &wt_path, proj_config) {
        warning = Some(format!("Warning: .env copy: {}", e));
    }
    if let Some(ref cmd) = proj_config.post_create {
        if let Err(e) = hooks::run_post_create(&wt_path, cmd) {
            warning = Some(format!("Warning: postCreate: {}", e));
        }
    }

    Ok((wt_path, warning))
}

/// Remove a git worktree and kill any associated tmux sessions.
pub fn delete_worktree(
    repo_path: &PathBuf,
    wt_path: &PathBuf,
    branch: &str,
    session_names: &[String],
) -> Result<()> {
    git_worktree::remove_worktree(repo_path, wt_path, branch)?;
    for sess in session_names {
        let _ = session::kill_session(sess);
    }
    Ok(())
}

// ── Session operations ────────────────────────────────────────────────────────

/// Create a named tmux session at `wt_path` and optionally send an initial command.
/// Returns (tmux_name, display_name). Tmux name is prefixed with `{proj_name}-{wt_slug}-`;
/// display_name is the user-visible part (what the user typed).
pub fn create_session(
    proj_name: &str,
    wt_slug: &str,
    wt_path: &PathBuf,
    session_name: Option<String>,
    command: Option<String>,
) -> Result<(String, String)> {
    // display name priority: explicit > command first word > proj_name
    let base_display = match &session_name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => match &command {
            Some(cmd) => cmd
                .split_whitespace()
                .next()
                .unwrap_or(proj_name)
                .to_string(),
            None => proj_name.to_string(),
        },
    };
    let base_tmux = format!("{}-{}-{}", proj_name, wt_slug, base_display);
    let tmux_name = session::unique_session_name(&base_tmux);
    // strip "{proj_name}-{wt_slug}-" prefix to get display name
    let prefix_len = proj_name.len() + 1 + wt_slug.len() + 1;
    let display_name = tmux_name[prefix_len..].to_string();
    session::create_session(&tmux_name, wt_path)?;
    if let Some(cmd) = command {
        session::send_keys(&tmux_name, &cmd)?;
    }
    Ok((tmux_name, display_name))
}

/// Rename a tmux session from `old_name` to `new_name`.
pub fn rename_session(old_name: &str, new_name: &str) -> Result<()> {
    session::rename_session(old_name, new_name)
}

/// Recreate tmux sessions after a server restart (reboot/crash).
///
/// ! Only restores when the tmux server PID has changed. If the same server is
/// ! still running, missing sessions were intentionally killed — skip restore.
///
/// Uses the session snapshot (written periodically to Application Support) as the
/// source of truth — more recent than the cache, which may lag behind live state.
///
/// Returns the number of sessions recreated.
pub fn restore_cached_sessions(workspace: &WorkspaceState, cached_pid: Option<u32>) -> usize {
    let current_pid = session::server_pid();
    if cached_pid.is_some() && cached_pid == current_pid {
        return 0;
    }

    let live: HashSet<String> = session::list_sessions_with_paths()
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    // Prefer snapshot (written on every cache flush) over workspace sessions,
    // which come from the cache and may not reflect the last live state.
    let snapshot = crate::cache::load_session_snapshot();
    let source = if !snapshot.is_empty() {
        snapshot
    } else {
        crate::cache::collect_session_names(workspace)
    };

    let mut restored = 0usize;
    for (path_str, names) in &source {
        let path = std::path::Path::new(path_str.as_str());
        for name in names {
            if !live.contains(name) {
                if session::create_session(name, path).is_ok() {
                    restored += 1;
                }
            }
        }
    }
    restored
}

// ── Alias operations ──────────────────────────────────────────────────────────

/// Persist an alias for a branch in the global config. Caller must call `config.save()`.
pub fn set_alias(config: &mut GlobalConfig, proj_path: &PathBuf, branch: &str, alias: &str) {
    config.set_alias(proj_path, branch, alias);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::{Project, WorkspaceState};
    use std::collections::HashMap;

    fn make_project(path: PathBuf) -> Project {
        Project {
            name: "test".to_string(),
            path,
            default_branch: "main".to_string(),
            worktrees: vec![],
            config: None,
            expanded: true,
            missing: false,
        }
    }

    #[test]
    fn refresh_drops_project_whose_directory_was_deleted() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("wsx-test-{}", suffix));
        std::fs::create_dir_all(&base).unwrap();
        let exists_path = base.join("real");
        std::fs::create_dir_all(&exists_path).unwrap();
        std::fs::create_dir(exists_path.join(".git")).unwrap();
        let missing_path = base.join("ghost");

        let config = GlobalConfig::default();
        let activity: HashMap<String, crate::tmux::monitor::SessionStatus> = HashMap::new();

        let mut workspace = WorkspaceState {
            projects: vec![
                make_project(exists_path.clone()),
                make_project(missing_path.clone()),
            ],
        };

        // First refresh: missing_path is newly gone — stays in tree, marked missing.
        refresh_workspace_with_worktrees(
            &mut workspace,
            &config,
            &[],
            &activity,
            vec![(exists_path.clone(), vec![]), (missing_path.clone(), vec![])],
        );
        assert_eq!(workspace.projects.len(), 2);
        assert!(workspace.projects.iter().any(|p| p.missing && p.path == missing_path));

        // Second refresh: missing_path still gone — now dropped.
        refresh_workspace_with_worktrees(
            &mut workspace,
            &config,
            &[],
            &activity,
            vec![(exists_path.clone(), vec![]), (missing_path, vec![])],
        );
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.projects[0].path, exists_path);
        let _ = std::fs::remove_dir_all(&base);
    }
}
