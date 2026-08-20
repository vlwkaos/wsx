use super::{
    Routine, RoutineError, RunCause, RunRecord, MAX_EVENT_RECEIPTS, PROJECT_CONFIG_VERSION,
    RUNTIME_STATE_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static RUNTIME_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
pub(crate) const MAX_STATE_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoutines {
    pub version: u32,
    pub revision: u64,
    pub project_path: PathBuf,
    #[serde(default)]
    pub routines: Vec<Routine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventReceipt {
    pub kind: String,
    pub event_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub version: u32,
    /// Greatest claimed epoch minute per routine. One entry per routine bounds
    /// restart-safe scheduling state and rejects duplicate slots after rollback.
    #[serde(default, deserialize_with = "deserialize_claims")]
    pub claims: BTreeMap<String, i64>,
    #[serde(default)]
    pub runs: BTreeMap<String, Vec<RunRecord>>,
    #[serde(default)]
    pub event_receipts: Vec<EventReceipt>,
}

impl RuntimeState {
    pub fn has_event_receipt(&self, kind: &str, event_id: &str) -> bool {
        self.event_receipts
            .iter()
            .any(|receipt| receipt.kind == kind && receipt.event_id == event_id)
    }

    pub fn record_event_receipt(&mut self, kind: String, event_id: String) {
        self.event_receipts.push(EventReceipt { kind, event_id });
        if self.event_receipts.len() > MAX_EVENT_RECEIPTS {
            self.event_receipts
                .drain(0..self.event_receipts.len() - MAX_EVENT_RECEIPTS);
        }
    }
}

#[derive(Clone)]
// ^ [[Durable Scheduler State and Process Cleanup]]
pub struct RoutineStore {
    root: PathBuf,
    project: PathBuf,
    key: String,
}

impl RoutineStore {
    pub fn new(root: PathBuf, project: &Path) -> Result<Self, RoutineError> {
        let project = canonical_working_dir(project)?;
        let key = project_key(&project);
        Ok(Self { root, project, key })
    }

    pub fn default_root() -> Result<PathBuf, RoutineError> {
        crate::RegistryStore::default_root()
            .map_err(|error| RoutineError::Unavailable(error.to_string()))
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
                version: PROJECT_CONFIG_VERSION,
                revision: 0,
                project_path: self.project.clone(),
                routines: vec![],
            });
        }
        let text = read_text_limited(&path)?;
        let config: ProjectRoutines = toml::from_str(&text)
            .map_err(|e| RoutineError::Corrupt(format!("{}: {e}", path.display())))?;
        if !matches!(config.version, 1 | PROJECT_CONFIG_VERSION) {
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
        config.version = PROJECT_CONFIG_VERSION;
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
                version: RUNTIME_STATE_VERSION,
                ..Default::default()
            });
        }
        let text = read_text_limited(&path)?;
        let mut state: RuntimeState = toml::from_str(&text)
            .map_err(|e| RoutineError::Corrupt(format!("{}: {e}", path.display())))?;
        if !matches!(state.version, 1 | RUNTIME_STATE_VERSION) {
            return Err(RoutineError::Corrupt("unsupported runtime schema".into()));
        }
        if state.version == 1 {
            for runs in state.runs.values_mut() {
                for run in runs {
                    if let Some(minute) = run.scheduled_epoch_minute {
                        run.cause = RunCause::Cron {
                            scheduled_epoch_minute: minute,
                        };
                    }
                }
            }
        }
        state.version = RUNTIME_STATE_VERSION;
        Ok(state)
    }

    pub fn save_runtime(&self, state: &RuntimeState) -> Result<(), RoutineError> {
        let mut state = state.clone();
        state.version = RUNTIME_STATE_VERSION;
        atomic_toml(&self.runtime_file(), &state)
    }

    pub fn admit_event(
        &self,
        kind: &str,
        event_id: &str,
        records: &[RunRecord],
    ) -> Result<bool, RoutineError> {
        self.modify_runtime(|state| {
            if state.has_event_receipt(kind, event_id) {
                return Ok(false);
            }
            state.record_event_receipt(kind.to_string(), event_id.to_string());
            for record in records {
                state
                    .runs
                    .entry(record.routine.clone())
                    .or_default()
                    .push(record.clone());
            }
            Ok(true)
        })
    }

    pub fn claim(&self, routine: &str, epoch_minute: i64) -> Result<bool, RoutineError> {
        self.modify_runtime(|state| {
            if state
                .claims
                .get(routine)
                .is_some_and(|claimed| epoch_minute <= *claimed)
            {
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

pub fn canonical_working_dir(path: &Path) -> Result<PathBuf, RoutineError> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(RoutineError::Validation(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(canonical)
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
        let validated = routine.clone().validated()?;
        if validated.name != routine.name {
            return Err(RoutineError::Validation(
                "stored routine name must be trimmed".into(),
            ));
        }
        if !names.insert(validated.name.clone()) {
            return Err(RoutineError::Duplicate(validated.name));
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
    if text.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err(RoutineError::Validation(format!(
            "serialized state exceeds the {MAX_STATE_FILE_BYTES} byte limit"
        )));
    }
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

pub(crate) fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let tmp = path.with_extension(format!("tmp-new-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&tmp, path)?;
        fs::remove_file(&tmp)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub(crate) fn read_text_limited(path: &Path) -> Result<String, std::io::Error> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_STATE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {} byte state-file limit",
                path.display(),
                MAX_STATE_FILE_BYTES
            ),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::{Trigger, SCHEMA_VERSION};
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
            trigger: Trigger::Cron("*/5 * * * *".into()),
            command: vec!["echo".into()],
            prompt: "hello".into(),
            enabled: true,
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
    fn legacy_project_routines_without_enabled_load_as_enabled() {
        #[derive(Serialize)]
        struct LegacyRoutine<'a> {
            name: &'a str,
            cron: &'a str,
            command: Vec<&'a str>,
            prompt: &'a str,
        }

        #[derive(Serialize)]
        struct LegacyProjectRoutines<'a> {
            version: u32,
            revision: u64,
            project_path: &'a Path,
            routines: Vec<LegacyRoutine<'a>>,
        }

        let store = test_store();
        let legacy = LegacyProjectRoutines {
            version: 1,
            revision: 7,
            project_path: store.project(),
            routines: vec![LegacyRoutine {
                name: "daily",
                cron: "0 9 * * *",
                command: vec!["echo"],
                prompt: "hello",
            }],
        };
        fs::create_dir_all(store.project_file().parent().unwrap()).unwrap();
        let legacy_text = toml::to_string(&legacy).unwrap();
        fs::write(store.project_file(), &legacy_text).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.revision, 7);
        assert_eq!(loaded.routines.len(), 1);
        assert!(loaded.routines[0].enabled);
        assert_eq!(
            fs::read_to_string(store.project_file()).unwrap(),
            legacy_text
        );
        let _ = fs::remove_dir_all(&store.root);
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
    fn durable_store_rejects_noncanonical_names() {
        let store = test_store();
        let config = ProjectRoutines {
            version: SCHEMA_VERSION,
            revision: 1,
            project_path: store.project.clone(),
            routines: vec![routine(" padded ")],
        };
        atomic_toml(&store.project_file(), &config).unwrap();

        assert!(matches!(
            store.load(),
            Err(RoutineError::Validation(message)) if message.contains("trimmed")
        ));
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn claims_are_bounded_and_reject_duplicate_or_rolled_back_minutes() {
        let store = test_store();
        for minute in 0..100 {
            assert!(store.claim("frequent", minute).unwrap());
            assert!(!store.claim("frequent", minute).unwrap());
        }
        assert!(!store.claim("frequent", 50).unwrap());
        assert!(store.claim("frequent", 100).unwrap());
        assert!(store.claim("other", 42).unwrap());
        let state = store.load_runtime().unwrap();
        assert_eq!(state.claims.len(), 2);
        assert_eq!(state.claims["frequent"], 100);
        assert_eq!(state.claims["other"], 42);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn legacy_runtime_load_derives_cron_cause_without_rewriting_file() {
        let store = test_store();
        let legacy = r#"version = 1
claims = {}

[runs]

[[runs.daily]]
id = "1-1"
routine = "daily"
started_epoch = 1
scheduled_epoch_minute = 123
status = "succeeded"
exit_code = 0
final_output = "done"
stdout_path = "/logs/stdout"
stderr_path = "/logs/stderr"
"#;
        fs::create_dir_all(store.runtime_file().parent().unwrap()).unwrap();
        fs::write(store.runtime_file(), legacy).unwrap();

        let loaded = store.load_runtime().unwrap();

        assert_eq!(
            loaded.runs["daily"][0].cause,
            RunCause::Cron {
                scheduled_epoch_minute: 123
            }
        );
        assert_eq!(fs::read_to_string(store.runtime_file()).unwrap(), legacy);
        let _ = fs::remove_dir_all(&store.root);
    }

    #[test]
    fn event_receipts_retain_only_the_latest_bounded_window() {
        let mut state = RuntimeState::default();
        for index in 0..=MAX_EVENT_RECEIPTS {
            state.record_event_receipt("test.changed".into(), index.to_string());
        }

        assert_eq!(state.event_receipts.len(), MAX_EVENT_RECEIPTS);
        assert!(!state.has_event_receipt("test.changed", "0"));
        assert!(state.has_event_receipt("test.changed", &MAX_EVENT_RECEIPTS.to_string()));
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

#[cfg(test)]
#[path = "store_contract_tests.rs"]
mod store_contract_tests;
