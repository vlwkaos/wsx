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
    working_copy_path_named(main, &format!("t{id}"))
}

/// `<parent>/<repo>-<suffix>` beside the main checkout.
pub fn working_copy_path_named(main: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = main
        .parent()
        .context("the main checkout has no parent folder")?;
    let name = main
        .file_name()
        .context("the main checkout has no name")?
        .to_string_lossy();
    Ok(parent.join(format!("{name}-{suffix}")))
}

/// The main repository's git folder, as `git rev-parse --git-common-dir` names it.
pub fn git_common_dir(repo: &Path) -> Result<PathBuf> {
    let text = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(text.trim()))
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

pub fn resolve_commit(repo: &Path, revision: &str) -> Result<String> {
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

pub fn tree(repo: &Path, revision: &str) -> Result<String> {
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{revision}^{{tree}}"),
        ],
    )
    .with_context(|| format!("{revision} has no tree in {}", repo.display()))
}

/// HEAD's tree when the checkout has no change, untracked files included; otherwise `None`.
pub fn clean_tree(checkout: &Path) -> Result<Option<String>> {
    if !git(
        checkout,
        &["status", "--porcelain", "--untracked-files=all"],
    )?
    .is_empty()
    {
        return Ok(None);
    }
    tree(checkout, "HEAD").map(Some)
}

/// Create the working copy. On failure nothing is left: no folder, worktree or branch.
pub fn create(main: &Path, base_revision: &str, id: u32) -> Result<WorkingCopy> {
    let path = working_copy_path(main, id)?;
    let branch = branch_name(id);
    if crate::git::worktree::branch_exists(main, &branch) {
        bail!(
            "branch {branch} already exists in {}; delete it first",
            main.display()
        );
    }
    let base = make(main, base_revision, &path, Some(&branch))?;
    Ok(WorkingCopy { path, branch, base })
}

/// A working copy at `path` with no branch, for a deploy or release of one commit.
/// Returns the commit.
pub fn create_detached(main: &Path, revision: &str, path: &Path) -> Result<String> {
    make(main, revision, path, None)
}

