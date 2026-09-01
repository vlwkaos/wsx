// Git info via CLI — branch, commits, modified files, ahead/behind

use super::git_cmd;
use crate::model::workspace::{
    CommitSummary, FetchFailReason, GitInfo, SubmoduleCommitState, SubmoduleInfo, SubtreeInfo,
};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct FetchOutcome {
    pub success: bool,
    pub reason: Option<FetchFailReason>,
}

pub fn get_git_info(
    worktree_path: &Path,
    _default_branch: &str,
    configured_subtrees: &[PathBuf],
) -> Option<GitInfo> {
    // ^ [[Worktree Model]] Git owns submodule gitlinks; project config only
    // declares otherwise-indistinguishable subtree roots.
    let status = status_porcelain2(worktree_path)?;
    let mut submodules = submodule_status(worktree_path);
    let mut ordinary_files = Vec::new();
    for entry in status.entries {
        let Some(token) = entry.submodule else {
            ordinary_files.push(entry.path);
            continue;
        };
        if let Some(submodules) = &mut submodules {
            let info = submodules
                .iter_mut()
                .find(|submodule| submodule.path == entry.path);
            let submodule = match info {
                Some(submodule) => submodule,
                None => {
                    submodules.push(SubmoduleInfo {
                        path: entry.path.clone(),
                        commit_state: SubmoduleCommitState::InSync,
                        modified_content: false,
                        untracked_content: false,
                    });
                    submodules.last_mut().expect("just inserted submodule")
                }
            };
            if entry.unmerged {
                submodule.commit_state = SubmoduleCommitState::Conflict;
            } else if token.as_bytes().get(1) == Some(&b'C') {
                submodule.commit_state = SubmoduleCommitState::CommitChanged;
            }
            submodule.modified_content |= token.as_bytes().get(2) == Some(&b'M');
            submodule.untracked_content |= token.as_bytes().get(3) == Some(&b'U');
        }
    }

    let mut subtrees = configured_subtrees
        .iter()
        .map(|path| SubtreeInfo {
            path: path.to_string_lossy().into_owned(),
            modified_files: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut modified_files = Vec::new();
    for file in ordinary_files {
        let file_path = Path::new(&file);
        if let Some(subtree) = subtrees
            .iter_mut()
            .find(|subtree| file_path.starts_with(Path::new(&subtree.path)))
        {
            subtree.modified_files.push(file);
        } else {
            modified_files.push(file);
        }
    }

    Some(GitInfo {
        recent_commits: recent_commits(worktree_path, 3),
        modified_files,
        submodules,
        subtrees,
        ahead: status.ahead,
        behind: status.behind,
        remote_branch: status.upstream,
    })
}

struct StatusEntry {
    path: String,
    submodule: Option<String>,
    unmerged: bool,
}

struct StatusResult {
    upstream: Option<String>,
    ahead: usize,
    behind: usize,
    entries: Vec<StatusEntry>,
}

/// Parse `git status --porcelain=2 --branch` output without collapsing
/// submodule gitlinks into ordinary modified-file rows.
fn status_porcelain2(path: &Path) -> Option<StatusResult> {
    let out = super::output_with_timeout(
        git_read(path).args(["status", "--porcelain=2", "--branch", "-z"]),
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut branch = String::new();
    let mut upstream = None;
    let mut ahead = 0usize;
    let mut behind = 0usize;
    let mut entries = Vec::new();

    let mut skip_rename_source = false;
    for line in text.split('\0').filter(|line| !line.is_empty()) {
        if skip_rename_source {
            skip_rename_source = false;
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.head ") {
            branch = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("# branch.upstream ") {
            let value = value.trim();
            if !value.is_empty() {
                upstream = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            let mut parts = value.split_whitespace();
            if let Some(value) = parts.next() {
                ahead = value.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(value) = parts.next() {
                behind = value.trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if let Some(entry) = parse_status_entry(line) {
            skip_rename_source = line.starts_with("2 ");
            entries.push(entry);
        }
    }
    if branch.is_empty() {
        return None;
    }
    Some(StatusResult {
        upstream,
        ahead,
        behind,
        entries,
    })
}

fn parse_status_entry(line: &str) -> Option<StatusEntry> {
    if let Some(path) = line.strip_prefix("? ") {
        return Some(StatusEntry {
            path: path.trim().to_string(),
            submodule: None,
            unmerged: false,
        });
    }
    let (field_count, unmerged) = if line.starts_with("1 ") {
        (9, false)
    } else if line.starts_with("2 ") {
        (10, false)
    } else if line.starts_with("u ") {
        (11, true)
    } else {
        return None;
    };
    let fields = line.splitn(field_count, ' ').collect::<Vec<_>>();
    if fields.len() != field_count {
        return None;
    }
    let token = fields.get(2)?.to_string();
    let path = fields.last()?.split('\t').next()?.to_string();
    Some(StatusEntry {
        path,
        submodule: token.starts_with('S').then_some(token),
        unmerged,
    })
}

fn submodule_status(path: &Path) -> Option<Vec<SubmoduleInfo>> {
    if !path.join(".gitmodules").exists() {
        return Some(Vec::new());
    }
    let out = super::output_with_timeout(
        git_read(path).args(["submodule", "status", "--recursive"]),
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(parse_submodule_status_line)
            .collect(),
    )
}

fn parse_submodule_status_line(line: &str) -> Option<SubmoduleInfo> {
    let marker = line.chars().next()?;
    let rest = line.get(1..)?.trim_start();
    let mut fields = rest.splitn(2, ' ');
    let commit = fields.next()?;
    if commit.len() < 7
        || !commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return None;
    }
    let path = fields.next()?.split(" (").next()?.trim();
    if path.is_empty() {
        return None;
    }
    let commit_state = match marker {
        '+' => SubmoduleCommitState::CommitChanged,
        '-' => SubmoduleCommitState::Uninitialized,
        'U' => SubmoduleCommitState::Conflict,
        _ => SubmoduleCommitState::InSync,
    };
    Some(SubmoduleInfo {
        path: path.to_string(),
        commit_state,
        modified_content: false,
        untracked_content: false,
    })
}

/// Advisory cross-process lockfile for git fetch. Created with O_CREAT|O_EXCL.
/// Returns the lock path if acquired, None if another process holds it (< 120s old).
fn try_fetch_lock(path: &Path) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let hash = h.finish();
    let lock_path = std::env::temp_dir().join(format!("wsx-fetch-{:x}.lock", hash));
    // Check if existing lock is stale (> 120s) — crashed process protection
    if let Ok(meta) = std::fs::metadata(&lock_path) {
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        if age < 120 {
            return None; // another process holds a fresh lock
        }
        let _ = std::fs::remove_file(&lock_path); // stale, clean up
    }
    // Try atomic create with O_CREAT|O_EXCL
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    match opts.open(&lock_path) {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
            Some(lock_path)
        }
        Err(_) => None, // lost the race
    }
}

/// RAII guard that removes the lockfile on drop.
struct FetchLockGuard(std::path::PathBuf);
impl Drop for FetchLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run `git fetch` — uses `output_with_timeout` for process-group cleanup on timeout.
/// Advisory cross-process lockfile prevents duplicate concurrent fetches from multiple instances.
pub fn git_fetch(path: &Path) -> FetchOutcome {
    let Some(lock_path) = try_fetch_lock(path) else {
        // Another instance is handling this fetch; report success so backoff stays low.
        return FetchOutcome {
            success: true,
            reason: None,
        };
    };
    let _lock = FetchLockGuard(lock_path);
    let result = super::output_with_timeout(
        git_cmd(path).args(["fetch", "--no-tags", "--quiet"]),
        std::time::Duration::from_secs(10),
    );
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => FetchOutcome {
            success: false,
            reason: Some(FetchFailReason::Timeout),
        },
        Err(_) => FetchOutcome {
            success: false,
            reason: Some(FetchFailReason::Network),
        },
        Ok(out) if out.status.success() => FetchOutcome {
            success: true,
            reason: None,
        },
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            FetchOutcome {
                success: false,
                reason: Some(classify_fetch_error(&stderr)),
            }
        }
    }
}

fn classify_fetch_error(stderr: &str) -> FetchFailReason {
    let lower = stderr.to_lowercase();
    if lower.contains("authentication failed")
        || lower.contains("permission denied")
        || lower.contains("could not read username")
        || lower.contains("invalid username or password")
        || lower.contains("repository not found")
    {
        FetchFailReason::Auth
    } else {
        FetchFailReason::Network
    }
}

pub fn current_branch(path: &Path) -> Option<String> {
    let out = super::output_with_timeout(
        git_read(path).args(["branch", "--show-current"]),
        std::time::Duration::from_secs(5),
    )
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
    let Ok(out) = super::output_with_timeout(
        git_read(path).args(["log", "--oneline", &format!("-{}", n)]),
        std::time::Duration::from_secs(5),
    ) else {
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
    use super::{
        get_git_info, parse_status_entry, parse_submodule_status_line, try_fetch_lock,
        FetchLockGuard,
    };
    use crate::model::workspace::SubmoduleCommitState;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn fetch_lock_acquired_on_fresh_path() {
        let path = PathBuf::from("/tmp/wsx_test_lock_fresh");
        let result = try_fetch_lock(&path);
        assert!(result.is_some(), "should acquire lock on a fresh path");
        let lock_path = result.unwrap();
        assert!(lock_path.exists(), "lockfile should exist after acquire");
        let _guard = FetchLockGuard(lock_path.clone());
        // guard drop removes file
        drop(_guard);
        assert!(!lock_path.exists(), "lockfile should be removed on drop");
    }

    #[test]
    fn fetch_lock_fails_when_held() {
        let path = PathBuf::from("/tmp/wsx_test_lock_held");
        let lock1 = try_fetch_lock(&path);
        assert!(lock1.is_some(), "first acquire should succeed");
        let lock2 = try_fetch_lock(&path);
        assert!(
            lock2.is_none(),
            "second acquire should fail while first is held"
        );
        drop(lock1.map(FetchLockGuard));
    }

    #[test]
    fn fetch_lock_different_paths_independent() {
        let path_a = PathBuf::from("/tmp/wsx_test_lock_a");
        let path_b = PathBuf::from("/tmp/wsx_test_lock_b");
        let lock_a = try_fetch_lock(&path_a);
        let lock_b = try_fetch_lock(&path_b);
        assert!(lock_a.is_some(), "lock for path_a should succeed");
        assert!(
            lock_b.is_some(),
            "lock for path_b should succeed independently"
        );
        drop(lock_a.map(FetchLockGuard));
        drop(lock_b.map(FetchLockGuard));
    }

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
        let info = get_git_info(&repo, "main", &[]).expect("git info should be available");
        assert!(
            info.modified_files.iter().any(|f| f == "tracked.txt"),
            "expected tracked.txt in modified files, got {:?}",
            info.modified_files
        );
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn porcelain_submodule_entry_retains_structured_flags_and_spaced_path() {
        let entry = parse_status_entry(
            "1 .M SCMU 160000 160000 160000 aaaaaaa bbbbbbb vendor/module with spaces",
        )
        .unwrap();
        assert_eq!(entry.path, "vendor/module with spaces");
        assert_eq!(entry.submodule.as_deref(), Some("SCMU"));
        assert!(!entry.unmerged);
    }

    #[test]
    fn nul_porcelain_preserves_spaced_and_renamed_paths() {
        let repo = init_temp_repo();
        fs::write(repo.join("old name.txt"), "one\n").unwrap();
        git(&repo, &["add", "old name.txt"]);
        git(&repo, &["commit", "-m", "add spaced file", "-q"]);
        git(&repo, &["mv", "old name.txt", "new name.txt"]);

        let info = get_git_info(&repo, "main", &[]).unwrap();

        assert!(
            info.modified_files
                .iter()
                .any(|path| path == "new name.txt"),
            "{:?}",
            info.modified_files
        );
        assert!(!info
            .modified_files
            .iter()
            .any(|path| path == "old name.txt"));
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn submodule_status_marker_maps_parent_gitlink_state() {
        for (marker, expected) in [
            (' ', SubmoduleCommitState::InSync),
            ('+', SubmoduleCommitState::CommitChanged),
            ('-', SubmoduleCommitState::Uninitialized),
            ('U', SubmoduleCommitState::Conflict),
        ] {
            let line = format!("{marker}0123456789abcdef vendor/module (heads/main)");
            let info = parse_submodule_status_line(&line).unwrap();
            assert_eq!(info.path, "vendor/module");
            assert_eq!(info.commit_state, expected);
        }
    }

    #[test]
    fn submodule_changes_are_separate_from_ordinary_local_files() {
        let child = init_temp_repo();
        let parent = init_temp_repo();
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                child.to_str().unwrap(),
                "vendor/module with spaces",
            ],
        );
        git(&parent, &["commit", "-m", "add submodule", "-q"]);
        let checkout = parent.join("vendor/module with spaces");
        fs::write(checkout.join("tracked.txt"), "dirty\n").unwrap();

        let dirty = get_git_info(&parent, "main", &[]).unwrap();

        assert!(dirty.modified_files.is_empty());
        let submodule = &dirty.submodules.as_ref().unwrap()[0];
        assert_eq!(submodule.path, "vendor/module with spaces");
        assert_eq!(submodule.commit_state, SubmoduleCommitState::InSync);
        assert!(submodule.modified_content);

        git(&checkout, &["config", "user.email", "test@example.com"]);
        git(&checkout, &["config", "user.name", "Test User"]);
        git(&checkout, &["add", "tracked.txt"]);
        git(&checkout, &["commit", "-m", "advance submodule", "-q"]);
        let advanced = get_git_info(&parent, "main", &[]).unwrap();
        let submodule = &advanced.submodules.as_ref().unwrap()[0];
        assert_eq!(submodule.commit_state, SubmoduleCommitState::CommitChanged);

        let _ = fs::remove_dir_all(parent);
        let _ = fs::remove_dir_all(child);
    }

    #[test]
    fn configured_subtree_changes_are_separate_from_local_files() {
        let repo = init_temp_repo();
        fs::create_dir_all(repo.join("vendor/asched")).unwrap();
        fs::write(repo.join("vendor/asched/source.rs"), "one\n").unwrap();
        fs::write(repo.join("ordinary.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "add files", "-q"]);
        fs::write(repo.join("vendor/asched/source.rs"), "two\n").unwrap();
        fs::write(repo.join("ordinary.txt"), "two\n").unwrap();

        let info = get_git_info(&repo, "main", &[PathBuf::from("vendor/asched")]).unwrap();

        assert_eq!(info.modified_files, ["ordinary.txt"]);
        assert_eq!(info.subtrees.len(), 1);
        assert_eq!(info.subtrees[0].path, "vendor/asched");
        assert_eq!(info.subtrees[0].modified_files, ["vendor/asched/source.rs"]);
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn get_git_info_returns_none_when_status_fails() {
        let repo = init_temp_repo();
        // Corrupt index so branch detection still works but status exits non-zero.
        fs::write(repo.join(".git").join("index"), "broken").expect("index should be overwritten");

        let info = get_git_info(&repo, "main", &[]);
        assert!(
            info.is_none(),
            "expected None when status fails, got {:?}",
            info
        );
        let _ = fs::remove_dir_all(repo);
    }
}
