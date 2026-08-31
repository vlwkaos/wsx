// Canonical per-project configuration and legacy .gtrconfig migration.
// ref: README.md#project-configuration

use crate::model::workspace::ProjectConfig;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const PROJECT_CONFIG_FILE: &str = "wsx.config.yml";
const LEGACY_CONFIG_FILE: &str = ".gtrconfig";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProjectConfigFile {
    hooks: HookConfig,
    copy: CopyConfig,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct HookConfig {
    #[serde(rename = "postCreate", skip_serializing_if = "Option::is_none")]
    post_create: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct CopyConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
}

pub fn load_project_config(repo_path: &Path) -> ProjectConfig {
    let canonical = repo_path.join(PROJECT_CONFIG_FILE);
    if canonical.exists() {
        return load_yaml(&canonical);
    }

    let legacy = repo_path.join(LEGACY_CONFIG_FILE);
    if !legacy.exists() {
        return ProjectConfig::default();
    }
    if let Err(error) = read_bounded(&legacy) {
        return ProjectConfig {
            notice: Some(format!("Could not read {}: {error}", legacy.display())),
            ..ProjectConfig::default()
        };
    }

    let mut config = load_legacy(&legacy);
    config.notice = Some(match write_yaml(&canonical, &config) {
        Ok(()) => format!(
            "Created {PROJECT_CONFIG_FILE} from {LEGACY_CONFIG_FILE}; remove the legacy file when ready"
        ),
        Err(error) => format!(
            "Using {LEGACY_CONFIG_FILE}; could not create {PROJECT_CONFIG_FILE}: {error}"
        ),
    });
    config
}

fn load_yaml(path: &Path) -> ProjectConfig {
    let text = match read_bounded(path) {
        Ok(text) => text,
        Err(error) => {
            return ProjectConfig {
                notice: Some(format!("Could not read {}: {error}", path.display())),
                ..ProjectConfig::default()
            };
        }
    };
    match yaml_serde::from_str::<ProjectConfigFile>(&text) {
        Ok(file) => ProjectConfig {
            post_create: file.hooks.post_create,
            copy_includes: file.copy.include,
            copy_excludes: file.copy.exclude,
            notice: None,
        },
        Err(error) => ProjectConfig {
            notice: Some(format!(
                "Could not parse {} (using defaults): {error}",
                path.display()
            )),
            ..ProjectConfig::default()
        },
    }
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "file is {} bytes; maximum is {MAX_CONFIG_BYTES}",
            metadata.len()
        ));
    }
    fs::read_to_string(path).map_err(|error| error.to_string())
}

fn write_yaml(path: &Path, config: &ProjectConfig) -> Result<(), String> {
    let document = ProjectConfigFile {
        hooks: HookConfig {
            post_create: config.post_create.clone(),
        },
        copy: CopyConfig {
            include: config.copy_includes.clone(),
            exclude: config.copy_excludes.clone(),
        },
    };
    let text = yaml_serde::to_string(&document).map_err(|error| error.to_string())?;
    let tmp = temporary_path(path);
    let result = (|| {
        let mut file = fs::File::create(&tmp).map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&tmp, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(name)
}

fn load_legacy(config_path: &Path) -> ProjectConfig {
    let path = config_path.to_string_lossy();
    ProjectConfig {
        post_create: git_config_get(&path, "hooks.postCreate"),
        copy_includes: git_config_get_all(&path, "copy.include"),
        copy_excludes: git_config_get_all(&path, "copy.exclude"),
        notice: None,
    }
}

fn git_config_get(config_path: &str, key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "-f", config_path, "--get", key])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_config_get_all(config_path: &str, key: &str) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["config", "-f", config_path, "--get-all", key])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::current_dir()
                .unwrap()
                .join(".work/project-config-tests")
                .join(format!("{name}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_canonical_yaml() {
        let dir = TestDir::new("yaml");
        fs::write(
            dir.0.join(PROJECT_CONFIG_FILE),
            "hooks:\n  postCreate: cargo build\ncopy:\n  include: [.env]\n  exclude: [target]\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(config.post_create.as_deref(), Some("cargo build"));
        assert_eq!(config.copy_includes, [".env"]);
        assert_eq!(config.copy_excludes, ["target"]);
        assert!(config.notice.is_none());
    }

    #[test]
    fn migrates_legacy_config_without_deleting_it() {
        let dir = TestDir::new("legacy");
        let legacy = dir.0.join(LEGACY_CONFIG_FILE);
        fs::write(
            &legacy,
            "[hooks]\n\tpostCreate = cargo build\n[copy]\n\tinclude = .env\n\tinclude = .tool-versions\n\texclude = target\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert_eq!(config.post_create.as_deref(), Some("cargo build"));
        assert_eq!(config.copy_includes, [".env", ".tool-versions"]);
        assert_eq!(config.copy_excludes, ["target"]);
        assert!(legacy.exists());
        assert!(dir.0.join(PROJECT_CONFIG_FILE).exists());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Created")));

        let canonical = load_project_config(&dir.0);
        assert_eq!(canonical.post_create, config.post_create);
        assert_eq!(canonical.copy_includes, config.copy_includes);
        assert_eq!(canonical.copy_excludes, config.copy_excludes);
        assert!(canonical.notice.is_none());
    }

    #[test]
    fn oversized_legacy_config_is_not_parsed_or_migrated() {
        let dir = TestDir::new("oversized-legacy");
        fs::File::create(dir.0.join(LEGACY_CONFIG_FILE))
            .unwrap()
            .set_len(MAX_CONFIG_BYTES + 1)
            .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.post_create.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("maximum")));
        assert!(!dir.0.join(PROJECT_CONFIG_FILE).exists());
    }

    #[test]
    fn malformed_canonical_config_does_not_fall_back_to_legacy() {
        let dir = TestDir::new("invalid");
        fs::write(dir.0.join(PROJECT_CONFIG_FILE), "hooks: [invalid]\n").unwrap();
        fs::write(
            dir.0.join(LEGACY_CONFIG_FILE),
            "[hooks]\n\tpostCreate = should-not-run\n",
        )
        .unwrap();

        let config = load_project_config(&dir.0);

        assert!(config.post_create.is_none());
        assert!(config
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("Could not parse")));
    }
}
