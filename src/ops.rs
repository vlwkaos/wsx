// Workspace operation functions — pure business logic, no App state.
// These take explicit arguments rather than &mut App so they can be
// tested and reasoned about independently of the TUI state machine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::{
    config::global::GlobalConfig,
    git::{info as git_info, worktree as git_worktree},
    hooks,
    model::workspace::{GitInfo, Project, ProjectConfig, SessionInfo, WorkspaceState, WorktreeInfo},
    tmux::{monitor::SessionStatus, session},
};

// (pane_capture, running_app_suppressed, muted)
type PaneSnap = HashMap<String, (Option<String>, bool, bool)>;
// session_order preserves user-defined sort across refresh
type WorktreeSnap = HashMap<PathBuf, (Option<GitInfo>, bool, PaneSnap, Vec<String>)>;

pub const IDLE_SECS: u64 = 3;

// ── Refresh helpers ───────────────────────────────────────────────────────────

fn unix_ts_to_instant(unix_ts: u64) -> Option<Instant> {
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let secs_ago = now_unix.saturating_sub(unix_ts);
    Instant::now().checked_sub(Duration::from_secs(secs_ago))
}

/// Rebuild all worktrees + sessions for every project from live data.
pub fn refresh_workspace(
    workspace: &mut WorkspaceState,
    config: &GlobalConfig,
    sessions_with_paths: &[(String, PathBuf)],
    activity: &HashMap<String, SessionStatus>,
) {
    let aliases_by_path: Vec<(PathBuf, HashMap<String, String>)> =
        config.projects.iter()
            .map(|e| (e.path.clone(), e.aliases.clone()))
            .collect();

    for i in 0..workspace.projects.len() {
        let path = workspace.projects[i].path.clone();
        let proj_name = workspace.projects[i].name.clone();
        let aliases = aliases_by_path.iter()
            .find(|(p, _)| p == &path)
            .map(|(_, a)| a.clone())
            .unwrap_or_default();

        let snapshot: WorktreeSnap =
            workspace.projects[i].worktrees.iter()
                .map(|w| {
                    let panes = w.sessions.iter()
                        .map(|s| (s.name.clone(), (s.pane_capture.clone(), s.running_app_suppressed, s.muted)))
                        .collect();
                    let order = w.sessions.iter().map(|s| s.name.clone()).collect();
                    (w.path.clone(), (w.git_info.clone(), w.expanded, panes, order))
                })
                .collect();

        if let Ok(entries) = git_worktree::list_worktrees(&path) {
            let mut new_worktrees = Vec::new();
            for entry in entries {
                let alias = aliases.get(&entry.branch).cloned();
                let wt_path = entry.path.clone();
                let prev = snapshot.get(&entry.path);

                let wt_slug = match alias.as_deref() {
                    Some(a) => a.to_owned(),
                    None => entry.branch.replace('/', "-"),
                };
                let prefix = format!("{}-{}-", proj_name, wt_slug);

                let prev_order: &[String] = prev
                    .map(|(_, _, _, order)| order.as_slice())
                    .unwrap_or(&[]);

                let mut sessions: Vec<SessionInfo> = sessions_with_paths.iter()
                    .filter(|(_, sp)| sp == &wt_path)
                    .map(|(name, _)| {
                        let display_name = name.strip_prefix(&prefix)
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| name.clone());
                        let prev_pane = prev.and_then(|(_, _, panes, _)| panes.get(name));
                        let (pane_capture, prev_suppressed, muted) = prev_pane
                            .map(|(p, s, m)| (p.clone(), *s, *m))
                            .unwrap_or((None, false, false));
                        // Muted sessions skip all activity tracking.
                        let (has_activity, has_running_app, last_activity, running_app_suppressed) =
                            if muted {
                                (false, false, None, false)
                            } else {
                                let status = activity.get(name.as_str());
                                let has_activity = status.map(|s| s.has_bell).unwrap_or(false);
                                let has_running_app = status.map(|s| s.has_running_app).unwrap_or(false);
                                let last_activity = status
                                    .filter(|s| s.last_activity_ts > 0)
                                    .and_then(|s| unix_ts_to_instant(s.last_activity_ts));
                                let currently_active = last_activity
                                    .map(|t| t.elapsed().as_secs() < IDLE_SECS)
                                    .unwrap_or(false);
                                // Reset suppressed when new activity arrives.
                                let running_app_suppressed = if currently_active { false } else { prev_suppressed };
                                (has_activity, has_running_app, last_activity, running_app_suppressed)
                            };
                        SessionInfo {
                            name: name.clone(),
                            display_name,
                            has_activity,
                            pane_capture,
                            last_activity,
                            has_running_app,
                            running_app_suppressed,
                            muted,
                        }
                    })
                    .collect();
                sessions.sort_by_key(|s| {
                    prev_order.iter().position(|n| n == &s.name).unwrap_or(usize::MAX)
                });

                let (git_info, expanded) = prev
                    .map(|(gi, exp, _, _)| (gi.clone(), *exp))
                    .unwrap_or((None, true));

                new_worktrees.push(WorktreeInfo {
                    name: entry.name,
                    branch: entry.branch,
                    path: entry.path,
                    is_main: entry.is_main,
                    alias,
                    sessions,
                    expanded,
                    git_info,
                });
            }
            workspace.projects[i].worktrees = new_worktrees;
        }
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
                if sess.muted { continue; }
                if let Some(status) = activity.get(&sess.name) {
                    let old_bell = sess.has_activity;
                    let old_running = sess.has_running_app;
                    sess.has_activity = status.has_bell;
                    sess.has_running_app = status.has_running_app;
                    sess.last_activity = Some(status.last_activity_ts)
                        .filter(|&ts| ts > 0)
                        .and_then(|ts| unix_ts_to_instant(ts));
                    let currently_active = sess.last_activity
                        .map(|t| t.elapsed().as_secs() < IDLE_SECS)
                        .unwrap_or(false);
                    if currently_active { sess.running_app_suppressed = false; }
                    if sess.has_activity != old_bell
                        || sess.has_running_app != old_running
                    {
                        changed = true;
                    }
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

    let projects = config.projects.iter()
        .filter_map(|entry| {
            let path = &entry.path;
            if !path.exists() { return None; }

            let default_branch = detect_default_branch(path);
            let proj_config = crate::config::project::load_project_config(path);
            let entries = git_worktree::list_worktrees(path).unwrap_or_default();
            let worktrees = git_worktree::to_worktree_infos(entries, &entry.aliases);

            Some(Project {
                name: entry.name.clone(),
                path: path.clone(),
                default_branch,
                worktrees,
                config: Some(proj_config),
                expanded: true,
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
pub fn register_project(
    path: PathBuf,
    config: &mut GlobalConfig,
) -> Result<Project> {
    if path.as_os_str().is_empty() { bail!("empty path"); }
    if !path.exists() { bail!("path does not exist: {}", path.display()); }
    if !path.join(".git").exists() { bail!("not a git repository: {}", path.display()); }

    let name = path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let default_branch = detect_default_branch(&path);
    let proj_config = crate::config::project::load_project_config(&path);
    let entries = git_worktree::list_worktrees(&path).unwrap_or_default();
    let aliases = config.projects.iter()
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
            Some(cmd) => cmd.split_whitespace().next().unwrap_or(proj_name).to_string(),
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

/// Kill a tmux session by name.
pub fn delete_session(name: &str) -> Result<()> {
    session::kill_session(name)
}

/// Rename a tmux session from `old_name` to `new_name`.
pub fn rename_session(old_name: &str, new_name: &str) -> Result<()> {
    session::rename_session(old_name, new_name)
}

/// Create an ephemeral tmux session for a one-off run command.
/// Returns the session name.
pub fn create_ephemeral_session(
    proj_name: &str,
    wt_slug: &str,
    wt_path: &PathBuf,
    command: &str,
) -> Result<String> {
    let base_name = format!("{}-{}-run", proj_name, wt_slug);
    let name = session::unique_session_name(&base_name);
    session::create_ephemeral_session(&name, wt_path, command)?;
    Ok(name)
}

// ── Alias operations ──────────────────────────────────────────────────────────

/// Persist an alias for a branch in the global config. Caller must call `config.save()`.
pub fn set_alias(
    config: &mut GlobalConfig,
    proj_path: &PathBuf,
    branch: &str,
    alias: &str,
) {
    config.set_alias(proj_path, branch, alias);
}
