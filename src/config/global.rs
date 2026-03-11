// ~/.config/wsx/config.toml
// ref: toml crate — https://docs.rs/toml/

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_exclude_worktree_paths() -> Vec<String> {
    vec![".claude/worktrees".to_string()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlobalConfig {
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
            projects: vec![],
            exclude_worktree_paths: default_exclude_worktree_paths(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
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
                    let s = entry
                        .path
                        .to_string_lossy()
                        .trim_end_matches('/')
                        .to_string();
                    entry.path = PathBuf::from(s);
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
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn is_worktree_excluded(&self, path: &PathBuf) -> bool {
        let path_str = path.to_string_lossy();
        self.exclude_worktree_paths
            .iter()
            .any(|pat| path_str.contains(pat.as_str()))
    }

    pub fn add_project(&mut self, name: String, path: PathBuf) {
        let s = path.to_string_lossy().trim_end_matches('/').to_string();
        let path = PathBuf::from(s);
        self.projects.retain(|p| p.path != path);
        self.projects.push(ProjectEntry {
            name,
            path,
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
