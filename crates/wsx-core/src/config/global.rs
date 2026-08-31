// ~/.config/wsx/config.toml
// ref: toml crate — https://docs.rs/toml/
// ^ [[wsx Architecture]] Groups are the sole project organization and workspace selection contract.

use anyhow::{Context, Result};
use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const RECENT_GROUP_WINDOW_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    Recent,
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
            Self::Recent => serializer.serialize_str("recent"),
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
        if value.eq_ignore_ascii_case("recent") {
            Ok(Self::Recent)
        } else if value.eq_ignore_ascii_case("ungrouped") {
            Ok(Self::Ungrouped)
        } else if value.eq_ignore_ascii_case("default") {
            Err(D::Error::custom("default is a reserved group name"))
        } else {
            Ok(Self::Named(value))
        }
    }
}

pub fn is_reserved_group_name(name: &str) -> bool {
    ["recent", "ungrouped", "default"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

pub fn project_is_recent(
    last_agent_active_unix_ms: Option<u64>,
    last_terminal_active_unix_ms: Option<u64>,
    now_unix_ms: u64,
) -> bool {
    [last_agent_active_unix_ms, last_terminal_active_unix_ms]
        .into_iter()
        .flatten()
        .any(|active| now_unix_ms.saturating_sub(active) <= RECENT_GROUP_WINDOW_MS)
}

/// Matches a project against the one active workspace group. No selection means all projects.
pub fn project_matches_group(
    project_groups: &[String],
    last_agent_active_unix_ms: Option<u64>,
    last_terminal_active_unix_ms: Option<u64>,
    active_group: Option<&GroupKey>,
    now_unix_ms: u64,
) -> bool {
    active_group.is_none_or(|group| match group {
        GroupKey::Recent => project_is_recent(
            last_agent_active_unix_ms,
            last_terminal_active_unix_ms,
            now_unix_ms,
        ),
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

/// Canonical form used for project-path identity. A trailing `/` is the only
/// divergence we've seen between a user-typed path and its stored form, and an
/// un-normalized duplicate silently breaks dedup / delete / cache lookups.
/// Single source of truth — `load`, `add_project`, and `ops::register_project`
/// must all route through this so the stored path and the in-memory path match.
pub fn normalize_project_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_end_matches('/').to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct GlobalConfig {
    pub groups: Vec<String>,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
    #[serde(default = "default_exclude_worktree_paths")]
    pub exclude_worktree_paths: Vec<String>,
    #[serde(default = "default_terminal_escape_chord")]
    pub terminal_escape_chord: String,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            groups: vec![],
            projects: vec![],
            exclude_worktree_paths: default_exclude_worktree_paths(),
            terminal_escape_chord: default_terminal_escape_chord(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
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

        let mut config = Self {
            groups,
            projects,
            exclude_worktree_paths: wire.exclude_worktree_paths,
            terminal_escape_chord: wire.terminal_escape_chord,
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
        dirs::config_dir().map(|d| d.join("wsx").join("config.toml"))
    }

    /// Returns `(config, warning)`. Parse errors retain the historical default
    /// fallback; migration write failures are returned to avoid hiding data loss.
    pub fn load() -> Result<(Self, Option<String>)> {
        let path = Self::config_path().context("no config dir")?;
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        match toml::from_str::<Self>(&text) {
            Err(e) => Ok((
                Self::default(),
                Some(format!("config parse error (using defaults): {e}")),
            )),
            Ok(config) => {
                if stored_data_needs_migration(&text) {
                    config.save()?;
                }
                Ok((config, None))
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path().context("no config dir")?;
        let text = toml::to_string_pretty(self)?;
        atomic_write_private(&path, text.as_bytes(), true)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub fn is_worktree_excluded(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.exclude_worktree_paths
            .iter()
            .any(|pat| path_str.contains(pat.as_str()))
    }

    pub fn ordered_group_keys(&self) -> Vec<GroupKey> {
        let mut keys = vec![GroupKey::Recent, GroupKey::Ungrouped];
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

pub(crate) fn atomic_write_private(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
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

    #[test]
    fn path_normalization_strips_only_trailing_slashes() {
        assert_eq!(
            normalize_project_path(Path::new("/foo//bar/")),
            PathBuf::from("/foo//bar")
        );
        assert_eq!(normalize_project_path(Path::new("///")), PathBuf::from(""));
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
    fn reserved_collisions_preserve_assignments_and_are_idempotent() {
        let config: GlobalConfig = toml::from_str(
            "tabs = [\"recent\", \"recent-2\", \"Default\"]\n[[projects]]\nname = \"p\"\npath = \"/p\"\ntab = \"recent\"\n",
        )
        .unwrap();
        assert_eq!(config.groups, ["recent-3", "recent-2", "Default-2"]);
        assert_eq!(config.projects[0].groups, ["recent-3"]);
        let encoded = toml::to_string(&config).unwrap();
        let again: GlobalConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(again.groups, config.groups);
        assert!(!stored_data_needs_migration(&encoded));
    }

    #[test]
    fn group_matching_is_unfiltered_ungrouped_or_named() {
        let now = RECENT_GROUP_WINDOW_MS * 2;
        assert!(project_matches_group(&[], None, None, None, now));
        assert!(project_matches_group(
            &[],
            None,
            None,
            Some(&GroupKey::Ungrouped),
            now
        ));
        assert!(project_matches_group(
            &["work".into()],
            None,
            None,
            Some(&GroupKey::Named("work".into())),
            now
        ));
        assert!(!project_matches_group(
            &["other".into()],
            None,
            None,
            Some(&GroupKey::Named("work".into())),
            now
        ));
    }

    #[test]
    fn recent_accepts_either_source_at_the_boundary_and_during_clock_rollback() {
        let now = RECENT_GROUP_WINDOW_MS * 2;
        for (agent, terminal) in [
            (Some(now - RECENT_GROUP_WINDOW_MS), None),
            (None, Some(now - RECENT_GROUP_WINDOW_MS)),
            (Some(now + 1), None),
        ] {
            assert!(project_matches_group(
                &[],
                agent,
                terminal,
                Some(&GroupKey::Recent),
                now
            ));
        }
        assert!(!project_matches_group(
            &[],
            Some(now - RECENT_GROUP_WINDOW_MS - 1),
            None,
            Some(&GroupKey::Recent),
            now
        ));
    }
}
