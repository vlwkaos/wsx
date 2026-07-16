// Git operations: pull, push, rebase, merge

use super::{git_cmd, info::current_branch};
use anyhow::{bail, Result};
use std::path::Path;

fn run(cmd: &mut std::process::Command) -> Result<String> {
    let out = super::output_with_timeout(cmd, std::time::Duration::from_secs(30))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        let msg = if !stderr.is_empty() { stderr } else { stdout };
        bail!("{}", msg.lines().next().unwrap_or("git error"))
    }
}

pub fn pull(path: &Path) -> Result<String> {
    match run(git_cmd(path).args(["pull", "--rebase"])) {
        Ok(output) => Ok(output),
        Err(error) => {
            if rebase_in_progress(path) {
                let _ = run(git_cmd(path).args(["rebase", "--abort"]));
                bail!("pull stopped on conflict; rebase aborted; resolve manually");
            }
            Err(error)
        }
    }
}

fn rebase_in_progress(path: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|state_dir| {
        super::output_with_timeout(
            git_cmd(path).args(["rev-parse", "--git-path", state_dir]),
            std::time::Duration::from_secs(5),
        )
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|git_path| !git_path.is_empty())
        .is_some_and(|git_path| path.join(git_path).exists())
    })
}

pub fn push(path: &Path) -> Result<String> {
    let result = run(git_cmd(path).args(["push"]));
    match result {
        Ok(s) => Ok(s),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no upstream") || msg.contains("--set-upstream") {
                let branch = current_branch(path).unwrap_or_else(|| "HEAD".to_string());
                run(git_cmd(path).args(["push", "-u", "origin", &branch]))
            } else {
                Err(e)
            }
        }
    }
}

pub fn pull_rebase(path: &Path, branch: &str) -> Result<String> {
    run(git_cmd(path).args(["pull", "--rebase", "origin", branch]))
}

pub fn merge_from(path: &Path, source: &str) -> Result<String> {
    run(git_cmd(path).args(["merge", source]))
}

pub fn merge_into(path: &Path, target: &str) -> Result<String> {
    let current = current_branch(path).ok_or_else(|| anyhow::anyhow!("not on a branch"))?;
    // checkout target
    run(git_cmd(path).args(["checkout", target]))?;
    // merge current into target; on failure, checkout back
    let merge_result = run(git_cmd(path).args(["merge", &current]));
    // ! must always return to original branch regardless of merge outcome
    run(git_cmd(path).args(["checkout", &current]))?;
    merge_result.map(|_| {
        format!(
            "Merged {} into {}, returned to {}",
            current, target, current
        )
    })
}

#[cfg(test)]
mod tests {
    use super::rebase_in_progress;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn linked_git_state_path_detects_and_clears_rebase_marker() {
        let repo = PathBuf::from("target").join(format!("rebase-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(initialized.success());

        let marker = repo.join(".git/rebase-merge");
        std::fs::create_dir_all(&marker).unwrap();
        assert!(rebase_in_progress(&repo));

        std::fs::remove_dir_all(marker).unwrap();
        assert!(!rebase_in_progress(&repo));
        std::fs::remove_dir_all(repo).unwrap();
    }
}
