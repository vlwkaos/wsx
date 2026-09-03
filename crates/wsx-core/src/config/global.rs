// ~/.config/wsx/config-v2.toml
// ref: toml crate — https://docs.rs/toml/
// ^ [[wsx Architecture]] Groups are the sole project organization and workspace selection contract.

use anyhow::{Context, Result};
use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MILLIS_PER_HOUR: u64 = 60 * 60 * 1_000;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const CONFIG_V2_FILE: &str = "config-v2.toml";
const LEGACY_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    Ungrouped,
    Named(String),
}

impl GroupKey {
    pub fn named(name: impl Into<String>) -> std::result::Result<Self, String> {
        let name = name.into();
        if is_reserved_group_name(&name) {
            Err(format!("reserved group name: {name}"))
        } else {
            Ok(Self::Named(name))
        }
    }
}

impl Serialize for GroupKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Ungrouped => serializer.serialize_str("ungrouped"),
            Self::Named(name) if is_reserved_group_name(name) => {
                Err(S::Error::custom(format!("reserved group name: {name}")))
            }
            Self::Named(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for GroupKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.eq_ignore_ascii_case("ungrouped") {
            Ok(Self::Ungrouped)
        } else if value.eq_ignore_ascii_case("default") {
            Err(D::Error::custom("default is a reserved group name"))
        } else {
            Ok(Self::Named(value))
        }
    }
}

