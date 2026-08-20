use super::*;
use crate::routine::{Routine, Trigger, SCHEMA_VERSION};
use serde::Serialize;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/migration-contract-tests")
            .join(format!(
                "{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn project_dir(&self, name: &str) -> PathBuf {
        let path = self.root.join("working").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn source_root(&self) -> PathBuf {
        self.root.join("source")
    }

    fn source_config(&self) -> PathBuf {
        self.root.join("wsx.toml")
    }

    fn destination(&self) -> RegistryStore {
        RegistryStore::new(self.root.join("destination"))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Serialize)]
struct SourceConfig {
    projects: Vec<SourceProject>,
}

#[derive(Serialize)]
struct SourceProject {
    name: String,
    path: PathBuf,
}

fn write_source_config(fixture: &TestDir, projects: Vec<(&str, PathBuf)>) {
    let config = SourceConfig {
        projects: projects
            .into_iter()
            .map(|(name, path)| SourceProject {
                name: name.into(),
                path,
            })
            .collect(),
    };
    fs::write(fixture.source_config(), toml::to_string(&config).unwrap()).unwrap();
}

fn routine(name: &str, enabled: bool) -> Routine {
    Routine {
        name: name.into(),
        trigger: Trigger::Cron("0 9 * * *".into()),
        command: vec!["echo".into(), "{prompt}".into()],
        prompt: "hello".into(),
        enabled,
    }
}

fn write_routines(
    fixture: &TestDir,
    project_path: &Path,
    version: u32,
    routines: Vec<Routine>,
) -> PathBuf {
    let path = fixture
        .source_root()
        .join("projects")
        .join(format!("{}.toml", project_key(project_path)));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        toml::to_string(&ProjectRoutines {
            version,
            revision: 7,
            project_path: project_path.to_path_buf(),
            routines,
        })
        .unwrap(),
    )
    .unwrap();
    path
}

fn plan_with_routine(fixture: &TestDir, enabled: bool) -> WsxImportPlan {
    let directory = fixture.project_dir("alpha");
    write_source_config(fixture, vec![("alpha", directory.clone())]);
    write_routines(
        fixture,
        &directory,
        SCHEMA_VERSION,
        vec![routine("daily", enabled)],
    );
    plan_wsx_import(&fixture.source_root(), &fixture.source_config()).unwrap()
}

fn transaction_for_plan(plan: &WsxImportPlan, destination: &RegistryStore) -> WsxImportTransaction {
    let project = plan.projects[0].project.clone();
    let source = plan
        .source_root
        .join("projects")
        .join(format!("{}.toml", project_key(&project.working_dir)));
    let config = load_source_routines(&source, &project.working_dir).unwrap();
    WsxImportTransaction {
        version: WSX_IMPORT_TRANSACTION_VERSION,
        registry_revision: 0,
        projects_before: 0,
        routines_imported: config.routines.len(),
        projects: vec![project.clone()],
        files: vec![WsxImportFile {
            target: RoutineStore::new(destination.root().to_path_buf(), &project.working_dir)
                .unwrap()
                .project_file(),
            contents: toml::to_string_pretty(&config).unwrap(),
        }],
    }
}

#[test]
fn given_missing_source_config_when_planned_then_io_error_is_reported() {
    let fixture = TestDir::new("missing-config");

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Io(_))));
}

#[test]
fn given_malformed_source_config_when_planned_then_corrupt_is_reported() {
    let fixture = TestDir::new("malformed-config");
    fs::write(fixture.source_config(), "[[projects]\n").unwrap();

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_malformed_routine_toml_when_planned_then_corrupt_is_reported() {
    let fixture = TestDir::new("malformed-routine");
    let directory = fixture.project_dir("alpha");
    write_source_config(&fixture, vec![("alpha", directory.clone())]);
    let path = fixture
        .source_root()
        .join("projects")
        .join(format!("{}.toml", project_key(&directory)));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "version = [\n").unwrap();

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_source_projects_and_partial_routine_files_when_planned_then_plan_is_sorted_and_counted() {
    let fixture = TestDir::new("plan");
    let zeta = fixture.project_dir("zeta");
    let alpha = fixture.project_dir("alpha");
    write_source_config(&fixture, vec![("zeta", zeta), ("alpha", alpha.clone())]);
    write_routines(
        &fixture,
        &alpha,
        SCHEMA_VERSION,
        vec![routine("one", true), routine("two", false)],
    );

    let plan = plan_wsx_import(&fixture.source_root(), &fixture.source_config()).unwrap();

    assert_eq!(
        plan.projects
            .iter()
            .map(|item| (
                item.project.name.as_str(),
                item.has_routine_file,
                item.routine_count
            ))
            .collect::<Vec<_>>(),
        vec![("alpha", true, 2), ("zeta", false, 0)]
    );
}

#[test]
fn given_wsx_v1_cron_file_when_planned_then_legacy_trigger_is_imported() {
    #[derive(Serialize)]
    struct LegacyRoutine<'a> {
        name: &'a str,
        cron: &'a str,
        command: Vec<&'a str>,
        prompt: &'a str,
        enabled: bool,
    }

    #[derive(Serialize)]
    struct LegacyProjectRoutines<'a> {
        version: u32,
        revision: u64,
        project_path: &'a Path,
        routines: Vec<LegacyRoutine<'a>>,
    }

    let fixture = TestDir::new("legacy-v1-routine");
    let directory = fixture.project_dir("alpha");
    write_source_config(&fixture, vec![("alpha", directory.clone())]);
    let path = write_routines(&fixture, &directory, 1, Vec::new());
    let legacy = LegacyProjectRoutines {
        version: 1,
        revision: 7,
        project_path: &directory,
        routines: vec![LegacyRoutine {
            name: "daily",
            cron: "0 9 * * *",
            command: vec!["echo", "{prompt}"],
            prompt: "hello",
            enabled: true,
        }],
    };
    fs::write(path, toml::to_string(&legacy).unwrap()).unwrap();

    let plan = plan_wsx_import(&fixture.source_root(), &fixture.source_config()).unwrap();
    let destination = fixture.destination();
    apply_wsx_import(&plan, &destination, true).unwrap();
    let imported = RoutineStore::new(destination.root().to_path_buf(), &directory)
        .unwrap()
        .load()
        .unwrap();

    assert_eq!(
        imported.routines[0].trigger,
        Trigger::Cron("0 9 * * *".into())
    );
}

