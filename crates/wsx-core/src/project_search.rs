//! Finding a project to add: the TUI's `p` and the web's Add project sheet use these
//! (docs/web-client.md "Projects"). The git repositories under home come from a background scan;
//! a typed path (`/…` or `~…`) lists that folder's folders instead, since the scan misses repos
//! outside home, below `MAX_SCAN_DEPTH` or under `SKIP_DIRS`. `register_project` still decides
//! on submit; `folder` says beforehand what a path is.
use crate::config::global::{normalize_project_path, GlobalConfig};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::SystemTime;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "Library",
    "Applications",
    ".Trash",
];

/// Depth from `$HOME` before the scan stops descending. Walks stop at any `.git` directory, so
/// this only matters for trees with no repos in them; 8 catches `~/work/<org>/<year>/<project>`.
const MAX_SCAN_DEPTH: usize = 8;

/// Streams the git repositories under home as `~/foo/bar/` over `tx`; the caller owns the
/// results, so they outlive one sheet or modal. Stops when the receiver is gone.
pub fn scan_git_repos(tx: mpsc::Sender<String>) {
    let Some(home) = dirs::home_dir() else { return };
    walk_for_git(&home, 0, &tx);
}

/// Returns false when the receiver dropped: the caller stops walking.
fn walk_for_git(dir: &Path, depth: usize, tx: &mpsc::Sender<String>) -> bool {
    if depth > MAX_SCAN_DEPTH {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    let mut has_git = false;
    let mut subdirs = vec![];
    for entry in entries.filter_map(|entry| entry.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        if name == ".git" && path.is_dir() {
            has_git = true;
            continue;
        }
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    if has_git {
        // ^ A repo found: nested repos are not listed.
        return tx.send(display_path(dir, true)).is_ok();
    }
    subdirs
        .into_iter()
        .all(|subdir| walk_for_git(&subdir, depth + 1, tx))
}

/// The list for what the owner typed: the folders of a path (`/…`, `~…`), else the scanned
/// repositories that match it, newest first while nothing is typed.
pub fn suggestions(query: &str, repos: &[String]) -> Vec<String> {
    if query.contains('/') || query.starts_with('~') {
        path_completions(query)
    } else {
        repo_matches(query, repos)
    }
}

fn repo_matches(query: &str, repos: &[String]) -> Vec<String> {
    if query.is_empty() {
        // ^ Newest first: the time the repo's git folder last changed (a commit, a checkout).
        let changed = |repo: &str| {
            std::fs::metadata(expand_input(repo).0.join(".git"))
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };
        let mut dated: Vec<(SystemTime, &String)> =
            repos.iter().map(|repo| (changed(repo), repo)).collect();
        dated.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        return dated.into_iter().map(|(_, repo)| repo.clone()).collect();
    }
    // ^ The repository's name first: the name itself, then a name that starts with the query, then
    // the name's letters in order, then the path's; within each, the closer match.
    let query_lower = query.to_lowercase();
    let mut scored: Vec<((u8, i32), &String)> = repos
        .iter()
        .filter_map(|repo| {
            let rel = repo.strip_prefix("~/").unwrap_or(repo);
            let rel = rel.strip_suffix('/').unwrap_or(rel);
            let name = rel.rsplit('/').next().unwrap_or(rel);
            let lower = name.to_lowercase();
            let rank = if lower == query_lower {
                (3, 0)
            } else if lower.starts_with(&query_lower) {
                (2, -(name.len() as i32))
            } else if let Some(score) = fuzzy_score(query, name) {
                (1, score)
            } else {
                (0, fuzzy_score(query, rel)?)
            };
            Some((rank, repo))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.into_iter().map(|(_, repo)| repo.clone()).collect()
}

/// The folders under a typed path that match its last part, as `~/…/` when it started with `~`.
fn path_completions(input: &str) -> Vec<String> {
    let (expanded, tilde) = expand_input(input);
    let (parent, prefix) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let parent = expanded
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| expanded.clone());
        let prefix = expanded
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        (parent, prefix)
    };
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return vec![];
    };
    let mut scored: Vec<(i32, String)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            let score = fuzzy_score(&prefix, &name)?;
            Some((score, display_path(&parent.join(&name), tilde)))
        })
        .collect();
    if prefix.is_empty() {
        scored.sort_by(|a, b| a.1.cmp(&b.1));
    } else {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    scored.into_iter().map(|(_, path)| path).collect()
}

/// Subsequence match, case-insensitive: every query character in order. Higher is better: runs
/// of characters and a match at the start count more.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let query: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut at = 0;
    let mut score = 0i32;
    let mut run = 0i32;
    for (index, c) in target.chars().map(|c| c.to_ascii_lowercase()).enumerate() {
        if at < query.len() && c == query[at] {
            run += 1;
            score += 1 + run;
            if index == 0 {
                score += 4;
            }
            at += 1;
        } else {
            run = 0;
        }
    }
    (at == query.len()).then_some(score)
}

/// The path a typed value names, and whether it was written from `~`.
fn expand_input(input: &str) -> (PathBuf, bool) {
    if let Some(home) = dirs::home_dir() {
        if let Some(rest) = input.strip_prefix("~/") {
            return (home.join(rest), true);
        }
        if input == "~" {
            return (home, true);
        }
    }
    (
        PathBuf::from(if input.is_empty() { "." } else { input }),
        false,
    )
}

/// `~/…/` under home when `tilde`, else the full path; always with a trailing `/`.
pub fn display_path(path: &Path, tilde: bool) -> String {
    if tilde {
        if let Some(rel) =
            dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        {
            let rel = rel.to_string_lossy();
            return if rel.is_empty() {
                "~/".into()
            } else {
                format!("~/{rel}/")
            };
        }
    }
    format!("{}/", path.to_string_lossy().trim_end_matches('/'))
}

/// What a typed folder is, before Add: a git repository (its branch and worktrees) or not, and
/// the project that already has it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Folder {
    pub path: String,
    pub name: String,
    pub git: bool,
    pub branch: Option<String>,
    pub worktrees: usize,
    /// The name of the project that already has this folder.
    pub added: Option<String>,
}

/// The folder a typed value names, or None when it names no folder.
pub fn folder(input: &str, config: &GlobalConfig) -> Option<Folder> {
    let input = input.trim();
    if input.is_empty() || !(input.starts_with('/') || input.starts_with('~')) {
        return None;
    }
    let trimmed = input.trim_end_matches('/');
    let (path, tilde) = if trimmed.is_empty() {
        (PathBuf::from("/"), false)
    } else {
        expand_input(trimmed)
    };
    if !path.is_dir() {
        return None;
    }
    let normal = normalize_project_path(&path);
    let git = normal.join(".git").exists();
    let worktrees = if git {
        crate::git::worktree::list_worktrees(&normal)
            .map(|list| list.len())
            .unwrap_or(1)
    } else {
        0
    };
    Some(Folder {
        path: display_path(&normal, tilde),
        name: normal
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        git,
        branch: git
            .then(|| crate::git::info::current_branch(&normal))
            .flatten(),
        worktrees,
        added: added(config, &normal),
    })
}

/// The name of the project registered at `path`, if any.
pub fn added(config: &GlobalConfig, path: &Path) -> Option<String> {
    let normal = normalize_project_path(path);
    config
        .projects
        .iter()
        .find(|project| normalize_project_path(&project.path) == normal)
        .map(|project| project.name.clone())
}

/// A suggestion's path as a folder, for `added` and its kind.
pub fn expand(display: &str) -> PathBuf {
    expand_input(display.trim_end_matches('/')).0
}
