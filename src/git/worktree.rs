// Worktree CRUD — all via git CLI
// ref: git-worktree(1) — https://git-scm.com/docs/git-worktree

use super::git_cmd;
use crate::model::workspace::WorktreeInfo;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub struct WorktreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub is_main: bool,
}

/// List worktrees via `git worktree list --porcelain`.
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = git_cmd(repo_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("git worktree list failed")?;
    parse_porcelain_output(&String::from_utf8_lossy(&output.stdout), repo_path)
}

fn parse_porcelain_output(output: &str, repo_path: &Path) -> Result<Vec<WorktreeEntry>> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut first = true;

    for line in output.lines() {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                let branch = current_branch.take().unwrap_or_else(|| "HEAD".to_string());
                let name = derive_name(&path, &branch, first);
                entries.push(WorktreeEntry {
                    name,
                    path,
                    branch,
                    is_main: first,
                });
                first = false;
            }
        } else if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(p.trim()));
        } else if let Some(b) = line.strip_prefix("branch ") {
            let b = b.trim().strip_prefix("refs/heads/").unwrap_or(b.trim());
            current_branch = Some(b.to_string());
        }
    }

    // Last entry (no trailing blank line)
    if let Some(path) = current_path {
        let branch = current_branch.unwrap_or_else(|| "HEAD".to_string());
        let name = derive_name(&path, &branch, first);
        entries.push(WorktreeEntry {
            name,
            path,
            branch,
            is_main: first,
        });
    }

    if entries.is_empty() {
        entries.push(WorktreeEntry {
            name: "main".to_string(),
            path: repo_path.to_path_buf(),
            branch: "main".to_string(),
            is_main: true,
        });
    }

    Ok(entries)
}

fn derive_name(path: &Path, branch: &str, is_main: bool) -> String {
    if is_main {
        return "main".to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| branch.replace('/', "-"))
}

/// Convert WorktreeEntry list to WorktreeInfo list (no sessions yet — populated by refresh_all).
pub fn to_worktree_infos(
    entries: Vec<WorktreeEntry>,
    aliases: &std::collections::HashMap<String, String>,
) -> Vec<WorktreeInfo> {
    entries
        .into_iter()
        .map(|e| {
            let alias = aliases.get(&e.branch).cloned();
            WorktreeInfo {
                name: e.name,
                branch: e.branch,
                path: e.path,
                is_main: e.is_main,
                alias,
                sessions: Vec::new(),
                expanded: true,
                git_info: None,
                fetch_failed: false,
                last_fetched: None,
            }
        })
        .collect()
}

/// `git worktree add -b {branch} {path} {base_branch}`
pub fn create_worktree(repo_path: &Path, branch: &str, base_branch: &str) -> Result<PathBuf> {
    let parent = repo_path.parent().context("repo has no parent dir")?;
    let repo_name = repo_path
        .file_name()
        .context("repo has no name")?
        .to_string_lossy();
    let slug = branch.replace('/', "-").replace(
        |c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.',
        "-",
    );
    let wt_path = parent.join(format!("{}-{}", repo_name, slug));

    let status = git_cmd(repo_path)
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &wt_path.to_string_lossy(),
            base_branch,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("git worktree add failed")?;

    if !status.success() {
        bail!("git worktree add exited {}", status);
    }
    Ok(wt_path)
}

/// `git worktree remove --force {path}` then `git branch -d {branch}`
pub fn remove_worktree(repo_path: &Path, worktree_path: &Path, branch: &str) -> Result<()> {
    let status = git_cmd(repo_path)
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_path.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("git worktree remove failed")?;

    if !status.success() {
        bail!("git worktree remove exited {}", status);
    }

    // Best-effort branch deletion
    let _ = git_cmd(repo_path)
        .args(["branch", "-d", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    Ok(())
}

/// Delete worktrees whose branches are merged into default_branch.
pub fn clean_merged(repo_path: &Path, default_branch: &str) -> Result<Vec<String>> {
    let output = git_cmd(repo_path)
        .args(["branch", "--merged", default_branch])
        .output()
        .context("git branch --merged failed")?;

    let merged: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim().to_string())
        .filter(|b| !b.is_empty() && b != default_branch && !b.starts_with("HEAD"))
        .collect();

    let entries = list_worktrees(repo_path)?;
    let mut removed = Vec::new();

    for entry in entries.iter().filter(|e| !e.is_main) {
        if merged.contains(&entry.branch) {
            if remove_worktree(repo_path, &entry.path, &entry.branch).is_ok() {
                removed.push(entry.branch.clone());
            }
        }
    }

    Ok(removed)
}

/// Check if branch is an ancestor of default_branch (i.e., merged).
pub fn is_branch_merged(repo_path: &Path, branch: &str, default_branch: &str) -> bool {
    git_cmd(repo_path)
        .args(["merge-base", "--is-ancestor", branch, default_branch])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
