// Git info via CLI — branch, commits, modified files, ahead/behind

use std::path::Path;
use std::process::Command;
use crate::model::workspace::{CommitSummary, GitInfo};

pub fn get_git_info(worktree_path: &Path, _default_branch: &str) -> Option<GitInfo> {
    // require a valid branch (confirms we're in a real worktree)
    current_branch(worktree_path)?;
    let recent_commits = recent_commits(worktree_path, 3);
    let modified_files = modified_files(worktree_path);
    let (ahead, behind) = ahead_behind(worktree_path);
    Some(GitInfo { recent_commits, modified_files, ahead, behind })
}

fn current_branch(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "branch", "--show-current"])
        .output().ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch.is_empty() { None } else { Some(branch) }
}

fn recent_commits(path: &Path, n: usize) -> Vec<CommitSummary> {
    let Ok(out) = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "log", "--oneline", &format!("-{}", n)])
        .output() else { return vec![] };
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
    let Ok(out) = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "status", "--short"])
        .output() else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| if line.len() > 3 { Some(line[3..].trim().to_string()) } else { None })
        .take(10)
        .collect()
}

fn ahead_behind(path: &Path) -> (usize, usize) {
    let Ok(out) = Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .output() else { return (0, 0) };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
}
