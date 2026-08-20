use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/registry-contract-tests")
            .join(format!(
                "{}-{}-{label}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn project_dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn store(&self) -> RegistryStore {
        RegistryStore::new(self.root.join("destination"))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn project(name: &str, working_dir: PathBuf) -> Project {
    Project {
        name: name.into(),
        working_dir,
    }
}

fn write_registry(store: &RegistryStore, registry: &ProjectRegistry) {
    fs::create_dir_all(store.root()).unwrap();
    fs::write(store.path(), toml::to_string(registry).unwrap()).unwrap();
}

#[test]
fn given_whitespace_name_and_relative_alias_when_validated_then_identity_is_normalized() {
    let fixture = TestDir::new("normalize");
    let directory = fixture.project_dir("project");
    let alias = directory.join("..").join("project");

    let validated = project("  alpha  ", alias).validated().unwrap();

    assert_eq!(
        (validated.name, validated.working_dir),
        ("alpha".into(), fs::canonicalize(directory).unwrap())
    );
}

#[test]
fn given_empty_name_when_validated_then_validation_fails() {
    let fixture = TestDir::new("empty-name");
    let result = project(" \t ", fixture.project_dir("project")).validated();

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_separator_in_name_when_validated_then_validation_fails() {
    let fixture = TestDir::new("separator-name");
    let result = project("alpha/beta", fixture.project_dir("project")).validated();

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_nul_in_name_when_validated_then_validation_fails() {
    let fixture = TestDir::new("nul-name");
    let result = project("alpha\0beta", fixture.project_dir("project")).validated();

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_any_unicode_control_in_name_when_validated_then_validation_fails() {
    let fixture = TestDir::new("unicode-controls");
    let directory = fixture.project_dir("project");

    let rejected = (0..=char::MAX as u32)
        .filter_map(char::from_u32)
        .filter(|character| character.is_control())
        .all(|character| {
            matches!(
                project(&format!("alpha{character}beta"), directory.clone()).validated(),
                Err(RegistryError::Validation(_))
            )
        });

    assert!(rejected);
}

#[test]
fn given_missing_working_directory_when_validated_then_validation_fails() {
    let fixture = TestDir::new("missing-directory");
    let result = project("alpha", fixture.root.join("missing")).validated();

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_regular_file_as_working_directory_when_validated_then_validation_fails() {
    let fixture = TestDir::new("regular-file");
    let path = fixture.root.join("file");
    fs::write(&path, "not a directory").unwrap();

    let result = project("alpha", path).validated();

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_absent_registry_when_loaded_then_empty_schema_one_registry_is_returned() {
    let fixture = TestDir::new("absent");

    let loaded = fixture.store().load().unwrap();

    assert_eq!(
        (loaded.version, loaded.revision, loaded.projects.len()),
        (1, 0, 0)
    );
}

#[test]
fn given_unknown_registry_schema_when_loaded_then_corrupt_is_reported() {
    let fixture = TestDir::new("schema");
    let store = fixture.store();
    fs::create_dir_all(store.root()).unwrap();
    fs::write(store.path(), "version = 999\nrevision = 0\nprojects = []\n").unwrap();

    let result = store.load();

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_malformed_registry_toml_when_loaded_then_corrupt_is_reported() {
    let fixture = TestDir::new("malformed");
    let store = fixture.store();
    fs::create_dir_all(store.root()).unwrap();
    fs::write(store.path(), "version = [\n").unwrap();

    let result = store.load();

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_unknown_registry_field_when_loaded_then_corrupt_is_reported() {
    let fixture = TestDir::new("unknown-field");
    let store = fixture.store();
    fs::create_dir_all(store.root()).unwrap();
    fs::write(
        store.path(),
        "version = 1\nrevision = 0\nunknown = true\nprojects = []\n",
    )
    .unwrap();

    let result = store.load();

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_structurally_valid_registry_with_invalid_project_when_loaded_then_corrupt_is_reported() {
    let fixture = TestDir::new("invalid-stored-project");
    let store = fixture.store();
    write_registry(
        &store,
        &ProjectRegistry {
            version: 1,
            revision: 4,
            projects: vec![project("invalid/name", fixture.project_dir("project"))],
        },
    );

    let result = store.load();

    assert!(matches!(result, Err(RegistryError::Corrupt(_))));
}

#[test]
fn given_added_projects_when_reloaded_then_they_are_persisted_sorted_and_revisioned() {
    let fixture = TestDir::new("persist");
    let store = fixture.store();
    let zeta = fixture.project_dir("zeta");
    let alpha = fixture.project_dir("alpha");
    store.add(0, project("zeta", zeta)).unwrap();
    store.add(1, project("alpha", alpha)).unwrap();

    let loaded = store.load().unwrap();

    assert_eq!(
        (
            loaded.revision,
            loaded
                .projects
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
        ),
        (2, vec!["alpha", "zeta"])
    );
}

#[test]
fn given_existing_name_when_added_again_then_duplicate_is_reported() {
    let fixture = TestDir::new("duplicate-name");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("one")))
        .unwrap();

    let result = store.add(1, project("alpha", fixture.project_dir("two")));
    let loaded = store.load().unwrap();

    assert!(
        matches!(result, Err(RegistryError::Duplicate(name)) if name == "alpha")
            && loaded.revision == 1
            && loaded.projects.len() == 1
            && loaded.projects[0].name == "alpha"
    );
}

#[test]
fn given_existing_canonical_path_when_added_through_alias_then_validation_fails() {
    let fixture = TestDir::new("duplicate-path");
    let store = fixture.store();
    let directory = fixture.project_dir("one");
    store.add(0, project("alpha", directory.clone())).unwrap();

    let result = store.add(1, project("beta", directory.join("..").join("one")));

    assert!(matches!(result, Err(RegistryError::Validation(_))));
}

#[test]
fn given_stale_expected_revision_when_adding_then_conflict_reports_both_revisions() {
    let fixture = TestDir::new("stale");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("one")))
        .unwrap();

    let result = store.add(0, project("beta", fixture.project_dir("two")));
    let loaded = store.load().unwrap();

    assert!(
        matches!(
            result,
            Err(RegistryError::Conflict {
                expected: 0,
                actual: 1
            })
        ) && loaded.revision == 1
            && loaded.projects.len() == 1
            && loaded.projects[0].name == "alpha"
    );
}

#[test]
fn given_missing_name_when_removed_then_not_found_is_reported() {
    let fixture = TestDir::new("remove-missing");
    let result = fixture.store().remove(0, "missing");

    assert!(matches!(result, Err(RegistryError::NotFound(name)) if name == "missing"));
}

#[test]
fn given_existing_name_when_removed_then_revision_and_removal_are_persisted() {
    let fixture = TestDir::new("remove");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("alpha")))
        .unwrap();

    let returned = store.remove(1, "alpha").unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(
        (
            returned.revision,
            returned.projects.len(),
            loaded.revision,
            loaded.projects.len()
        ),
        (2, 0, 2, 0)
    );
}

#[test]
fn given_batch_with_late_path_conflict_when_merged_then_no_project_is_persisted() {
    let fixture = TestDir::new("atomic-merge");
    let store = fixture.store();
    let shared = fixture.project_dir("shared");

    let _error = store
        .merge(
            0,
            vec![project("alpha", shared.clone()), project("beta", shared)],
        )
        .unwrap_err();

    let loaded = store.load().unwrap();

    assert_eq!((loaded.revision, loaded.projects.len()), (0, 0));
}

#[test]
fn given_out_of_range_revision_when_mutated_then_corrupt_is_reported_without_rewrite() {
    let fixture = TestDir::new("overflow");
    let store = fixture.store();
    fs::create_dir_all(store.root()).unwrap();
    let original = "version = 1\nrevision = 18446744073709551615\nprojects = []\n";
    fs::write(store.path(), original).unwrap();

    let result = store.merge(u64::MAX, vec![]);

    assert!(
        matches!(result, Err(RegistryError::Corrupt(_)))
            && fs::read_to_string(store.path()).unwrap() == original
    );
}

#[test]
fn given_two_mutations_at_same_revision_when_run_concurrently_then_one_wins_without_lost_update() {
    let fixture = TestDir::new("concurrent");
    let root = fixture.root.join("destination");
    let stores = [
        RegistryStore::new(root.clone()),
        RegistryStore::new(root.clone()),
    ];
    let barrier = Arc::new(Barrier::new(3));
    let projects = [
        project("alpha", fixture.project_dir("alpha")),
        project("beta", fixture.project_dir("beta")),
    ];
    let handles = stores
        .into_iter()
        .zip(projects)
        .map(|(store, project)| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.add(0, project)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();

    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();
    let successes = results.iter().filter(|result| result.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|result| matches!(result, Err(RegistryError::Conflict { .. })))
        .count();
    let loaded = RegistryStore::new(root).load().unwrap();

    assert_eq!(
        (successes, conflicts, loaded.revision, loaded.projects.len()),
        (1, 1, 1, 1)
    );
}

#[test]
fn given_repeated_exact_name_when_selected_then_project_appears_once() {
    let fixture = TestDir::new("repeated-selection");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("alpha")))
        .unwrap();
    let names = vec!["alpha".to_string(), "alpha".to_string()];

    let selected = store.select(&names, None).unwrap();

    assert_eq!(selected.len(), 1);
}

#[test]
fn given_mixed_case_path_substring_when_filtered_then_matching_project_is_selected() {
    let fixture = TestDir::new("path-filter");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("MiXeDPath")))
        .unwrap();

    let selected = store.select(&[], Some("mixedpath")).unwrap();

    assert_eq!(
        selected
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha"]
    );
}

#[test]
fn given_exact_name_and_nonmatching_filter_when_selected_then_intersection_is_empty() {
    let fixture = TestDir::new("selection-intersection");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("alpha")))
        .unwrap();
    let names = vec!["alpha".to_string()];

    let selected = store.select(&names, Some("beta")).unwrap();

    assert!(selected.is_empty());
}

#[test]
fn given_one_missing_exact_name_when_selected_then_not_found_names_it() {
    let fixture = TestDir::new("selection-missing");
    let store = fixture.store();
    store
        .add(0, project("alpha", fixture.project_dir("alpha")))
        .unwrap();
    let names = vec!["alpha".to_string(), "missing".to_string()];

    let result = store.select(&names, Some("alpha"));

    assert!(matches!(result, Err(RegistryError::NotFound(name)) if name == "missing"));
}
