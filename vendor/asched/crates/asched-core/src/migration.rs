//! Explicit import paths from schedulers that predate asched.
//! ref: README.md#migrating-from-wsx

use crate::routine::store::{
    atomic_create, atomic_toml, project_key, read_text_limited, ProjectRoutines, RoutineStore,
};
use crate::{Project, ProjectRegistry, RegistryError, RegistryStore};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const WSX_IMPORT_TRANSACTION_VERSION: u32 = 1;

pub fn default_wsx_paths() -> Result<(PathBuf, PathBuf), RegistryError> {
    let root = dirs::config_dir()
        .map(|path| path.join("wsx"))
        .ok_or_else(|| RegistryError::Unavailable("no config directory".into()))?;
    Ok((root.join("routines"), root.join("config.toml")))
}

#[derive(Debug, Clone, Serialize)]
pub struct WsxImportPlan {
    pub source_root: PathBuf,
    pub source_config: PathBuf,
    pub projects: Vec<WsxImportProject>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsxImportProject {
    pub project: Project,
    pub routine_count: usize,
    pub has_routine_file: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WsxImportResult {
    pub registry: ProjectRegistry,
    pub projects_registered: usize,
    pub routine_files_imported: usize,
    pub routines_imported: usize,
}

#[derive(Debug, Deserialize)]
struct WsxConfig {
    #[serde(default)]
    projects: Vec<WsxProject>,
}

#[derive(Debug, Deserialize)]
struct WsxProject {
    name: String,
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct WsxImportTransaction {
    version: u32,
    registry_revision: u64,
    projects_before: usize,
    routines_imported: usize,
    projects: Vec<Project>,
    files: Vec<WsxImportFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WsxImportFile {
    target: PathBuf,
    contents: String,
}

pub fn plan_wsx_import(
    source_root: &Path,
    source_config: &Path,
) -> Result<WsxImportPlan, RegistryError> {
    let text = read_text_limited(source_config).map_err(|error| {
        RegistryError::Io(format!("reading {}: {error}", source_config.display()))
    })?;
    let config: WsxConfig = toml::from_str(&text)
        .map_err(|error| RegistryError::Corrupt(format!("{}: {error}", source_config.display())))?;
    let mut projects = Vec::with_capacity(config.projects.len());
    for source in config.projects {
        let project = Project {
            name: source.name,
            working_dir: source.path,
        }
        .validated()?;
        let routine_file = source_root
            .join("projects")
            .join(format!("{}.toml", project_key(&project.working_dir)));
        let has_routine_file = routine_file.exists();
        let routine_count = if has_routine_file {
            load_source_routines(&routine_file, &project.working_dir)?
                .routines
                .len()
        } else {
            0
        };
        projects.push(WsxImportProject {
            project,
            routine_count,
            has_routine_file,
        });
    }
    projects.sort_by(|left, right| left.project.name.cmp(&right.project.name));
    Ok(WsxImportPlan {
        source_root: source_root.to_path_buf(),
        source_config: source_config.to_path_buf(),
        projects,
    })
}

// ^ [[Crash-Safe wsx Import]]
pub fn apply_wsx_import(
    plan: &WsxImportPlan,
    destination: &RegistryStore,
    keep_enabled: bool,
) -> Result<WsxImportResult, RegistryError> {
    let _guard = destination.exclusive_lock()?;
    let _daemon_guard = DaemonOfflineGuard::acquire(destination.root())?;
    if let Some(result) = recover_wsx_import(destination)? {
        return Ok(result);
    }
    let before = destination.load()?;
    validate_destination(&before, plan, destination.root())?;

    let mut files = Vec::new();
    let mut routines_imported = 0;
    for item in &plan.projects {
        if !item.has_routine_file {
            continue;
        }
        let key = project_key(&item.project.working_dir);
        let source = plan
            .source_root
            .join("projects")
            .join(format!("{key}.toml"));
        let mut config = load_source_routines(&source, &item.project.working_dir)?;
        if !keep_enabled {
            for routine in &mut config.routines {
                routine.enabled = false;
            }
        }
        routines_imported += config.routines.len();
        let target = RoutineStore::new(destination.root().to_path_buf(), &item.project.working_dir)
            .map_err(|error| RegistryError::Validation(error.to_string()))?
            .project_file();
        let contents = toml::to_string_pretty(&config)
            .map_err(|error| RegistryError::Corrupt(error.to_string()))?;
        files.push(WsxImportFile { target, contents });
    }

    let transaction = WsxImportTransaction {
        version: WSX_IMPORT_TRANSACTION_VERSION,
        registry_revision: before.revision,
        projects_before: before.projects.len(),
        routines_imported,
        projects: plan
            .projects
            .iter()
            .map(|item| item.project.clone())
            .collect(),
        files,
    };
    atomic_toml(&wsx_transaction_path(destination.root()), &transaction)
        .map_err(|error| RegistryError::Io(error.to_string()))?;

    for file in &transaction.files {
        if let Err(error) = atomic_create(&file.target, file.contents.as_bytes()) {
            rollback_wsx_import(destination, &transaction)?;
            return Err(if error.kind() == std::io::ErrorKind::AlreadyExists {
                RegistryError::Validation(format!(
                    "refusing to overwrite existing routine file {}",
                    file.target.display()
                ))
            } else {
                RegistryError::Io(error.to_string())
            });
        }
    }
    let registry = match destination.merge_locked(before.revision, transaction.projects.clone()) {
        Ok(registry) => registry,
        // ^ A registry rename may commit even when the following directory sync
        // fails. Re-read the transaction before deciding whether rollback is safe.
        Err(error) => match recover_wsx_import(destination)? {
            Some(result) => return Ok(result),
            None => return Err(error),
        },
    };
    remove_transaction(destination.root())?;
    Ok(WsxImportResult {
        projects_registered: registry.projects.len() - before.projects.len(),
        routine_files_imported: transaction.files.len(),
        routines_imported,
        registry,
    })
}

fn recover_wsx_import(
    destination: &RegistryStore,
) -> Result<Option<WsxImportResult>, RegistryError> {
    let path = wsx_transaction_path(destination.root());
    if !path.exists() {
        return Ok(None);
    }
    let text = read_text_limited(&path)
        .map_err(|error| RegistryError::Io(format!("reading {}: {error}", path.display())))?;
    let transaction: WsxImportTransaction = toml::from_str(&text)
        .map_err(|error| RegistryError::Corrupt(format!("{}: {error}", path.display())))?;
    validate_transaction(destination.root(), &transaction)?;
    let registry = destination.load()?;
    let committed = registry.revision > transaction.registry_revision
        && transaction
            .projects
            .iter()
            .all(|project| registry.projects.iter().any(|stored| stored == project));
    if committed {
        for file in &transaction.files {
            ensure_transaction_file(file)?;
        }
        remove_transaction(destination.root())?;
        Ok(Some(WsxImportResult {
            projects_registered: registry
                .projects
                .len()
                .saturating_sub(transaction.projects_before),
            routine_files_imported: transaction.files.len(),
            routines_imported: transaction.routines_imported,
            registry,
        }))
    } else {
        rollback_wsx_import(destination, &transaction)?;
        Ok(None)
    }
}

fn rollback_wsx_import(
    destination: &RegistryStore,
    transaction: &WsxImportTransaction,
) -> Result<(), RegistryError> {
    validate_transaction(destination.root(), transaction)?;
    for file in &transaction.files {
        if !file.target.exists() {
            continue;
        }
        ensure_transaction_file(file)?;
        fs::remove_file(&file.target).map_err(|error| {
            RegistryError::Io(format!("removing {}: {error}", file.target.display()))
        })?;
    }
    remove_transaction(destination.root())
}

fn ensure_transaction_file(file: &WsxImportFile) -> Result<(), RegistryError> {
    let contents = read_text_limited(&file.target).map_err(|error| {
        RegistryError::Io(format!("reading {}: {error}", file.target.display()))
    })?;
    if contents != file.contents {
        return Err(RegistryError::Corrupt(format!(
            "migration target changed after installation: {}",
            file.target.display()
        )));
    }
    Ok(())
}

fn validate_transaction(
    root: &Path,
    transaction: &WsxImportTransaction,
) -> Result<(), RegistryError> {
    if transaction.version != WSX_IMPORT_TRANSACTION_VERSION {
        return Err(RegistryError::Corrupt(format!(
            "unsupported wsx import transaction schema {}",
            transaction.version
        )));
    }
    for file in &transaction.files {
        let valid = transaction.projects.iter().any(|project| {
            file.target
                == root
                    .join("projects")
                    .join(format!("{}.toml", project_key(&project.working_dir)))
        });
        if !valid {
            return Err(RegistryError::Corrupt(format!(
                "wsx import target is outside the transaction: {}",
                file.target.display()
            )));
        }
    }
    Ok(())
}

fn wsx_transaction_path(root: &Path) -> PathBuf {
    root.join("migrations").join("wsx-import-v1.toml")
}

pub(crate) fn ensure_no_pending_wsx_import(root: &Path) -> Result<(), RegistryError> {
    let path = wsx_transaction_path(root);
    if path.exists() {
        return Err(RegistryError::Unavailable(format!(
            "unfinished wsx import at {}; run 'asched migrate wsx' to recover it before starting the daemon",
            path.display()
        )));
    }
    Ok(())
}

fn remove_transaction(root: &Path) -> Result<(), RegistryError> {
    let path = wsx_transaction_path(root);
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|error| RegistryError::Io(format!("removing {}: {error}", path.display())))?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

struct DaemonOfflineGuard(File);

impl DaemonOfflineGuard {
    fn acquire(root: &Path) -> Result<Self, RegistryError> {
        let path = root.join("daemon-v1.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| RegistryError::Io(format!("opening {}: {error}", path.display())))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            return Err(RegistryError::Unavailable(
                "stop the asched daemon before importing wsx routines".into(),
            ));
        }
        Ok(Self(file))
    }
}

impl Drop for DaemonOfflineGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn validate_destination(
    current: &ProjectRegistry,
    plan: &WsxImportPlan,
    destination_root: &Path,
) -> Result<(), RegistryError> {
    for item in &plan.projects {
        if let Some(existing) = current.projects.iter().find(|existing| {
            existing.name == item.project.name || existing.working_dir == item.project.working_dir
        }) {
            if existing != &item.project {
                return Err(RegistryError::Validation(format!(
                    "imported project '{}' conflicts with '{}' ({})",
                    item.project.name,
                    existing.name,
                    existing.working_dir.display()
                )));
            }
        }
        if item.has_routine_file {
            let target = destination_root
                .join("projects")
                .join(format!("{}.toml", project_key(&item.project.working_dir)));
            if target.exists() {
                return Err(RegistryError::Validation(format!(
                    "refusing to overwrite existing routine file {}",
                    target.display()
                )));
            }
        }
    }
    Ok(())
}

fn load_source_routines(path: &Path, working_dir: &Path) -> Result<ProjectRoutines, RegistryError> {
    let text = read_text_limited(path)
        .map_err(|error| RegistryError::Io(format!("reading {}: {error}", path.display())))?;
    let mut config: ProjectRoutines = toml::from_str(&text)
        .map_err(|error| RegistryError::Corrupt(format!("{}: {error}", path.display())))?;
    if !matches!(config.version, 1 | crate::routine::PROJECT_CONFIG_VERSION) {
        return Err(RegistryError::Corrupt(format!(
            "{} uses unsupported routine schema {}",
            path.display(),
            config.version
        )));
    }
    if config.project_path != working_dir {
        return Err(RegistryError::Corrupt(format!(
            "{} stores project {}, expected {}",
            path.display(),
            config.project_path.display(),
            working_dir.display()
        )));
    }
    for routine in &mut config.routines {
        *routine = routine
            .clone()
            .validated()
            .map_err(|error| RegistryError::Corrupt(error.to_string()))?;
    }
    Ok(config)
}

#[cfg(test)]
#[path = "migration_contract_tests.rs"]
mod migration_contract_tests;
