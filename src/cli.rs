use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use crate::{
    config::global::GlobalConfig,
    model::workspace::{Project, WorkspaceState},
    ops,
    tmux::{capture, monitor, session},
};

#[derive(Parser)]
#[command(name = "wsx", version, about = "Workspace manager — git worktrees + tmux sessions")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Print workspace state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        subcommand: WorktreeCmd,
    },
    /// Session operations
    Session {
        #[command(subcommand)]
        subcommand: SessionCmd,
    },
}

#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Create a worktree and default session
    Create {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Delete a worktree and its sessions
    Delete {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// List worktrees
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum SessionCmd {
    /// Send keys to a session (Enter appended unless --no-enter)
    SendKeys {
        session: String,
        keys: String,
        #[arg(long)]
        no_enter: bool,
    },
    /// Capture pane output
    Capture {
        session: String,
        #[arg(long)]
        trim: bool,
    },
    /// Rename a session
    Rename { old: String, new_name: String },
    /// List sessions
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Status { json } => cmd_status(json),
        Command::Worktree { subcommand } => match subcommand {
            WorktreeCmd::Create { branch, project } => cmd_worktree_create(&branch, project.as_deref()),
            WorktreeCmd::Delete { branch, project } => cmd_worktree_delete(&branch, project.as_deref()),
            WorktreeCmd::List { project, json } => cmd_worktree_list(project.as_deref(), json),
        },
        Command::Session { subcommand } => match subcommand {
            SessionCmd::SendKeys { session: s, keys, no_enter } => cmd_session_send_keys(&s, &keys, no_enter),
            SessionCmd::Capture { session: s, trim } => cmd_session_capture(&s, trim),
            SessionCmd::Rename { old, new_name } => cmd_session_rename(&old, &new_name),
            SessionCmd::List { project, json } => cmd_session_list(project.as_deref(), json),
        },
    }
}

// --- Helpers ---

fn load_full_workspace() -> Result<(GlobalConfig, WorkspaceState)> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    let mut workspace = ops::load_workspace(&config);
    let sessions = session::list_sessions_with_paths();
    let activity = monitor::session_activity();
    ops::refresh_workspace(&mut workspace, &config, &sessions, &activity);
    Ok((config, workspace))
}

fn filter_projects<'a>(workspace: &'a WorkspaceState, project_name: Option<&str>) -> Result<Vec<&'a Project>> {
    match project_name {
        Some(n) => Ok(vec![resolve_project(workspace, Some(n))?]),
        None => Ok(workspace.projects.iter().collect()),
    }
}

fn resolve_project<'a>(workspace: &'a WorkspaceState, name: Option<&str>) -> Result<&'a Project> {
    match name {
        Some(n) => workspace
            .projects
            .iter()
            .find(|p| p.name == n)
            .ok_or_else(|| anyhow::anyhow!("project '{}' not found", n)),
        None => match workspace.projects.len() {
            0 => bail!("no projects registered"),
            1 => Ok(&workspace.projects[0]),
            _ => bail!(
                "multiple projects — use -p to specify: {}",
                workspace.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        },
    }
}

fn activity_label(s: &crate::model::workspace::SessionInfo) -> &'static str {
    if s.has_activity {
        "active"
    } else if s.has_running_app {
        "running"
    } else {
        "idle"
    }
}

// --- Command implementations ---

fn cmd_status(json: bool) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&workspace)?);
        return Ok(());
    }
    for project in &workspace.projects {
        println!("{} {}", project.name, project.path.display());
        for wt in &project.worktrees {
            let git = wt.git_info.as_ref().map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind)).unwrap_or_default();
            println!("  {}{} ({} sessions)", wt.branch, git, wt.sessions.len());
            for s in &wt.sessions {
                println!("    {:<40} {}", s.name, activity_label(s));
            }
        }
    }
    Ok(())
}

fn cmd_worktree_create(branch: &str, project_name: Option<&str>) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    let proj_config = project.config.clone().unwrap_or_default();
    let (wt_path, warning) = ops::create_worktree(&project.path, &project.default_branch, &proj_config, branch)?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    let wt_slug = crate::model::workspace::canonical_session_slug(&project.name, &wt_path);
    let (tmux_name, _) = ops::create_session(&project.name, &wt_slug, &wt_path, None, None)?;
    println!("worktree: {}", wt_path.display());
    println!("session:  {}", tmux_name);
    let mut cache = crate::cache::WorkspaceCache::load();
    cache.sessions.insert(wt_path.to_string_lossy().to_string(), vec![tmux_name.clone()]);
    cache.tmux_server_pid = crate::tmux::session::server_pid();
    if let Err(e) = cache.save(false) {
        eprintln!("warning: cache save failed: {}", e);
    }
    Ok(())
}

fn cmd_worktree_delete(branch: &str, project_name: Option<&str>) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    let wt = project
        .worktrees
        .iter()
        .find(|w| w.branch == branch || w.alias.as_deref() == Some(branch))
        .ok_or_else(|| anyhow::anyhow!("worktree for branch '{}' not found in project '{}'", branch, project.name))?;
    let session_names: Vec<String> = wt.sessions.iter().map(|s| s.name.clone()).collect();
    ops::delete_worktree(&project.path, &wt.path, &wt.branch, &session_names)?;
    println!("deleted worktree: {}", wt.path.display());
    let mut cache = crate::cache::WorkspaceCache::load();
    cache.sessions.remove(&wt.path.to_string_lossy().to_string());
    if let Err(e) = cache.save(false) {
        eprintln!("warning: cache save failed: {}", e);
    }
    Ok(())
}

fn cmd_worktree_list(project_name: Option<&str>, json: bool) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let projects = filter_projects(&workspace, project_name)?;
    if json {
        let worktrees: Vec<_> = projects.iter().flat_map(|p| p.worktrees.iter()).collect();
        println!("{}", serde_json::to_string_pretty(&worktrees)?);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            let git = wt.git_info.as_ref().map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind)).unwrap_or_default();
            println!("{}/{}{} — {}", p.name, wt.branch, git, wt.path.display());
        }
    }
    Ok(())
}

fn cmd_session_send_keys(sess: &str, keys: &str, no_enter: bool) -> Result<()> {
    if no_enter {
        session::send_keys_raw(sess, keys)?;
    } else {
        session::send_keys(sess, keys)?;
    }
    Ok(())
}

fn cmd_session_capture(sess: &str, trim: bool) -> Result<()> {
    let raw = capture::capture_pane(sess).ok_or_else(|| anyhow::anyhow!("session '{}' not found or empty", sess))?;
    let output = if trim { capture::trim_capture(&raw) } else { raw };
    print!("{}", output);
    Ok(())
}

fn cmd_session_rename(old: &str, new: &str) -> Result<()> {
    session::rename_session(old, new)?;
    println!("renamed: {} → {}", old, new);
    let mut cache = crate::cache::WorkspaceCache::load();
    for sessions in cache.sessions.values_mut() {
        for s in sessions.iter_mut() {
            if s == old {
                *s = new.to_string();
            }
        }
    }
    if let Err(e) = cache.save(false) {
        eprintln!("warning: cache save failed: {}", e);
    }
    Ok(())
}

fn cmd_session_list(project_name: Option<&str>, json: bool) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let projects = filter_projects(&workspace, project_name)?;
    if json {
        let sessions: Vec<_> = projects.iter().flat_map(|p| p.worktrees.iter().flat_map(|w| w.sessions.iter())).collect();
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            for s in &wt.sessions {
                println!("{}/{} — {} ({})", p.name, wt.branch, s.name, activity_label(s));
            }
        }
    }
    Ok(())
}
