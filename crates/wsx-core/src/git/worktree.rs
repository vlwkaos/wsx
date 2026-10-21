// Worktree CRUD — all via git CLI
// ref: git-worktree(1) — https://git-scm.com/docs/git-worktree

use super::git_cmd;
use crate::model::workspace::WorktreeInfo;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    pub is_main: bool,
}

/// List worktrees via `git worktree list --porcelain`.
pub fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = super::output_with_timeout(
        git_cmd(repo_path).args(["worktree", "list", "--porcelain"]),
        std::time::Duration::from_secs(5),
    )
    .context("git worktree list")?;
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
                fetch_fail_count: 0,
                fetch_fail_reason: None,
                last_fetched: None,
                git_info_fetched_at: None,
            }
        })
        .collect()
}

/// True when `refs/heads/{branch}` exists in the repository.
pub fn branch_exists(repo_path: &Path, branch: &str) -> bool {
    git_cmd(repo_path)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn git_failure(action: &str, output: &std::process::Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        anyhow::anyhow!("{action} exited {}", output.status)
    } else {
        anyhow::anyhow!("{action} exited {}: {stderr}", output.status)
    }
}

/// Create `refs/heads/{branch}` at `base` and return the commit it points at.
/// `git branch` refuses an existing name, so success proves this call made the branch.
fn create_branch(repo_path: &Path, branch: &str, base: &str) -> Result<String> {
    let output = git_cmd(repo_path)
        .args(["branch", "--", branch, base])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("git branch failed")?;
    if !output.status.success() {
        return Err(git_failure("git branch", &output));
    }
    let output = git_cmd(repo_path)
        .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
        .stderr(Stdio::null())
        .output()
        .context("git rev-parse failed")?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Remove a branch that `create_branch` made, only while it still points at `start`.
/// ^ `update-ref -d <ref> <old>` compares and deletes in one git transaction, so a
/// branch that moved, or one that someone else made, is never removed.
fn discard_created_branch(repo_path: &Path, branch: &str, start: &str) -> bool {
    if start.is_empty() {
        return false;
    }
    let removed = git_cmd(repo_path)
        .args(["update-ref", "-d", &format!("refs/heads/{branch}"), start])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if removed {
        // The upstream that `git branch` may have recorded belongs to the removed branch.
        let _ = git_cmd(repo_path)
            .args(["config", "--remove-section", &format!("branch.{branch}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    removed
}

/// `git branch {branch} {base_branch}` then `git worktree add {path} {branch}`, or only
/// the second command when the branch already exists. A branch this call made is removed
/// again when the worktree cannot be added, so the same command can be repeated.
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

    let wt_arg = wt_path.to_string_lossy();
    let created_at = if branch_exists(repo_path, branch) {
        None
    } else {
        Some(create_branch(repo_path, branch, base_branch)?)
    };
    let added = git_cmd(repo_path)
        .args(["worktree", "add", wt_arg.as_ref(), branch])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("git worktree add failed");
    let error = match added {
        Ok(output) if output.status.success() => return Ok(wt_path),
        Ok(output) => git_failure("git worktree add", &output),
        Err(error) => error,
    };
    match created_at {
        Some(start) if !discard_created_branch(repo_path, branch, &start) => {
            bail!("{error}; branch '{branch}' was created and could not be removed")
        }
        _ => Err(error),
    }
}

/// Uncommitted work in a worktree: tracked paths with changes, then untracked paths.
/// Ignored files (build output) are not work.
pub fn uncommitted_paths(worktree_path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let output = git_cmd(worktree_path)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=normal"])
        .output()
        .context("git status failed")?;
    if !output.status.success() {
        return Err(git_failure("git status", &output));
    }
    let (mut tracked, mut untracked) = (Vec::new(), Vec::new());
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = text.split('\0').filter(|entry| entry.len() > 3);
    while let Some(entry) = entries.next() {
        let (code, path) = entry.split_at(3);
        if code.starts_with("??") {
            untracked.push(path.to_string());
        } else {
            tracked.push(path.to_string());
            // ^ A rename or copy is followed by its source path.
            if code.starts_with('R') || code.starts_with('C') {
                entries.next();
            }
        }
    }
    Ok((tracked, untracked))
}

/// Refuse a worktree with uncommitted work, naming the paths (at most 10 per kind).
pub fn require_clean(worktree_path: &Path) -> Result<()> {
    let (tracked, untracked) = uncommitted_paths(worktree_path)?;
    let named = |paths: &[String]| {
        let shown = paths
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        match paths.len() {
            n if n > 10 => format!("{shown} and {} more", n - 10),
            _ => shown,
        }
    };
    let mut parts = Vec::new();
    if !tracked.is_empty() {
        parts.push(format!("uncommitted changes: {}", named(&tracked)));
    }
    if !untracked.is_empty() {
        parts.push(format!("untracked files: {}", named(&untracked)));
    }
    if !parts.is_empty() {
        bail!(
            "{} has {}; commit or remove them first. Nothing was removed",
            worktree_path.display(),
            parts.join("; ")
        );
    }
    Ok(())
}

/// `git worktree remove {path}` (never `--force`: git refuses uncommitted work too), then
/// `git branch -d {branch}`, which keeps a branch with unmerged commits. Returns whether
/// the branch was deleted.
pub fn remove_worktree(repo_path: &Path, worktree_path: &Path, branch: &str) -> Result<bool> {
    let output = git_cmd(repo_path)
        .args(["worktree", "remove", &worktree_path.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("git worktree remove failed")?;
    if !output.status.success() {
        return Err(git_failure("git worktree remove", &output));
    }
    let deleted = git_cmd(repo_path)
        .args(["branch", "-d", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    Ok(deleted)
}