/// Make the worktree at `path` and fill it. On failure nothing is left: no folder,
/// worktree or branch.
fn make(main: &Path, revision: &str, path: &Path, branch: Option<&str>) -> Result<String> {
    if main.join(".gitmodules").exists() {
        bail!(
            "{} has submodules; task working copies do not support them yet",
            main.display()
        );
    }
    let base = resolve_commit(main, revision)?;
    if fs::symlink_metadata(path).is_ok() {
        bail!(
            "{} already exists; remove it or close its task first",
            path.display()
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
    if let Err(error) = fill(main, path, &base, branch) {
        return match remove(main, path, branch) {
            Ok(()) => Err(error.context("nothing was left behind")),
            Err(cleanup) => Err(error.context(format!(
                "cleanup also failed, remove {} by hand: {cleanup:#}",
                path.display()
            ))),
        };
    }
    Ok(base)
}

fn fill(main: &Path, path: &Path, base: &str, branch: Option<&str>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(main)
        .with_context(|| format!("cannot list {}", main.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != ".git")
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    if !entries.is_empty() && clone_entries(&entries, path, true).is_err() {
        // ^ Not a copy-on-write volume: clear the partial copy and copy plainly.
        clear_except_git(path)?;
        clone_entries(&entries, path, false)?;
    }
    // ^ A worktree nested in the main checkout keeps a `.git` file pointing at
    // the main repository's admin folder. Its copy must not exist here.
    for nested in list_worktrees(main)? {
        if let Ok(relative) = nested.path.strip_prefix(main) {
            if !relative.as_os_str().is_empty() {
                let _ = fs::remove_dir_all(path.join(relative));
            }
        }
    }
    // ^ Fill the empty index first: `reset --hard` on it rewrites every file with a new
    // mtime, and cargo then rebuilds every workspace crate instead of reusing the clone.
    git(path, &["reset", "--quiet", base])?;
    git(path, &["reset", "--hard", "--quiet", base])?;
    git(path, &["clean", "-fdq"])?;
    if let Some(branch) = branch {
        git(path, &["switch", "--quiet", "-c", branch])?;
    }
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

/// Remove the worktree, its folder and its branch, if it has one.
pub fn remove(main: &Path, path: &Path, branch: Option<&str>) -> Result<()> {
    let path_text = path.to_string_lossy().to_string();
    let removed = git(main, &["worktree", "remove", "--force", &path_text]);
    if fs::symlink_metadata(path).is_ok() {
        fs::remove_dir_all(path).with_context(|| format!("cannot remove {}", path.display()))?;
    }
    if removed.is_err() {
        git(main, &["worktree", "prune"])?;
    }
    if let Some(branch) = branch.filter(|branch| crate::git::worktree::branch_exists(main, branch))
    {
        git(main, &["branch", "-D", branch])?;
    }
    Ok(())
}

/// Most paths a record keeps, and most bytes they may take together.
const MAX_PATHS: usize = 500;
const MAX_PATH_BYTES: usize = 32 * 1024;

/// Paths changed between two commits: the first ones that fit the caps, and the full count.
pub fn changed_paths(repo: &Path, from: &str, to: &str) -> Result<(Vec<String>, u32)> {
    let output = crate::git::output_with_timeout_limit(
        git_cmd(repo).args(["diff", "--name-only", "-z", "--no-renames", from, to]),
        std::time::Duration::from_secs(60),
        4 * 1024 * 1024,
    )
    .context("cannot run git diff")?;
    if !output.status.success() {
        bail!(
            "git diff {from} {to} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut paths = Vec::new();
    let (mut total, mut bytes) = (0u32, 0usize);
    for path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        total += 1;
        bytes += path.len();
        if paths.len() < MAX_PATHS && bytes <= MAX_PATH_BYTES {
            paths.push(String::from_utf8_lossy(path).into_owned());
        }
    }
    Ok((paths, total))
}

/// The commit where `head` left `target`.
pub fn merge_base(repo: &Path, head: &str, target: &str) -> Result<String> {
    git(repo, &["merge-base", head, target])
}

pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = git_cmd(repo)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("cannot run git merge-base")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {ancestor} {descendant} failed"),
    }
}

/// Tracked files with changes in a checkout (untracked files are left to git's own checks).
pub fn tracked_changes(checkout: &Path) -> Result<Vec<String>> {
    // ^ Paths only: porcelain status columns would be cut by the helper's trim.
    let changed = git(checkout, &["diff", "--name-only", "--no-renames", "HEAD"])?;
    Ok(changed
        .lines()
        .take(MAX_LISTED_CHANGES)
        .map(str::to_string)
        .collect())
}

const MAX_LISTED_CHANGES: usize = 50;

pub enum Rebase {
    Done,
    /// The rebase was aborted; these paths conflicted.
    Conflict(Vec<String>),
}

/// Rebase the checked-out branch of `working_copy` onto `onto`. A conflict is aborted,
/// so the branch is left as it was.
pub fn rebase(working_copy: &Path, onto: &str) -> Result<Rebase> {
    let output = git_cmd(working_copy)
        .args(["rebase", "--quiet", onto])
        .env("GIT_EDITOR", "true")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("cannot run git rebase")?;
    if output.status.success() {
        return Ok(Rebase::Done);
    }
    let conflicted =
        git(working_copy, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
    git(working_copy, &["rebase", "--abort"])
        .context("git rebase failed and could not be aborted; finish it by hand")?;
    if conflicted.is_empty() {
        bail!(
            "git rebase onto {onto} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Rebase::Conflict(
        conflicted
            .lines()
            .take(MAX_LISTED_CHANGES)
            .map(str::to_string)
            .collect(),
    ))
}

/// The worktree that has `branch` checked out, if one has.
fn checkout_of(main: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(list_worktrees(main)?
        .into_iter()
        .find(|worktree| worktree.branch == branch)
        .map(|worktree| worktree.path))
}

/// Files `checkout` has changed (tracked or new) that the commits from its HEAD to `commit`
/// change too: a fast-forward would overwrite them. Other changes stay as they are.
/// ref: docs/tasks.md "Landing"
fn overlapping_changes(checkout: &Path, commit: &str) -> Result<Vec<String>> {
    // ^ Three dots: from where `commit` left HEAD, so a task that still needs its rebase
    // shows the files its own commits change.
    let landing = git(
        checkout,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            &format!("HEAD...{commit}"),
        ],
    )?;
    let landing: std::collections::HashSet<&str> = landing.lines().collect();
    let tracked = git(checkout, &["diff", "--name-only", "--no-renames", "HEAD"])?;
    let new = git(checkout, &["ls-files", "--others", "--exclude-standard"])?;
    Ok(tracked
        .lines()
        .chain(new.lines())
        .filter(|path| landing.contains(path))
        .take(MAX_LISTED_CHANGES)
        .map(str::to_string)
        .collect())
}

/// What would stop `commit` from landing on `default_branch`: the checkout's changed files
/// it overlaps, or none when the branch is not checked out. ref: docs/tasks.md "Ledger"
pub fn land_blockers(main: &Path, default_branch: &str, commit: &str) -> Result<Vec<String>> {
    match checkout_of(main, default_branch)? {
        Some(checkout) => overlapping_changes(&checkout, commit),
        None => Ok(Vec::new()),
    }
}

/// Move `default_branch` from `old_tip` to `commit`, fast-forward only. Where the branch is
/// checked out, its changed files that the move also changes refuse it; other changes stay
/// (`git merge --ff-only` keeps them). Elsewhere the ref moves with `old_tip` as guard, so a
/// branch that moved meanwhile is never overwritten. Nothing stashes or commits the owner's
/// files. ref: docs/tasks.md "Landing"
pub fn fast_forward(main: &Path, default_branch: &str, old_tip: &str, commit: &str) -> Result<()> {
    match checkout_of(main, default_branch)? {
        Some(checkout) => {
            let blocked = overlapping_changes(&checkout, commit)?;
            if !blocked.is_empty() {
                // ^ Words first: a path at the start of the line lost its `/` when the owner
                // copied the message from the page (2026-10-25).
                bail!(
                    "{default_branch} has uncommitted changes in {} that this land changes too; commit or stash them in {}, then land again",
                    blocked.join(", "),
                    checkout.display()
                );
            }
            git(&checkout, &["merge", "--ff-only", "--quiet", commit]).map(drop)
        }
        None => git(
            main,
            &[
                "update-ref",
                &format!("refs/heads/{default_branch}"),
                commit,
                old_tip,
            ],
        )
        .map(drop),
    }
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
