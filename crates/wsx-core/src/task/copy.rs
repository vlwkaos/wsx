//! A task's working copy: its own git worktree holding a copy-on-write clone
//! of the main checkout's files, so builds start warm.
//! ref: docs/tasks.md "Working copy"

use crate::git::{git_cmd, worktree::list_worktrees};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct WorkingCopy {
    pub path: PathBuf,
    pub branch: String,
    pub base: String,
}

fn branch_name(id: u32) -> String {
    format!("wsx/t{id}")
}

/// `<parent>/<repo>-t<n>`: the main checkout's volume, so the clone shares blocks.
fn working_copy_path(main: &Path, id: u32) -> Result<PathBuf> {
    let parent = main
        .parent()
        .context("the main checkout has no parent folder")?;
    let name = main
        .file_name()
        .context("the main checkout has no name")?
        .to_string_lossy();
    Ok(parent.join(format!("{name}-t{id}")))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git_cmd(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("cannot run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} exited {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn resolve_commit(repo: &Path, revision: &str) -> Result<String> {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{revision}^{{commit}}"),
        ],
    )
    .with_context(|| format!("{revision} is not a commit in {}", repo.display()))
}

/// Create the working copy. On failure nothing is left: no folder, worktree or branch.
pub fn create(main: &Path, base_revision: &str, id: u32) -> Result<WorkingCopy> {
    if main.join(".gitmodules").exists() {
        bail!(
            "{} has submodules; task working copies do not support them yet",
            main.display()
        );
    }
    let base = resolve_commit(main, base_revision)?;
    let path = working_copy_path(main, id)?;
    if fs::symlink_metadata(&path).is_ok() {
        bail!(
            "{} already exists; remove it or close its task first",
            path.display()
        );
    }
    let branch = branch_name(id);
    if crate::git::worktree::branch_exists(main, &branch) {
        bail!(
            "branch {branch} already exists in {}; delete it first",
            main.display()
        );
    }
    let path_text = path.to_string_lossy().to_string();
    git(
        main,
        &[
            "worktree",
            "add",
            "--no-checkout",
            "--detach",
            &path_text,
            &base,
        ],
    )?;
    let copy = WorkingCopy { path, branch, base };
    let filled = fill(main, &copy);
    if let Err(error) = filled {
        return match remove(main, &copy.path, &copy.branch) {
            Ok(()) => Err(error.context("nothing was left behind")),
            Err(cleanup) => Err(error.context(format!(
                "cleanup also failed, remove {} by hand: {cleanup:#}",
                copy.path.display()
            ))),
        };
    }
    Ok(copy)
}

fn fill(main: &Path, copy: &WorkingCopy) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(main)
        .with_context(|| format!("cannot list {}", main.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != ".git")
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    if !entries.is_empty() && clone_entries(&entries, &copy.path, true).is_err() {
        // ^ Not a copy-on-write volume: clear the partial copy and copy plainly.
        clear_except_git(&copy.path)?;
        clone_entries(&entries, &copy.path, false)?;
    }
    // ^ A worktree nested in the main checkout keeps a `.git` file pointing at
    // the main repository's admin folder. Its copy must not exist here.
    for nested in list_worktrees(main)? {
        if let Ok(relative) = nested.path.strip_prefix(main) {
            if !relative.as_os_str().is_empty() {
                let _ = fs::remove_dir_all(copy.path.join(relative));
            }
        }
    }
    git(&copy.path, &["reset", "--hard", "--quiet", &copy.base])?;
    git(&copy.path, &["clean", "-fdq"])?;
    git(&copy.path, &["switch", "--quiet", "-c", &copy.branch])?;
    Ok(())
}

fn clone_entries(entries: &[PathBuf], destination: &Path, copy_on_write: bool) -> Result<()> {
    let mut command = Command::new("cp");
    if copy_on_write {
        command.arg(if cfg!(target_os = "macos") {
            "-c"
        } else {
            "--reflink=always"
        });
    }
    let output = command
        .arg("-R")
        .args(entries)
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run cp")?;
    if !output.status.success() {
        bail!(
            "cp exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn clear_except_git(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Remove the worktree, its folder and its branch.
pub fn remove(main: &Path, path: &Path, branch: &str) -> Result<()> {
    let path_text = path.to_string_lossy().to_string();
    let removed = git(main, &["worktree", "remove", "--force", &path_text]);
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_dir_all(path).with_context(|| format!("cannot remove {}", path.display()))?;
    }
    if removed.is_err() {
        git(main, &["worktree", "prune"])?;
    }
    if crate::git::worktree::branch_exists(main, branch) {
        git(main, &["branch", "-D", branch])?;
    }
    Ok(())
}

/// Commits on `branch` that `target` does not have.
pub fn unlanded_commits(main: &Path, branch: &str, target: &str) -> Result<u32> {
    let count = git(
        main,
        &["rev-list", "--count", &format!("{target}..{branch}")],
    )?;
    count
        .parse()
        .with_context(|| format!("git rev-list returned {count:?}"))
}
