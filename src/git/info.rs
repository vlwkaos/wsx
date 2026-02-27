// Git info via CLI — branch, commits, modified files, ahead/behind

use super::git_cmd;
use crate::model::workspace::{CommitSummary, GitInfo};
use std::path::Path;

pub fn get_git_info(worktree_path: &Path, _default_branch: &str) -> Option<GitInfo> {
    // require a valid branch (confirms we're in a real worktree)
    current_branch(worktree_path)?;
    let recent_commits = recent_commits(worktree_path, 3);
    let modified_files = modified_files(worktree_path);
    let (ahead, behind) = ahead_behind(worktree_path);
    let remote_branch = upstream_branch(worktree_path);
    Some(GitInfo {
        recent_commits,
        modified_files,
        ahead,
        behind,
        remote_branch,
    })
}

/// Returns the upstream tracking branch name (e.g. "origin/main"), or None if untracked.
fn upstream_branch(path: &Path) -> Option<String> {
    let out = git_cmd(path)
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Run `git fetch` in the background thread — polls with timeout to avoid hanging.
pub(crate) fn git_fetch(path: &Path) -> bool {
    let Ok(mut child) = std::process::Command::new("git")
        .args(["fetch", "--no-tags", "--quiet"])
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    // Edge race: process may have exited after the previous `try_wait`.
                    if let Ok(Some(status)) = child.try_wait() {
                        return status.success();
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(_) => return false,
        }
    }
}

pub fn current_branch(path: &Path) -> Option<String> {
    let out = git_cmd(path)
        .args(["branch", "--show-current"])
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

fn recent_commits(path: &Path, n: usize) -> Vec<CommitSummary> {
    let Ok(out) = git_cmd(path)
        .args(["log", "--oneline", &format!("-{}", n)])
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let hash = parts.next()?.to_string();
            let message = parts.next().unwrap_or("").to_string();
            Some(CommitSummary { hash, message })
        })
        .collect()
}

fn modified_files(path: &Path) -> Vec<String> {
    let Ok(out) = git_cmd(path).args(["status", "--short"]).output() else {
        return vec![];
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            if line.len() > 3 {
                Some(line[3..].trim().to_string())
            } else {
                None
            }
        })
        .take(10)
        .collect()
}

fn ahead_behind(path: &Path) -> (usize, usize) {
    let Ok(out) = git_cmd(path)
        .args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .output()
    else {
        return (0, 0);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}
