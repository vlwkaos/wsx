// Git info via CLI — branch, commits, modified files, ahead/behind

use super::git_cmd;
use crate::model::workspace::{CommitSummary, GitInfo};
use std::path::Path;
use std::process::Command;

pub fn get_git_info(worktree_path: &Path, _default_branch: &str) -> Option<GitInfo> {
    // Single subprocess: captures branch, upstream, ahead/behind, and modified files.
    // If status fails (e.g. corrupt index), do not overwrite existing UI state.
    let (branch, remote_branch, ahead, behind, modified_files) = status_porcelain2(worktree_path)?;
    let _ = branch; // branch confirmed valid; value unused for now
    let recent_commits = recent_commits(worktree_path, 3);
    Some(GitInfo {
        recent_commits,
        modified_files,
        ahead,
        behind,
        remote_branch,
    })
}

type StatusResult = (String, Option<String>, usize, usize, Vec<String>);

/// Parse `git status --porcelain=2 --branch` output.
/// Returns (branch, upstream, ahead, behind, modified_files) or None on failure.
fn status_porcelain2(path: &Path) -> Option<StatusResult> {
    let out = git_read(path)
        .args(["status", "--porcelain=2", "--branch"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut branch = String::new();
    let mut upstream: Option<String> = None;
    let mut ahead = 0usize;
    let mut behind = 0usize;
    let mut modified_files: Vec<String> = Vec::new();

    for line in text.lines() {
        if let Some(val) = line.strip_prefix("# branch.head ") {
            branch = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("# branch.upstream ") {
            let u = val.trim().to_string();
            if !u.is_empty() {
                upstream = Some(u);
            }
        } else if let Some(val) = line.strip_prefix("# branch.ab ") {
            // "+<ahead> -<behind>"
            let mut parts = val.split_whitespace();
            if let Some(a) = parts.next() {
                ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(b) = parts.next() {
                behind = b.trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            // Type "1": "1 XY ... path" — path is last whitespace token
            // Type "2": "2 XY ... score path\toldpath" — tab separates new/old paths
            let path_str = if line.starts_with("2 ") {
                // Split off the tab-separated part first, take the new path
                line.split('\t').next().and_then(|before_tab| before_tab.split_whitespace().last())
            } else {
                line.split_whitespace().last()
            };
            if let Some(path_part) = path_str {
                if modified_files.len() < 10 {
                    modified_files.push(path_part.to_string());
                }
            }
        } else if line.starts_with("? ") {
            // Untracked file
            if let Some(path_part) = line.strip_prefix("? ") {
                if modified_files.len() < 10 {
                    modified_files.push(path_part.trim().to_string());
                }
            }
        }
    }

    if branch.is_empty() || branch == "(detached)" {
        // Confirm we're in a real worktree with a branch
        if branch.is_empty() {
            return None;
        }
    }

    Some((branch, upstream, ahead, behind, modified_files))
}

/// Run `git fetch` in the background thread — polls with timeout to avoid hanging.
pub(crate) fn git_fetch(path: &Path) -> bool {
    let Ok(mut child) = std::process::Command::new("git")
        .args(["fetch", "--no-tags", "--quiet"])
        .current_dir(path)
        .stdin(std::process::Stdio::null())
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
    let out = git_read(path)
        .args(["branch", "--show-current"])
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

fn recent_commits(path: &Path, n: usize) -> Vec<CommitSummary> {
    let Ok(out) = git_read(path)
        .args(["log", "--oneline", &format!("-{}", n)])
        .output()
    else {
        return vec![];
    };
    if !out.status.success() {
        return vec![];
    }
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

fn git_read(path: &Path) -> Command {
    let mut cmd = git_cmd(path);
    cmd.arg("--no-optional-locks");
    cmd
}

#[cfg(test)]
mod tests {
    use super::get_git_info;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn git(path: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .expect("git command should run");
        assert!(
            status.success(),
            "git command failed: git -C {:?} {:?}",
            path,
            args
        );
    }

    fn init_temp_repo() -> PathBuf {
        let mut path = std::env::temp_dir();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "wsx-git-info-test-{}-{}-{}",
            std::process::id(),
            suffix,
            id
        ));
        fs::create_dir_all(&path).expect("temp repo dir should be created");

        git(&path, &["init", "-q"]);
        git(&path, &["config", "user.email", "test@example.com"]);
        git(&path, &["config", "user.name", "Test User"]);
        fs::write(path.join("tracked.txt"), "first\n").expect("tracked file should be written");
        git(&path, &["add", "tracked.txt"]);
        git(&path, &["commit", "-m", "init", "-q"]);
        path
    }

    #[test]
    fn get_git_info_reports_dirty_file() {
        let repo = init_temp_repo();
        fs::write(repo.join("tracked.txt"), "changed\n").expect("tracked file should be updated");
        let info = get_git_info(&repo, "main").expect("git info should be available");
        assert!(
            info.modified_files.iter().any(|f| f == "tracked.txt"),
            "expected tracked.txt in modified files, got {:?}",
            info.modified_files
        );
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn get_git_info_returns_none_when_status_fails() {
        let repo = init_temp_repo();
        // Corrupt index so branch detection still works but status exits non-zero.
        fs::write(repo.join(".git").join("index"), "broken").expect("index should be overwritten");

        let info = get_git_info(&repo, "main");
        assert!(
            info.is_none(),
            "expected None when status fails, got {:?}",
            info
        );
        let _ = fs::remove_dir_all(repo);
    }
}
