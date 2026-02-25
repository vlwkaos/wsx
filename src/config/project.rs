// .gtrconfig — per-project config (gitconfig INI format, gtr-compatible)
// Reads via `git config -f .gtrconfig` to support multi-value keys.

use crate::model::workspace::ProjectConfig;
use std::path::Path;
use std::process::Command;

pub fn load_project_config(repo_path: &Path) -> ProjectConfig {
    let config_path = repo_path.join(".gtrconfig");
    if !config_path.exists() {
        return ProjectConfig::default();
    }

    let path_str = config_path.to_string_lossy();
    let mut pc = ProjectConfig::default();

    pc.post_create = git_config_get(&path_str, "hooks.postCreate");
    pc.copy_includes = git_config_get_all(&path_str, "copy.include");
    pc.copy_excludes = git_config_get_all(&path_str, "copy.exclude");

    pc
}

fn git_config_get(config_path: &str, key: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["config", "-f", config_path, "--get", key])
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn git_config_get_all(config_path: &str, key: &str) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["config", "-f", config_path, "--get-all", key])
        .output()
    else { return vec![] };
    if !output.status.success() { return vec![]; }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