pub fn is_reserved_group_name(name: &str) -> bool {
    ["ungrouped", "default"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

pub fn project_has_activity_within(
    last_agent_active_unix_ms: Option<u64>,
    last_terminal_active_unix_ms: Option<u64>,
    now_unix_ms: u64,
    window_ms: u64,
) -> bool {
    [last_agent_active_unix_ms, last_terminal_active_unix_ms]
        .into_iter()
        .flatten()
        .any(|active| now_unix_ms.saturating_sub(active) <= window_ms)
}

/// Matches a project against the one active workspace group. No selection means all projects.
pub fn project_matches_group(project_groups: &[String], active_group: Option<&GroupKey>) -> bool {
    active_group.is_none_or(|group| match group {
        GroupKey::Ungrouped => project_groups.is_empty(),
        GroupKey::Named(name) => project_groups.iter().any(|candidate| candidate == name),
    })
}

fn default_exclude_worktree_paths() -> Vec<String> {
    vec![".claude/worktrees".to_string()]
}

fn default_terminal_escape_chord() -> String {
    "ctrl+a w".to_string()
}

fn default_resume_agents_on_restore() -> bool {
    true
}

fn default_wake_mode() -> bool {
    true
}

fn default_auto_collapse_after_hours() -> u64 {
    24
}

fn default_notification_timeout_seconds() -> u64 {
    4
}

fn default_show_release_status() -> bool {
    true
}

// ^ [[Configuration Model]] Terminal presentation choices remain typed and default safely for older files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSidebar {
    #[default]
    Compact,
    Expanded,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortVisibility {
    Hidden,
    #[default]
    NonAgentic,
    All,
}

impl PortVisibility {
    pub fn shows_session(self, is_agentic: bool) -> bool {
        match self {
            Self::Hidden => false,
            Self::NonAgentic => !is_agentic,
            Self::All => true,
        }
    }
}

/// Canonical form used for project-path identity. A trailing `/` is the only
/// divergence we've seen between a user-typed path and its stored form, and an
/// un-normalized duplicate silently breaks dedup / delete / cache lookups.
/// Single source of truth — `load`, `add_project`, and `ops::register_project`
/// must all route through this so the stored path and the in-memory path match.
pub fn normalize_project_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_end_matches('/').to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GlobalConfig {
    pub groups: Vec<String>,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
    #[serde(default = "default_exclude_worktree_paths")]
    pub exclude_worktree_paths: Vec<String>,
    #[serde(default = "default_terminal_escape_chord")]
    pub terminal_escape_chord: String,
    #[serde(default = "default_resume_agents_on_restore")]
    pub resume_agents_on_restore: bool,
    #[serde(default = "default_wake_mode")]
    pub wake_mode: bool,
    #[serde(default = "default_auto_collapse_after_hours")]
    pub auto_collapse_after_hours: u64,
    #[serde(default = "default_notification_timeout_seconds")]
    pub notification_timeout_seconds: u64,
    #[serde(default = "default_show_release_status")]
    pub show_release_status: bool,
    #[serde(default)]
    pub terminal_sidebar: TerminalSidebar,
    #[serde(default)]
    pub port_visibility: PortVisibility,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            groups: vec![],
            projects: vec![],
            exclude_worktree_paths: default_exclude_worktree_paths(),
            terminal_escape_chord: default_terminal_escape_chord(),
            resume_agents_on_restore: default_resume_agents_on_restore(),
            wake_mode: default_wake_mode(),
            auto_collapse_after_hours: default_auto_collapse_after_hours(),
            notification_timeout_seconds: default_notification_timeout_seconds(),
            show_release_status: default_show_release_status(),
            terminal_sidebar: TerminalSidebar::default(),
            port_visibility: PortVisibility::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    pub groups: Vec<String>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringList {
    One(String),
    Many(Vec<String>),
}

impl StringList {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Deserialize)]
struct GlobalConfigWire {
    #[serde(default)]
    groups: Option<StringList>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    tabs: Option<StringList>,
    #[serde(default)]
    projects: Vec<ProjectEntryWire>,
    #[serde(default = "default_exclude_worktree_paths")]
    exclude_worktree_paths: Vec<String>,
    #[serde(default = "default_terminal_escape_chord")]
    terminal_escape_chord: String,
    #[serde(default = "default_resume_agents_on_restore")]
    resume_agents_on_restore: bool,
    #[serde(default = "default_wake_mode")]
    wake_mode: bool,
    #[serde(default = "default_auto_collapse_after_hours")]
    auto_collapse_after_hours: u64,
    #[serde(default = "default_notification_timeout_seconds")]
    notification_timeout_seconds: u64,
    #[serde(default = "default_show_release_status")]
    show_release_status: bool,
    #[serde(default)]
    terminal_sidebar: TerminalSidebar,
    #[serde(default)]
    port_visibility: PortVisibility,
}

#[derive(Deserialize)]
struct ProjectEntryWire {
    name: String,
    path: PathBuf,
    #[serde(default)]
    groups: Option<StringList>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    tab: Option<String>,
    #[serde(default)]
    aliases: HashMap<String, String>,
}

fn append_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

impl<'de> Deserialize<'de> for GlobalConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GlobalConfigWire::deserialize(deserializer)?;
        let mut groups = Vec::new();
        if let Some(canonical) = wire.groups {
            append_unique(&mut groups, canonical.into_vec());
        }
        if let Some(group) = wire.group {
            append_unique(&mut groups, [group]);
        }
        if let Some(tabs) = wire.tabs {
            append_unique(&mut groups, tabs.into_vec());
        }

        let mut projects = Vec::with_capacity(wire.projects.len());
        for project in wire.projects {
            let mut project_groups = Vec::new();
            if let Some(canonical) = project.groups {
                append_unique(&mut project_groups, canonical.into_vec());
            }
            if let Some(group) = project.group {
                append_unique(&mut project_groups, [group]);
            }
            if let Some(tab) = project.tab {
                append_unique(&mut project_groups, [tab]);
            }
            projects.push(ProjectEntry {
                name: project.name,
                path: normalize_project_path(&project.path),
                groups: project_groups,
                aliases: project.aliases,
            });
        }

        if wire.notification_timeout_seconds == 0 {
            return Err(D::Error::custom(
                "notification_timeout_seconds must be at least 1",
            ));
        }
        let mut config = Self {
            groups,
            projects,
            exclude_worktree_paths: wire.exclude_worktree_paths,
            terminal_escape_chord: wire.terminal_escape_chord,
            resume_agents_on_restore: wire.resume_agents_on_restore,
            wake_mode: wire.wake_mode,
            auto_collapse_after_hours: wire.auto_collapse_after_hours,
            notification_timeout_seconds: wire.notification_timeout_seconds,
            show_release_status: wire.show_release_status,
            terminal_sidebar: wire.terminal_sidebar,
            port_visibility: wire.port_visibility,
        };
        config.migrate_reserved_names();
        Ok(config)
    }
}

