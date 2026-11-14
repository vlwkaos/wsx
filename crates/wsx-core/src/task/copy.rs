//! A task's working copy: its own git worktree holding a copy-on-write clone
//! of the main checkout's files, so builds start warm.
//! ref: docs/tasks.md "Working copy"

use crate::git::{git_cmd, run_clearing_stale_lock, worktree::list_worktrees, LockCleared};
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
    finished(args, output)
}

/// How long a read-only git call of the ledger and the land check waits.
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `git` that reads and gives up after `READ_TIMEOUT`: a git that hangs is an error (an
/// `io::Error` of kind `TimedOut` at the root), not a caller that waits for ever.
pub fn read(repo: &Path, args: &[&str]) -> Result<String> {
    let output = crate::git::output_with_timeout(git_cmd(repo).args(args), READ_TIMEOUT)
        .with_context(|| format!("cannot run git {}", args.join(" ")))?;
    finished(args, output)
}

/// `git` for a step that takes the checkout's index lock; a stale one is cleared and the
/// step runs again. ref: docs/tasks.md "Landing"
fn git_clearing_lock(repo: &Path, args: &[&str]) -> Result<(String, Option<LockCleared>)> {
    let (output, cleared) = run_clearing_stale_lock(repo, args)
        .with_context(|| format!("cannot run git {}", args.join(" ")))?;
    let text = finished(args, output).map_err(|error| match &cleared {
        // ^ The caller never sees `cleared` of a step that failed: say it here.
        Some(lock) => anyhow::anyhow!(
            "{error:#}; wsx had removed the stale {} ({} s old) first",
            lock.path.display(),
            lock.age_s
        ),
        None => error,
    })?;
    Ok((text, cleared))
}

fn finished(args: &[&str], output: std::process::Output) -> Result<String> {
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
    let groups = super::build::clone_groups(main, path)?;
    let clone_all = |copy_on_write| {
        groups.iter().try_for_each(|(sources, destination)| {
            clone_entries(sources, destination, copy_on_write)
        })
    };
    if clone_all(true).is_err() {
        // ^ Not a copy-on-write volume: clear the partial copy and copy plainly.
        clear_except_git(path)?;
        clone_all(false)?;
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
    fs::create_dir_all(destination)
        .with_context(|| format!("cannot make {}", destination.display()))?;
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

/// Remove the detached `<repo>-<kind>-<pid>` copies of `kinds` (deploy, release) whose
/// process is gone, after `cargo clean`: a run killed before its end (a caller's timeout)
/// leaves its copy. A worktree on a branch is never one of them. Returns the removed paths. ref: docs/tasks.md "Deploy and release"
pub fn remove_orphans(main: &Path, kinds: &[&str]) -> Vec<PathBuf> {
    let Some(repo) = main
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for worktree in list_worktrees(main).unwrap_or_default() {
        let Some(name) = worktree.path.file_name().map(|name| name.to_string_lossy()) else {
            continue;
        };
        let orphan = worktree.branch == "HEAD"
            && kinds.iter().any(|kind| {
                name.strip_prefix(&format!("{repo}-{kind}-"))
                    .and_then(|pid| pid.parse::<i32>().ok())
                    .is_some_and(|pid| pid > 0 && !alive(pid))
            });
        if orphan {
            crate::ops::cargo_clean(&worktree.path);
            if remove(main, &worktree.path, None).is_ok() {
                removed.push(worktree.path);
            }
        }
    }
    removed
}

/// `kill(pid, 0)`: a process another user owns (`EPERM`) is alive too.
fn alive(pid: i32) -> bool {
    (unsafe { libc::kill(pid, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

/// Files `checkout` has changed (tracked or new) that `landing` (a `git diff` range from its
/// HEAD) changes too: moving the checkout there would overwrite them. Other changes stay as
/// they are. ref: docs/tasks.md "Landing"
fn overlapping_changes(checkout: &Path, landing: &str) -> Result<Vec<String>> {
    let landing = read(checkout, &["diff", "--name-only", "--no-renames", landing])?;
    let landing: std::collections::HashSet<&str> = landing.lines().collect();
    let tracked = read(checkout, &["diff", "--name-only", "--no-renames", "HEAD"])?;
    let new = read(checkout, &["ls-files", "--others", "--exclude-standard"])?;
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
        // ^ Three dots: from where `commit` left HEAD, so a task that still needs its rebase
        // shows the files its own commits change.
        Some(checkout) => overlapping_changes(&checkout, &format!("HEAD...{commit}")),
        None => Ok(Vec::new()),
    }
}

/// Refuse to move `branch` to `commit` (a `what`: land or push) when its checkout has changed
/// files that differ between its HEAD and `commit`: the move would overwrite them. Other
/// changes stay. Nothing to check when the branch is not checked out.
pub fn check_move(main: &Path, branch: &str, commit: &str, what: &str) -> Result<()> {
    let Some(checkout) = checkout_of(main, branch)? else {
        return Ok(());
    };
    let blocked = overlapping_changes(&checkout, &format!("HEAD..{commit}"))?;
    if !blocked.is_empty() {
        // ^ Words first: a path at the start of the line lost its `/` when the owner
        // copied the message from the page (2026-10-25).
        bail!(
            "{branch} has uncommitted changes in {} that this {what} changes too; commit or stash them in {}, then {what} again",
            blocked.join(", "),
            checkout.display(),
        );
    }
    Ok(())
}

/// Move `branch` from `old_tip` to `commit`: a fast-forward, or any commit with `force` (a
/// push that rewrites the default branch). Where the branch is checked out, its changed
/// files that the move also changes refuse it (`check_move`); other changes stay (`git merge
/// --ff-only` and `git reset --keep` keep them). Elsewhere the ref moves with `old_tip` as
/// guard, so a branch that moved meanwhile is never overwritten. Nothing stashes or commits
/// the owner's files. Gives back the stale index lock it cleared, if any.
/// ref: docs/tasks.md "Landing"
pub fn move_branch(
    main: &Path,
    branch: &str,
    old_tip: &str,
    commit: &str,
    force: bool,
) -> Result<Option<LockCleared>> {
    check_move(main, branch, commit, if force { "push" } else { "land" })?;
    match checkout_of(main, branch)? {
        Some(checkout) => {
            let step: &[&str] = if force {
                &["reset", "--keep", "--quiet", commit]
            } else {
                &["merge", "--ff-only", "--quiet", commit]
            };
            git_clearing_lock(&checkout, step).map(|(_, cleared)| cleared)
        }
        None => git(
            main,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                commit,
                old_tip,
            ],
        )
        .map(|_| None),
    }
}

/// A commit of the main checkout's `files` as they are on disk, on top of `branch`'s tip,
/// which must be the checkout's HEAD; the checkout, its index and the branch stay as they are.
/// Gives back the commit and the tip it is on; `take_commit` moves the branch there.
/// ref: docs/tasks.md "Manager"
pub fn commit_files(
    main: &Path,
    branch: &str,
    files: &[String],
    message: &str,
) -> Result<(String, String)> {
    let head = git(main, &["symbolic-ref", "--quiet", "HEAD"]).unwrap_or_default();
    if head.trim() != format!("refs/heads/{branch}") {
        bail!(
            "{} has {} checked out, not {branch}",
            main.display(),
            head.trim().trim_start_matches("refs/heads/")
        );
    }
    if let Some(bad) = files.iter().find(|file| {
        let path = Path::new(file);
        file.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
    }) {
        bail!("{bad:?} is not a path inside the checkout");
    }
    let tip = resolve_commit(main, "HEAD")?;
    // ^ A private index, so the owner's staged changes stay out of the commit and in place.
    let index = std::env::temp_dir().join(format!("wsx-commit-index-{}", std::process::id()));
    let with_index = |args: &[&str]| -> Result<String> {
        let output = git_cmd(main)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .with_context(|| format!("cannot run git {}", args.join(" ")))?;
        finished(args, output)
    };
    let tree = (|| {
        with_index(&["read-tree", &tip])?;
        let mut add = vec!["update-index", "--add", "--remove", "--"];
        add.extend(files.iter().map(String::as_str));
        with_index(&add)?;
        let changed = with_index(&[
            "diff-index",
            "--cached",
            "--name-only",
            "--no-renames",
            &tip,
        ])?;
        let changed: std::collections::HashSet<&str> = changed.lines().collect();
        let unchanged: Vec<&str> = files
            .iter()
            .map(String::as_str)
            .filter(|file| !changed.contains(file))
            .collect();
        if !unchanged.is_empty() {
            bail!("{} has no change to commit", unchanged.join(", "));
        }
        with_index(&["write-tree"])
    })();
    let _ = fs::remove_file(&index);
    let tree = tree?;
    let commit = git(
        main,
        &["commit-tree", tree.trim(), "-p", &tip, "-m", message],
    )?;
    Ok((commit.trim().to_string(), tip))
}

/// Move `branch`, checked out in `main`, from `tip` to `commit` of `commit_files` (refused when
/// the branch moved meanwhile), and match the index of `files` to it; the files on disk and
/// the owner's other staged changes stay.
pub fn take_commit(
    main: &Path,
    branch: &str,
    tip: &str,
    commit: &str,
    files: &[String],
) -> Result<()> {
    let reflog = format!(
        "commit: {}",
        read(main, &["log", "-1", "--format=%s", commit])?.trim()
    );
    git(
        main,
        &[
            "update-ref",
            "-m",
            &reflog,
            &format!("refs/heads/{branch}"),
            commit,
            tip,
        ],
    )
    .context("nothing was committed")?;
    let mut reset = vec!["reset", "--quiet", commit, "--"];
    reset.extend(files.iter().map(String::as_str));
    git_clearing_lock(main, &reset)
        .with_context(|| format!("{branch} is at {commit}, but the index of the files is not"))
        .map(drop)
}

/// How long a call to the remote may take: `git_cmd`'s ssh never asks (BatchMode), but a
/// server can stall after it connects. ref: docs/tasks.md "Push"
const REMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn remote_git(main: &Path, args: &[&str]) -> Result<String> {
    let (mut command, refused) = crate::git::remote_cmd(main);
    let output =
        crate::git::output_with_timeout(command.args(args), REMOTE_TIMEOUT).with_context(|| {
            format!(
                "git {} did not end within {} s",
                args.join(" "),
                REMOTE_TIMEOUT.as_secs()
            )
        })?;
    // ^ OpenSSH's words when no offered key is taken: say which agent was asked, or none.
    let keys_refused = !output.status.success()
        && String::from_utf8_lossy(&output.stderr).contains("Permission denied (publickey");
    finished(args, output).map_err(|error| match keys_refused {
        true => anyhow::anyhow!("{error}; {refused}"),
        false => error,
    })
}

/// `branch`'s tip on `remote`, or none when the remote has no such branch.
pub fn remote_tip(main: &Path, remote: &str, branch: &str) -> Result<Option<String>> {
    let text = remote_git(
        main,
        &[
            "ls-remote",
            "--heads",
            remote,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    Ok(text
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .map(str::to_string))
}

/// Push `commit` to `remote`'s `branch`. With `lease` the push may rewrite the branch, but
/// only while the remote still has that tip (empty: no such branch): `--force-with-lease`.
pub fn push(
    main: &Path,
    remote: &str,
    branch: &str,
    commit: &str,
    lease: Option<&str>,
) -> Result<()> {
    let target = format!("refs/heads/{branch}");
    let lease = lease.map(|tip| format!("--force-with-lease={target}:{tip}"));
    let refspec = format!("{commit}:{target}");
    let mut args = vec!["push", "--quiet"];
    args.extend(lease.as_deref());
    args.extend([remote, refspec.as_str()]);
    remote_git(main, &args).map(|_| ())
}

/// Commits on `branch` that `target` does not have.
pub fn unlanded_commits(main: &Path, branch: &str, target: &str) -> Result<u32> {
    commits_after(main, Some(target), branch)
}

/// Commits in `from..to`, or all of `to` when `from` is none.
pub fn commits_after(main: &Path, from: Option<&str>, to: &str) -> Result<u32> {
    let count = git(main, &["rev-list", "--count", &commit_range(from, to)])?;
    count
        .parse()
        .with_context(|| format!("git rev-list returned {count:?}"))
}

/// The commits in `from..to` (all of `to` when `from` is none), newest first, full ids.
pub fn commits_in(main: &Path, from: Option<&str>, to: &str) -> Result<Vec<String>> {
    let list = git(main, &["rev-list", &commit_range(from, to)])?;
    Ok(list.lines().map(str::to_string).collect())
}

fn commit_range(from: Option<&str>, to: &str) -> String {
    from.map_or_else(|| to.to_string(), |from| format!("{from}..{to}"))
}
