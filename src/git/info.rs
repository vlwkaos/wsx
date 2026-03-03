// Git info via CLI — branch, commits, modified files, ahead/behind

use super::git_cmd;
use crate::model::workspace::{CommitSummary, GitInfo};
use std::path::Path;
use std::process::Command;

pub fn get_git_info(worktree_path: &Path, _default_branch: &str) -> Option<GitInfo> {
    // require a valid branch (confirms we're in a real worktree)
    current_branch(worktree_path)?;
    // If status fails, do not overwrite existing UI state with a false "clean" result.
    let modified_files = modified_files(worktree_path)?;
    let recent_commits = recent_commits(worktree_path, 3);
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
    let out = git_read(path)
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

fn modified_files(path: &Path) -> Option<Vec<String>> {
    let out = git_read(path).args(["status", "--short"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
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
            .collect(),
    )
}

fn ahead_behind(path: &Path) -> (usize, usize) {
    let Ok(out) = git_read(path)
        .args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .output()
    else {
        return (0, 0);
    };
    if !out.status.success() {
        return (0, 0);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.split_whitespace();
    let ahead = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let behind = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (ahead, behind)
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