fn stored_data_needs_migration(text: &str) -> bool {
    fn contains_reserved(value: Option<&toml::Value>) -> bool {
        match value {
            Some(toml::Value::Array(values)) => values
                .iter()
                .any(|value| value.as_str().is_some_and(is_reserved_group_name)),
            Some(toml::Value::String(value)) => is_reserved_group_name(value),
            _ => false,
        }
    }

    let Ok(toml::Value::Table(root)) = text.parse::<toml::Value>() else {
        return false;
    };
    if root.contains_key("tabs")
        || root.contains_key("group")
        || root.get("groups").is_some_and(|value| !value.is_array())
        || contains_reserved(root.get("groups"))
    {
        return true;
    }
    root.get("projects")
        .and_then(toml::Value::as_array)
        .is_some_and(|projects| {
            projects.iter().any(|project| {
                project.as_table().is_some_and(|project| {
                    project.contains_key("tab")
                        || project.contains_key("group")
                        || project.get("groups").is_some_and(|value| !value.is_array())
                        || contains_reserved(project.get("groups"))
                })
            })
        })
}

impl GlobalConfig {
    fn migrate_reserved_names(&mut self) {
        let mut occupied: HashSet<String> = self
            .groups
            .iter()
            .chain(self.projects.iter().flat_map(|project| &project.groups))
            .filter(|name| !is_reserved_group_name(name))
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let mut replacements = HashMap::<String, String>::new();
        for name in self
            .groups
            .iter()
            .chain(self.projects.iter().flat_map(|project| &project.groups))
        {
            if !is_reserved_group_name(name) || replacements.contains_key(name) {
                continue;
            }
            let mut suffix = 2;
            let replacement = loop {
                let candidate = format!("{name}-{suffix}");
                if !occupied.contains(&candidate.to_ascii_lowercase()) {
                    occupied.insert(candidate.to_ascii_lowercase());
                    break candidate;
                }
                suffix += 1;
            };
            replacements.insert(name.clone(), replacement);
        }
        if !replacements.is_empty() {
            for name in &mut self.groups {
                if let Some(replacement) = replacements.get(name) {
                    *name = replacement.clone();
                }
            }
            for project in &mut self.projects {
                for name in &mut project.groups {
                    if let Some(replacement) = replacements.get(name) {
                        *name = replacement.clone();
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        self.groups.retain(|name| seen.insert(name.clone()));
        for project in &mut self.projects {
            let mut seen = HashSet::new();
            project.groups.retain(|name| seen.insert(name.clone()));
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|directory| directory.join("wsx").join(CONFIG_V2_FILE))
    }

    fn legacy_config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|directory| directory.join("wsx").join(LEGACY_CONFIG_FILE))
    }

    /// Returns `(config, warning)`. The v2 path isolates wsx 0.20 from older
    /// whole-file serializers; first load copies either legacy tabs or current
    /// group data without modifying the old path.
    pub fn load() -> Result<(Self, Option<String>)> {
        let canonical = Self::config_path().context("no config dir")?;
        let legacy = Self::legacy_config_path().context("no config dir")?;
        Self::load_from_paths(&canonical, &legacy)
    }

    fn load_from_paths(canonical: &Path, legacy: &Path) -> Result<(Self, Option<String>)> {
        if canonical.exists() {
            let text = std::fs::read_to_string(canonical)
                .with_context(|| format!("reading {}", canonical.display()))?;
            return match toml::from_str::<Self>(&text) {
                Err(error) => Ok((
                    Self::default(),
                    Some(format!("config parse error (using defaults): {error}")),
                )),
                Ok(config) => {
                    if stored_data_needs_migration(&text) {
                        config.save_to(canonical)?;
                    }
                    Ok((config, None))
                }
            };
        }
        if !legacy.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(legacy)
            .with_context(|| format!("reading {}", legacy.display()))?;
        match toml::from_str::<Self>(&text) {
            Err(error) => Ok((
                Self::default(),
                Some(format!("config parse error (using defaults): {error}")),
            )),
            Ok(config) => {
                let encoded = toml::to_string_pretty(&config)?;
                match atomic_create_private(canonical, encoded.as_bytes(), true) {
                    Ok(()) => Ok((config, None)),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        Self::load_from_paths(canonical, legacy)
                    }
                    Err(error) => {
                        Err(error).with_context(|| format!("writing {}", canonical.display()))
                    }
                }
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path().context("no config dir")?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        atomic_write_private(path, text.as_bytes(), true)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Ensures the global config has editable content without replacing an
    /// existing nonempty or nonregular path.
    pub fn prepare_for_edit(&self) -> Result<PathBuf> {
        let path = Self::config_path().context("no config dir")?;
        let text = toml::to_string_pretty(self)?;
        prepare_private_file_for_edit(&path, text.as_bytes())
            .with_context(|| format!("preparing {}", path.display()))?;
        Ok(path)
    }

    pub fn auto_collapse_window_ms(&self) -> Option<u64> {
        (self.auto_collapse_after_hours > 0).then(|| {
            self.auto_collapse_after_hours
                .saturating_mul(MILLIS_PER_HOUR)
        })
    }

    pub fn is_worktree_excluded(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.exclude_worktree_paths
            .iter()
            .any(|pat| path_str.contains(pat.as_str()))
    }

    pub fn ordered_group_keys(&self) -> Vec<GroupKey> {
        let mut keys = vec![GroupKey::Ungrouped];
        keys.extend(self.groups.iter().cloned().map(GroupKey::Named));
        keys
    }

    pub fn named_group_exists(&self, name: &str) -> bool {
        self.groups.iter().any(|group| group == name)
    }

    pub fn project_groups<'a>(&'a self, path: &Path) -> &'a [String] {
        self.projects
            .iter()
            .find(|entry| entry.path == path)
            .map_or(&[], |entry| entry.groups.as_slice())
    }

    pub fn add_project_to_group(&mut self, path: &Path, group: &str) -> bool {
        if !self.named_group_exists(group) {
            return false;
        }
        let Some(entry) = self.projects.iter_mut().find(|entry| entry.path == path) else {
            return false;
        };
        if !entry.groups.iter().any(|existing| existing == group) {
            entry.groups.push(group.to_owned());
        }
        true
    }

    pub fn remove_project_from_group(&mut self, path: &Path, group: &str) -> bool {
        let Some(entry) = self.projects.iter_mut().find(|entry| entry.path == path) else {
            return false;
        };
        let old_len = entry.groups.len();
        entry.groups.retain(|existing| existing != group);
        entry.groups.len() != old_len
    }

    pub fn add_project(&mut self, name: String, path: PathBuf) {
        let path = normalize_project_path(&path);
        self.projects.retain(|project| project.path != path);
        self.projects.push(ProjectEntry {
            name,
            path,
            groups: Vec::new(),
            aliases: Default::default(),
        });
    }

    pub fn remove_project(&mut self, path: &PathBuf) {
        self.projects.retain(|project| &project.path != path);
    }

    pub fn set_alias(&mut self, project_path: &PathBuf, branch: &str, alias: &str) {
        if let Some(entry) = self
            .projects
            .iter_mut()
            .find(|project| &project.path == project_path)
        {
            if alias.is_empty() {
                entry.aliases.remove(branch);
            } else {
                entry.aliases.insert(branch.to_string(), alias.to_string());
            }
        }
    }
}

fn prepare_private_file_for_edit(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "config path is not a regular file",
                ));
            }
            let existing = std::fs::read_to_string(path)?;
            if existing.trim().is_empty() {
                atomic_write_private(path, bytes, true)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_create_private(path, bytes, true)
        }
        Err(error) => Err(error),
    }
}

