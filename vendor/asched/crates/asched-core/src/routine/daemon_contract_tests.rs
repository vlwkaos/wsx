use super::*;
use crate::{Project, RegistryStore};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/daemon-contract-tests")
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
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn routine(name: &str) -> super::super::Routine {
    super::super::Routine {
        name: name.into(),
        trigger: Trigger::Cron("0 0 1 1 *".into()),
        command: vec!["/bin/echo".into(), "ok".into()],
        prompt: String::new(),
        enabled: true,
    }
}

fn project_actions() -> Vec<Action> {
    vec![
        Action::List,
        Action::Show { name: "one".into() },
        Action::Add {
            revision: 0,
            routine: routine("one"),
        },
        Action::Edit {
            revision: 0,
            old_name: "one".into(),
            routine: routine("two"),
        },
        Action::Delete {
            revision: 0,
            name: "one".into(),
        },
        Action::SetEnabled {
            revision: 0,
            name: "one".into(),
            enabled: false,
        },
        Action::Run { name: "one".into() },
        Action::Fire {
            kind: "test.changed".into(),
            payload: serde_json::json!({}),
            event_id: "delivery-1".into(),
        },
        Action::Cancel { name: "one".into() },
        Action::Logs { name: "one".into() },
    ]
}

#[test]
fn given_registry_excluding_canonical_project_when_project_actions_are_processed_then_each_is_rejected(
) {
    let fixture = TestDir::new("unregistered");
    let registered = fixture.project_dir("registered");
    let unregistered = fixture.project_dir("unregistered");
    RegistryStore::new(fixture.root.clone())
        .add(
            0,
            Project {
                name: "registered".into(),
                working_dir: registered,
            },
        )
        .unwrap();
    let state = Arc::new(DaemonState::default());

    let rejected = project_actions().into_iter().all(|action| {
        matches!(
            process(
                &fixture.root,
                Request::new(unregistered.clone(), action),
                &state
            ),
            Err(RoutineError::Validation(_))
        )
    });

    assert!(rejected);
}

#[test]
fn given_legacy_root_without_registry_when_list_is_processed_then_project_is_accepted() {
    let fixture = TestDir::new("legacy");
    let project = fixture.project_dir("legacy-project");

    let result = process(
        &fixture.root,
        Request::new(project, Action::List),
        &Arc::new(DaemonState::default()),
    );

    assert!(matches!(
        result,
        Ok(Response::Routines {
            revision: 0,
            routines
        }) if routines.is_empty() && !fixture.root.join("projects.toml").exists()
    ));
}

#[test]
fn given_unfinished_wsx_import_transaction_when_daemon_starts_then_startup_refuses_and_preserves_it(
) {
    let fixture = TestDir::new("unfinished-import");
    let transaction = fixture.root.join("migrations/wsx-import-v1.toml");
    fs::create_dir_all(transaction.parent().unwrap()).unwrap();
    fs::write(
        &transaction,
        "version = 1\nregistry_revision = 0\nprojects_before = 0\nroutines_imported = 0\nprojects = []\nfiles = []\n",
    )
    .unwrap();

    let result = setup(&fixture.root);
    let refused = match result {
        Err(RoutineError::Unavailable(_) | RoutineError::Corrupt(_)) => true,
        Ok((lock, socket, listener)) => {
            drop(listener);
            drop(lock);
            let _ = fs::remove_file(socket);
            false
        }
        Err(_) => false,
    };

    assert!(refused && transaction.exists());
}

#[test]
fn given_admitted_tick_after_registry_snapshot_when_project_is_removed_then_removal_waits_for_run_registration(
) {
    let fixture = TestDir::new("tick-removal");
    let project = fixture.project_dir("project");
    let store = RoutineStore::new(fixture.root.clone(), &project).unwrap();
    store
        .save(
            ProjectRoutines {
                version: SCHEMA_VERSION,
                revision: 0,
                project_path: project.clone(),
                routines: vec![super::super::Routine {
                    name: "once".into(),
                    trigger: Trigger::Cron("* * * * *".into()),
                    command: vec!["/bin/true".into()],
                    prompt: String::new(),
                    enabled: true,
                }],
            },
            0,
        )
        .unwrap();
    RegistryStore::new(fixture.root.clone())
        .add(
            0,
            Project {
                name: "project".into(),
                working_dir: project.clone(),
            },
        )
        .unwrap();
    let state = Arc::new(DaemonState::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *TICK_ADMISSION_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some((entered_tx, release_rx));
    let tick_root = fixture.root.clone();
    let tick_state = Arc::clone(&state);
    let tick_worker = std::thread::spawn(move || tick(&tick_root, now_epoch() / 60, &tick_state));
    let entered = entered_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let removal_store = RegistryStore::new(fixture.root.clone());
    let (removed_tx, removed_rx) = std::sync::mpsc::channel();
    let removal_worker = std::thread::spawn(move || {
        let _ = removed_tx.send(removal_store.remove(1, "project"));
    });
    let blocked = matches!(
        removed_rx.recv_timeout(Duration::from_millis(100)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );

    let _ = release_tx.send(());
    let tick_joined = tick_worker.join().is_ok();
    let removed = removed_rx.recv_timeout(Duration::from_secs(2)).ok();
    let removal_joined = removal_worker.join().is_ok();
    drain_workers(&state);
    let claimed = store.load_runtime().unwrap().claims.contains_key("once");

    assert!(
        entered
            && blocked
            && tick_joined
            && matches!(removed, Some(Ok(registry)) if registry.projects.is_empty())
            && removal_joined
            && claimed
    );
}
