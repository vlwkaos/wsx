// Background discovery of git repos under $HOME. Streams `~/foo/bar/` paths
// over a channel; caller owns the cache so results survive across modal opens.

use std::path::Path;
use std::sync::mpsc;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "Library",
    "Applications",
    ".Trash",
];

/// Depth from `$HOME` before we stop descending. Walks short-circuit at
/// any `.git` directory, so this only matters for trees with no repos in
/// them; raised from 6 to 8 to catch e.g. `~/work/<org>/<year>/<project>`.
const MAX_SCAN_DEPTH: usize = 8;

pub fn scan_git_repos(tx: mpsc::Sender<String>) {
    let Some(home) = dirs::home_dir() else { return };
    walk_for_git(&home, 0, &tx);
}

/// Returns false when the receiver dropped — caller should stop walking.
fn walk_for_git(dir: &Path, depth: usize, tx: &mpsc::Sender<String>) -> bool {
    if depth > MAX_SCAN_DEPTH {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };

    let mut has_git = false;
    let mut subdirs: Vec<std::path::PathBuf> = vec![];

    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let path = entry.path();

        if name_str == ".git" && path.is_dir() {
            has_git = true;
            continue;
        }
        if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }

    if has_git {
        let display = format_repo_path(dir);
        if tx.send(display).is_err() {
            return false;
        }
        // Found a repo — don't descend further, nested repos are skipped.
        return true;
    }

    for subdir in subdirs {
        if !walk_for_git(&subdir, depth + 1, tx) {
            return false;
        }
    }
    true
}

/// Format a repo path for display + auto-completion. Prefer `~/...` for
/// paths under home so the completion list reads short and matches what
/// the user is likely to type. Always trailing-slash for consistency
/// with `path_completions`.
fn format_repo_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            let rel_str = rel.to_string_lossy();
            return if rel_str.is_empty() {
                "~/".to_string()
            } else {
                format!("~/{}/", rel_str)
            };
        }
    }
    format!("{}/", path.to_string_lossy())
}
