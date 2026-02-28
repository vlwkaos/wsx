// Git operations: pull, push, rebase, merge

use super::{git_cmd, info::current_branch};
use anyhow::{bail, Result};
use std::path::Path;

fn run(cmd: &mut std::process::Command) -> Result<String> {
    let out = cmd.output()?;
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
    run(git_cmd(path).args(["pull"]))
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
    merge_result.map(|_| format!("Merged {} into {}, returned to {}", current, target, current))
}
