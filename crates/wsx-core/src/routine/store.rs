use super::{Routine, RoutineError, RunRecord, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

static RUNTIME_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoutines {
    pub version: u32,
    pub revision: u64,
    pub project_path: PathBuf,
    #[serde(default)]
    pub routines: Vec<Routine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub version: u32,
    /// Latest claimed epoch minute per routine. One entry per routine bounds
    /// restart-safe scheduling state regardless of tick frequency.
    #[serde(default, deserialize_with = "deserialize_claims")]
    pub claims: BTreeMap<String, i64>,
    #[serde(default)]
    pub runs: BTreeMap<String, Vec<RunRecord>>,
}

#[derive(Clone)]
pub struct RoutineStore {
    root: PathBuf,
    project: PathBuf,
    key: String,
}

impl RoutineStore {
    pub fn new(root: PathBuf, project: &Path) -> Result<Self, RoutineError> {
        let project = canonical_main_project(project)?;
        let key = project_key(&project);
        Ok(Self { root, project, key })
    }

    pub fn default_root() -> Result<PathBuf, RoutineError> {
        // ^ On macOS dirs::config_dir ignores XDG_CONFIG_HOME; use this override for isolation.
        if let Some(path) = std::env::var_os("WSX_ROUTINE_ROOT") {
            return Ok(PathBuf::from(path));
        }
        let config = crate::config::global::GlobalConfig::config_path()
            .ok_or_else(|| RoutineError::Unavailable("no config directory".into()))?;
        Ok(config.parent().unwrap_or(Path::new(".")).join("routines"))
    }

    pub fn project(&self) -> &Path {
        &self.project
    }
    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn project_file(&self) -> PathBuf {
        self.root
            .join("projects")
            .join(format!("{}.toml", self.key))
    }
    pub fn runtime_file(&self) -> PathBuf {
        self.root.join("runtime").join(format!("{}.toml", self.key))
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs").join(&self.key)
    }
    pub fn socket_path(&self) -> PathBuf {
        self.root.join("daemon-v1.sock")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("daemon-v1.lock")
    }

    pub fn load(&self) -> Result<ProjectRoutines, RoutineError> {
        let path = self.project_file();
        if !path.exists() {
            return Ok(ProjectRoutines {
                version: SCHEMA_VERSION,
                revision: 0,
                project_path: self.project.clone(),
                routines: vec![],
            });
        }
        let text = fs::read_to_string(&path)?;
        let config: ProjectRoutines = toml::from_str(&text)
            .map_err(|e| RoutineError::Corrupt(format!("{}: {e}", path.display())))?;
        if config.version != SCHEMA_VERSION {
            return Err(RoutineError::Corrupt(format!(
                "unsupported routine schema {}",
                config.version
            )));
        }
        if config.project_path != self.project {
            return Err(RoutineError::ProjectCollision {
                expected: self.project.clone(),
                stored: config.project_path,
            });
        }
        validate_unique(&config.routines)?;
        Ok(config)
    }

    pub fn save(
        &self,
        mut config: ProjectRoutines,
        expected: u64,
    ) -> Result<ProjectRoutines, RoutineError> {
        let current = self.load()?;
        if current.revision != expected {
            return Err(RoutineError::Conflict {
                expected,
                actual: current.revision,
            });
        }
        config.version = SCHEMA_VERSION;
        config.project_path = self.project.clone();
        config.revision = expected
            .checked_add(1)
            .ok_or_else(|| RoutineError::Corrupt("revision overflow".into()))?;
        validate_unique(&config.routines)?;
        atomic_toml(&self.project_file(), &config)?;
        Ok(config)
    }

    pub fn load_runtime(&self) -> Result<RuntimeState, RoutineError> {
        let path = self.runtime_file();
        if !path.exists() {
            return Ok(RuntimeState {
                version: SCHEMA_VERSION,
                ..Default::default()
            });
        }
        let text = fs::read_to_string(&path)?;
        let state: RuntimeState = toml::from_str(&text)
            .map_err(|e| RoutineError::Corrupt(format!("{}: {e}", path.display())))?;
        if state.version != SCHEMA_VERSION {
            return Err(RoutineError::Corrupt("unsupported runtime schema".into()));
        }
        Ok(state)
    }

    pub fn save_runtime(&self, state: &RuntimeState) -> Result<(), RoutineError> {
        atomic_toml(&self.runtime_file(), state)
    }

    pub fn claim(&self, routine: &str, epoch_minute: i64) -> Result<bool, RoutineError> {
        self.modify_runtime(|state| {
            if state.claims.get(routine) == Some(&epoch_minute) {
                return Ok(false);
            }
            state.claims.insert(routine.to_string(), epoch_minute);
            Ok(true)
        })
    }

    pub fn modify_runtime<T>(
        &self,
        update: impl FnOnce(&mut RuntimeState) -> Result<T, RoutineError>,
    ) -> Result<T, RoutineError> {
        self.with_runtime_lock(|| {
            let mut state = self.load_runtime()?;
            let result = update(&mut state)?;
            self.save_runtime(&state)?;
            Ok(result)
        })
    }

    pub(crate) fn with_runtime_lock<T>(
        &self,
        transaction: impl FnOnce() -> Result<T, RoutineError>,
    ) -> Result<T, RoutineError> {
        let lock = RUNTIME_WRITE_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock
            .lock()
            .map_err(|_| RoutineError::Io("runtime lock poisoned".into()))?;
        transaction()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredClaims {
    Current(BTreeMap<String, i64>),
    Legacy(BTreeSet<String>),
}

fn deserialize_claims<'de, D>(deserializer: D) -> Result<BTreeMap<String, i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stored = StoredClaims::deserialize(deserializer)?;
    Ok(match stored {
        StoredClaims::Current(claims) => claims,
        StoredClaims::Legacy(claims) => {
            let mut latest = BTreeMap::<String, i64>::new();
            for claim in claims {
                let Some((routine, minute)) = claim.rsplit_once("\\0") else {
                    continue;
                };
                let Ok(minute) = minute.parse::<i64>() else {
                    continue;
                };
                latest
                    .entry(routine.to_string())
                    .and_modify(|current| *current = (*current).max(minute))
                    .or_insert(minute);
            }
            latest
        }
    })
}

pub fn canonical_main_project(path: &Path) -> Result<PathBuf, RoutineError> {
    let canonical = fs::canonicalize(path)?;
    let output = Command::new("git")
        .args([
            "-C",
            canonical.to_string_lossy().as_ref(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()?;
    if !output.status.success() {
        return Err(RoutineError::Validation(format!(
            "{} is not a git repository",
            path.display()
        )));
    }
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .ok_or_else(|| RoutineError::Corrupt("git returned no main worktree".into()))?
        .to_string();
    fs::canonicalize(first).map_err(Into::into)
}

/// Stable FNV-1a-128 identity. This is persistence format, not `Hash` state.
pub fn project_key(path: &Path) -> String {
    // ^ Persistence contract: FNV-1a-128("/repo") = e3905a3dac83d94f708074314a8c762a.
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for byte in path.as_os_str().to_string_lossy().as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:032x}")
}

fn validate_unique(routines: &[Routine]) -> Result<(), RoutineError> {
    let mut names = BTreeSet::new();
    for routine in routines {
        let routine = routine.clone().validated()?;
        if !names.insert(routine.name.clone()) {
            return Err(RoutineError::Duplicate(routine.name));
        }
    }
    Ok(())
}

pub(crate) fn atomic_toml<T: Serialize>(path: &Path, value: &T) -> Result<(), RoutineError> {
    let parent = path
        .parent()
        .ok_or_else(|| RoutineError::Io("path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let text = toml::to_string_pretty(value).map_err(|e| RoutineError::Corrupt(e.to_string()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok::<_, RoutineError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn test_store() -> RoutineStore {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/routine-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let project =
            fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
        let key = project_key(&project);
        RoutineStore { root, project, key }
    }

    fn routine(name: &str) -> Routine {
        Routine {
            name: name.into(),
            cron: "*/5 * * * *".into(),
            command: vec!["echo".into()],
            prompt: "hello".into(),
        }
    }

    #[test]
    fn fnv_key_is_stable_known_vector() {
        assert_eq!(
            project_key(Path::new("/repo")),
            "e3905a3dac83d94f708074314a8c762a"
        );
        assert_eq!(
            project_key(Path::new("/repo")),
            project_key(Path::new("/repo"))
        );
    }

    #[test]
    fn durable_store_rejects_stale_revision_and_path_collision() {
        let store = test_store();
        let first = store
            .save(
                ProjectRoutines {
                    version: 1,
                    revision: 0,
                    project_path: store.project.clone(),
                    routines: vec![routine("one")],
                },
                0,
            )
            .unwrap();
        assert_eq!(first.revision, 1);
        let stale = store.save(first.clone(), 0).unwrap_err();
        assert!(matches!(
            stale,
            RoutineError::Conflict {
                expected: 0,
                actual: 1
            }
        ));

        let mut wrong = first;
        wrong.project_path = PathBuf::from("/another/project");
        atomic_toml(&store.project_file(), &wrong).unwrap();
        assert!(matches!(
            store.load(),
            Err(RoutineError::ProjectCollision { .. })
        ));
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn durable_store_rejects_duplicate_exact_names() {
        let store = test_store();
        let result = store.save(
            ProjectRoutines {
                version: 1,
                revision: 0,
                project_path: store.project.clone(),
                routines: vec![routine("same"), routine("same")],
            },
            0,
        );
        assert!(matches!(result, Err(RoutineError::Duplicate(name)) if name == "same"));
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn repeated_claims_remain_bounded_to_one_epoch_per_routine() {
        let store = test_store();
        for minute in 0..100 {
            assert!(store.claim("frequent", minute).unwrap());
            assert!(!store.claim("frequent", minute).unwrap());
        }
        assert!(store.claim("other", 42).unwrap());
        let state = store.load_runtime().unwrap();
        assert_eq!(state.claims.len(), 2);
        assert_eq!(state.claims["frequent"], 99);
        assert_eq!(state.claims["other"], 42);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn legacy_claim_sets_load_as_latest_epoch_per_routine() {
        let parsed: RuntimeState = toml::from_str(
            r#"
version = 1
claims = ["one\\01", "one\\02", "two\\07"]
"#,
        )
        .unwrap();
        assert_eq!(parsed.claims.get("one"), Some(&2));
        assert_eq!(parsed.claims.get("two"), Some(&7));
    }
}