#[test]
fn given_unknown_routine_schema_when_planned_then_corrupt_is_reported() {
    let fixture = TestDir::new("routine-schema");
    let directory = fixture.project_dir("alpha");
    write_source_config(&fixture, vec![("alpha", directory.clone())]);
    write_routines(&fixture, &directory, 999, vec![routine("daily", true)]);

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_structurally_valid_routine_with_invalid_cron_when_planned_then_corrupt_is_reported() {
    let fixture = TestDir::new("semantic-routine");
    let directory = fixture.project_dir("alpha");
    write_source_config(&fixture, vec![("alpha", directory.clone())]);
    let mut invalid = routine("daily", true);
    invalid.trigger = Trigger::Cron("99 99 99 99 99".into());
    write_routines(&fixture, &directory, SCHEMA_VERSION, vec![invalid]);

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_routine_file_for_different_project_when_planned_then_corrupt_is_reported() {
    let fixture = TestDir::new("routine-project");
    let directory = fixture.project_dir("alpha");
    let other = fixture.project_dir("other");
    write_source_config(&fixture, vec![("alpha", directory.clone())]);
    let source = write_routines(
        &fixture,
        &directory,
        SCHEMA_VERSION,
        vec![routine("daily", true)],
    );
    let text = toml::to_string(&ProjectRoutines {
        version: SCHEMA_VERSION,
        revision: 0,
        project_path: other,
        routines: vec![routine("daily", true)],
    })
    .unwrap();
    fs::write(source, text).unwrap();

    let result = plan_wsx_import(&fixture.source_root(), &fixture.source_config());

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_source_enabled_and_keep_enabled_matrix_when_applied_then_enabled_state_follows_contract() {
    let cases = [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ];

    let observed = cases
        .into_iter()
        .map(|(source_enabled, keep_enabled, expected)| {
            let fixture = TestDir::new(&format!("enabled-{source_enabled}-keep-{keep_enabled}"));
            let plan = plan_with_routine(&fixture, source_enabled);
            let destination = fixture.destination();
            apply_wsx_import(&plan, &destination, keep_enabled).unwrap();
            let imported = RoutineStore::new(
                destination.root().to_path_buf(),
                &plan.projects[0].project.working_dir,
            )
            .unwrap()
            .load()
            .unwrap();
            (
                source_enabled,
                keep_enabled,
                imported.routines[0].enabled,
                expected,
            )
        })
        .collect::<Vec<_>>();

    assert!(observed
        .iter()
        .all(|(_, _, actual, expected)| actual == expected));
}

#[test]
fn given_existing_destination_routine_file_when_applied_then_file_is_not_overwritten() {
    let fixture = TestDir::new("no-overwrite");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    let target = RoutineStore::new(
        destination.root().to_path_buf(),
        &plan.projects[0].project.working_dir,
    )
    .unwrap()
    .project_file();
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "sentinel").unwrap();

    let error = apply_wsx_import(&plan, &destination, true).unwrap_err();
    let registry = destination.load().unwrap();

    assert!(
        matches!(error, RegistryError::Validation(_))
            && fs::read_to_string(target).unwrap() == "sentinel"
            && registry.revision == 0
            && registry.projects.is_empty()
    );
}

#[test]
fn given_destination_name_collision_when_applied_then_destination_state_is_unchanged() {
    let fixture = TestDir::new("name-collision");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    destination
        .add(
            0,
            Project {
                name: "alpha".into(),
                working_dir: fixture.project_dir("different"),
            },
        )
        .unwrap();
    let target = RoutineStore::new(
        destination.root().to_path_buf(),
        &plan.projects[0].project.working_dir,
    )
    .unwrap()
    .project_file();

    let result = apply_wsx_import(&plan, &destination, true);
    let registry = destination.load().unwrap();

    assert!(
        matches!(result, Err(RegistryError::Validation(_)))
            && registry.revision == 1
            && registry.projects.len() == 1
            && registry.projects[0].name == "alpha"
            && !target.exists()
    );
}

#[test]
fn given_new_project_and_routine_file_when_applied_then_result_counts_all_imported_items() {
    let fixture = TestDir::new("counts");
    let plan = plan_with_routine(&fixture, true);

    let result = apply_wsx_import(&plan, &fixture.destination(), true).unwrap();

    assert_eq!(
        (
            result.projects_registered,
            result.routine_files_imported,
            result.routines_imported,
            result.registry.revision
        ),
        (1, 1, 1, 1)
    );
}

#[test]
fn given_held_destination_daemon_lock_when_applied_then_unavailable_is_reported_without_state_writes(
) {
    let fixture = TestDir::new("daemon-lock");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    fs::create_dir_all(destination.root()).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(destination.root().join("daemon-v1.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let result = apply_wsx_import(&plan, &destination, true);
    let target = transaction_for_plan(&plan, &destination)
        .files
        .remove(0)
        .target;

    assert!(
        matches!(result, Err(RegistryError::Unavailable(_)))
            && !destination.path().exists()
            && !target.exists()
            && !wsx_transaction_path(destination.root()).exists()
    );
}

#[test]
fn given_uncommitted_transaction_with_exact_files_when_applied_then_it_rolls_back_and_retries() {
    let fixture = TestDir::new("uncommitted-recovery");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    let transaction = transaction_for_plan(&plan, &destination);
    atomic_toml(&wsx_transaction_path(destination.root()), &transaction).unwrap();
    atomic_create(
        &transaction.files[0].target,
        transaction.files[0].contents.as_bytes(),
    )
    .unwrap();
    let installed = fs::File::open(&transaction.files[0].target).unwrap();
    let installed_inode = installed.metadata().unwrap().ino();

    let result = apply_wsx_import(&plan, &destination, true).unwrap();
    let retried_inode = fs::metadata(&transaction.files[0].target).unwrap().ino();

    assert!(
        result.registry.revision == 1
            && result.projects_registered == 1
            && retried_inode != installed_inode
            && !wsx_transaction_path(destination.root()).exists()
    );
}

#[test]
fn given_committed_transaction_with_exact_files_when_applied_then_result_is_recovered_without_rewrite(
) {
    let fixture = TestDir::new("committed-recovery");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    let transaction = transaction_for_plan(&plan, &destination);
    destination.add(0, transaction.projects[0].clone()).unwrap();
    atomic_create(
        &transaction.files[0].target,
        transaction.files[0].contents.as_bytes(),
    )
    .unwrap();
    atomic_toml(&wsx_transaction_path(destination.root()), &transaction).unwrap();
    let before = fs::metadata(&transaction.files[0].target).unwrap();
    let before_identity = (
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
    );

    let result = apply_wsx_import(&plan, &destination, false).unwrap();
    let after = fs::metadata(&transaction.files[0].target).unwrap();
    let after_identity = (after.ino(), after.len(), after.mtime(), after.mtime_nsec());

    assert!(
        result.registry.revision == 1
            && result.projects_registered == 1
            && after_identity == before_identity
            && !wsx_transaction_path(destination.root()).exists()
    );
}

#[test]
fn given_transaction_and_committed_registry_when_recovered_then_committed_import_is_returned_without_deleting_files(
) {
    let fixture = TestDir::new("ambiguous-commit");
    let plan = plan_with_routine(&fixture, true);
    let destination = fixture.destination();
    let transaction = transaction_for_plan(&plan, &destination);
    atomic_toml(&wsx_transaction_path(destination.root()), &transaction).unwrap();
    atomic_create(
        &transaction.files[0].target,
        transaction.files[0].contents.as_bytes(),
    )
    .unwrap();
    destination.add(0, transaction.projects[0].clone()).unwrap();
    let installed = fs::File::open(&transaction.files[0].target).unwrap();
    let installed_inode = installed.metadata().unwrap().ino();

    let recovered = recover_wsx_import(&destination).unwrap();

    assert!(
        matches!(
            recovered,
            Some(WsxImportResult {
                projects_registered: 1,
                routine_files_imported: 1,
                routines_imported: 1,
                ..
            })
        ) && fs::metadata(&transaction.files[0].target).unwrap().ino() == installed_inode
            && !wsx_transaction_path(destination.root()).exists()
    );
}
