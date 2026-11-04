//! The routine store beside wsxd's socket: `routines/projects/<key>.json` per project (its
//! routines, revision, claimed minutes, run records and event receipts) and
//! `routines/logs/<key>/<run id>/` with each run's output. wsxd writes; others only read.

use super::{Routine, RoutineError, RoutineErrorKind, RunRecord};
use crate::fs_util::{private_dir, write_atomic, AtomicWrite};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Event ids kept per project, so one event runs its routines once.
const MAX_EVENT_RECEIPTS: usize = 4_096;
/// The largest store file read, as asched's store had.
const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;

/// The store of the daemon that `WSX_SOCKET` (or the state folder) names.
pub fn root() -> PathBuf {
    root_for(&crate::runtime::default_socket_path())
}

pub fn root_for(socket: &Path) -> PathBuf {
    socket.with_file_name("routines")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRoutines {
    pub project_path: PathBuf,
    /// Bumps with each change of `routines`, so a stale edit is refused.
    pub revision: u64,
    pub routines: Vec<Routine>,
    /// The newest minute each routine ran for; a minute runs once, also after a restart.
    #[serde(default)]
    pub claims: BTreeMap<String, i64>,
    /// Oldest first, at most `MAX_RUNS` per routine.
    #[serde(default)]
    pub runs: BTreeMap<String, Vec<RunRecord>>,
    #[serde(default)]
    pub event_receipts: Vec<EventReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventReceipt {
    pub kind: String,
    pub event_id: String,
}

impl ProjectRoutines {
    pub fn has_receipt(&self, kind: &str, event_id: &str) -> bool {
        self.event_receipts
            .iter()
            .any(|receipt| receipt.kind == kind && receipt.event_id == event_id)
    }

    pub fn add_receipt(&mut self, kind: &str, event_id: &str) {
        self.event_receipts.push(EventReceipt {
            kind: kind.into(),
            event_id: event_id.into(),
        });
        let excess = self.event_receipts.len().saturating_sub(MAX_EVENT_RECEIPTS);
        self.event_receipts.drain(..excess);
    }

    /// The routine's newest run, while it runs.
    pub fn running(&self, name: &str) -> Option<&RunRecord> {
        self.runs
            .get(name)
            .and_then(|runs| runs.last())
            .filter(|run| run.status == super::RunStatus::Running)
    }
}

/// FNV-1a-128 of the canonical project path: asched's key, so an imported store keeps it.
pub fn project_key(path: &Path) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for byte in path.as_os_str().to_string_lossy().as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:032x}")
}

pub fn canonical_project(project: &Path) -> Result<PathBuf, RoutineError> {
    let canonical = fs::canonicalize(project)?;
    if !canonical.is_dir() {
        return Err(RoutineError::validation(format!(
            "{} is not a directory",
            project.display()
        )));
    }
    Ok(canonical)
}

pub fn project_file(root: &Path, project: &Path) -> PathBuf {
    root.join("projects")
        .join(format!("{}.json", project_key(project)))
}

pub fn run_dir(root: &Path, project: &Path, id: &str) -> PathBuf {
    root.join("logs").join(project_key(project)).join(id)
}

/// The project's routines; an empty set when it has none. `project` is canonical.
pub fn load(root: &Path, project: &Path) -> Result<ProjectRoutines, RoutineError> {
    read(&project_file(root, project)).map(|stored| {
        stored.unwrap_or_else(|| ProjectRoutines {
            project_path: project.to_path_buf(),
            ..Default::default()
        })
    })
}

pub fn read(path: &Path) -> Result<Option<ProjectRoutines>, RoutineError> {
    let Some(bytes) = read_bounded(path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        RoutineError::new(
            RoutineErrorKind::Corrupt,
            format!("{}: {error}", path.display()),
        )
    })
}

pub fn save(root: &Path, routines: &ProjectRoutines) -> Result<(), RoutineError> {
    let path = project_file(root, &routines.project_path);
    let bytes = serde_json::to_vec_pretty(routines)
        .map_err(|error| RoutineError::new(RoutineErrorKind::Corrupt, error.to_string()))?;
    private_dir(path.parent().expect("a project file has a folder"))?;
    write_atomic(&path, &bytes, AtomicWrite::replace().mode(0o600))?;
    Ok(())
}

/// A store file's bytes, `None` when it is missing; a file past 64 MiB is an error.
pub fn read_bounded(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_STORE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {MAX_STORE_BYTES} bytes", path.display()),
        ));
    }
    Ok(Some(bytes))
}
