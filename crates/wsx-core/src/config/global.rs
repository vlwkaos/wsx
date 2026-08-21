// ~/.config/wsx/config.toml
// ref: toml crate — https://docs.rs/toml/

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

fn default_exclude_worktree_paths() -> Vec<String> {
    vec![".claude/worktrees".to_string()]
}

/// Canonical form used for project-path identity. A trailing `/` is the only
/// divergence we've seen between a user-typed path and its stored form, and an
/// un-normalized duplicate silently breaks dedup / delete / cache lookups.
/// Single source of truth — `load`, `add_project`, and `ops::register_project`
/// must all route through this so the stored path and the in-memory path match.
pub fn normalize_project_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_end_matches('/').to_string())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlobalConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<String>,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
    /// Worktree paths containing any of these substrings are hidden.
    /// Defaults to [".claude/worktrees"] to exclude Claude agent worktrees.
    #[serde(default = "default_exclude_worktree_paths")]
    pub exclude_worktree_paths: Vec<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            tabs: vec![],
            projects: vec![],
            exclude_worktree_paths: default_exclude_worktree_paths(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    /// Which tab this project belongs to. None = default tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// branch -> alias mapping (stored at app level, independent of git)
    #[serde(default)]
    pub aliases: std::collections::HashMap<String, String>,
}