fn atomic_create_private(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = private_temporary_path(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        if sync {
            file.sync_all()?;
        }
        drop(file);
        std::fs::hard_link(&temporary, path)?;
        let _ = std::fs::remove_file(&temporary);
        if sync {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn private_temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = private_temporary_path(path);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        if sync {
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        if sync {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
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
                .join(".work/global-config-tests")
                .join(format!("{name}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn edit_preparation_initializes_missing_and_empty_private_files() {
        let dir = TestDir::new("edit-empty");
        let missing = dir.0.join("missing.toml");
        prepare_private_file_for_edit(&missing, b"value = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&missing).unwrap(), "value = 1\n");

        let empty = dir.0.join("empty.toml");
        std::fs::write(&empty, " \n").unwrap();
        prepare_private_file_for_edit(&empty, b"value = 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(empty).unwrap(), "value = 2\n");
    }

    #[test]
    fn edit_preparation_preserves_nonempty_private_file() {
        let dir = TestDir::new("edit-existing");
        let path = dir.0.join("config.toml");
        std::fs::write(&path, "malformed = [\n").unwrap();

        prepare_private_file_for_edit(&path, b"replacement = true\n").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "malformed = [\n");
    }

    #[test]
    fn edit_preparation_rejects_nonregular_private_path() {
        let dir = TestDir::new("edit-directory");
        let path = dir.0.join("config.toml");
        std::fs::create_dir(&path).unwrap();

        let error = prepare_private_file_for_edit(&path, b"value = 1\n").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn path_normalization_strips_only_trailing_slashes() {
        assert_eq!(
            normalize_project_path(Path::new("/foo//bar/")),
            PathBuf::from("/foo//bar")
        );
        assert_eq!(normalize_project_path(Path::new("///")), PathBuf::from(""));
    }

    #[test]
    fn first_v2_load_migrates_legacy_tabs_without_rewriting_legacy_file() {
        let dir = TestDir::new("v2-migration");
        let canonical = dir.0.join(CONFIG_V2_FILE);
        let legacy = dir.0.join(LEGACY_CONFIG_FILE);
        let legacy_text = "tabs = [\"personal\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ntab = \"personal\"\n";
        std::fs::write(&legacy, legacy_text).unwrap();

        let (config, warning) = GlobalConfig::load_from_paths(&canonical, &legacy).unwrap();

        assert!(warning.is_none());
        assert_eq!(config.groups, ["personal"]);
        assert_eq!(config.projects[0].groups, ["personal"]);
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), legacy_text);
        let canonical_text = std::fs::read_to_string(&canonical).unwrap();
        assert!(canonical_text.contains("groups = [\"personal\"]"));
        assert!(!canonical_text.contains("tabs"));
        assert!(!canonical_text.contains("tab ="));
    }

    #[test]
    fn first_v2_load_copies_existing_group_format_without_rewriting_source() {
        let dir = TestDir::new("v2-current-copy");
        let canonical = dir.0.join(CONFIG_V2_FILE);
        let legacy = dir.0.join(LEGACY_CONFIG_FILE);
        let source = "groups = [\"personal\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ngroups = [\"personal\"]\n";
        std::fs::write(&legacy, source).unwrap();

        let (config, warning) = GlobalConfig::load_from_paths(&canonical, &legacy).unwrap();

        assert!(warning.is_none());
        assert_eq!(config.groups, ["personal"]);
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), source);
        assert_eq!(
            toml::from_str::<GlobalConfig>(&std::fs::read_to_string(canonical).unwrap())
                .unwrap()
                .groups,
            ["personal"]
        );
    }

    #[test]
    fn existing_v2_config_wins_even_when_malformed() {
        let dir = TestDir::new("v2-wins");
        let canonical = dir.0.join(CONFIG_V2_FILE);
        let legacy = dir.0.join(LEGACY_CONFIG_FILE);
        std::fs::write(&canonical, "groups = [\n").unwrap();
        std::fs::write(&legacy, "tabs = [\"personal\"]\n").unwrap();

        let (config, warning) = GlobalConfig::load_from_paths(&canonical, &legacy).unwrap();

        assert!(config.groups.is_empty());
        assert!(warning.is_some_and(|warning| warning.contains("config parse error")));
        assert_eq!(std::fs::read_to_string(canonical).unwrap(), "groups = [\n");
    }

    #[test]
    fn canonical_serialization_has_only_group_fields() {
        let config: GlobalConfig = toml::from_str(
            "groups = [\"work\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ngroups = [\"work\"]\n",
        )
        .unwrap();
        let encoded = toml::to_string(&config).unwrap();
        assert!(encoded.contains("groups = [\"work\"]"));
        assert!(!encoded.contains("tab"));
        assert!(!encoded.contains("tag"));
        assert!(!encoded.contains("filter"));
    }

    #[test]
    fn notification_timeout_defaults_and_rejects_zero() {
        let defaulted: GlobalConfig = toml::from_str("").unwrap();
        assert_eq!(defaulted.notification_timeout_seconds, 4);

        let configured: GlobalConfig =
            toml::from_str("notification_timeout_seconds = 9\n").unwrap();
        assert_eq!(configured.notification_timeout_seconds, 9);

        let error =
            toml::from_str::<GlobalConfig>("notification_timeout_seconds = 0\n").unwrap_err();
        assert!(error
            .to_string()
            .contains("notification_timeout_seconds must be at least 1"));
    }

    #[test]
    fn presentation_settings_default_to_compact_sidebar_release_status_and_non_agentic_ports() {
        let defaulted: GlobalConfig = toml::from_str("").unwrap();
        assert!(defaulted.show_release_status);
        assert!(defaulted.wake_mode);
        assert_eq!(defaulted.terminal_sidebar, TerminalSidebar::Compact);
        assert_eq!(defaulted.port_visibility, PortVisibility::NonAgentic);
        assert!(!defaulted.port_visibility.shows_session(true));
        assert!(defaulted.port_visibility.shows_session(false));

        let configured: GlobalConfig = toml::from_str(
            "show_release_status = false\nwake_mode = false\nterminal_sidebar = \"expanded\"\nport_visibility = \"all\"\n",
        )
        .unwrap();
        assert!(!configured.show_release_status);
        assert!(!configured.wake_mode);
        assert_eq!(configured.terminal_sidebar, TerminalSidebar::Expanded);
        assert_eq!(configured.port_visibility, PortVisibility::All);
        assert!(configured.port_visibility.shows_session(true));

        assert!(toml::from_str::<GlobalConfig>("port_visibility = \"sometimes\"\n").is_err());
        assert!(toml::from_str::<GlobalConfig>("terminal_sidebar = \"sometimes\"\n").is_err());
    }

    #[test]
    fn legacy_tabs_and_project_tab_migrate() {
        let config: GlobalConfig = toml::from_str(
            "tabs = [\"work\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ntab = \"work\"\n",
        )
        .unwrap();
        assert_eq!(config.groups, ["work"]);
        assert_eq!(config.projects[0].groups, ["work"]);
        assert!(stored_data_needs_migration(
            "tabs = [\"work\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ntab = \"work\"\n"
        ));
    }

    #[test]
    fn mixed_canonical_and_legacy_values_merge_canonical_first() {
        let config: GlobalConfig = toml::from_str(
            "groups = [\"new\", \"shared\"]\ntabs = [\"old\", \"shared\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ngroups = [\"new\"]\ntab = \"old\"\n",
        )
        .unwrap();
        assert_eq!(config.groups, ["new", "shared", "old"]);
        assert_eq!(config.projects[0].groups, ["new", "old"]);
    }

    #[test]
    fn recent_is_a_normal_named_group_but_reserved_names_remain_rejected() {
        assert_eq!(
            GroupKey::named("recent").unwrap(),
            GroupKey::Named("recent".into())
        );
        assert_eq!(
            GroupKey::named("Recent").unwrap(),
            GroupKey::Named("Recent".into())
        );
        for name in ["ungrouped", "UNGROUPED", "default", "DeFaUlT"] {
            assert!(GroupKey::named(name).is_err(), "{name} must be reserved");
            assert!(serde_json::to_string(&GroupKey::Named(name.into())).is_err());
        }

        assert_eq!(
            serde_json::from_str::<GroupKey>(r#""UNGROUPED""#).unwrap(),
            GroupKey::Ungrouped
        );
        assert_eq!(
            serde_json::from_str::<GroupKey>(r#""recent""#).unwrap(),
            GroupKey::Named("recent".into())
        );
        assert!(serde_json::from_str::<GroupKey>(r#""default""#).is_err());
    }

    #[test]
    fn legacy_recent_group_stays_named_while_default_is_migrated() {
        let config: GlobalConfig = toml::from_str(
            "tabs = [\"recent\", \"Default\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ntab = \"recent\"\n",
        )
        .unwrap();

        assert!(config.groups.iter().any(|group| group == "recent"));
        assert!(!config
            .groups
            .iter()
            .any(|group| group.eq_ignore_ascii_case("default")));
        assert_eq!(config.projects[0].groups, ["recent"]);
    }

    #[test]
    fn ordered_group_keys_start_with_ungrouped_and_preserve_configured_order() {
        let config = GlobalConfig {
            groups: vec!["work".into(), "recent".into()],
            ..GlobalConfig::default()
        };

        assert_eq!(
            config.ordered_group_keys(),
            vec![
                GroupKey::Ungrouped,
                GroupKey::Named("work".into()),
                GroupKey::Named("recent".into()),
            ]
        );
    }

    #[test]
    fn group_matching_uses_only_memberships_and_exact_names() {
        assert!(project_matches_group(&[], None));
        assert!(project_matches_group(&["work".into()], None));
        assert!(project_matches_group(&[], Some(&GroupKey::Ungrouped)));
        assert!(!project_matches_group(
            &["work".into()],
            Some(&GroupKey::Ungrouped)
        ));
        assert!(!project_matches_group(
            &[],
            Some(&GroupKey::Named("recent".into()))
        ));
        assert!(project_matches_group(
            &["other".into(), "recent".into()],
            Some(&GroupKey::Named("recent".into()))
        ));
        assert!(!project_matches_group(
            &["Recent".into()],
            Some(&GroupKey::Named("recent".into()))
        ));
        assert!(!project_matches_group(
            &["other".into()],
            Some(&GroupKey::Named("work".into()))
        ));
    }
}
