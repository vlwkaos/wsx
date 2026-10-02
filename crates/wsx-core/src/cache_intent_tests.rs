use super::*;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    process::{Child, Command},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join(".work/cache-intent")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("workspace-v3.toml")
    }
    fn legacy(&self) -> PathBuf {
        self.0.join("workspace.toml")
    }
    fn read(&self) -> WorkspaceCache {
        WorkspaceCache::load_from_paths(&self.path(), &self.legacy()).unwrap()
    }
    fn save(&self, changes: &WorkspaceCacheChanges) -> anyhow::Result<()> {
        changes
            .save_to(&self.path(), &self.legacy(), true)
            .map(|_| ())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if std::thread::panicking() {
                eprintln!("cache fixture cleanup failed: {error}");
            } else {
                panic!("cache fixture cleanup failed: {error}");
            }
        }
    }
}

struct Writers(Vec<Child>);
impl Drop for Writers {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn project_change(touch: u64, expanded: bool) -> WorkspaceCacheChanges {
    WorkspaceCacheChanges {
        projects: HashMap::from([(
            PathBuf::from("/project"),
            ProjectCacheChange {
                observed_touch_unix_ms: touch,
                touched_unix_ms: Some(touch),
                expanded: Some(expanded),
                stale: Some(false),
                // Flat-policy interaction explicitly retires the old adaptive metadata.
                adaptive: Some(None),
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

#[test]
fn stale_decision_and_unrelated_commands_cannot_undo_a_newer_interaction() {
    let fixture = Fixture::new();
    fixture.save(&project_change(10, true)).unwrap();
    let old_collapse = WorkspaceCacheChanges {
        projects: HashMap::from([(
            PathBuf::from("/project"),
            ProjectCacheChange {
                observed_touch_unix_ms: 10,
                expanded: Some(false),
                stale: Some(true),
                adaptive: Some(Some(AdaptiveCollapseState::new(24, 10).into())),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    fixture.save(&old_collapse).unwrap();
    assert!(fixture.read().stale_collapsed_projects.contains("/project"));
    fixture.save(&project_change(20, true)).unwrap();
    fixture.save(&old_collapse).unwrap();
    let mut unrelated = WorkspaceCacheChanges::default();
    unrelated
        .worktree_expanded
        .insert("/other/worktree".into(), false);
    unrelated.muted_terminals.insert("42".into(), true);
    fixture.save(&unrelated).unwrap();
    fixture.save(&WorkspaceCacheChanges::default()).unwrap();
    let cache = fixture.read();
    assert_eq!(cache.project_expanded.get("/project"), Some(&true));
    assert_eq!(cache.project_touched_unix_ms.get("/project"), Some(&20));
    assert!(!cache.stale_collapsed_projects.contains("/project"));
    assert!(!cache.adaptive_collapse.contains_key("/project"));
    assert_eq!(cache.worktree_expanded.get("/other/worktree"), Some(&false));
    assert!(cache.muted_terminals.contains("42"));
    // The same automatic decision is legitimate when based on the current interaction.
    let mut legitimate = old_collapse;
    legitimate
        .projects
        .get_mut(Path::new("/project"))
        .unwrap()
        .observed_touch_unix_ms = 20;
    fixture.save(&legitimate).unwrap();
    assert!(fixture.read().stale_collapsed_projects.contains("/project"));
    fixture.save(&project_change(30, false)).unwrap();
    assert!(!fixture.read().stale_collapsed_projects.contains("/project"));
    assert_eq!(
        fixture.read().project_expanded.get("/project"),
        Some(&false)
    );
}

#[test]
fn fresh_activity_from_an_old_window_preserves_earned_adaptive_credit() {
    let fixture = Fixture::new();
    let day = 24 * MILLIS_PER_HOUR;
    let first = 5 * day;
    let initial = AdaptiveCollapseState::new(24, first);
    let mut latest = initial;
    for activity in [first, first + day, first + 2 * day] {
        latest.observe(24, activity);
        let mut change = project_change(activity, true);
        change
            .projects
            .get_mut(Path::new("/project"))
            .unwrap()
            .adaptive = Some(Some(latest.into()));
        fixture.save(&change).unwrap();
    }
    assert_eq!(
        fixture.read().adaptive_collapse["/project"].window_hours,
        48
    );
    let mut old_view = initial;
    old_view.observe(24, first + 2 * day + 1);
    assert_eq!(old_view.window_hours, 24);
    let mut fresh = project_change(first + 2 * day + 1, true);
    fresh
        .projects
        .get_mut(Path::new("/project"))
        .unwrap()
        .adaptive = Some(Some(old_view.into()));
    let committed = fresh
        .save_to(&fixture.path(), &fixture.legacy(), true)
        .unwrap();
    assert_eq!(
        committed[Path::new("/project")]
            .adaptive
            .unwrap()
            .window_hours,
        48
    );
    assert_eq!(
        fixture.read().adaptive_collapse["/project"].window_hours,
        48
    );
}

#[test]
fn an_old_window_cannot_collapse_an_unexpired_durable_adaptive_window() {
    let fixture = Fixture::new();
    let activity = now_unix_ms() - 40 * MILLIS_PER_HOUR;
    let mut durable = AdaptiveCollapseState::new(24, activity);
    durable.window_hours = 48;
    let mut seed = WorkspaceCache {
        project_touched_unix_ms: HashMap::from([("/project".into(), activity)]),
        project_expanded: HashMap::from([("/project".into(), true)]),
        adaptive_collapse: HashMap::from([("/project".into(), durable)]),
        ..Default::default()
    };
    seed.save_to(&fixture.path(), true).unwrap();
    let mut automatic = WorkspaceCacheChanges {
        projects: HashMap::from([(
            PathBuf::from("/project"),
            ProjectCacheChange {
                observed_touch_unix_ms: activity,
                expanded: Some(false),
                stale: Some(true),
                adaptive: Some(Some(AdaptiveCollapseState::new(24, activity).into())),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let committed = automatic
        .save_to(&fixture.path(), &fixture.legacy(), true)
        .unwrap();
    let state = &committed[Path::new("/project")];
    assert_eq!(state.expanded, Some(true));
    assert!(!state.stale);
    assert_eq!(state.adaptive, Some(durable));
    assert_eq!(fixture.read().adaptive_collapse["/project"], durable);
    // A genuinely expired current window still collapses and resets to the configured base.
    let expired = now_unix_ms() - 49 * MILLIS_PER_HOUR;
    seed.project_touched_unix_ms
        .insert("/project".into(), expired);
    seed.adaptive_collapse
        .get_mut("/project")
        .unwrap()
        .last_activity_unix_ms = expired;
    seed.adaptive_collapse
        .get_mut("/project")
        .unwrap()
        .last_credit_unix_ms = expired;
    seed.save_to(&fixture.path(), true).unwrap();
    let change = automatic.projects.get_mut(Path::new("/project")).unwrap();
    change.observed_touch_unix_ms = expired;
    change.adaptive = Some(Some(AdaptiveCollapseState::new(24, expired).into()));
    fixture.save(&automatic).unwrap();
    let cache = fixture.read();
    assert!(!cache.project_expanded["/project"]);
    assert!(cache.stale_collapsed_projects.contains("/project"));
    assert_eq!(cache.adaptive_collapse["/project"].window_hours, 24);
}

#[test]
fn removals_and_revision_acknowledgements_preserve_unrelated_intent() {
    let fixture = Fixture::new();
    let mut first = WorkspaceCacheChanges::default();
    first
        .muted_terminals
        .extend([("one".into(), true), ("two".into(), true)]);
    first.acknowledged_outcomes.insert("one".into(), 8);
    first
        .dismissed_integration_prompts
        .insert(crate::integration::IntegrationTarget::Pi, true);
    fixture.save(&first).unwrap();
    let mut second = WorkspaceCacheChanges::default();
    second.muted_terminals.insert("one".into(), false);
    second.acknowledged_outcomes.insert("one".into(), 3);
    second
        .dismissed_integration_prompts
        .insert(crate::integration::IntegrationTarget::Pi, false);
    fixture.save(&second).unwrap();
    let cache = fixture.read();
    assert_eq!(cache.muted_terminals, HashSet::from(["two".into()]));
    assert_eq!(cache.acknowledged_outcomes.get("one"), Some(&8));
    assert!(cache.dismissed_integration_prompts.is_empty());
    // Discovery with no live sessions must not discard durable mute or worktree state.
    let mut workspace = WorkspaceState::empty();
    let (_, _, _, _, _, muted, acknowledged, _, _) =
        apply_workspace_cache(&mut workspace, cache, |_| Ok(())).unwrap();
    assert_eq!(muted, HashSet::from(["two".into()]));
    assert_eq!(acknowledged.get("one"), Some(&8));
}

#[test]
fn failed_read_and_busy_lock_retain_commands_for_retry_without_file_replacement() {
    let fixture = Fixture::new();
    fs::write(fixture.path(), "broken = [").unwrap();
    let changes = project_change(1, true);
    assert!(fixture.save(&changes).is_err());
    assert_eq!(fs::read_to_string(fixture.path()).unwrap(), "broken = [");
    fs::remove_file(fixture.path()).unwrap();
    let lock = CacheLock::acquire(&fixture.path()).unwrap();
    let started = Instant::now();
    assert!(fixture.save(&changes).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!fixture.path().exists());
    drop(lock);
    fixture.save(&changes).unwrap();
    assert_eq!(fixture.read().project_expanded.get("/project"), Some(&true));
    let inode = fs::metadata(fixture.path()).unwrap().ino();
    fixture.save(&changes).unwrap();
    assert_eq!(
        fs::metadata(fixture.path()).unwrap().ino(),
        inode,
        "idempotent commands must not republish the file"
    );
    let before = fs::read(fixture.path()).unwrap();
    fixture.save(&WorkspaceCacheChanges::default()).unwrap();
    assert_eq!(
        fs::read(fixture.path()).unwrap(),
        before,
        "quit must not publish a snapshot"
    );
}

#[test]
fn unsafe_lock_files_fail_closed_without_touching_the_target() {
    let fixture = Fixture::new();
    let target = fixture.0.join("preserved");
    fs::write(&target, "preserve me").unwrap();
    let lock = fixture.path().with_extension("lock");
    symlink(&target, &lock).unwrap();
    assert!(fixture.save(&project_change(1, true)).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserve me");
    fs::remove_file(&lock).unwrap();
    fs::write(&lock, "").unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.save(&project_change(1, true)).is_err());
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
    fixture.save(&project_change(1, true)).unwrap();
    assert_eq!(
        fs::metadata(fixture.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn concurrent_processes_preserve_disjoint_commands() {
    let fixture = Fixture::new();
    fixture.save(&project_change(50, false)).unwrap();
    let mut children = Writers(Vec::new());
    for id in 0..6 {
        children.0.push(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "cache::intent_tests::cache_process_writer",
                    "--ignored",
                    "--exact",
                ])
                .env("WSX_CACHE_TEST_PATH", fixture.path())
                .env("WSX_CACHE_TEST_ID", id.to_string())
                .stdout(std::process::Stdio::inherit())
                .spawn()
                .unwrap(),
        );
    }
    // ^ Reap every writer before asserting so failure cannot race fixture cleanup.
    let statuses = children.0.iter_mut().map(Child::wait).collect::<Vec<_>>();
    for (id, status) in statuses.into_iter().enumerate() {
        assert!(status.unwrap().success(), "cache writer {id} failed");
    }
    let cache = fixture.read();
    for id in 0..6 {
        assert_eq!(
            cache.worktree_expanded.get(&format!("/worktree/{id}")),
            Some(&false)
        );
        assert!(cache.muted_terminals.contains(&format!("terminal-{id}")));
    }
    assert_eq!(cache.project_expanded.get("/project"), Some(&false));
    assert_eq!(cache.project_touched_unix_ms.get("/project"), Some(&50));
}

#[test]
#[ignore = "child process helper exercised by concurrent_processes_preserve_disjoint_commands"]
fn cache_process_writer() {
    let path = PathBuf::from(std::env::var_os("WSX_CACHE_TEST_PATH").unwrap());
    let id = std::env::var("WSX_CACHE_TEST_ID").unwrap();
    let mut changes = WorkspaceCacheChanges::default();
    changes
        .worktree_expanded
        .insert(format!("/worktree/{id}").into(), false);
    changes
        .muted_terminals
        .insert(format!("terminal-{id}"), true);
    // ^ Match the app's retained-intent retry contract, not an unbounded lock acquisition.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match changes.save_to(&path, &path.with_file_name("workspace.toml"), true) {
            Ok(_) => return,
            Err(error)
                if error.to_string() == "workspace cache is busy; retry pending intent"
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cache writer failed: {error:#}"),
        }
    }
}