impl GlobalConfig {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("wsx").join("config.toml"))
    }

    /// Returns `(config, warning)`. On TOML parse error, falls back to defaults
    /// and sets `warning` so the caller can surface it in the TUI status bar.
    pub fn load() -> Result<(Self, Option<String>)> {
        let path = Self::config_path().context("no config dir")?;
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        match toml::from_str::<Self>(&text) {
            Err(e) => {
                let warn = format!("config parse error (using defaults): {e}");
                Ok((Self::default(), Some(warn)))
            }
            Ok(mut config) => {
                for entry in &mut config.projects {
                    entry.path = normalize_project_path(&entry.path);
                }
                Ok((config, None))
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path().context("no config dir")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", tmp.display()))?;
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| format!("renaming {}", path.display()))?;
        Ok(())
    }

    pub fn is_worktree_excluded(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.exclude_worktree_paths
            .iter()
            .any(|pat| path_str.contains(pat.as_str()))
    }

    /// Returns tab order as [None, Some("work"), …]. None = default tab (always first).
    pub fn ordered_tabs(&self) -> Vec<Option<&str>> {
        let mut out: Vec<Option<&str>> = vec![None];
        for t in &self.tabs {
            out.push(Some(t.as_str()));
        }
        out
    }

    pub fn tab_exists(&self, name: &str) -> bool {
        self.tabs.iter().any(|t| t == name)
    }

    /// Returns the tab name for the project at `path`, or `None` if unassigned.
    pub fn project_tab<'a>(&'a self, path: &Path) -> Option<&'a str> {
        self.projects
            .iter()
            .find(|e| e.path == path)
            .and_then(|e| e.tab.as_deref())
    }

    /// Set or clear the tab assignment for the project at `path`.
    pub fn move_project_tab(&mut self, path: &PathBuf, tab: Option<String>) {
        if let Some(entry) = self.projects.iter_mut().find(|p| &p.path == path) {
            entry.tab = tab;
        }
    }

    pub fn add_project(&mut self, name: String, path: PathBuf) {
        let path = normalize_project_path(&path);
        self.projects.retain(|p| p.path != path);
        self.projects.push(ProjectEntry {
            name,
            path,
            tab: None,
            aliases: Default::default(),
        });
    }

    pub fn remove_project(&mut self, path: &PathBuf) {
        self.projects.retain(|p| &p.path != path);
    }

    pub fn set_alias(&mut self, project_path: &PathBuf, branch: &str, alias: &str) {
        if let Some(entry) = self.projects.iter_mut().find(|p| &p.path == project_path) {
            if alias.is_empty() {
                entry.aliases.remove(branch);
            } else {
                entry.aliases.insert(branch.to_string(), alias.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_project_path ---

    #[test]
    fn given_path_with_single_trailing_slash_when_normalized_then_slash_stripped() {
        assert_eq!(
            normalize_project_path(Path::new("/foo/")),
            PathBuf::from("/foo")
        );
    }

    #[test]
    fn given_path_with_multiple_trailing_slashes_when_normalized_then_all_stripped() {
        assert_eq!(
            normalize_project_path(Path::new("/foo///")),
            PathBuf::from("/foo")
        );
    }

    #[test]
    fn given_relative_path_with_trailing_slash_when_normalized_then_slash_stripped() {
        assert_eq!(
            normalize_project_path(Path::new("foo/bar/")),
            PathBuf::from("foo/bar")
        );
    }

    // Semantically-wrong-input guard: only the trailing run is stripped.
    // A naive "collapse all slashes" impl would wrongly yield "/foo/bar".
    #[test]
    fn given_path_with_interior_and_trailing_slashes_when_normalized_then_only_trailing_stripped() {
        assert_eq!(
            normalize_project_path(Path::new("/foo//bar/")),
            PathBuf::from("/foo//bar")
        );
    }

    #[test]
    fn given_empty_path_when_normalized_then_does_not_panic() {
        let _ = normalize_project_path(Path::new(""));
    }

    // All-slashes collapses to empty (not just "no panic").
    #[test]
    fn given_all_slashes_path_when_normalized_then_empty() {
        assert_eq!(normalize_project_path(Path::new("///")), PathBuf::from(""));
    }

    #[test]
    fn given_root_slash_path_when_normalized_then_empty() {
        assert_eq!(normalize_project_path(Path::new("/")), PathBuf::from(""));
    }

    // --- GlobalConfig::default (precondition for add_project suite) ---

    #[test]
    fn given_default_config_when_constructed_then_projects_is_empty() {
        let config = GlobalConfig::default();
        assert!(config.projects.is_empty());
    }

    // --- GlobalConfig::add_project ---

    #[test]
    fn given_path_with_trailing_slash_when_add_project_then_stored_path_normalized() {
        let mut config = GlobalConfig::default();
        config.add_project("a".to_string(), PathBuf::from("/foo/"));
        assert_eq!(config.projects[0].path, PathBuf::from("/foo"));
    }

    #[test]
    fn given_duplicate_normalized_path_when_add_project_twice_then_len_is_one() {
        let mut config = GlobalConfig::default();
        config.add_project("a".to_string(), PathBuf::from("/foo"));
        config.add_project("b".to_string(), PathBuf::from("/foo/"));
        assert_eq!(config.projects.len(), 1);
    }

    #[test]
    fn given_duplicate_normalized_path_when_add_project_twice_then_last_name_wins() {
        let mut config = GlobalConfig::default();
        config.add_project("a".to_string(), PathBuf::from("/foo"));
        config.add_project("b".to_string(), PathBuf::from("/foo/"));
        assert_eq!(config.projects[0].name, "b");
    }

    // Identity is keyed on the normalized path: the survivor's stored path is
    // the normalized form regardless of which call carried the trailing slash.
    #[test]
    fn given_duplicate_normalized_path_when_add_project_twice_then_survivor_path_normalized() {
        let mut config = GlobalConfig::default();
        config.add_project("a".to_string(), PathBuf::from("/foo/"));
        config.add_project("b".to_string(), PathBuf::from("/foo"));
        assert_eq!(config.projects[0].path, PathBuf::from("/foo"));
    }

    #[test]
    fn given_two_distinct_paths_when_add_project_each_then_both_persist() {
        let mut config = GlobalConfig::default();
        config.add_project("a".to_string(), PathBuf::from("/foo"));
        config.add_project("b".to_string(), PathBuf::from("/bar"));
        assert_eq!(config.projects.len(), 2);
    }
}
