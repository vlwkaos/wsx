// Workspace operation functions — pure business logic, no App state.
// These take explicit arguments rather than &mut App so they can be
// tested and reasoned about independently of the TUI state machine.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::{
    config::global::GlobalConfig,
    git::worktree as git_worktree,
    hooks,
    model::workspace::{Project, ProjectConfig, WorkspaceState},
    tmux::session,
};

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
    let out = std::process::Command::new("git")
        .args(["-C", &path.to_string_lossy(), "branch", "--show-current"])
        .output();
    if let Ok(o) = out {
        let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !b.is_empty() { return b; }
    }
    "main".to_string()
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
/// Returns the session name that was created.
pub fn create_session(
    proj_name: &str,
    wt_slug: &str,
    wt_path: &PathBuf,
    command: Option<String>,
) -> Result<String> {
    let base_name = format!("wsx_{}_{}", proj_name, wt_slug);
    let name = session::unique_session_name(&base_name);
    session::create_session(&name, wt_path)?;
    if let Some(cmd) = command {
        session::send_keys(&name, &cmd)?;
    }
    Ok(name)
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
    let base_name = format!("wsx_{}_{}_run", proj_name, wt_slug);
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
