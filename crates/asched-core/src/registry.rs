//! Generic project registry.
//! ref: README.md#architecture

use crate::routine::store::{atomic_toml, read_text_limited};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

const REGISTRY_VERSION: u32 = 1;
static REGISTRY_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub working_dir: PathBuf,
}

impl Project {
    pub fn validated(mut self) -> Result<Self, RegistryError> {
        self.name = self.name.trim().to_string();
        validate_name(&self.name)?;
        self.working_dir = fs::canonicalize(&self.working_dir).map_err(|error| {
            RegistryError::Validation(format!(
                "cannot resolve working directory {}: {error}",
                self.working_dir.display()
            ))
        })?;
        if !self.working_dir.is_dir() {
            return Err(RegistryError::Validation(format!(
                "{} is not a directory",
                self.working_dir.display()
            )));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
// ^ [[Project Registry and Scheduling Allowlist]]
pub struct ProjectRegistry {
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            revision: 0,
            projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RegistryStore {
    root: PathBuf,
}

impl RegistryStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn default_root() -> Result<PathBuf, RegistryError> {
        if let Some(path) = std::env::var_os("ASCHED_ROOT") {
            return Ok(PathBuf::from(path));
        }
        dirs::config_dir()
            .map(|path| path.join("asched"))
            .ok_or_else(|| RegistryError::Unavailable("no config directory".into()))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("projects.toml")
    }

    pub fn load(&self) -> Result<ProjectRegistry, RegistryError> {
        let path = self.path();
        if !path.exists() {
            return Ok(ProjectRegistry::default());
        }
        let text = read_text_limited(&path)?;
        let registry: ProjectRegistry = toml::from_str(&text)
            .map_err(|error| RegistryError::Corrupt(format!("{}: {error}", path.display())))?;
        validate_registry(registry)
    }

    pub fn add(
        &self,
        expected_revision: u64,
        project: Project,
    ) -> Result<ProjectRegistry, RegistryError> {
        self.modify(expected_revision, |registry| {
            let project = project.validated()?;
            if registry
                .projects
                .iter()
                .any(|item| item.name == project.name)
            {
                return Err(RegistryError::Duplicate(project.name));
            }
            if let Some(existing) = registry
                .projects
                .iter()
                .find(|item| item.working_dir == project.working_dir)
            {
                return Err(RegistryError::Validation(format!(
                    "{} is already registered as project '{}'",
                    project.working_dir.display(),
                    existing.name
                )));
            }
            registry.projects.push(project);
            registry
                .projects
                .sort_by(|left, right| left.name.cmp(&right.name));
            Ok(())
        })
    }

    pub fn remove(
        &self,
        expected_revision: u64,
        name: &str,
    ) -> Result<ProjectRegistry, RegistryError> {
        self.modify(expected_revision, |registry| {
            let before = registry.projects.len();
            registry.projects.retain(|project| project.name != name);
            if registry.projects.len() == before {
                return Err(RegistryError::NotFound(name.to_string()));
            }
            Ok(())
        })
    }

    pub fn merge(
        &self,
        expected_revision: u64,
        projects: Vec<Project>,
    ) -> Result<ProjectRegistry, RegistryError> {
        self.modify(expected_revision, |registry| merge_into(registry, projects))
    }

    pub fn select(
        &self,
        names: &[String],
        filter: Option<&str>,
    ) -> Result<Vec<Project>, RegistryError> {
        let registry = self.load()?;
        let requested = names.iter().collect::<HashSet<_>>();
        let missing = names
            .iter()
            .filter(|name| {
                !registry
                    .projects
                    .iter()
                    .any(|project| &project.name == *name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(RegistryError::NotFound(missing.join(", ")));
        }
        let filter = filter
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        Ok(registry
            .projects
            .into_iter()
            .filter(|project| requested.is_empty() || requested.contains(&project.name))
            .filter(|project| {
                filter.as_deref().is_none_or(|value| {
                    project.name.to_lowercase().contains(value)
                        || project
                            .working_dir
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(value)
                })
            })
            .collect())
    }

    fn modify(
        &self,
        expected_revision: u64,
        update: impl FnOnce(&mut ProjectRegistry) -> Result<(), RegistryError>,
    ) -> Result<ProjectRegistry, RegistryError> {
        // ^ Registry writes bypass the daemon; keep file locking, revision checks, and atomic_toml together.
        let _guard = self.exclusive_lock()?;
        self.modify_locked(expected_revision, update)
    }

    pub(crate) fn merge_locked(
        &self,
        expected_revision: u64,
        projects: Vec<Project>,
    ) -> Result<ProjectRegistry, RegistryError> {
        self.modify_locked(expected_revision, |registry| merge_into(registry, projects))
    }

    pub(crate) fn exclusive_lock(&self) -> Result<RegistryWriteGuard, RegistryError> {
        let lock = REGISTRY_WRITE_LOCK.get_or_init(|| Mutex::new(()));
        let process = lock
            .lock()
            .map_err(|_| RegistryError::Io("project registry lock poisoned".into()))?;
        let file = RegistryFileLock::acquire(&self.root)?;
        Ok(RegistryWriteGuard {
            _process: process,
            _file: file,
        })
    }

    fn modify_locked(
        &self,
        expected_revision: u64,
        update: impl FnOnce(&mut ProjectRegistry) -> Result<(), RegistryError>,
    ) -> Result<ProjectRegistry, RegistryError> {
        let mut registry = self.load()?;
        if registry.revision != expected_revision {
            return Err(RegistryError::Conflict {
                expected: expected_revision,
                actual: registry.revision,
            });
        }
        update(&mut registry)?;
        registry.revision = registry
            .revision
            .checked_add(1)
            .ok_or_else(|| RegistryError::Corrupt("registry revision overflow".into()))?;
        atomic_toml(&self.path(), &registry)
            .map_err(|error| RegistryError::Io(error.to_string()))?;
        Ok(registry)
    }
}

fn merge_into(registry: &mut ProjectRegistry, projects: Vec<Project>) -> Result<(), RegistryError> {
    for project in projects {
        let project = project.validated()?;
        if let Some(existing) = registry
            .projects
            .iter()
            .find(|item| item.name == project.name || item.working_dir == project.working_dir)
        {
            if existing != &project {
                return Err(RegistryError::Validation(format!(
                    "project '{}' conflicts with existing project '{}' ({})",
                    project.name,
                    existing.name,
                    existing.working_dir.display()
                )));
            }
            continue;
        }
        registry.projects.push(project);
    }
    registry
        .projects
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(())
}

fn validate_registry(registry: ProjectRegistry) -> Result<ProjectRegistry, RegistryError> {
    if registry.version != REGISTRY_VERSION {
        return Err(RegistryError::Corrupt(format!(
            "unsupported project registry schema {}",
            registry.version
        )));
    }
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for project in &registry.projects {
        validate_name(&project.name).map_err(|error| RegistryError::Corrupt(error.to_string()))?;
        if project.name.trim() != project.name {
            return Err(RegistryError::Corrupt(
                "stored project name must be normalized".into(),
            ));
        }
        if !project.working_dir.is_absolute() {
            return Err(RegistryError::Corrupt(format!(
                "stored working directory must be absolute: {}",
                project.working_dir.display()
            )));
        }
        if project.working_dir.exists() {
            if !project.working_dir.is_dir() {
                return Err(RegistryError::Corrupt(format!(
                    "stored working directory is not a directory: {}",
                    project.working_dir.display()
                )));
            }
            let canonical = fs::canonicalize(&project.working_dir)?;
            if canonical != project.working_dir {
                return Err(RegistryError::Corrupt(format!(
                    "stored working directory must be canonical: {}",
                    project.working_dir.display()
                )));
            }
        }
        if !names.insert(&project.name) {
            return Err(RegistryError::Duplicate(project.name.clone()));
        }
        if !paths.insert(&project.working_dir) {
            return Err(RegistryError::Corrupt(format!(
                "working directory registered more than once: {}",
                project.working_dir.display()
            )));
        }
    }
    Ok(registry)
}

fn validate_name(name: &str) -> Result<(), RegistryError> {
    if name.is_empty() {
        return Err(RegistryError::Validation(
            "project name must not be empty".into(),
        ));
    }
    if name.contains('/') || name.chars().any(char::is_control) {
        return Err(RegistryError::Validation(
            "project name must not contain '/' or control characters".into(),
        ));
    }
    Ok(())
}

struct RegistryFileLock(File);

pub(crate) struct RegistryWriteGuard {
    _process: MutexGuard<'static, ()>,
    _file: RegistryFileLock,
}

impl RegistryFileLock {
    fn acquire(root: &Path) -> Result<Self, RegistryError> {
        fs::create_dir_all(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let path = root.join("projects.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| RegistryError::Io(format!("opening {}: {error}", path.display())))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(RegistryError::Io(format!(
                "locking {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self(file))
    }
}

impl Drop for RegistryFileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid project: {0}")]
    Validation(String),
    #[error("project '{0}' already exists")]
    Duplicate(String),
    #[error("project '{0}' not found")]
    NotFound(String),
    #[error("stale registry revision: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("project registry unavailable: {0}")]
    Unavailable(String),
    #[error("project registry I/O error: {0}")]
    Io(String),
    #[error("invalid project registry: {0}")]
    Corrupt(String),
}

impl From<std::io::Error> for RegistryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

#[cfg(test)]
#[path = "registry_contract_tests.rs"]
mod registry_contract_tests;
