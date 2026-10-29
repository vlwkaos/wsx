use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn sibling_session_indices_wrap_and_reject_missing_siblings() {
    assert_eq!(cyclic_sibling_index(0, 3, 1), Some(1));
    assert_eq!(cyclic_sibling_index(2, 3, 1), Some(0));
    assert_eq!(cyclic_sibling_index(0, 3, -1), Some(2));
    assert_eq!(cyclic_sibling_index(1, 3, -1), Some(0));
    assert_eq!(cyclic_sibling_index(0, 1, 1), None);
    assert_eq!(cyclic_sibling_index(1, 1, 1), None);
}

#[test]
fn search_feedback_tracks_case_insensitive_empty_and_absent_queries() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("feature-a"),
            make_project("other"),
            make_project("feature-b"),
        ],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    for (query, expected) in [("FEAT", vec![0, 2]), ("", vec![]), ("xyz", vec![])] {
        app.mode = Mode::Search {
            query: query.into(),
            match_idx: 0,
        };
        app.search_apply();
        assert_eq!(app.search_matches(query), expected);
        assert_eq!(app.search_total(), expected.len());
        assert_eq!(
            app.search_counts().unwrap().iter().sum::<usize>(),
            expected.len()
        );
    }
    app.mode = Mode::Workspace;
    assert!(app.search_counts().is_none());
}

#[test]
fn search_counts_hidden_descendants_at_visible_ancestors_and_excludes_other_groups() {
    let mut project = make_project("alpha");
    let mut worktree = make_worktree("/fixture/alpha");
    worktree.branch = "needle-main".into();
    worktree.expanded = false;
    worktree.sessions = (1..=2)
        .map(|id| {
            let mut session = make_sess_with_id(id, runtime::AgentState::Idle);
            session.display_name = format!("needle-{id}");
            session
        })
        .collect();
    project.worktrees.push(worktree);
    let workspace = WorkspaceState {
        projects: vec![project, make_project("needle-unrelated")],
    };
    let filtered = vec![
        FlatEntry::Project { idx: 0 },
        FlatEntry::Worktree {
            project_idx: 0,
            worktree_idx: 0,
        },
    ];
    let mut index = SearchIndex::new(&workspace, &filtered);
    index.update("NEEDLE");
    assert_eq!(index.total, 3);
    assert_eq!(index.counts, vec![0, 3]);
    assert_eq!(index.matches("NEEDLE"), vec![1]);
    assert!(!workspace.projects[0].worktrees[0].expanded);
    let mut folded = SearchIndex::new(&workspace, &[FlatEntry::Project { idx: 0 }]);
    folded.update("needle");
    assert_eq!(folded.counts, vec![3]);
    folded.update("");
    assert_eq!(folded.total, 0);
    assert_eq!(folded.counts, vec![0]);
}

#[test]
fn enter_finishes_search_when_only_one_result_remains() {
    let workspace = WorkspaceState {
        projects: vec![make_project("alpha"), make_project("beta")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.mode = Mode::Search {
        query: "alpha".into(),
        match_idx: 0,
    };
    app.search_apply();
    let selected = app.tree_selected;

    app.search_advance();

    assert!(matches!(app.mode, Mode::Workspace));
    assert_eq!(app.tree_selected, selected);
}

#[test]
fn enter_keeps_search_open_and_advances_when_multiple_results_remain() {
    let workspace = WorkspaceState {
        projects: vec![make_project("alpha-one"), make_project("alpha-two")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.mode = Mode::Search {
        query: "alpha".into(),
        match_idx: 0,
    };
    app.search_apply();
    let first = app.tree_selected;

    app.search_advance();

    assert!(matches!(app.mode, Mode::Search { match_idx: 1, .. }));
    assert_ne!(app.tree_selected, first);
}

fn make_project(name: &str) -> wsx_core::model::workspace::Project {
    wsx_core::model::workspace::Project {
        name: name.to_string(),
        path: std::path::PathBuf::from(format!("/tmp/{name}")),
        default_branch: "main".to_string(),
        last_agent_active_unix_ms: None,
        last_terminal_active_unix_ms: None,
        worktrees: vec![],
        routines: Vec::new(),
        routine_revision: 0,
        routines_expanded: true,
        config: None,
        expanded: true,
        missing: false,
    }
}

fn make_worktree(path: &str) -> wsx_core::model::workspace::WorktreeInfo {
    wsx_core::model::workspace::WorktreeInfo {
        name: "main".to_string(),
        branch: "main".to_string(),
        path: std::path::PathBuf::from(path),
        is_main: true,
        alias: None,
        sessions: vec![],
        expanded: true,
        git_info: None,
        fetch_failed: false,
        fetch_fail_count: 0,
        fetch_fail_reason: None,
        last_fetched: None,
        git_info_fetched_at: None,
    }
}

fn test_terminal_frame(
    pane_id: runtime::PaneId,
    terminal_id: runtime::TerminalId,
    revision: u64,
    rows: u16,
    cols: u16,
) -> runtime::TerminalFrame {
    runtime::TerminalFrame {
        pane_id,
        terminal_id,
        revision,
        rows,
        cols,
        cells: vec![runtime::Cell::default(); usize::from(rows) * usize::from(cols)],
        cursor: runtime::Cursor {
            x: 0,
            y: rows.saturating_sub(1),
            visible: true,
            blinking: false,
            shape: 0,
        },
        selection: Vec::new(),
    }
}

fn terminal_stream_listener(_name: &str) -> (PathBuf, UnixListener) {
    let directory = std::env::current_dir().unwrap().join(".work/s");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!(
        "{:x}-{:x}",
        std::process::id(),
        runtime::new_client_id()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (path, listener)
}

fn read_runtime_request(reader: &mut impl BufRead) -> runtime::Request {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn read_terminal_client_message(reader: &mut impl BufRead) -> runtime::TerminalClientMessage {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn write_runtime_message(stream: &mut impl Write, message: &impl serde::Serialize) {
    stream
        .write_all(&runtime::encode_line(message).unwrap())
        .unwrap();
    stream.flush().unwrap();
}

fn make_git_info() -> GitInfo {
    GitInfo {
        recent_commits: vec![],
        modified_files: vec![],
        submodules: Some(vec![]),
        subtrees: vec![],
        ahead: 1,
        behind: 0,
        remote_branch: Some("origin/main".to_string()),
    }
}

fn test_runtime_client() -> runtime::Client {
    runtime::Client::new(
        std::env::current_dir()
            .unwrap()
            .join(".work/tests/nonexistent-runtime.sock"),
    )
}

// ^ docs/ui-state-ownership.md "Test isolation": each App gets a fresh cache file named after its
// test, so no test reads another test's intent or writes the user's cache.
fn test_cache_path() -> PathBuf {
    thread_local!(static NEXT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });
    let index = NEXT.with(|next| next.replace(next.get() + 1));
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "-");
    let path = std::env::current_dir()
        .unwrap()
        .join(".work/tests/workspace-cache")
        .join(format!("{test}-{index}.toml"));
    let _ = std::fs::remove_file(&path);
    path
}

fn make_test_app(
    config: GlobalConfig,
    workspace: WorkspaceState,
    active_group: Option<String>,
) -> App {
    let active_group = active_group
        .map(GroupKey::Named)
        .unwrap_or(GroupKey::Ungrouped);
    let visible_projects = compute_visible_projects(&config, &workspace, Some(&active_group), None);
    let group_header_scroll = config
        .ordered_group_keys()
        .iter()
        .position(|candidate| candidate == &active_group)
        .unwrap_or(0);
    let cached_flat = flatten_tree_filtered(&workspace, &visible_projects);
    let search_index = None;
    let worktree_index = build_worktree_index(&workspace);
    let (bg_tx, bg_rx) = std::sync::mpsc::channel();
    let terminal_input_tx = spawn_terminal_input_worker(test_runtime_client(), bg_tx.clone());
    let (git_local_tx, git_local_rx) = std::sync::mpsc::channel();
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
    let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
    let (_runtime_event_tx, runtime_event_rx) = std::sync::mpsc::channel();
    let (_update_tx, update_rx) = std::sync::mpsc::channel();
    let (routine_tx, routine_rx) = std::sync::mpsc::channel();
    let (_integration_scan_tx, integration_scan_rx) = std::sync::mpsc::channel();
    let terminal_escape_chord = EscapeSequence::parse(&config.terminal_escape_chord).unwrap();

    App {
        workspace,
        tree_selected: 0,
        tree_scroll: 0,
        tree_visible_height: 20,
        tree_scroll_manual: false,
        tree_area: Rect::default(),
        preview_area: Rect::default(),
        terminal_area: Rect::default(),
        plugin_area: Rect::default(),
        mode: Mode::Workspace,
        config,
        active_group,
        active_profile: None,
        group_header_scroll,
        group_header_area: Rect::default(),
        visible_projects,
        project_touched_unix_ms: HashMap::new(),
        stale_collapsed_projects: HashSet::new(),
        adaptive_collapse: HashMap::new(),
        review: None,
        review_available: false,
        notice: None,
        notice_started: None,
        notice_queue: VecDeque::new(),
        jobs: Vec::new(),
        next_job_id: 0,
        spinner_frame: 0,
        terminal_input_tx,
        bg_tx,
        bg_rx,
        needs_redraw: false,
        should_quit: false,
        force_terminal_redraw: false,
        reassert_terminal_modes: false,
        last_terminal_event: Instant::now(),
        force_preview_redraw: false,
        last_rendered_preview_was_session: false,
        fast_timer: Timer::new(FAST_INTERVAL_MS),
        git_sweep_timer: Timer::new(GIT_SWEEP_INTERVAL_MS),
        slow_timer: Timer::new(SLOW_INTERVAL_MS),
        cached_flat,
        flat_dirty: false,
        search_index,
        git_local_tx,
        git_local_rx,
        git_local_pending: HashSet::new(),
        fetch_tx,
        fetch_rx,
        fetch_pending: HashSet::new(),
        cache_dirty: false,
        cache_path: test_cache_path(),
        cache_changes: Default::default(),
        cache_cursor: (0, None),
        cached_worktree_expansion: HashMap::new(),
        git_semaphore: GitSemaphore::new(1),
        worktree_index,
        runtime_client: test_runtime_client(),
        _runtime_monitor: None,
        runtime_event_rx,
        runtime_health: RuntimeHealth::Connecting,
        runtime_tx,
        runtime_rx,
        runtime_refresh_pending: false,
        startup_availability_pending: true,
        startup_cursor_identity: None,
        runtime_refresh_stale: false,
        runtime_full_refresh_stale: false,
        runtime_capture_pending: false,
        plugin_sidecars: Vec::new(),
        plugin_view: None,
        plugin_view_generation: 0,
        plugin_view_pending: false,
        plugin_view_last_refresh: None,
        plugin_view_last_owner: None,
        pending_deletions: HashSet::new(),
        pending_session_kills: HashSet::new(),
        pending_session_orders: HashMap::new(),
        muted_terminal_ids: HashSet::new(),
        acknowledged_outcomes: HashMap::new(),
        terminal_controller_id: runtime::new_client_id(),
        terminal_surfaces: TerminalSurfaces::default(),
        terminal_stream: None,
        terminal_stream_generation: 0,
        pending_terminal_entry: None,
        pending_terminal_resume: None,
        pending_shell: None,
        last_stream_reattach: None,
        installed: None,
        follow_check_pending: false,
        follow_target: None,
        follow_resume_pane: None,
        upgrade: None,
        suspend_detector: SuspendDetector::new(),
        applied_revision: None,
        terminal_escape_chord,
        terminal_sidebar_override: None,
        update_rx,
        update_available: None,
        is_mobile: false,
        force_mobile: false,
        scanned_repos: Vec::new(),
        repo_scan_rx: None,
        routine_tx,
        routine_rx,
        routine_refresh_generation: HashMap::new(),
        integration_scan_rx,
        integration_metadata: Vec::new(),
        pending_integration_demand: None,
        pending_integration_updates: Vec::new(),
        dismissed_integration_prompts: HashSet::new(),
        persist_group_selection: false,
    }
}

#[test]
fn terminal_sidecar_is_width_gated_and_rejects_stale_views() {
    let pane_id = runtime::PaneId(11);
    let terminal_id = runtime::TerminalId(12);
    let session = wsx_core::model::workspace::SessionInfo {
        session_id: runtime::SessionId(10),
        pane_id,
        terminal_id,
        agent: None,
        display_name: "session".into(),
        agent_status: runtime::AgentState::Idle,
        revision: 1,
        layout: runtime::PaneLayout::Leaf { pane_id },
        panes: vec![wsx_core::model::workspace::PaneInfo {
            pane_id,
            terminal_id,
            label: "terminal".into(),
            agent: None,
            agent_status: runtime::AgentState::Idle,
            revision: 1,
            exited: false,
            listening_ports: Vec::new(),
            foreground_job: false,
            outcome_acknowledged: false,
        }],
        muted: false,
        outcome_acknowledged: false,
    };
    let mut project = make_project("project");
    let mut worktree = make_worktree("/repo");
    worktree.sessions.push(session);
    project.worktrees.push(worktree);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.tree_selected = 2;
    app.mode = Mode::Terminal { pane_id };
    app.plugin_sidecars = vec![runtime::PluginSidecarDescriptor {
        plugin_id: "example".into(),
        title: "Example".into(),
        spec: runtime::PluginSidecarSpec {
            surface: runtime::PluginSurface::TerminalRight,
            priority: 0,
            minimum_columns: 120,
            preferred_width: 36,
            refresh_ms: 2_000,
        },
    }];
    assert!(app.terminal_sidecar_descriptor(119, 117).is_none());
    assert!(app.terminal_sidecar_descriptor(160, 158).is_some());

    app.plugin_view_generation = 4;
    app.terminal_surfaces
        .activate_stream(7, pane_id, terminal_id);
    app.terminal_surfaces
        .install_full(7, test_terminal_frame(pane_id, terminal_id, 1, 10, 80));
    let view = |epoch, generation| runtime::PluginSidecarView {
        plugin_id: "example".into(),
        title: "Example".into(),
        epoch,
        pane_id,
        worktree_id: runtime::WorktreeId(2),
        generation,
        payload: runtime::PluginViewPayload {
            empty: Some("clean".into()),
            rows: Vec::new(),
            remaining: 0,
        },
    };
    app.apply_plugin_view(4, "example", pane_id, Ok(view(6, 4)));
    assert!(
        app.plugin_view.is_none(),
        "old daemon epoch must be rejected"
    );
    app.apply_plugin_view(3, "example", pane_id, Ok(view(7, 3)));
    assert!(
        app.plugin_view.is_none(),
        "old request generation must be rejected"
    );
    app.apply_plugin_view(4, "example", pane_id, Ok(view(7, 4)));
    assert!(app.plugin_view.is_some());
}

fn make_project_entry(name: &str, group: Option<&str>) -> wsx_core::config::global::ProjectEntry {
    wsx_core::config::global::ProjectEntry {
        name: name.to_string(),
        path: std::path::PathBuf::from(format!("/tmp/{name}")),
        groups: group.into_iter().map(str::to_string).collect(),
        aliases: Default::default(),
        worktree_order: Vec::new(),
        profile: None,
    }
}

fn integration_metadata(
    target: wsx_core::integration::IntegrationTarget,
    available: bool,
    install_status: wsx_core::integration::InstallStatus,
) -> wsx_core::integration::IntegrationMetadata {
    wsx_core::integration::IntegrationMetadata {
        target,
        cli_value: target.cli_value(),
        label: target.label(),
        lifecycle: target.lifecycle(),
        available,
        compatible: true,
        compatibility_note: None,
        install_status,
        installed_version: None,
        expected_version: target.expected_version(),
    }
}

fn routine_view(name: &str) -> asched_core::routine::ipc::RoutineView {
    asched_core::routine::ipc::RoutineView {
        routine: asched_core::routine::Routine {
            name: name.into(),
            trigger: asched_core::routine::Trigger::Cron("0 9 * * *".into()),
            command: vec!["echo".into(), "{prompt}".into()],
            prompt: "hello".into(),
            enabled: true,
        },
        capabilities: asched_core::routine::Capabilities::for_running(false),
        next_run_epoch: None,
        latest_run: None,
        recent_runs: Vec::new(),
    }
}

#[test]
fn hard_quit_requires_confirmation() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);

    app.action_hard_quit();

    assert!(matches!(
        app.mode,
        Mode::Confirm {
            pending: PendingAction::ShutdownDaemon,
            ..
        }
    ));
    assert!(!app.should_quit);
}

#[test]
fn terminal_control_intent_transfers_only_for_explicit_entry() {
    assert!(TerminalControlIntent::Explicit.takeover());
    assert!(!TerminalControlIntent::Resume.takeover());
}

#[test]
fn adjacent_group_wraps_in_both_directions_and_rejects_a_single_group() {
    let groups = vec![
        GroupKey::Ungrouped,
        GroupKey::Named("work".into()),
        GroupKey::Named("personal".into()),
    ];

    assert_eq!(
        adjacent_group(&groups, &groups[2], 1),
        Some(groups[0].clone())
    );
    assert_eq!(
        adjacent_group(&groups, &groups[0], -1),
        Some(groups[2].clone())
    );
    assert_eq!(adjacent_group(&groups[..1], &groups[0], 1), None);
    assert_eq!(adjacent_group(&groups[..1], &groups[0], -1), None);
}

#[test]
fn default_ungrouped_filter_keeps_only_projects_without_memberships() {
    let config = GlobalConfig {
        groups: vec!["work".into()],
        projects: vec![
            make_project_entry("work-project", Some("work")),
            make_project_entry("ungrouped-project", None),
        ],
        ..GlobalConfig::default()
    };
    let workspace = WorkspaceState {
        projects: vec![
            make_project("work-project"),
            make_project("ungrouped-project"),
        ],
    };
    let active = GroupKey::Ungrouped;

    assert_eq!(
        compute_visible_projects(&config, &workspace, Some(&active), None),
        HashSet::from([1])
    );
}

#[test]
fn suspend_detector_requires_continuous_clock_to_outpace_active_clock() {
    let mut detector = SuspendDetector {
        previous: Some(LifecycleClockSample {
            active: Duration::from_secs(10),
            continuous: Duration::from_secs(20),
        }),
    };

    assert!(!detector.observe(LifecycleClockSample {
        active: Duration::from_secs(11),
        continuous: Duration::from_secs(21),
    }));
    assert!(detector.observe(LifecycleClockSample {
        active: Duration::from_secs(12),
        continuous: Duration::from_secs(24),
    }));
    assert!(!detector.observe(LifecycleClockSample {
        active: Duration::from_secs(13),
        continuous: Duration::from_secs(25),
    }));
}

#[test]
fn pending_terminal_entry_accepts_only_the_exact_full_baseline() {
    let pending = PendingTerminalEntry {
        pane_id: runtime::PaneId(3),
        terminal_id: runtime::TerminalId(4),
        generation: 5,
        rows: 6,
        cols: 7,
    };
    let exact = test_terminal_frame(runtime::PaneId(3), runtime::TerminalId(4), 9, 6, 7);

    assert!(pending.matches_frame(5, &exact));
    assert!(!pending.matches_frame(4, &exact));
    assert!(!pending.matches_frame(
        5,
        &test_terminal_frame(runtime::PaneId(8), runtime::TerminalId(4), 9, 6, 7,)
    ));
    assert!(!pending.matches_frame(
        5,
        &test_terminal_frame(runtime::PaneId(3), runtime::TerminalId(8), 9, 6, 7,)
    ));
    assert!(!pending.matches_frame(
        5,
        &test_terminal_frame(runtime::PaneId(3), runtime::TerminalId(4), 9, 5, 7,)
    ));
    assert!(!pending.matches_frame(
        5,
        &test_terminal_frame(runtime::PaneId(3), runtime::TerminalId(4), 9, 6, 8,)
    ));
}

#[test]
fn pending_terminal_entry_cancels_on_selection_change_and_suspend() {
    let mut project = make_project("cancel-entry");
    let mut worktree = make_worktree("/tmp/cancel-entry");
    worktree
        .sessions
        .push(make_sess(false, runtime::AgentState::Idle));
    project.worktrees.push(worktree);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let session_index = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { .. }))
        .unwrap();
    let pending = PendingTerminalEntry {
        pane_id: runtime::PaneId(1),
        terminal_id: runtime::TerminalId(1),
        generation: 1,
        rows: 5,
        cols: 20,
    };
    app.tree_selected = session_index;
    app.pending_terminal_entry = Some(pending);

    app.tree_selected = 0;
    app.cancel_pending_terminal_entry_if_stale();
    assert!(app.pending_terminal_entry.is_none());
    assert!(matches!(app.mode, Mode::Workspace));

    app.pending_terminal_entry = Some(pending);
    assert_eq!(app.prepare_terminal_resume(), None);
    assert!(app.pending_terminal_entry.is_none());
    assert!(app.terminal_stream.is_none());
    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn terminal_entry_waits_for_the_resized_full_baseline_before_switching_mode() {
    let pane_id = runtime::PaneId(1);
    let terminal_id = runtime::TerminalId(1);
    let mut project = make_project("baseline-entry");
    let mut worktree = make_worktree("/tmp/baseline-entry");
    worktree
        .sessions
        .push(make_sess(false, runtime::AgentState::Idle));
    project.worktrees.push(worktree);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let project_path = app.workspace.projects[0].path.clone();
    app.stale_collapsed_projects.insert(project_path.clone());
    app.tree_selected = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { .. }))
        .unwrap();
    app.terminal_surfaces
        .activate_stream(7, pane_id, terminal_id);
    assert_eq!(
        app.terminal_surfaces
            .install_full(7, test_terminal_frame(pane_id, terminal_id, 1, 2, 20)),
        SurfaceUpdate::Applied
    );

    let (socket_path, listener) = terminal_stream_listener("baseline-entry");
    app.runtime_client = runtime::Client::new(socket_path.clone());
    let (patch_tx, patch_rx) = mpsc::channel();
    let (resync_tx, resync_rx) = mpsc::channel();
    let (baseline_tx, baseline_rx) = mpsc::channel();
    let server_path = socket_path.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert_eq!(
            read_runtime_request(&mut reader),
            runtime::Request::Hello {
                protocol: runtime::PROTOCOL_VERSION,
            }
        );
        write_runtime_message(
            &mut stream,
            &runtime::Response::Hello {
                protocol: runtime::PROTOCOL_VERSION,
                epoch: 7,
                capabilities: runtime::Capabilities::default(),
            },
        );
        let (rows, cols) = match read_runtime_request(&mut reader) {
            runtime::Request::TerminalSubscribe {
                pane_id: requested,
                rows,
                cols,
                ..
            } => {
                assert_eq!(requested, pane_id);
                (rows, cols)
            }
            request => panic!("unexpected request: {request:?}"),
        };
        write_runtime_message(&mut stream, &runtime::Response::Ack { revision: 1 });
        patch_rx.recv().unwrap();
        write_runtime_message(
            &mut stream,
            &runtime::TerminalServerMessage::Update(runtime::TerminalUpdate::Patch {
                pane_id,
                terminal_id,
                base_revision: 1,
                revision: 2,
                cols,
                rows,
                changed_rows: Vec::new(),
                cursor: test_terminal_frame(pane_id, terminal_id, 2, rows, cols).cursor,
                selection: Vec::new(),
            }),
        );
        assert_eq!(
            read_terminal_client_message(&mut reader),
            runtime::TerminalClientMessage::Resync
        );
        resync_tx.send(()).unwrap();
        baseline_rx.recv().unwrap();
        write_runtime_message(
            &mut stream,
            &runtime::TerminalServerMessage::Update(runtime::TerminalUpdate::Full(
                test_terminal_frame(pane_id, terminal_id, 3, rows, cols),
            )),
        );
        std::thread::sleep(Duration::from_millis(25));
        drop(listener);
        let _ = std::fs::remove_file(server_path);
    });

    let mut terminal = workspace_terminal();
    app.attach_session(0, 0, 0, &mut terminal).unwrap();
    assert!(app.project_touched_unix_ms[&project_path] > 0);
    assert!(!app.stale_collapsed_projects.contains(&project_path));
    assert!(matches!(app.mode, Mode::Workspace));
    assert!(app.pending_terminal_entry.is_some());
    assert_eq!(app.terminal_cursor(), None);

    patch_tx.send(()).unwrap();
    let resync_deadline = Instant::now() + Duration::from_secs(1);
    while resync_rx.try_recv().is_err() && Instant::now() < resync_deadline {
        app.drain_terminal_stream();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(app.mode, Mode::Workspace));
    assert!(app.pending_terminal_entry.is_some());

    baseline_tx.send(()).unwrap();
    // ^ This checks eventual delivery, not latency; loaded CI runners can exceed one second.
    let baseline_deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(app.mode, Mode::Terminal { .. }) && Instant::now() < baseline_deadline {
        app.drain_terminal_stream();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(app.mode, Mode::Terminal { pane_id: id } if id == pane_id));
    assert!(app.pending_terminal_entry.is_none());
    let frame = app.terminal_surface(pane_id, terminal_id).unwrap();
    let (rows, cols) = app.terminal_pane_size(&terminal);
    assert_eq!((frame.rows, frame.cols), (rows, cols));

    app.terminal_stream = None;
    server.join().unwrap();
}

#[test]
fn terminal_sidebar_toggle_is_process_local_and_resizes_the_active_stream() {
    let pane_id = runtime::PaneId(1);
    let terminal_id = runtime::TerminalId(1);
    let mut project = make_project("sidebar-toggle");
    let mut worktree = make_worktree("/tmp/sidebar-toggle");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Idle)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.tree_selected = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { .. }))
        .unwrap();

    let (socket_path, listener) = terminal_stream_listener("sidebar-toggle");
    app.runtime_client = runtime::Client::new(socket_path.clone());
    let server_path = socket_path.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert_eq!(
            read_runtime_request(&mut reader),
            runtime::Request::Hello {
                protocol: runtime::PROTOCOL_VERSION,
            }
        );
        write_runtime_message(
            &mut stream,
            &runtime::Response::Hello {
                protocol: runtime::PROTOCOL_VERSION,
                epoch: 7,
                capabilities: runtime::Capabilities::default(),
            },
        );
        let (rows, cols) = match read_runtime_request(&mut reader) {
            runtime::Request::TerminalSubscribe {
                pane_id: requested,
                rows,
                cols,
                ..
            } => {
                assert_eq!(requested, pane_id);
                (rows, cols)
            }
            request => panic!("unexpected request: {request:?}"),
        };
        write_runtime_message(&mut stream, &runtime::Response::Ack { revision: 1 });
        write_runtime_message(
            &mut stream,
            &runtime::TerminalServerMessage::Update(runtime::TerminalUpdate::Full(
                test_terminal_frame(pane_id, terminal_id, 1, rows, cols),
            )),
        );
        let prefix_expanded = read_terminal_client_message(&mut reader);
        let prefix_compact = read_terminal_client_message(&mut reader);
        let command_prefix_expanded = read_terminal_client_message(&mut reader);
        let toggled_expanded = read_terminal_client_message(&mut reader);
        let toggled_compact = read_terminal_client_message(&mut reader);
        drop(listener);
        let _ = std::fs::remove_file(server_path);
        (
            (rows, cols),
            prefix_expanded,
            prefix_compact,
            command_prefix_expanded,
            toggled_expanded,
            toggled_compact,
        )
    });
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 12)),
        },
    )
    .unwrap();

    app.enter_terminal(pane_id, terminal_id, "sidebar toggle".into(), &terminal)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !matches!(app.mode, Mode::Terminal { .. }) && Instant::now() < deadline {
        app.drain_terminal_stream();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(app.mode, Mode::Terminal { .. }));

    let prefix = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('a'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let unknown_suffix = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    );
    let compact_before_prefix = Some(app.effective_terminal_sidebar());
    assert_eq!(
        app.terminal_escape_chord.terminal_key(prefix),
        crate::event::TerminalEscapeAction::Pending
    );
    app.sync_terminal_sidebar_geometry(compact_before_prefix, &terminal);
    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Expanded);
    let expanded_before_suffix = Some(app.effective_terminal_sidebar());
    assert!(matches!(
        app.terminal_escape_chord.terminal_key(unknown_suffix),
        crate::event::TerminalEscapeAction::Forward(_)
    ));
    app.sync_terminal_sidebar_geometry(expanded_before_suffix, &terminal);
    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Compact);

    let compact_before_command = Some(app.effective_terminal_sidebar());
    assert_eq!(
        app.terminal_escape_chord.terminal_key(prefix),
        crate::event::TerminalEscapeAction::Pending
    );
    app.sync_terminal_sidebar_geometry(compact_before_command, &terminal);
    let toggle = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(
        app.terminal_escape_chord.terminal_key(toggle),
        crate::event::TerminalEscapeAction::ToggleSidebar
    );
    app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
        .unwrap();
    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Expanded);

    assert_eq!(
        app.terminal_escape_chord.terminal_key(prefix),
        crate::event::TerminalEscapeAction::Pending
    );
    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Expanded);
    assert_eq!(
        app.terminal_escape_chord.terminal_key(toggle),
        crate::event::TerminalEscapeAction::ToggleSidebar
    );
    app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
        .unwrap();
    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Compact);
    assert_eq!(app.config.terminal_sidebar, TerminalSidebar::Compact);

    let (
        initial,
        prefix_expanded,
        prefix_compact,
        command_prefix_expanded,
        toggled_expanded,
        toggled_compact,
    ) = server.join().unwrap();
    app.terminal_stream = None;
    let expanded_resize = runtime::TerminalClientMessage::Resize {
        rows: initial.0,
        cols: initial.1.saturating_sub(30),
    };
    let compact_resize = runtime::TerminalClientMessage::Resize {
        rows: initial.0,
        cols: initial.1,
    };
    assert_eq!(prefix_expanded, expanded_resize);
    assert_eq!(prefix_compact, compact_resize);
    assert_eq!(command_prefix_expanded, expanded_resize);
    assert_eq!(toggled_expanded, expanded_resize);
    assert_eq!(toggled_compact, compact_resize);
}

#[test]
fn prefix_sidebar_projection_respects_config_baseline_and_mobile_mode() {
    let prefix = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('a'),
        crossterm::event::KeyModifiers::CONTROL,
    );

    for (enabled, baseline, mobile, expected) in [
        (
            true,
            TerminalSidebar::Compact,
            false,
            TerminalSidebar::Expanded,
        ),
        (
            false,
            TerminalSidebar::Compact,
            false,
            TerminalSidebar::Compact,
        ),
        (
            true,
            TerminalSidebar::Expanded,
            false,
            TerminalSidebar::Expanded,
        ),
        (
            true,
            TerminalSidebar::Compact,
            true,
            TerminalSidebar::Compact,
        ),
    ] {
        let mut app = make_test_app(
            GlobalConfig {
                terminal_prefix_shows_sidebar: enabled,
                terminal_sidebar: baseline,
                ..GlobalConfig::default()
            },
            WorkspaceState::empty(),
            None,
        );
        app.mode = Mode::Terminal {
            pane_id: runtime::PaneId(1),
        };
        app.is_mobile = mobile;

        assert_eq!(
            app.terminal_escape_chord.terminal_key(prefix),
            crate::event::TerminalEscapeAction::Pending
        );
        assert!(matches!(app.mode, Mode::Terminal { .. }));
        assert_eq!(app.effective_terminal_sidebar(), expected);

        assert_eq!(
            app.terminal_escape_chord
                .terminal_key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Esc,
                    crossterm::event::KeyModifiers::NONE,
                )),
            crate::event::TerminalEscapeAction::Cancel
        );
        assert_eq!(app.effective_terminal_sidebar(), baseline);
    }
}

#[test]
fn terminal_state_iteration_without_candidates_stays_in_terminal_and_reports_status() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    let mut terminal = workspace_terminal();

    app.dispatch(Action::NextIdle, &mut terminal).unwrap();
    assert!(matches!(app.mode, Mode::Terminal { .. }));
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("No idle sessions")
    );

    let first_started = app.notice_started.unwrap();
    app.dispatch(Action::PrevActive, &mut terminal).unwrap();
    assert!(matches!(app.mode, Mode::Terminal { .. }));
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("No idle sessions")
    );
    app.expire_notice(first_started + NOTICE_MIN_DWELL);
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("No active sessions")
    );
}

#[test]
fn mobile_terminal_sidebar_toggle_is_unavailable() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    app.is_mobile = true;
    let mut terminal = workspace_terminal();

    app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
        .unwrap();

    assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Compact);
    assert!(app.terminal_sidebar_override.is_none());
    assert!(app.notice.is_none());
    assert!(app.terminal_sidebar_hint().is_none());
}

#[test]
fn terminal_sibling_switch_waits_for_the_target_baseline_before_accepting_input() {
    let mut project = make_project("terminal-sibling-switch");
    let mut worktree = make_worktree("/tmp/terminal-sibling-switch");
    worktree.sessions = vec![
        make_sess_with_id(1, runtime::AgentState::Idle),
        make_sess_with_id(2, runtime::AgentState::Blocked),
    ];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.tree_selected = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { session_idx: 0, .. }))
        .unwrap();
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };

    let (socket_path, listener) = terminal_stream_listener("terminal-sibling-switch");
    app.runtime_client = runtime::Client::new(socket_path.clone());
    let (subscribed_tx, subscribed_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let (input_tx, input_rx) = mpsc::channel();
    let (close_tx, close_rx) = mpsc::channel();
    let server_path = socket_path.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert!(matches!(
            read_runtime_request(&mut reader),
            runtime::Request::Hello { .. }
        ));
        write_runtime_message(
            &mut stream,
            &runtime::Response::Hello {
                protocol: runtime::PROTOCOL_VERSION,
                epoch: 8,
                capabilities: runtime::Capabilities::default(),
            },
        );
        let (rows, cols) = match read_runtime_request(&mut reader) {
            runtime::Request::TerminalSubscribe {
                pane_id,
                rows,
                cols,
                ..
            } => {
                assert_eq!(pane_id, runtime::PaneId(2));
                (rows, cols)
            }
            request => panic!("unexpected request: {request:?}"),
        };
        write_runtime_message(&mut stream, &runtime::Response::Ack { revision: 2 });
        subscribed_tx.send((rows, cols)).unwrap();
        continue_rx.recv().unwrap();
        let (resized_rows, resized_cols) = match read_terminal_client_message(&mut reader) {
            runtime::TerminalClientMessage::Resize {
                rows: resized_rows,
                cols: resized_cols,
            } => {
                assert_eq!(resized_rows, rows);
                assert_eq!(resized_cols, cols.saturating_sub(30));
                (resized_rows, resized_cols)
            }
            message => panic!("unexpected message: {message:?}"),
        };
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut line = String::new();
        let no_input = reader.read_line(&mut line).is_err_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        });
        input_tx.send(no_input).unwrap();
        write_runtime_message(
            &mut stream,
            &runtime::TerminalServerMessage::Update(runtime::TerminalUpdate::Full(
                test_terminal_frame(
                    runtime::PaneId(2),
                    runtime::TerminalId(2),
                    2,
                    resized_rows,
                    resized_cols,
                ),
            )),
        );
        close_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(listener);
        let _ = std::fs::remove_file(server_path);
    });
    let mut terminal = workspace_terminal();

    app.dispatch(Action::NextSession, &mut terminal).unwrap();
    let _ = subscribed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        app.current_selection(),
        Selection::Session(0, 0, 1)
    ));
    assert!(matches!(app.mode, Mode::Terminal { pane_id } if pane_id == runtime::PaneId(2)));
    assert!(app.pending_terminal_entry.is_some());
    assert!(app.terminal_cursor().is_none());

    app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
        .unwrap();
    app.dispatch(
        Action::TerminalKey(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        )),
        &mut terminal,
    )
    .unwrap();
    continue_tx.send(()).unwrap();
    assert!(input_rx.recv_timeout(Duration::from_secs(1)).unwrap());

    let deadline = Instant::now() + Duration::from_secs(1);
    while app.pending_terminal_entry.is_some() && Instant::now() < deadline {
        app.drain_terminal_stream();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(app.pending_terminal_entry.is_none());
    assert!(matches!(app.mode, Mode::Terminal { pane_id } if pane_id == runtime::PaneId(2)));
    assert!(app.terminal_cursor().is_some());

    close_tx.send(()).unwrap();
    app.terminal_stream = None;
    server.join().unwrap();
}

#[test]
fn repeated_suspend_restarts_pending_terminal_resume_with_the_same_identity() {
    let pane_id = runtime::PaneId(3);
    let terminal_id = runtime::TerminalId(4);
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.terminal_stream_generation = 2;
    app.pending_terminal_resume = Some(PendingTerminalResume {
        pane_id,
        terminal_id,
        generation: 2,
        snapshot_ready: true,
    });

    assert_eq!(app.prepare_terminal_resume(), Some(3));
    assert!(app.pending_terminal_resume.as_ref().is_some_and(|pending| {
        pending.pane_id == pane_id
            && pending.terminal_id == terminal_id
            && pending.generation == 3
            && !pending.snapshot_ready
    }));
    assert_eq!(app.prepare_terminal_resume(), Some(4));
    assert!(app.pending_terminal_resume.as_ref().is_some_and(|pending| {
        pending.pane_id == pane_id
            && pending.terminal_id == terminal_id
            && pending.generation == 4
            && !pending.snapshot_ready
    }));
}

#[test]
fn resume_snapshot_must_confirm_the_exact_live_terminal_before_reconnect() {
    fn snapshot(panes: Vec<runtime::Pane>) -> runtime::Snapshot {
        runtime::Snapshot {
            protocol: runtime::PROTOCOL_VERSION,
            epoch: 7,
            revision: 9,
            projects: vec![],
            worktrees: vec![],
            sessions: vec![],
            panes,
            listening_ports: vec![],
            pane_activity: vec![],
            plugin_sidecars: Vec::new(),
            capabilities: runtime::Capabilities::default(),
        }
    }

    let pane_id = runtime::PaneId(3);
    let terminal_id = runtime::TerminalId(4);
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal { pane_id };
    app.pending_terminal_resume = Some(PendingTerminalResume {
        pane_id,
        terminal_id,
        generation: 2,
        snapshot_ready: false,
    });
    app.apply_resume_refresh(
        2,
        Ok(snapshot(vec![runtime::Pane {
            id: pane_id,
            terminal_id,
            session_id: runtime::SessionId(1),
            label: "terminal".into(),
            agent: None,
            exited: false,
            revision: 9,
        }])),
    );
    assert!(app
        .pending_terminal_resume
        .as_ref()
        .is_some_and(|pending| pending.snapshot_ready));
    assert!(matches!(app.mode, Mode::Terminal { .. }));

    app.pending_terminal_resume = Some(PendingTerminalResume {
        pane_id,
        terminal_id,
        generation: 3,
        snapshot_ready: false,
    });
    app.apply_resume_refresh(3, Ok(snapshot(vec![])));
    assert!(app.pending_terminal_resume.is_none());
    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn group_manager_modal_cancel_returns_to_named_row_after_one_virtual_row() {
    let mut app = make_test_app(
        GlobalConfig {
            groups: vec!["work".into(), "personal".into()],
            ..GlobalConfig::default()
        },
        WorkspaceState::empty(),
        None,
    );
    let mut terminal = workspace_terminal();

    app.mode = Mode::Input {
        context: InputContext::RenameGroup { group_idx: 0 },
        state: InputState::new("new name: "),
    };
    app.dispatch(Action::InputEscape, &mut terminal).unwrap();
    assert!(matches!(
        app.mode,
        Mode::GroupManager {
            selected: 1,
            purpose: GroupManagerPurpose::Switch,
            ..
        }
    ));

    app.mode = Mode::Confirm {
        message: "Delete group?".into(),
        pending: PendingAction::DeleteGroup { group_idx: 1 },
    };
    app.dispatch(Action::InputEscape, &mut terminal).unwrap();
    assert!(matches!(
        app.mode,
        Mode::GroupManager {
            selected: 2,
            purpose: GroupManagerPurpose::Switch,
            ..
        }
    ));
}

#[test]
fn global_settings_opens_and_returns_from_agent_integration_manager() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let mut form = GlobalSettingsForm::new(GlobalConfig::default());
    for _ in 0..3 {
        form.next_category(false);
    }
    while !form.opens_agent_integrations() {
        form.next_field(false);
    }
    app.mode = Mode::GlobalSettings { form };
    let mut terminal = workspace_terminal();

    app.dispatch_global_settings(Action::Select, &mut terminal)
        .unwrap();
    assert!(matches!(app.mode, Mode::IntegrationManager { .. }));
    app.dispatch_integration_manager(Action::InputEscape)
        .unwrap();
    assert!(matches!(app.mode, Mode::GlobalSettings { .. }));
}

#[test]
fn integration_scan_never_prompts_without_explicit_agent_demand() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let metadata = integration_metadata(
        wsx_core::integration::IntegrationTarget::Pi,
        true,
        wsx_core::integration::InstallStatus::Missing,
    );

    app.apply_integration_scan(Ok(vec![metadata]));

    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn explicit_agent_demand_waits_for_scan_and_rejects_incompatible_cli() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.prompt_for_integration_if_needed(wsx_core::integration::IntegrationTarget::Codex);
    assert_eq!(
        app.pending_integration_demand,
        Some(wsx_core::integration::IntegrationTarget::Codex)
    );
    let mut incompatible = integration_metadata(
        wsx_core::integration::IntegrationTarget::Codex,
        true,
        wsx_core::integration::InstallStatus::Outdated,
    );
    incompatible.compatible = false;
    incompatible.compatibility_note = Some("Requires Codex 0.150 or newer");

    app.apply_integration_scan(Ok(vec![incompatible]));

    assert!(matches!(app.mode, Mode::Workspace));
    assert_eq!(app.pending_integration_demand, None);
}

#[test]
fn explicit_agent_demand_prompts_after_delayed_scan() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.prompt_for_integration_if_needed(wsx_core::integration::IntegrationTarget::Pi);

    app.apply_integration_scan(Ok(vec![integration_metadata(
        wsx_core::integration::IntegrationTarget::Pi,
        true,
        wsx_core::integration::InstallStatus::Missing,
    )]));

    assert!(matches!(
        &app.mode,
        Mode::Confirm {
            pending: PendingAction::InstallIntegrations { targets, .. },
            ..
        } if targets == &[wsx_core::integration::IntegrationTarget::Pi]
    ));
}

#[test]
fn explicit_agent_demand_prompts_once_and_decline_is_per_agent() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.integration_metadata = vec![integration_metadata(
        wsx_core::integration::IntegrationTarget::Pi,
        true,
        wsx_core::integration::InstallStatus::Missing,
    )];

    app.prompt_for_integration_if_needed(wsx_core::integration::IntegrationTarget::Pi);
    assert!(matches!(
        &app.mode,
        Mode::Confirm {
            pending: PendingAction::InstallIntegrations { targets, .. },
            ..
        } if targets == &[wsx_core::integration::IntegrationTarget::Pi]
    ));
    let mut terminal = workspace_terminal();
    app.dispatch_confirm(Action::InputEscape, &mut terminal)
        .unwrap();
    assert!(app
        .dismissed_integration_prompts
        .contains(&wsx_core::integration::IntegrationTarget::Pi));

    app.prompt_for_integration_if_needed(wsx_core::integration::IntegrationTarget::Pi);
    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn delayed_demand_scan_waits_for_active_modal() {
    let target = wsx_core::integration::IntegrationTarget::Pi;
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Help;
    app.prompt_for_integration_if_needed(target);

    app.apply_integration_scan(Ok(vec![integration_metadata(
        target,
        true,
        wsx_core::integration::InstallStatus::Missing,
    )]));
    assert!(matches!(app.mode, Mode::Help));
    assert_eq!(app.pending_integration_demand, Some(target));

    app.mode = Mode::Workspace;
    app.tick().unwrap();
    assert!(matches!(
        &app.mode,
        Mode::Confirm {
            pending: PendingAction::InstallIntegrations { targets, .. },
            ..
        } if targets == &[target]
    ));
}

#[test]
fn startup_scan_prompts_for_outdated_integrations_despite_prior_dismissal() {
    let target = wsx_core::integration::IntegrationTarget::Claude;
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.dismissed_integration_prompts.insert(target);

    app.apply_integration_scan(Ok(vec![integration_metadata(
        target,
        true,
        wsx_core::integration::InstallStatus::Outdated,
    )]));

    assert!(matches!(
        &app.mode,
        Mode::Confirm {
            pending: PendingAction::InstallIntegrations {
                targets,
                remember_dismissal: false,
            },
            ..
        } if targets == &[target]
    ));
}

#[test]
fn delayed_outdated_scan_waits_for_workspace_and_decline_is_run_local() {
    let target = wsx_core::integration::IntegrationTarget::Claude;
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Help;

    app.apply_integration_scan(Ok(vec![integration_metadata(
        target,
        true,
        wsx_core::integration::InstallStatus::Outdated,
    )]));
    assert!(matches!(app.mode, Mode::Help));
    assert_eq!(app.pending_integration_updates, vec![target]);

    app.mode = Mode::Workspace;
    app.prompt_for_pending_integration_updates();
    let mut terminal = workspace_terminal();
    app.dispatch_confirm(Action::InputEscape, &mut terminal)
        .unwrap();
    assert!(matches!(app.mode, Mode::Workspace));
    assert!(!app.dismissed_integration_prompts.contains(&target));

    app.apply_integration_scan(Ok(vec![integration_metadata(
        target,
        true,
        wsx_core::integration::InstallStatus::Outdated,
    )]));
    assert!(matches!(
        &app.mode,
        Mode::Confirm {
            pending: PendingAction::InstallIntegrations {
                targets,
                remember_dismissal: false,
            },
            ..
        } if targets == &[target]
    ));
}

#[test]
fn routines_header_is_nonempty_searchable_and_mobile_safe() {
    let mut project = make_project("demo");
    project.routines = vec![routine_view("morning")];
    let workspace = WorkspaceState {
        projects: vec![project],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.is_mobile = true;
    assert!(app
        .flat()
        .iter()
        .any(|entry| matches!(entry, FlatEntry::RoutinesHeader { .. })));
    assert!(app
        .flat()
        .iter()
        .any(|entry| matches!(entry, FlatEntry::Routine { .. })));
    assert_eq!(app.search_matches("morning").len(), 1);
    let backend = ratatui::backend::TestBackend::new(40, 18);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.preview_area, Rect::default());
    app.mode = Mode::RoutineDetail {
        project_path: app.workspace.projects[0].path.clone(),
        routine_name: "morning".into(),
        scroll: 0,
    };
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn failed_routine_delete_restores_selection_and_surfaces_status() {
    let mut project = make_project("demo");
    project.routines = vec![routine_view("morning")];
    project.routines_expanded = true;
    let workspace = WorkspaceState {
        projects: vec![project],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.tree_selected = 0;
    app.mode = Mode::Confirm {
        message: "delete".into(),
        pending: PendingAction::DeleteRoutine {
            project_path: app.workspace.projects[0].path.clone(),
            name: "morning".into(),
            revision: 1,
        },
    };
    app.restore_failed_routine_delete(0, "morning", "stale config revision".into());
    assert!(matches!(app.mode, Mode::Workspace));
    assert!(matches!(app.current_selection(), Selection::Routine(0, 0)));
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("Routine 'morning' not deleted: stale config revision")
    );
}

#[test]
fn protocol_mismatch_surfaces_packaged_wsx_upgrade_guidance() {
    let error = anyhow::Error::new(asched_core::routine::RoutineError::ProtocolMismatch {
        client: 3,
        daemon: 2,
    });
    assert_eq!(
        routine_error_text(&error),
        "routine scheduler is newer than wsx; upgrade wsx and retry"
    );
}

#[test]
fn remote_conflict_surfaces_refresh_guidance() {
    let error = anyhow::Error::new(asched_core::routine::RoutineError::RemoteDaemon {
        kind: asched_core::routine::RoutineErrorKind::Conflict,
        message: "stale config revision".into(),
    });
    assert_eq!(
        routine_error_text(&error),
        "routine changed in asched; refreshed the latest revision"
    );
}

#[test]
fn remote_already_running_has_stable_user_facing_status() {
    let error = anyhow::Error::new(asched_core::routine::RoutineError::RemoteDaemon {
        kind: asched_core::routine::RoutineErrorKind::AlreadyRunning,
        message: "routine 'daily' is already running".into(),
    });
    assert_eq!(routine_error_text(&error), "routine is already running");
}

#[test]
fn routine_refresh_applies_by_path_and_discards_superseded_generation() {
    let workspace = WorkspaceState {
        projects: vec![make_project("a"), make_project("b")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    let path = PathBuf::from("/tmp/a");
    let generation = app.invalidate_routine_refresh(&path);
    app.workspace.projects.swap(0, 1);
    app.routine_tx
        .send(RoutineRefreshResult {
            project_path: path.clone(),
            kind: RoutineResultKind::Refresh {
                generation,
                expand: false,
                selection: RoutineSelection::Preserve,
            },
            response: Ok(asched_core::routine::ipc::Response::Routines {
                revision: 1,
                routines: vec![routine_view("fresh")],
            }),
        })
        .unwrap();
    app.drain_async_results();
    assert_eq!(app.workspace.projects[0].name, "b");
    assert!(app.workspace.projects[0].routines.is_empty());
    assert_eq!(app.workspace.projects[1].routines[0].routine.name, "fresh");

    app.invalidate_routine_refresh(&path);
    app.routine_tx
        .send(RoutineRefreshResult {
            project_path: path,
            kind: RoutineResultKind::Refresh {
                generation,
                expand: false,
                selection: RoutineSelection::Preserve,
            },
            response: Ok(asched_core::routine::ipc::Response::Routines {
                revision: 0,
                routines: vec![routine_view("stale")],
            }),
        })
        .unwrap();
    app.drain_async_results();
    assert_eq!(app.workspace.projects[1].routines[0].routine.name, "fresh");
    assert_eq!(app.workspace.projects[1].routine_revision, 1);
}

#[test]
fn empty_project_opens_runner_picker_before_first_creation_form() {
    let workspace = WorkspaceState {
        projects: vec![make_project("demo")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    assert!(!app
        .flat()
        .iter()
        .any(|entry| matches!(entry, FlatEntry::RoutinesHeader { .. })));
    app.action_add_routine().unwrap();
    assert!(matches!(
        app.mode,
        Mode::RoutinePresetPicker {
            project_idx: 0,
            selected: 0
        }
    ));

    app.dispatch_routine_preset_picker(Action::InputChar('j'))
        .unwrap();
    app.dispatch_routine_preset_picker(Action::NavigateDown)
        .unwrap();
    app.dispatch_routine_preset_picker(Action::Select).unwrap();
    let Mode::RoutineEditor {
        project_idx,
        original_name,
        can_rename,
        form,
    } = &app.mode
    else {
        panic!("runner selection did not open the editor");
    };
    assert_eq!(*project_idx, 0);
    assert_eq!(original_name, &None);
    assert!(*can_rename);
    let mut form = form.clone();
    form.name = "review".into();
    assert_eq!(
        form.routine().unwrap().command,
        vec!["claude", "-p", "{prompt}"]
    );
}

#[test]
fn runner_picker_wraps_and_escape_cancels() {
    let workspace = WorkspaceState {
        projects: vec![make_project("demo")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.action_add_routine().unwrap();
    app.dispatch_routine_preset_picker(Action::InputChar('k'))
        .unwrap();
    assert!(matches!(
        app.mode,
        Mode::RoutinePresetPicker { selected: 14, .. }
    ));
    app.dispatch_routine_preset_picker(Action::InputEscape)
        .unwrap();
    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn running_routine_editor_keeps_name_locked() {
    let workspace = WorkspaceState {
        projects: vec![make_project("demo")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    let mut form = RoutineForm::from_routine(routine_view("running").routine);
    form.field = 0;
    form.cursor = form.name.len();
    app.mode = Mode::RoutineEditor {
        project_idx: 0,
        original_name: Some("running".into()),
        can_rename: false,
        form,
    };

    app.dispatch_routine_editor(Action::InputChar('x')).unwrap();

    let Mode::RoutineEditor { form, .. } = &app.mode else {
        panic!("editor closed unexpectedly");
    };
    assert_eq!(form.name, "running");
}

#[test]
fn routine_detail_tracks_name_when_refresh_reorders_views() {
    let mut project = make_project("demo");
    project.routines = vec![routine_view("morning"), routine_view("evening")];
    let workspace = WorkspaceState {
        projects: vec![project],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.mode = Mode::RoutineDetail {
        project_path: app.workspace.projects[0].path.clone(),
        routine_name: "morning".into(),
        scroll: 0,
    };
    app.workspace.projects[0].routines.swap(0, 1);

    let Mode::RoutineDetail {
        project_path,
        routine_name,
        ..
    } = &app.mode
    else {
        panic!("detail closed unexpectedly");
    };
    assert_eq!(project_path, &PathBuf::from("/tmp/demo"));
    assert_eq!(routine_name, "morning");
    assert_eq!(
        app.workspace.projects[0]
            .routines
            .iter()
            .find(|view| view.routine.name == *routine_name)
            .unwrap()
            .routine
            .name,
        "morning"
    );
}

#[test]
fn routine_detail_tracks_project_path_when_projects_reorder() {
    let mut first = make_project("first");
    first.routines = vec![routine_view("morning")];
    let workspace = WorkspaceState {
        projects: vec![first, make_project("second")],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    app.mode = Mode::RoutineDetail {
        project_path: PathBuf::from("/tmp/first"),
        routine_name: "morning".into(),
        scroll: 0,
    };

    app.workspace.projects.swap(0, 1);

    let Mode::RoutineDetail { project_path, .. } = &app.mode else {
        panic!("detail closed unexpectedly");
    };
    let project = app
        .workspace
        .projects
        .iter()
        .find(|project| project.path == *project_path)
        .unwrap();
    assert_eq!(project.name, "first");
    assert_eq!(project.routines[0].routine.name, "morning");
}

#[test]
fn routine_refresh_closes_detail_when_routine_was_deleted() {
    let mut project = make_project("demo");
    project.routines = vec![routine_view("morning")];
    let workspace = WorkspaceState {
        projects: vec![project],
    };
    let mut app = make_test_app(GlobalConfig::default(), workspace, None);
    let project_path = app.workspace.projects[0].path.clone();
    app.mode = Mode::RoutineDetail {
        project_path: project_path.clone(),
        routine_name: "morning".into(),
        scroll: 0,
    };
    let generation = app.invalidate_routine_refresh(&project_path);
    app.routine_tx
        .send(RoutineRefreshResult {
            project_path,
            kind: RoutineResultKind::Refresh {
                generation,
                expand: false,
                selection: RoutineSelection::Preserve,
            },
            response: Ok(asched_core::routine::ipc::Response::Routines {
                revision: 2,
                routines: Vec::new(),
            }),
        })
        .unwrap();

    app.drain_async_results();

    assert!(matches!(app.mode, Mode::Workspace));
}

#[test]
fn given_projects_with_visibility_gaps_when_moving_forward_then_returns_next_visible_index() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("b1"),
            make_project("a2"),
            make_project("b2"),
            make_project("c1"),
            make_project("c2"),
        ],
    };
    let visible = HashSet::from([0, 2, 5]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 2, 1),
        Some(5)
    );
}

#[test]
fn given_projects_with_visibility_gaps_when_moving_backward_then_returns_previous_visible_index() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("b1"),
            make_project("a2"),
            make_project("b2"),
            make_project("c1"),
            make_project("c2"),
        ],
    };
    let visible = HashSet::from([0, 2, 5]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 2, -1),
        Some(0)
    );
}

#[test]
fn given_empty_visible_set_when_moving_then_returns_none() {
    let workspace = WorkspaceState {
        projects: vec![make_project("a1"), make_project("a2")],
    };
    let visible = HashSet::new();

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 0, 1),
        None
    );
}

#[test]
fn given_non_visible_project_index_when_moving_then_returns_none() {
    let workspace = WorkspaceState {
        projects: vec![make_project("a1"), make_project("a2"), make_project("a3")],
    };
    let visible = HashSet::from([0, 2]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 1, 1),
        None
    );
}

#[test]
fn given_large_positive_delta_when_moving_past_last_visible_project_then_returns_none() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("a2"),
            make_project("a3"),
            make_project("a4"),
        ],
    };
    let visible = HashSet::from([0, 2, 3]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 0, 5),
        None
    );
}

#[test]
fn given_large_negative_delta_when_moving_before_first_visible_project_then_returns_none() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("a2"),
            make_project("a3"),
            make_project("a4"),
        ],
    };
    let visible = HashSet::from([0, 2, 3]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 3, -5),
        None
    );
}

#[test]
fn given_visible_set_with_out_of_range_index_when_project_index_is_not_enumerated_then_returns_none(
) {
    let workspace = WorkspaceState {
        projects: vec![make_project("a1"), make_project("a2"), make_project("a3")],
    };
    let visible = HashSet::from([0, 99]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 99, -1),
        None
    );
}

#[test]
fn given_zero_delta_when_current_project_is_visible_then_returns_same_index() {
    let workspace = WorkspaceState {
        projects: vec![make_project("a1"), make_project("a2"), make_project("a3")],
    };
    let visible = HashSet::from([0, 2]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 2, 0),
        Some(2)
    );
}

#[test]
fn given_multi_step_positive_delta_when_target_visible_exists_then_returns_that_visible_index() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("b1"),
            make_project("a2"),
            make_project("b2"),
            make_project("c1"),
            make_project("c2"),
        ],
    };
    let visible = HashSet::from([0, 2, 5]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 0, 2),
        Some(5)
    );
}

#[test]
fn given_multi_step_negative_delta_when_target_visible_exists_then_returns_that_visible_index() {
    let workspace = WorkspaceState {
        projects: vec![
            make_project("a1"),
            make_project("b1"),
            make_project("a2"),
            make_project("b2"),
            make_project("c1"),
            make_project("c2"),
        ],
    };
    let visible = HashSet::from([0, 2, 5]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 5, -2),
        Some(0)
    );
}

#[test]
fn given_visible_set_with_out_of_range_member_when_current_project_is_valid_then_ignores_invalid_member(
) {
    let workspace = WorkspaceState {
        projects: vec![make_project("a1"), make_project("a2"), make_project("a3")],
    };
    let visible = HashSet::from([0, 2, 99]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 0, 1),
        Some(2)
    );
}

#[test]
fn given_empty_workspace_with_non_empty_visible_set_when_moving_then_returns_none() {
    let workspace = WorkspaceState { projects: vec![] };
    let visible = HashSet::from([0]);

    assert_eq!(
        adjacent_visible_project_index(&workspace, &visible, 0, 1),
        None
    );
}

#[test]
fn given_project_removed_from_group_when_flat_rebuilt_then_shifted_project_does_not_leak() {
    let config = GlobalConfig {
        groups: vec!["work".to_string(), "personal".to_string()],
        projects: vec![
            make_project_entry("work-a", Some("work")),
            make_project_entry("personal", Some("personal")),
            make_project_entry("work-b", Some("work")),
        ],
        exclude_worktree_paths: vec![],
        terminal_escape_chord: "ctrl+a w".into(),
        resume_agents_on_restore: true,
        auto_collapse: AutoCollapsePolicy::Adaptive { base_hours: 24 },
        notification_timeout_seconds: 4,
        ..GlobalConfig::default()
    };
    let workspace = WorkspaceState {
        projects: vec![
            make_project("work-a"),
            make_project("personal"),
            make_project("work-b"),
        ],
    };
    let mut app = make_test_app(config, workspace, Some("work".to_string()));

    app.workspace.projects.remove(0);
    app.config
        .projects
        .retain(|p| p.path.as_path() != std::path::Path::new("/tmp/work-a"));
    app.rebuild_flat();

    assert_eq!(app.flat(), &[FlatEntry::Project { idx: 1 }]);
}

#[test]
fn given_fresh_git_info_when_forced_refresh_requested_then_old_info_stays_visible_and_refresh_starts(
) {
    let path = std::path::PathBuf::from("/tmp/wsx-force-git-refresh");
    let mut project = make_project("repo");
    let mut worktree = make_worktree(path.to_string_lossy().as_ref());
    worktree.git_info = Some(make_git_info());
    worktree.git_info_fetched_at = Some(Instant::now());
    project.worktrees.push(worktree);

    let config = GlobalConfig::default();
    let workspace = WorkspaceState {
        projects: vec![project],
    };
    let mut app = make_test_app(config, workspace, None);

    app.spawn_git_local(path.clone(), "main".to_string());
    assert!(!app.git_local_pending.contains(&path));

    app.spawn_git_local_with_options(path.clone(), "main".to_string(), true);
    assert!(app.git_local_pending.contains(&path));
    assert!(app.workspace.projects[0].worktrees[0].git_info.is_some());
}

#[test]
fn selected_git_status_uses_short_cache_while_background_rows_keep_long_cache() {
    let path = std::path::PathBuf::from("/tmp/wsx-selected-git-refresh");
    let mut project = make_project("repo");
    let mut worktree = make_worktree(path.to_string_lossy().as_ref());
    worktree.git_info = Some(make_git_info());
    worktree.git_info_fetched_at = Some(Instant::now() - Duration::from_secs(2));
    project.worktrees.push(worktree);
    let workspace = WorkspaceState {
        projects: vec![project],
    };

    let mut selected = make_test_app(GlobalConfig::default(), workspace.clone(), None);
    selected.tree_selected = selected
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Worktree { .. }))
        .unwrap();
    selected.spawn_git_local_for_selected();
    assert!(selected.git_local_pending.contains(&path));

    let mut background = make_test_app(GlobalConfig::default(), workspace, None);
    background.spawn_git_local(path.clone(), "main".into());
    assert!(!background.git_local_pending.contains(&path));
}

// Regression guard for the attention_candidates logic (commit c118ea2).
// Ensures active sessions never trigger attention, and muted/suppressed are ignored.

fn make_sess(
    muted: bool,
    status: wsx_core::runtime::AgentState,
) -> wsx_core::model::workspace::SessionInfo {
    wsx_core::model::workspace::SessionInfo {
        session_id: runtime::SessionId(1),
        pane_id: runtime::PaneId(1),
        terminal_id: runtime::TerminalId(1),
        agent: Some("codex".into()),
        display_name: "session".into(),
        agent_status: status,
        revision: 1,
        layout: runtime::PaneLayout::Leaf {
            pane_id: runtime::PaneId(1),
        },
        panes: vec![wsx_core::model::workspace::PaneInfo {
            pane_id: runtime::PaneId(1),
            terminal_id: runtime::TerminalId(1),
            label: "terminal".into(),
            agent: Some("codex".into()),
            agent_status: status,
            revision: 1,
            exited: false,
            listening_ports: vec![],
            foreground_job: false,
            outcome_acknowledged: false,
        }],
        muted,
        outcome_acknowledged: false,
    }
}

fn make_sess_with_id(
    id: u64,
    status: runtime::AgentState,
) -> wsx_core::model::workspace::SessionInfo {
    let mut session = make_sess(false, status);
    session.session_id = runtime::SessionId(id);
    session.pane_id = runtime::PaneId(id);
    session.terminal_id = runtime::TerminalId(id);
    session.display_name = format!("session-{id}");
    session.layout = runtime::PaneLayout::Leaf {
        pane_id: runtime::PaneId(id),
    };
    session.panes[0].pane_id = runtime::PaneId(id);
    session.panes[0].terminal_id = runtime::TerminalId(id);
    session
}

fn session_position(app: &App, session_idx: usize) -> usize {
    app.flat()
        .iter()
        .position(|entry| {
            matches!(
                entry,
                FlatEntry::Session {
                    session_idx: candidate,
                    ..
                } if *candidate == session_idx
            )
        })
        .unwrap()
}

fn preferred_session_fixture(
    sessions: Vec<wsx_core::model::workspace::SessionInfo>,
) -> (GlobalConfig, WorkspaceState, GroupKey) {
    let mut project = make_project("preferred");
    project.expanded = false;
    let mut worktree = make_worktree("/tmp/preferred");
    worktree.expanded = false;
    worktree.sessions = sessions;
    project.worktrees = vec![worktree];
    (
        GlobalConfig {
            groups: vec!["target".into()],
            projects: vec![make_project_entry("preferred", Some("target"))],
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project],
        },
        GroupKey::Named("target".into()),
    )
}

#[test]
fn preferred_group_session_keeps_workspace_order_for_attention_before_idle() {
    let (config, workspace, group) = preferred_session_fixture(vec![
        make_sess_with_id(1, runtime::AgentState::Idle),
        make_sess_with_id(2, runtime::AgentState::Done),
        make_sess_with_id(3, runtime::AgentState::Blocked),
    ]);

    assert_eq!(
        preferred_group_session(&config, &workspace, &group),
        Some((0, 0, 1))
    );
}

#[test]
fn preferred_group_session_falls_back_to_first_agentic_idle() {
    let mut agentless_idle = make_sess_with_id(1, runtime::AgentState::Idle);
    agentless_idle.agent = None;
    agentless_idle.panes[0].agent = None;
    let (config, workspace, group) = preferred_session_fixture(vec![
        agentless_idle,
        make_sess_with_id(2, runtime::AgentState::Working),
        make_sess_with_id(3, runtime::AgentState::Idle),
        make_sess_with_id(4, runtime::AgentState::Idle),
    ]);

    assert_eq!(
        preferred_group_session(&config, &workspace, &group),
        Some((0, 0, 2))
    );
}

#[test]
fn preferred_group_session_rejects_agentless_idle_and_active_only_groups() {
    let mut agentless_idle = make_sess_with_id(1, runtime::AgentState::Idle);
    agentless_idle.agent = None;
    agentless_idle.panes[0].agent = None;
    let (config, workspace, group) = preferred_session_fixture(vec![
        agentless_idle,
        make_sess_with_id(2, runtime::AgentState::Working),
    ]);

    assert_eq!(preferred_group_session(&config, &workspace, &group), None);
}

fn make_navigation_test_app() -> App {
    let mut project = make_project("navigation");
    let mut worktree = make_worktree("./navigation");
    worktree.sessions = vec![
        make_sess(false, wsx_core::runtime::AgentState::Idle),
        make_sess(false, wsx_core::runtime::AgentState::Idle),
    ];
    project.worktrees = vec![worktree];
    project.routines = vec![routine_view("morning")];

    make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    )
}

#[test]
fn idle_navigation_skips_sessions_without_an_agent() {
    let mut project = make_project("idle-navigation");
    let mut worktree = make_worktree("./idle-navigation");
    let mut shell = make_sess(false, runtime::AgentState::Idle);
    shell.agent = None;
    let agent = make_sess(false, runtime::AgentState::Idle);
    worktree.sessions = vec![shell, agent];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.action_move_idle(1);
    assert_eq!(app.current_selection(), Selection::Session(0, 0, 1));

    app.tree_selected = 0;
    app.action_move_idle(-1);
    assert_eq!(app.current_selection(), Selection::Session(0, 0, 1));
}

fn select_rendered_navigation_entry(app: &mut App, selection: Selection) {
    let index = (0..app.flat().len())
        .find(|&index| app.workspace.get_selection(index, app.flat()) == selection)
        .expect("navigation fixture must contain requested selection");
    app.tree_selected = index;
    app.force_preview_redraw = false;
    app.last_rendered_preview_was_session =
        !app.is_mobile && matches!(selection, Selection::Session(..));
}

fn assert_navigation_transition(
    from: Selection,
    action: Action,
    to: Selection,
    mobile: bool,
    clear_preview: bool,
) {
    let mut app = make_navigation_test_app();
    app.is_mobile = mobile;
    select_rendered_navigation_entry(&mut app, from);
    match action {
        Action::NavigateUp => app.nav_up(),
        Action::NavigateDown => app.nav_down(),
        Action::NavigateRight => app.nav_right(),
        _ => unreachable!(),
    }
    assert_eq!(app.current_selection(), to);
    assert_eq!(app.force_preview_redraw, clear_preview);
    assert!(!app.force_terminal_redraw);
}

fn expandable_is_expanded(app: &App, selection: &Selection) -> bool {
    match selection {
        Selection::Project(project_idx) => app.workspace.projects[*project_idx].expanded,
        Selection::Worktree(project_idx, worktree_idx) => {
            app.workspace.projects[*project_idx].worktrees[*worktree_idx].expanded
        }
        Selection::RoutinesHeader(project_idx) => {
            app.workspace.projects[*project_idx].routines_expanded
        }
        other => panic!("not an expandable workspace row: {other:?}"),
    }
}

fn set_expandable(app: &mut App, selection: &Selection, expanded: bool) {
    match selection {
        Selection::Project(project_idx) => app.workspace.projects[*project_idx].expanded = expanded,
        Selection::Worktree(project_idx, worktree_idx) => {
            app.workspace.projects[*project_idx].worktrees[*worktree_idx].expanded = expanded;
        }
        Selection::RoutinesHeader(project_idx) => {
            app.workspace.projects[*project_idx].routines_expanded = expanded;
        }
        other => panic!("not an expandable workspace row: {other:?}"),
    }
}

#[test]
fn workspace_expansion_inputs_mark_cache_dirty_for_every_expandable_row() {
    let expandable_rows = [
        Selection::Project(0),
        Selection::Worktree(0, 0),
        Selection::RoutinesHeader(0),
    ];
    let mut terminal = workspace_terminal();

    for (input, initially_expanded, expected_expanded) in [
        ("left", true, false),
        ("right", false, true),
        ("select", true, false),
        ("select", false, true),
    ] {
        for selection in &expandable_rows {
            let mut app = make_navigation_test_app();
            set_expandable(&mut app, selection, initially_expanded);
            app.rebuild_flat();
            select_rendered_navigation_entry(&mut app, selection.clone());
            let project_path = app.workspace.projects[0].path.clone();
            app.stale_collapsed_projects.insert(project_path.clone());
            app.cache_dirty = false;

            match input {
                "left" => app.nav_left(),
                "right" => app.nav_right(),
                "select" => app.action_select(&mut terminal).unwrap(),
                _ => unreachable!(),
            }

            assert_eq!(
                expandable_is_expanded(&app, selection),
                expected_expanded,
                "{input} did not change {selection:?} as expected"
            );
            assert!(
                app.cache_dirty,
                "{input} changed {selection:?} without marking the cache dirty"
            );
            assert!(
                app.project_touched_unix_ms.contains_key(&project_path),
                "{input} changed {selection:?} without refreshing project staleness"
            );
            assert!(
                !app.stale_collapsed_projects.contains(&project_path),
                "{input} changed {selection:?} without clearing stale provenance"
            );
        }
    }
}

#[test]
fn workspace_cursor_navigation_without_expansion_keeps_cache_clean() {
    let mut child_app = make_navigation_test_app();
    let child_project_path = child_app.workspace.projects[0].path.clone();
    child_app
        .stale_collapsed_projects
        .insert(child_project_path.clone());
    select_rendered_navigation_entry(&mut child_app, Selection::Project(0));
    child_app.nav_right();
    assert_eq!(child_app.current_selection(), Selection::Worktree(0, 0));
    assert!(child_app.workspace.projects[0].expanded);
    assert!(!child_app.cache_dirty);
    assert!(child_app
        .stale_collapsed_projects
        .contains(&child_project_path));

    let mut parent_app = make_navigation_test_app();
    parent_app.workspace.projects[0].worktrees[0].expanded = false;
    parent_app.rebuild_flat();
    select_rendered_navigation_entry(&mut parent_app, Selection::Worktree(0, 0));
    parent_app.nav_left();
    assert_eq!(parent_app.current_selection(), Selection::Project(0));
    assert!(!parent_app.workspace.projects[0].worktrees[0].expanded);
    assert!(!parent_app.cache_dirty);
}

#[test]
fn given_desktop_when_navigating_down_across_session_boundaries_then_clears_preview() {
    let cases = [
        (Selection::Worktree(0, 0), Selection::Session(0, 0, 0)),
        (Selection::Session(0, 0, 0), Selection::Session(0, 0, 1)),
        (Selection::Session(0, 0, 1), Selection::RoutinesHeader(0)),
    ];

    for (from, to) in cases {
        assert_navigation_transition(from, Action::NavigateDown, to, false, true);
    }
}

#[test]
fn given_desktop_when_navigating_down_between_plain_previews_then_skips_preview_clear() {
    let cases = [
        (Selection::Project(0), Selection::Worktree(0, 0)),
        (Selection::RoutinesHeader(0), Selection::Routine(0, 0)),
    ];

    for (from, to) in cases {
        assert_navigation_transition(from, Action::NavigateDown, to, false, false);
    }
}

#[test]
fn given_mobile_when_navigating_across_session_boundaries_then_skips_preview_clear() {
    let cases = [
        (Selection::Worktree(0, 0), Selection::Session(0, 0, 0)),
        (Selection::Session(0, 0, 0), Selection::Session(0, 0, 1)),
        (Selection::Session(0, 0, 1), Selection::RoutinesHeader(0)),
    ];

    for (from, to) in cases {
        assert_navigation_transition(from, Action::NavigateDown, to, true, false);
    }
}

#[test]
fn given_desktop_when_navigating_up_across_session_boundaries_then_clears_preview() {
    let cases = [
        (Selection::RoutinesHeader(0), Selection::Session(0, 0, 1)),
        (Selection::Session(0, 0, 1), Selection::Session(0, 0, 0)),
        (Selection::Session(0, 0, 0), Selection::Worktree(0, 0)),
    ];

    for (from, to) in cases {
        assert_navigation_transition(from, Action::NavigateUp, to, false, true);
    }
}

#[test]
fn given_navigation_boundary_when_navigation_is_noop_then_selection_and_redraw_stay_unchanged() {
    let cases = [
        (Selection::Project(0), Action::NavigateUp),
        (Selection::Routine(0, 0), Action::NavigateDown),
        (Selection::Session(0, 0, 0), Action::NavigateRight),
    ];

    for (selection, action) in cases {
        assert_navigation_transition(selection.clone(), action, selection, false, false);
    }
}

#[test]
fn given_empty_workspace_when_navigating_then_selection_and_redraw_stay_unchanged() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);

    app.nav_up();
    app.nav_down();

    assert_eq!(app.current_selection(), Selection::None);
    assert_eq!(app.tree_selected, 0);
    assert!(!app.force_preview_redraw);
    assert!(!app.force_terminal_redraw);
}

fn add_split_pane(session: &mut wsx_core::model::workspace::SessionInfo) {
    session.panes.push(wsx_core::model::workspace::PaneInfo {
        pane_id: runtime::PaneId(2),
        terminal_id: runtime::TerminalId(2),
        label: "split".into(),
        agent: None,
        agent_status: runtime::AgentState::Idle,
        revision: 1,
        exited: false,
        listening_ports: vec![],
        foreground_job: false,
        outcome_acknowledged: false,
    });
    session.layout = runtime::PaneLayout::Split {
        axis: runtime::SplitAxis::Vertical,
        ratio_millis: 500,
        first: Box::new(runtime::PaneLayout::Leaf {
            pane_id: runtime::PaneId(1),
        }),
        second: Box::new(runtime::PaneLayout::Leaf {
            pane_id: runtime::PaneId(2),
        }),
    };
}

#[test]
fn multi_pane_session_keeps_session_visible_and_adds_subordinate_rows() {
    let mut project = make_project("panes");
    let mut worktree = make_worktree("/tmp/panes");
    let mut session = make_sess(false, runtime::AgentState::Idle);
    add_split_pane(&mut session);
    worktree.sessions.push(session);
    project.worktrees.push(worktree);
    let app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    assert!(matches!(app.flat()[2], FlatEntry::Session { .. }));
    assert!(matches!(app.flat()[3], FlatEntry::Pane { pane_idx: 0, .. }));
    assert!(matches!(app.flat()[4], FlatEntry::Pane { pane_idx: 1, .. }));
}

#[test]
fn leaving_terminal_mode_does_not_show_an_obvious_mode_notification() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };

    app.leave_terminal_mode(runtime::PaneId(1));

    assert!(matches!(app.mode, Mode::Workspace));
    assert!(app.notice.is_none());
}

#[test]
fn terminal_quit_action_exits_only_the_tui() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 20, 8)),
        },
    )
    .unwrap();

    app.dispatch(Action::Quit, &mut terminal).unwrap();

    assert!(app.should_quit);
    assert!(matches!(app.mode, Mode::Terminal { .. }));
    assert!(app.workspace.projects.is_empty());
}

#[test]
fn global_header_spans_both_modes_without_workspace_spacer() {
    let mut project = make_project("demo");
    project.last_terminal_active_unix_ms = Some(0);
    let config = GlobalConfig {
        groups: vec!["work".into(), "personal".into()],
        terminal_escape_chord: "alt+g z".into(),
        projects: vec![wsx_core::config::global::ProjectEntry {
            name: "demo".into(),
            path: project.path.clone(),
            groups: vec!["work".into()],
            aliases: HashMap::new(),
            worktree_order: Vec::new(),
            profile: None,
        }],
        ..GlobalConfig::default()
    };
    let mut app = make_test_app(
        config,
        WorkspaceState {
            projects: vec![project],
        },
        Some("work".into()),
    );
    let stale_path = app.workspace.projects[0].path.clone();
    app.stale_collapsed_projects.insert(stale_path);
    let backend = ratatui::backend::TestBackend::new(100, 16);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert_eq!(app.tree_area.width, 32);
    let groups = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    for label in ["workspace", "ungrouped", "work", "personal"] {
        assert!(groups.contains(label), "missing {label}: {groups:?}");
    }
    assert!(
        !groups.contains("recent"),
        "unexpected Recent group: {groups:?}"
    );
    assert!(
        !groups.contains("+N") && !groups.contains("… +"),
        "{groups:?}"
    );
    let strip = crate::ui::workspace_nav::fit_group_strip(
        crate::ui::workspace_nav::WORKSPACE_HEADER_TITLE,
        &app.config.ordered_group_keys(),
        &app.active_group,
        100,
        app.group_header_scroll,
    );
    let ungrouped = strip
        .chips
        .iter()
        .find(|chip| chip.key == GroupKey::Ungrouped)
        .unwrap();
    let work = strip
        .chips
        .iter()
        .find(|chip| chip.key == GroupKey::Named("work".into()))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(ungrouped.cells.start as u16, 0)].bg,
        crate::ui::theme::group_chip(false).bg.unwrap()
    );
    assert_eq!(
        terminal.backend().buffer()[(work.cells.start as u16, 0)].bg,
        crate::ui::theme::group_chip(true).bg.unwrap()
    );
    assert_eq!(
        terminal.backend().buffer()[(99, 0)].bg,
        ratatui::style::Color::Reset
    );
    assert!(groups.contains("workspace"), "{groups:?}");
    assert!(!groups.contains("Workspace"), "{groups:?}");
    let projects = (0..30)
        .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
        .collect::<String>();
    assert!(projects.contains("demo"), "{projects:?}");
    let footer = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 15)].symbol())
        .collect::<String>();
    assert!(!footer.contains("Enter:"), "{footer:?}");
    assert!(footer.contains(" WORKSPACE "), "{footer:?}");
    assert!(!footer.contains("[WORKSPACE]"), "{footer:?}");
    assert!(
        footer.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "{footer:?}"
    );
    let wrap_guard = &terminal.backend().buffer()[(99, 15)];
    assert_eq!(wrap_guard.symbol(), " ");
    assert_eq!(wrap_guard.fg, ratatui::style::Color::Reset);
    assert_eq!(wrap_guard.bg, ratatui::style::Color::Reset);
    assert_eq!(
        terminal.backend().buffer()[(0, 15)].bg,
        crate::ui::theme::ACCENT
    );
    assert_eq!(
        terminal.backend().buffer()[(30, 15)].bg,
        ratatui::style::Color::Reset
    );
    assert_eq!(
        terminal.backend().buffer()[(99, 14)].bg,
        ratatui::style::Color::Reset
    );
    for ((x, y), symbol) in [
        ((0, 1), "┌"),
        ((31, 1), "┐"),
        ((0, 14), "└"),
        ((31, 14), "┘"),
    ] {
        assert_eq!(terminal.backend().buffer()[(x, y)].symbol(), symbol);
        assert_eq!(
            terminal.backend().buffer()[(x, y)].fg,
            crate::ui::theme::TEXT_SUBTLE
        );
    }
    let workspace_tree_height = app.tree_visible_height;

    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let terminal_header = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(terminal_header.contains("workspace"), "{terminal_header:?}");
    assert_eq!(app.tree_visible_height, workspace_tree_height);
    assert_eq!(app.tree_area.width, 2);
    assert_eq!(app.preview_area.x, 2);
    assert_eq!(terminal.backend().buffer()[(0, 2)].symbol(), "·");
    assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "│");
    assert_eq!(
        terminal.backend().buffer()[(1, 1)].fg,
        crate::ui::theme::DIVIDER
    );

    app.config.terminal_sidebar = wsx_core::config::global::TerminalSidebar::Expanded;
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.tree_area.width, 32);
    let terminal_projects = (0..30)
        .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
        .collect::<String>();
    assert!(terminal_projects.contains("demo"), "{terminal_projects:?}");
    assert_eq!(terminal.backend().buffer()[(31, 1)].symbol(), "│");
    assert_eq!(
        terminal.backend().buffer()[(31, 1)].fg,
        crate::ui::theme::DIVIDER
    );
    let terminal_footer = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 15)].symbol())
        .collect::<String>();
    assert_eq!(
        terminal.backend().buffer()[(0, 15)].bg,
        crate::ui::theme::mode_badge(crate::ui::theme::ModeBadge::Terminal)
            .bg
            .unwrap()
    );
    assert_ne!(
        terminal.backend().buffer()[(0, 15)].bg,
        crate::ui::theme::mode_badge(crate::ui::theme::ModeBadge::Navigation)
            .bg
            .unwrap()
    );
    assert_eq!(
        app.terminal_command_hints(),
        vec![
            "(alt+g)commands",
            "(z)workspace",
            "({/})group",
            crate::ui::IDLE_ITERATION_HINT,
            crate::ui::ACTIVE_ITERATION_HINT,
            crate::ui::ATTENTION_ITERATION_HINT,
            "(b)sidebar",
            concat!("(q)", "quit"),
        ]
    );
    // This project has no session to switch to, so the session hint is absent.
    for hint in ["(alt+g)commands", "(z)workspace", "({/})group"] {
        assert!(
            terminal_footer.contains(hint),
            "missing {hint}: {terminal_footer:?}"
        );
    }
    assert!(
        !terminal_footer.contains("(esc)cancel"),
        "{terminal_footer:?}"
    );
    assert_eq!(
        terminal.backend().buffer()[(11, 15)].fg,
        crate::ui::theme::TEXT_MUTED
    );

    app.config.terminal_sidebar = TerminalSidebar::Compact;
    let _ = app
        .terminal_escape_chord
        .terminal_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('g'),
            crossterm::event::KeyModifiers::ALT,
        ));
    assert!(app.terminal_prefix_pending());
    assert_eq!(
        app.terminal_command_hints(),
        vec![
            "(alt+g)commands",
            "(esc)cancel",
            "(z)workspace",
            "({/})group",
            crate::ui::IDLE_ITERATION_HINT,
            crate::ui::ACTIVE_ITERATION_HINT,
            crate::ui::ATTENTION_ITERATION_HINT,
            "(b)sidebar",
            concat!("(q)", "quit"),
            "(alt+g)send",
        ]
    );
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.tree_area.width, 32);
    assert_eq!(app.preview_area.x, 32);
    let prefix_footer = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 15)].symbol())
        .collect::<String>();
    assert!(prefix_footer.contains(" TERMINAL "), "{prefix_footer:?}");
    for hint in ["(alt+g)commands", "(esc)cancel", "(z)workspace"] {
        assert!(
            prefix_footer.contains(hint),
            "missing {hint}: {prefix_footer:?}"
        );
    }
    assert_eq!(
        terminal.backend().buffer()[(11, 15)].fg,
        crate::ui::theme::ACCENT
    );

    assert_eq!(
        app.terminal_escape_chord
            .terminal_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            )),
        crate::event::TerminalEscapeAction::Cancel
    );
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.tree_area.width, 2);
    assert_eq!(app.preview_area.x, 2);
    assert!(matches!(app.mode, Mode::Terminal { .. }));
}

#[test]
fn compact_terminal_sidebar_mirrors_session_status_and_preserves_terminal_geometry() {
    let mut project = make_project("compact");
    project.last_terminal_active_unix_ms = Some(0);
    let mut worktree = make_worktree("/tmp/compact");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Blocked)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.tree_selected = 2;
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    app.runtime_health = RuntimeHealth::Healthy {
        last_success: Instant::now(),
    };
    let stale_path = app.workspace.projects[0].path.clone();
    app.stale_collapsed_projects.insert(stale_path);
    let backend = ratatui::backend::TestBackend::new(80, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert_eq!(app.tree_area, Rect::new(0, 1, 2, 8));
    assert_eq!(app.preview_area, Rect::new(2, 1, 78, 8));
    assert_eq!(app.terminal_area, Rect::new(2, 1, 78, 7));
    let bottom_title = (2..80)
        .map(|x| terminal.backend().buffer()[(x, 8)].symbol())
        .collect::<String>();
    assert!(bottom_title.contains("compact"), "{bottom_title:?}");
    let footer = (0..80)
        .map(|x| terminal.backend().buffer()[(x, 9)].symbol())
        .collect::<String>();
    assert!(footer.contains("TERMINAL"), "{footer:?}");
    assert_eq!(terminal.backend().buffer()[(0, 2)].symbol(), "·");
    assert_eq!(terminal.backend().buffer()[(0, 3)].symbol(), "▾");
    assert_eq!(terminal.backend().buffer()[(0, 4)].symbol(), "◐");
    assert_eq!(
        terminal.backend().buffer()[(0, 4)].fg,
        crate::ui::theme::TEXT
    );
    assert_eq!(
        terminal.backend().buffer()[(0, 4)].bg,
        crate::ui::theme::selected_row(false).bg.unwrap()
    );
    for y in 1..9 {
        assert_eq!(terminal.backend().buffer()[(1, y)].symbol(), "│");
        assert_eq!(
            terminal.backend().buffer()[(1, y)].fg,
            crate::ui::theme::DIVIDER
        );
    }

    app.mode = Mode::Workspace;
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.terminal_area, Rect::new(32, 1, 48, 7));
    let workspace_bottom_title = (32..80)
        .map(|x| terminal.backend().buffer()[(x, 8)].symbol())
        .collect::<String>();
    assert!(
        workspace_bottom_title.contains("compact"),
        "{workspace_bottom_title:?}"
    );

    app.config.terminal_title_position = wsx_core::config::global::TerminalTitlePosition::Top;
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.terminal_area, Rect::new(32, 2, 48, 7));
    let workspace_top_title = (32..80)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect::<String>();
    assert!(
        workspace_top_title.contains("compact"),
        "{workspace_top_title:?}"
    );
}

#[test]
fn contextual_titlebar_tracks_the_current_session_without_changing_terminal_geometry() {
    use wsx_core::config::global::{TerminalSidebar, TerminalTitlePosition};
    let mut project = make_project("demo");
    let mut main = make_worktree("/demo/main");
    main.sessions = vec![
        make_sess_with_id(1, runtime::AgentState::Working),
        make_sess_with_id(2, runtime::AgentState::Idle),
        make_sess_with_id(3, runtime::AgentState::Done),
    ];
    let mut fix = make_worktree("/demo/fix");
    fix.name = "fix".into();
    fix.expanded = false;
    fix.sessions = vec![make_sess_with_id(4, runtime::AgentState::Blocked)];
    project.worktrees = vec![main, fix];
    let mut other = make_project("other");
    let mut other_worktree = make_worktree("/other/main");
    let mut outsider = make_sess_with_id(5, runtime::AgentState::Blocked);
    outsider.display_name = "outside-project".into();
    other_worktree.sessions = vec![outsider];
    other.worktrees = vec![other_worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project, other],
        },
        None,
    );
    app.tree_selected = 2;
    app.runtime_health = RuntimeHealth::Healthy {
        last_success: Instant::now(),
    };
    for width in [120, 80, 56] {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 18)).unwrap();
        for sidebar in [TerminalSidebar::Compact, TerminalSidebar::Expanded] {
            app.config.terminal_sidebar = sidebar;
            for position in [TerminalTitlePosition::Bottom, TerminalTitlePosition::Top] {
                app.config.terminal_title_position = position;
                for interactive in [true, false] {
                    app.mode = if interactive {
                        Mode::Terminal {
                            pane_id: runtime::PaneId(1),
                        }
                    } else {
                        Mode::Workspace
                    };
                    terminal
                        .draw(|frame| crate::ui::render(frame, &mut app))
                        .unwrap();
                    if width < 60 && !interactive {
                        assert_eq!(app.preview_area, Rect::default());
                        continue;
                    }
                    let left = if width < 60 {
                        0
                    } else if interactive && sidebar == TerminalSidebar::Compact {
                        2
                    } else {
                        32
                    };
                    let title_y = if position == TerminalTitlePosition::Top {
                        1
                    } else {
                        16
                    };
                    let viewport_y = if position == TerminalTitlePosition::Top {
                        2
                    } else {
                        1
                    };
                    assert_eq!(
                        app.terminal_area,
                        Rect::new(left, viewport_y, width - left, 15)
                    );
                    let row: String = (left..width)
                        .map(|x| terminal.backend().buffer()[(x, title_y)].symbol())
                        .collect();
                    assert!(row.contains("session-1") && row.contains('◎'), "{row}");
                    assert!(!row.contains(['[', ']']), "{row}");
                    assert!(!row.contains("outside-project"), "{row}");
                    assert!(!row.contains(':'));
                    if width == 120 {
                        assert!(
                            row.find("session-2").unwrap() < row.find("session-3").unwrap(),
                            "{row}"
                        );
                    }
                    for x in left..width {
                        assert_ne!(
                            terminal.backend().buffer()[(x, title_y)].bg,
                            ratatui::style::Color::Reset
                        );
                        assert_eq!(
                            terminal.backend().buffer()[(x, viewport_y)].bg,
                            ratatui::style::Color::Reset
                        );
                    }
                }
            }
        }
    }
    // Selection, not cached title state, decides which identity is primary.
    app.tree_selected = session_position(&app, 1);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(2),
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 18)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let row: String = (32..120)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect();
    assert!(row.contains("session-2") && row.contains('○'), "{row}");
    assert!(!row.contains(['[', ']']), "{row}");
    assert_eq!(app.current_selection(), Selection::Session(0, 0, 1));
}

#[test]
fn compact_terminal_sidebar_tiny_height_degrades_without_panicking() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    let backend = ratatui::backend::TestBackend::new(60, 2);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert_eq!(app.tree_area, Rect::new(0, 1, 2, 0));
    assert_eq!(app.preview_area, Rect::new(2, 1, 58, 0));
    assert_eq!(app.terminal_area, Rect::default());
}

#[test]
fn help_describes_done_acknowledgement_before_mute() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Help;
    let backend = ratatui::backend::TestBackend::new(80, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(
        rendered.contains("Acknowledge done, otherwise toggle ⊘ mute"),
        "{rendered:?}"
    );
}

#[test]
fn help_actions_render_on_the_popup_bottom_border() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Help;
    let backend = ratatui::backend::TestBackend::new(40, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let bottom_y = (0..12)
        .find(|&y| {
            (0..40)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
                .contains("Esc close")
        })
        .expect("help hint");
    assert_eq!(terminal.backend().buffer()[(0, bottom_y)].symbol(), "└");
    assert_eq!(terminal.backend().buffer()[(39, bottom_y)].symbol(), "┘");
    for y in 0..bottom_y {
        let row = (0..40)
            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
            .collect::<String>();
        assert!(!row.contains("Esc close"), "{row:?}");
    }
}

#[test]
fn help_advertises_terminal_idle_and_active_iteration() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Help;
    let backend = ratatui::backend::TestBackend::new(100, 50);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Prefix+I / i"), "{rendered:?}");
    assert!(rendered.contains("Prefix+A / a"), "{rendered:?}");
}

#[test]
fn mobile_workspace_focus_box_uses_the_content_bounds() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.runtime_health = RuntimeHealth::Healthy {
        last_success: Instant::now(),
    };
    let backend = ratatui::backend::TestBackend::new(40, 8);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    assert!(app.is_mobile);
    for ((x, y), symbol) in [((0, 1), "┌"), ((39, 1), "┐"), ((0, 6), "└"), ((39, 6), "┘")] {
        assert_eq!(terminal.backend().buffer()[(x, y)].symbol(), symbol);
    }
}

#[test]
fn workspace_footer_advertises_session_state_navigation() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let backend = ratatui::backend::TestBackend::new(160, 8);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let footer = (0..160)
        .map(|x| terminal.backend().buffer()[(x, 7)].symbol())
        .collect::<String>();
    for hint in [
        crate::ui::IDLE_ITERATION_HINT,
        crate::ui::ACTIVE_ITERATION_HINT,
        crate::ui::ATTENTION_ITERATION_HINT,
    ] {
        assert!(footer.contains(hint), "missing {hint}: {footer:?}");
    }
}

#[test]
fn workspace_footer_advertises_move_for_worktrees_and_sessions() {
    let mut project = make_project("move-hints");
    let mut worktree = make_worktree("/tmp/move-hints");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Idle)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let backend = ratatui::backend::TestBackend::new(160, 8);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    for selection in [Selection::Worktree(0, 0), Selection::Session(0, 0, 0)] {
        select_rendered_navigation_entry(&mut app, selection);
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();
        let footer = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 7)].symbol())
            .collect::<String>();
        assert!(footer.contains("(m)ove"), "{footer:?}");
    }
}

#[test]
fn workspace_footer_shows_compact_capability_aware_routine_hints() {
    let mut project = make_project("routines");
    project.routines = vec![routine_view("build")];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let routine_index = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Routine { .. }))
        .unwrap();
    app.tree_selected = routine_index;
    let backend = ratatui::backend::TestBackend::new(160, 8);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let footer = (0..160)
        .map(|x| terminal.backend().buffer()[(x, 7)].symbol())
        .collect::<String>();
    assert!(footer.contains("(e)dit"), "{footer:?}");
    assert!(footer.contains("(d)elete"), "{footer:?}");

    app.workspace.projects[0].routines[0].capabilities.can_edit = false;
    app.workspace.projects[0].routines[0]
        .capabilities
        .can_delete = false;
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let footer = (0..160)
        .map(|x| terminal.backend().buffer()[(x, 7)].symbol())
        .collect::<String>();
    assert!(!footer.contains("(e)dit"), "{footer:?}");
    assert!(!footer.contains("(d)elete"), "{footer:?}");
    assert!(footer.contains("(?)help"), "{footer:?}");
}

#[test]
fn startup_restores_valid_group_and_defaults_missing_or_invalid_to_ungrouped() {
    let config = GlobalConfig {
        groups: vec!["recent".into(), "work".into()],
        ..GlobalConfig::default()
    };

    assert_eq!(initial_active_group(&config, None), GroupKey::Ungrouped);
    assert_eq!(
        initial_active_group(&config, Some(GroupKey::Named("deleted".into()))),
        GroupKey::Ungrouped
    );
    assert_eq!(
        initial_active_group(&config, Some(GroupKey::Named("work".into()))),
        GroupKey::Named("work".into())
    );
}

#[test]
fn workspace_footer_uses_normal_unregister_behavior_without_recent_hint() {
    let mut project = make_project("registered");
    project.last_terminal_active_unix_ms = Some(u64::MAX);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let backend = ratatui::backend::TestBackend::new(100, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let footer = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 11)].symbol())
        .collect::<String>();
    assert!(footer.contains("(e)dit"), "{footer:?}");
    assert!(footer.contains("(u)routine"), "{footer:?}");
    assert!(footer.contains("(d)unregister"), "{footer:?}");
    assert!(!footer.contains("remove recent"), "{footer:?}");
    assert!(footer.contains("(,)config"), "{footer:?}");
    assert!(footer.contains("(q)uit"), "{footer:?}");
    let malformed_quit_hint = ["(q)", "quit"].concat();
    assert!(!footer.contains(&malformed_quit_hint), "{footer:?}");
    let wake = if cfg!(target_os = "macos") {
        "☕︎"
    } else {
        ""
    };
    assert!(
        footer.contains(&format!("(q)uit  {wake}v{}", env!("CARGO_PKG_VERSION"))),
        "{footer:?}"
    );
}

#[test]
fn workspace_header_scrolls_to_keep_the_active_group_visible() {
    let project = make_project("demo");
    let groups = (0..10).map(|index| format!("group-{index}")).collect();
    let config = GlobalConfig {
        groups,
        projects: vec![wsx_core::config::global::ProjectEntry {
            name: "demo".into(),
            path: project.path.clone(),
            groups: vec!["group-9".into()],
            aliases: HashMap::new(),
            worktree_order: Vec::new(),
            profile: None,
        }],
        ..GlobalConfig::default()
    };
    let mut app = make_test_app(
        config,
        WorkspaceState {
            projects: vec![project],
        },
        Some("group-9".into()),
    );
    let backend = ratatui::backend::TestBackend::new(100, 16);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let header = (0..100)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(header.contains("workspace"), "{header:?}");
    assert!(header.contains("group-9"), "{header:?}");
    assert!(header.contains('‹'), "{header:?}");
    assert!(!header.contains("… +"), "{header:?}");
}

#[test]
fn mobile_terminal_mode_places_title_without_changing_viewport_size() {
    let mut project = make_project("mobile-terminal");
    let mut worktree = make_worktree("/tmp/mobile-terminal");
    let session = make_sess(false, runtime::AgentState::Idle);
    let terminal_frame = runtime::TerminalFrame {
        pane_id: runtime::PaneId(1),
        terminal_id: runtime::TerminalId(1),
        revision: 1,
        cols: 5,
        rows: 1,
        cells: "hello"
            .chars()
            .map(|ch| runtime::Cell {
                symbol: ch.to_string(),
                ..runtime::Cell::default()
            })
            .collect(),
        cursor: runtime::Cursor {
            x: 0,
            y: 0,
            visible: false,
            blinking: false,
            shape: 0,
        },
        selection: Vec::new(),
    };
    worktree.sessions.push(session);
    project.worktrees.push(worktree);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.terminal_surfaces.reconcile(&runtime::Snapshot {
        protocol: runtime::PROTOCOL_VERSION,
        epoch: 1,
        revision: 1,
        projects: vec![],
        worktrees: vec![],
        sessions: vec![],
        panes: vec![runtime::Pane {
            id: runtime::PaneId(1),
            terminal_id: runtime::TerminalId(1),
            session_id: runtime::SessionId(1),
            label: "terminal".into(),
            agent: None,
            exited: false,
            revision: 1,
        }],
        listening_ports: vec![],
        pane_activity: vec![],
        plugin_sidecars: Vec::new(),
        capabilities: runtime::Capabilities::default(),
    });
    assert_eq!(
        app.terminal_surfaces.install_full(1, terminal_frame),
        SurfaceUpdate::Applied
    );
    app.visible_projects.insert(0);
    app.cached_flat = flatten_tree_filtered(&app.workspace, &app.visible_projects);
    app.flat_dirty = false;
    app.tree_selected = 2;
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    app.runtime_health = RuntimeHealth::Healthy {
        last_success: Instant::now(),
    };
    let backend = ratatui::backend::TestBackend::new(56, 16);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert!(app.is_mobile);
    assert_eq!(app.tree_area, Rect::default());
    assert_eq!(app.group_header_area, Rect::new(0, 0, 56, 1));
    assert_eq!(app.preview_area, Rect::new(0, 1, 56, 14));
    assert_eq!(app.terminal_area, Rect::new(0, 1, 56, 13));
    let bottom_title = (0..56)
        .map(|x| terminal.backend().buffer()[(x, 14)].symbol())
        .collect::<String>();
    assert!(
        bottom_title.contains("mobile-terminal") && bottom_title.contains("main ›"),
        "{bottom_title:?}"
    );
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("workspace"), "{rendered:?}");
    assert!(
        rendered.contains("mobile-terminal") && rendered.contains("main ›"),
        "{rendered:?}"
    );
    assert!(rendered.contains("hello"), "{rendered:?}");
    assert!(!rendered.contains(")sidebar"), "{rendered:?}");

    app.config.terminal_title_position = wsx_core::config::global::TerminalTitlePosition::Top;
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert_eq!(app.terminal_area, Rect::new(0, 2, 56, 13));
    let top_title = (0..56)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect::<String>();
    assert!(
        top_title.contains("mobile-terminal") && top_title.contains("main ›"),
        "{top_title:?}"
    );
}

#[test]
fn state_targets_follow_visible_order_in_both_directions_and_wrap() {
    let mut project = make_project("state-order");
    let mut worktree = make_worktree("/tmp/state-order");
    let idle_first = make_sess_with_id(1, runtime::AgentState::Idle);
    let active_first = make_sess_with_id(2, runtime::AgentState::Working);
    let idle_second = make_sess_with_id(3, runtime::AgentState::Idle);
    let active_second = make_sess_with_id(4, runtime::AgentState::Working);
    let blocked = make_sess_with_id(5, runtime::AgentState::Blocked);
    let done = make_sess_with_id(6, runtime::AgentState::Done);
    let mut muted = make_sess_with_id(7, runtime::AgentState::Blocked);
    muted.muted = true;
    worktree.sessions = vec![
        idle_first,
        active_first,
        idle_second,
        active_second,
        blocked,
        done,
        muted,
    ];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig {
            attention_priority: AttentionPriority::WorkspaceOrder,
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.tree_selected = session_position(&app, 0);
    assert_eq!(app.active_target(1), Some(session_position(&app, 1)));
    assert_eq!(app.active_target(-1), Some(session_position(&app, 3)));

    app.tree_selected = session_position(&app, 1);
    assert_eq!(app.idle_target(1), Some(session_position(&app, 2)));
    assert_eq!(app.idle_target(-1), Some(session_position(&app, 0)));

    app.tree_selected = session_position(&app, 2);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 4)));
    app.tree_selected = session_position(&app, 4);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 5)));
    assert_eq!(app.attention_target(-1), Some(session_position(&app, 5)));
    app.tree_selected = session_position(&app, 5);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 4)));
}

#[test]
fn project_context_cycle_shares_title_order_and_reveals_collapsed_targets() {
    let mut project = make_project("context");
    let mut main = make_worktree("/context/main");
    main.sessions = vec![
        make_sess_with_id(1, runtime::AgentState::Done),
        make_sess_with_id(2, runtime::AgentState::Working),
        make_sess_with_id(3, runtime::AgentState::Idle),
        make_sess_with_id(4, runtime::AgentState::Blocked),
        make_sess_with_id(5, runtime::AgentState::Error),
        make_sess_with_id(6, runtime::AgentState::Blocked),
        make_sess_with_id(7, runtime::AgentState::Unknown),
    ];
    main.sessions[5].muted = true;
    main.sessions[6].agent = None;
    main.sessions[6].panes[0].agent = None;
    main.sessions[6].panes[0].foreground_job = true;
    let mut fix = make_worktree("/context/fix");
    fix.expanded = false;
    fix.sessions = vec![
        make_sess_with_id(8, runtime::AgentState::Blocked),
        make_sess_with_id(9, runtime::AgentState::Done),
    ];
    fix.sessions[1].outcome_acknowledged = true;
    project.worktrees = vec![main, fix];
    let mut outsider = make_project("outside");
    let mut wt = make_worktree("/outside/main");
    wt.sessions = vec![make_sess_with_id(10, runtime::AgentState::Blocked)];
    outsider.worktrees = vec![wt];
    let mut app = make_test_app(
        GlobalConfig {
            attention_priority: AttentionPriority::WorkspaceOrder,
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project, outsider],
        },
        None,
    );
    let order = [
        (0, 0, 0),
        (0, 0, 1),
        (0, 0, 2),
        (0, 0, 3),
        (0, 0, 4),
        (0, 0, 5),
        (0, 0, 6),
        (0, 1, 0),
        (0, 1, 1),
    ];
    assert!(!app.workspace.projects[0].worktrees[1].expanded);
    app.tree_selected = app.reveal_session(0, 0, 6).unwrap();
    assert_eq!(app.context_session_target(1), Some((0, 1, 0)));
    app.tree_selected = app.reveal_session(0, 1, 0).unwrap();
    assert!(app.workspace.projects[0].worktrees[1].expanded);
    for (index, &(pi, wi, si)) in order.iter().enumerate() {
        app.tree_selected = app.reveal_session(pi, wi, si).unwrap();
        assert_eq!(
            app.context_session_target(1),
            Some(order[(index + 1) % order.len()])
        );
        assert_eq!(
            app.context_session_target(-1),
            Some(order[(index + order.len() - 1) % order.len()])
        );
    }
    // Live lifecycle/acknowledgement changes do not reshuffle spatial navigation.
    app.workspace.projects[0].worktrees[1].sessions[0].outcome_acknowledged = false;
    app.workspace.projects[0].worktrees[1].sessions[0].agent_status = runtime::AgentState::Done;
    app.tree_selected = app.reveal_session(0, 0, 4).unwrap();
    assert_eq!(app.context_session_target(1), Some((0, 0, 5)));
    app.workspace.projects[0].worktrees[1].sessions[0].outcome_acknowledged = true;
    app.tree_selected = app.reveal_session(0, 0, 0).unwrap();
    assert_eq!(app.context_session_target(1), Some((0, 0, 1)));
    app.workspace.projects[0].worktrees[0].sessions.clear();
    app.workspace.projects[0].worktrees[1].sessions.truncate(1);
    app.tree_selected = app.reveal_session(0, 1, 0).unwrap();
    assert_eq!(app.context_session_target(1), None);
    assert_eq!(app.context_session_target(-1), None);
}

#[test]
fn blocked_first_attention_priority_applies_in_both_directions_until_blockers_clear() {
    let mut project = make_project("blocked-first");
    let mut worktree = make_worktree("/tmp/blocked-first");
    worktree.sessions = vec![
        make_sess_with_id(1, runtime::AgentState::Done),
        make_sess_with_id(2, runtime::AgentState::Blocked),
        make_sess_with_id(3, runtime::AgentState::Done),
        make_sess_with_id(4, runtime::AgentState::Blocked),
        make_sess_with_id(5, runtime::AgentState::Error),
    ];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.tree_selected = session_position(&app, 0);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 1)));
    assert_eq!(app.attention_target(-1), Some(session_position(&app, 3)));
    app.tree_selected = session_position(&app, 1);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 3)));
    app.tree_selected = session_position(&app, 3);
    assert_eq!(app.attention_target(-1), Some(session_position(&app, 1)));

    for session in &mut app.workspace.projects[0].worktrees[0].sessions {
        if session.agent_status == runtime::AgentState::Blocked {
            session.muted = true;
        }
    }
    app.tree_selected = session_position(&app, 0);
    assert_eq!(app.attention_target(1), Some(session_position(&app, 2)));
    assert_eq!(app.attention_target(-1), Some(session_position(&app, 4)));
}

#[test]
fn blocked_session_needs_attention() {
    let session = make_sess(false, wsx_core::runtime::AgentState::Blocked);
    assert!(session_needs_attention(&session));
}

#[test]
fn muted_blocked_session_does_not_need_attention() {
    let session = make_sess(true, wsx_core::runtime::AgentState::Blocked);
    assert!(!session_needs_attention(&session));
}

#[test]
fn explicit_interaction_acknowledges_only_the_current_done_revision() {
    let mut project = make_project("acknowledge");
    let mut worktree = make_worktree("./acknowledge");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Done)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.unmute_on_interaction(runtime::PaneId(1));

    let session = &app.workspace.projects[0].worktrees[0].sessions[0];
    assert!(session.outcome_acknowledged);
    assert!(session.panes[0].outcome_acknowledged);
    assert_eq!(
        session_state::derive(session),
        session_state::SessionHeuristic::Idle
    );
    assert_eq!(app.acknowledged_outcomes.get("1"), Some(&1));

    let session = &mut app.workspace.projects[0].worktrees[0].sessions[0];
    session.panes[0].revision = 2;
    session.panes[0].outcome_acknowledged = false;
    session.outcome_acknowledged = false;
    assert_eq!(
        session_state::derive(session),
        session_state::SessionHeuristic::Done
    );
}

#[test]
fn dismiss_done_acknowledges_before_toggling_mute() {
    let mut project = make_project("dismiss-done");
    let mut worktree = make_worktree("./dismiss-done");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Done)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.tree_selected = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { .. }))
        .unwrap();
    assert_eq!(app.attention_candidates(), vec![app.tree_selected]);

    app.action_dismiss_attention();

    let session = &app.workspace.projects[0].worktrees[0].sessions[0];
    assert!(session.outcome_acknowledged);
    assert!(!session.muted);
    assert_eq!(
        session_state::derive(session),
        session_state::SessionHeuristic::Idle
    );
    assert!(app.attention_candidates().is_empty());
    assert_eq!(app.acknowledged_outcomes.get("1"), Some(&1));

    app.action_dismiss_attention();

    let session = &app.workspace.projects[0].worktrees[0].sessions[0];
    assert!(session.outcome_acknowledged);
    assert!(session.muted);
    assert!(app.muted_terminal_ids.contains("1"));
}

fn worktree_entry(path: &str) -> git_worktree::WorktreeEntry {
    git_worktree::WorktreeEntry {
        name: path.to_string(),
        path: PathBuf::from(path),
        branch: "branch".to_string(),
        is_main: false,
    }
}

#[test]
fn add_worktree_prefills_the_editable_project_branch_prefix() {
    let mut project = make_project("prefixed");
    project.config = Some(wsx_core::model::workspace::ProjectConfig {
        branch_prefix: "feature/".into(),
        ..Default::default()
    });
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.action_add_worktree().unwrap();

    let Mode::Input { context, state } = &mut app.mode else {
        panic!("expected worktree branch input");
    };
    assert!(matches!(
        context,
        InputContext::AddWorktree { project_idx: 0 }
    ));
    assert_eq!(state.value(), "feature/");
    state.insert_char(char::from_digit(4, 10).unwrap());
    assert_eq!(state.value(), "feature/4");
}

#[test]
fn partial_default_session_failure_keeps_worktree_refresh_and_error_visible() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.jobs.push(BgJob {
        id: 1,
        label: "create feature/4".into(),
    });

    app.apply_bg_result(BgResult {
        id: 1,
        label: "create feature/4".into(),
        outcome: Ok(BgOutcome::WorktreeCreated {
            label: "Created worktree: feature/4".into(),
            warning: Some("Warning: postCreate: exited 1".into()),
            session_error: Some("daemon unavailable".into()),
        }),
    });

    assert!(app.runtime_refresh_pending);
    let notice = app.notice.as_ref().expect("partial failure notice");
    assert_eq!(notice.level, NoticeLevel::Error);
    assert!(notice.title.contains("default session failed"));
    assert!(notice.body.as_deref().is_some_and(|body| {
        body.contains("daemon unavailable") && body.contains("postCreate")
    }));
}

#[test]
fn successful_background_session_close_removes_pane_and_registers_tombstone() {
    let mut project = make_project("demo");
    let mut worktree = make_worktree("/tmp/demo");
    worktree.sessions = vec![make_sess(false, wsx_core::runtime::AgentState::Idle)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.jobs.push(BgJob {
        id: 1,
        label: "kill session".into(),
    });

    app.apply_bg_result(BgResult {
        id: 1,
        label: "kill session".into(),
        outcome: Ok(BgOutcome::SessionKilled {
            session_id: runtime::SessionId(1),
            display_name: "session".into(),
        }),
    });

    assert!(app.workspace.projects[0].worktrees[0].sessions.is_empty());
    assert!(app.pending_session_kills.contains(&runtime::SessionId(1)));
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("Killed session: session")
    );
}

#[test]
fn daemon_upgrade_outcomes_are_actionable() {
    let replaced = runtime_availability_notice(&runtime::Availability::DaemonReplaced {
        previous_version: "0.24.0".into(),
    })
    .unwrap();
    assert_eq!(replaced.0, NoticeLevel::Success);
    assert!(replaced.1.contains("Runtime upgraded"), "{}", replaced.1);
    assert!(replaced.1.contains("0.24.0"), "{}", replaced.1);

    let deferred = runtime_availability_notice(&runtime::Availability::ReplacementDeferred {
        daemon_version: "0.24.0".into(),
        target_version: "0.25.0".into(),
        live_runtimes: 4,
        blockers: vec![
            runtime::ReplacementBlocker::OtherTui,
            runtime::ReplacementBlocker::WorkingAgent,
            runtime::ReplacementBlocker::ForegroundJob,
            runtime::ReplacementBlocker::ListeningPort,
        ],
    })
    .unwrap();
    assert_eq!(deferred.0, NoticeLevel::Warning);
    assert!(deferred.1.contains("is waiting"), "{}", deferred.1);
    assert!(
        deferred.1.contains("Quit the other wsx window"),
        "{}",
        deferred.1
    );
    assert!(deferred.1.contains("4"), "{}", deferred.1);

    let legacy = runtime_availability_notice(&runtime::Availability::LegacyCompatible).unwrap();
    assert_eq!(legacy.0, NoticeLevel::Warning);
    assert!(legacy.1.contains("Legacy wsxd"), "{}", legacy.1);

    assert!(runtime_availability_notice(&runtime::Availability::Current).is_none());
    assert!(
        runtime_availability_notice(&runtime::Availability::NewerDaemon {
            daemon_version: runtime::WSX_VERSION.into(),
        })
        .is_none()
    );

    let newer = runtime_availability_notice(&runtime::Availability::NewerDaemon {
        daemon_version: "99.0.0".into(),
    })
    .unwrap()
    .1;
    assert!(newer.contains("open wsx 99.0.0"), "{newer}");
}

#[test]
fn all_app_notice_levels_expire_after_the_configured_timeout() {
    let mut app = make_test_app(
        GlobalConfig {
            notification_timeout_seconds: 2,
            ..GlobalConfig::default()
        },
        WorkspaceState::empty(),
        None,
    );
    let started = Instant::now();

    for level in [
        NoticeLevel::Success,
        NoticeLevel::Warning,
        NoticeLevel::Error,
    ] {
        app.set_notice_at(level, "notice", started);
        app.expire_notice(started + Duration::from_secs(1));
        assert!(app.notice.is_some());
        app.expire_notice(started + Duration::from_secs(2));
        assert!(app.notice.is_none());
    }
}

#[test]
fn notice_bursts_preserve_minimum_dwell_and_fifo_order() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let started = Instant::now();
    app.set_notice_at(NoticeLevel::Error, "first", started);
    app.set_notice_at(
        NoticeLevel::Success,
        "second",
        started + Duration::from_millis(100),
    );
    app.set_notice_at(
        NoticeLevel::Warning,
        "third",
        started + Duration::from_millis(200),
    );

    app.expire_notice(started + Duration::from_millis(1_999));
    assert_eq!(app.notice.as_ref().unwrap().title, "first");
    app.expire_notice(started + Duration::from_secs(2));
    assert_eq!(app.notice.as_ref().unwrap().title, "second");
    app.expire_notice(started + Duration::from_secs(4));
    assert_eq!(app.notice.as_ref().unwrap().title, "third");
    app.expire_notice(started + Duration::from_secs(8));
    assert!(app.notice.is_none());
}

#[test]
fn duplicate_notices_are_coalesced_without_extending_dwell() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let started = Instant::now();
    app.set_notice_at(NoticeLevel::Warning, "same", started);
    app.set_notice_at(
        NoticeLevel::Warning,
        "same",
        started + Duration::from_secs(1),
    );
    assert!(app.notice_queue.is_empty());
    assert_eq!(app.notice_started, Some(started));
}

#[test]
fn notice_queue_is_bounded_and_retains_new_errors() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let started = Instant::now();
    app.set_notice_at(NoticeLevel::Info, "active", started);
    for index in 0..NOTICE_QUEUE_CAPACITY {
        app.set_notice_at(
            NoticeLevel::Success,
            format!("progress {index}"),
            started + Duration::from_millis(1),
        );
    }
    app.set_notice_at(
        NoticeLevel::Error,
        "terminal failure",
        started + Duration::from_millis(2),
    );

    assert_eq!(app.notice_queue.len(), NOTICE_QUEUE_CAPACITY);
    assert_eq!(app.notice_queue.front().unwrap().title, "terminal failure");
    assert!(app
        .notice_queue
        .iter()
        .any(|notice| notice.title == "terminal failure"));
    assert!(!app
        .notice_queue
        .iter()
        .any(|notice| notice.title == "progress 0"));
}

#[test]
fn disconnect_notice_survives_an_immediate_success_notice() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let started = Instant::now();
    app.apply_runtime_event(runtime::EventSignal::Disconnected("socket closed".into()));
    app.notice_started = Some(started);
    app.set_notice_at(
        NoticeLevel::Success,
        "Runtime upgraded",
        started + Duration::from_millis(10),
    );

    assert_eq!(
        app.notice.as_ref().unwrap().title,
        "Runtime disconnected; retrying"
    );
    assert_eq!(app.notice_queue.len(), 1);
    app.expire_notice(started + NOTICE_MIN_DWELL);
    assert_eq!(app.notice.as_ref().unwrap().title, "Runtime upgraded");
}

#[test]
fn terminal_stream_transfer_projects_to_an_exact_warning() {
    let (level, message) = terminal_stream_failure_notice(&runtime::ApiError::new(
        "lease_transferred",
        "replacement controller acquired the pane",
    ));

    assert_eq!(level, NoticeLevel::Warning);
    assert_eq!(message, "Terminal control moved to another wsx instance");
}

#[test]
fn non_transfer_terminal_stream_failure_remains_an_error_with_details() {
    let error = runtime::ApiError::new("stream_disconnected", "socket closed");
    let (level, message) = terminal_stream_failure_notice(&error);

    assert_eq!(level, NoticeLevel::Error);
    assert_eq!(
        message,
        "Terminal stream: stream_disconnected: socket closed"
    );
}

#[test]
fn terminal_stream_errors_include_the_authoritative_compact_target() {
    let mut project = make_project("kgeditor");
    let mut worktree = make_worktree("/tmp/kgeditor-feature--312");
    worktree.name = "feature/#312".into();
    let mut session = make_sess(false, wsx_core::runtime::AgentState::Idle);
    session.display_name = "ss".into();
    session.panes[0].label = "terminal".into();
    let pane_id = session.pane_id;
    worktree.sessions.push(session);
    project.worktrees.push(worktree);
    let app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    let target = app.terminal_target_label(0, 0, 0, pane_id).unwrap();
    let error = std::io::Error::other("terminal_busy: pane has another writable controller");

    assert_eq!(target, "kgeditor › feature/#312 › ss › terminal");
    assert_eq!(
        terminal_stream_error_notice(&error, &target),
        "Terminal busy: another writable controller\nTarget: kgeditor › feature/#312 › ss › terminal"
    );
}

#[test]
fn initial_runtime_refresh_starts_monitor_only_after_upgrade_attempt() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    assert!(app._runtime_monitor.is_none());

    app.apply_runtime_refresh(Err(anyhow::anyhow!("initial probe failed")));

    assert!(app._runtime_monitor.is_some());
    assert!(matches!(
        app.runtime_health,
        RuntimeHealth::Reconnecting { .. }
    ));
}

#[test]
fn manual_refresh_queues_behind_an_inflight_snapshot() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.runtime_refresh_pending = true;

    app.refresh_all().unwrap();

    assert!(app.runtime_refresh_pending);
    assert!(app.runtime_full_refresh_stale);
    assert!(!app.runtime_refresh_stale);
}

#[test]
fn pending_deletion_stays_hidden_until_git_stops_reporting_it() {
    let deleted = PathBuf::from("/repo/issue");
    let mut pending = HashSet::from([deleted.clone()]);

    let visible = filter_pending_deletions(
        &mut pending,
        vec![(
            PathBuf::from("/repo"),
            vec![worktree_entry("/repo/issue"), worktree_entry("/repo/main")],
        )],
    );

    assert_eq!(visible[0].1.len(), 1);
    assert_eq!(visible[0].1[0].path, PathBuf::from("/repo/main"));
    assert!(pending.contains(&deleted));
}

#[test]
fn pending_deletion_clears_after_git_confirms_removal() {
    let deleted = PathBuf::from("/repo/issue");
    let mut pending = HashSet::from([deleted]);

    let visible = filter_pending_deletions(
        &mut pending,
        vec![(PathBuf::from("/repo"), vec![worktree_entry("/repo/main")])],
    );

    assert_eq!(visible[0].1.len(), 1);
    assert!(pending.is_empty());
}

#[test]
fn single_line_workspace_hints_and_compact_notice_render_safely() {
    let mut app = make_navigation_test_app();
    let session_index = app
        .flat()
        .iter()
        .position(|entry| matches!(entry, FlatEntry::Session { .. }))
        .unwrap();
    app.tree_selected = session_index;
    app.set_error(
        "Runtime 연결 실패: a long notification remains readable outside the project column",
    );
    let backend = ratatui::backend::TestBackend::new(56, 16);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    assert!(app.is_mobile);
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    let status_row = terminal.backend().buffer().area.height - 1;
    let status = (0..terminal.backend().buffer().area.width)
        .map(|x| terminal.backend().buffer()[(x, status_row)].symbol())
        .collect::<String>();
    assert!(status.contains(" WORKSPACE "));
    assert!(!status.contains("[WORKSPACE]"));
    assert!(status.contains("(C)interrupt"), "{status:?}");
    assert!(
        status.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "{status:?}"
    );
    assert!(!rendered.contains("S:prompt"));
    assert!(rendered.contains("Runtime"));
    assert!(rendered.contains('연'));
}

#[test]
fn tiny_input_geometry_renders_without_underflow() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    app.mode = Mode::Input {
        context: InputContext::AddProject,
        state: InputState::new("path: "),
    };
    let backend = ratatui::backend::TestBackend::new(1, 1);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn backend_disconnect_keeps_last_success_and_recovery_becomes_healthy() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let last_success = Instant::now();
    app.runtime_health = RuntimeHealth::Healthy { last_success };
    app.apply_runtime_event(runtime::EventSignal::Disconnected("socket closed".into()));
    assert!(matches!(
        app.runtime_health,
        RuntimeHealth::Reconnecting {
            last_success: Some(_),
            ..
        }
    ));
    app.apply_runtime_event(runtime::EventSignal::Connected(None));
    assert!(matches!(
        app.runtime_health,
        RuntimeHealth::Reconnecting { .. }
    ));
    assert!(app.runtime_refresh_pending);
    app.apply_projected_snapshot(
        runtime::Snapshot {
            protocol: runtime::PROTOCOL_VERSION,
            epoch: 1,
            revision: 1,
            projects: vec![],
            worktrees: vec![],
            sessions: vec![],
            panes: vec![],
            listening_ports: vec![],
            pane_activity: vec![],
            plugin_sidecars: Vec::new(),
            capabilities: runtime::Capabilities::default(),
        },
        None,
    );
    assert!(matches!(app.runtime_health, RuntimeHealth::Healthy { .. }));
    assert_eq!(
        app.notice.as_ref().map(|notice| notice.title.as_str()),
        Some("Runtime disconnected; retrying")
    );
}

#[test]
fn terminal_mouse_coordinates_start_at_the_panel_origin() {
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 42,
        row: 7,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    let projected = runtime_mouse_event(mouse, Rect::new(36, 1, 60, 20)).unwrap();
    assert_eq!((projected.x, projected.y), (6, 6));
    assert!(projected.in_bounds);
}

#[test]
fn terminal_mouse_release_and_vertical_drag_outside_the_panel_are_forwarded() {
    let release = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        column: 2,
        row: 30,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    let projected = runtime_mouse_event(release, Rect::new(36, 1, 60, 20)).unwrap();
    assert_eq!((projected.x, projected.y), (0, 19));
    assert!(!projected.in_bounds);

    let drag = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        ..release
    };
    let projected = runtime_mouse_event(drag, Rect::new(36, 1, 60, 20)).unwrap();
    assert_eq!((projected.x, projected.y), (0, 19));
    assert!(!projected.in_bounds);

    let above_drag = crossterm::event::MouseEvent {
        column: 42,
        row: 0,
        ..drag
    };
    let projected = runtime_mouse_event(above_drag, Rect::new(36, 1, 60, 20)).unwrap();
    assert_eq!((projected.x, projected.y), (6, 0));
    assert!(!projected.in_bounds);

    let horizontal_drag = crossterm::event::MouseEvent {
        column: 2,
        row: 7,
        ..drag
    };
    assert!(runtime_mouse_event(horizontal_drag, Rect::new(36, 1, 60, 20)).is_none());
}

#[test]
fn left_panel_click_is_a_workspace_target_while_terminal_is_focused() {
    let workspace = WorkspaceState {
        projects: vec![make_project("demo")],
    };
    let mut app = make_test_app(
        GlobalConfig {
            groups: vec!["work".into()],
            ..GlobalConfig::default()
        },
        workspace,
        None,
    );
    app.tree_area = Rect::new(0, 1, 36, 19);
    app.group_header_area = Rect::new(0, 0, 80, 1);
    app.mode = Mode::Terminal {
        pane_id: runtime::PaneId(1),
    };
    let left_click = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: 4,
        row: 3,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    let header_click = crossterm::event::MouseEvent {
        column: 40,
        row: 0,
        ..left_click
    };
    let right_click = crossterm::event::MouseEvent {
        column: 40,
        ..left_click
    };
    assert!(app.is_workspace_click(left_click));
    assert!(app.is_workspace_click(header_click));
    assert!(!app.is_workspace_click(right_click));

    let header_scroll = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        ..header_click
    };
    assert!(app.handle_terminal_group_header_scroll(header_scroll));
    assert_eq!(app.group_header_scroll, 1);
}

fn workspace_terminal() -> ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 20, 8)),
        },
    )
    .unwrap()
}

fn projects_for_tree(count: usize) -> WorkspaceState {
    WorkspaceState {
        projects: (0..count)
            .map(|index| make_project(&format!("project-{index}")))
            .collect(),
    }
}

#[test]
fn workspace_wheel_scrolls_three_rows_without_changing_a_visible_selection() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(10), None);
    app.tree_area = Rect::new(0, 1, 20, 4);
    app.tree_visible_height = 4;
    app.tree_scroll = 1;
    app.tree_selected = 4;
    let mut terminal = workspace_terminal();

    app.dispatch(
        Action::MouseScroll {
            col: 1,
            row: 2,
            delta: 1,
        },
        &mut terminal,
    )
    .unwrap();

    assert_eq!(
        (app.tree_scroll, app.current_selection()),
        (4, Selection::Project(4))
    );
}

#[test]
fn workspace_wheel_moves_selection_only_when_its_row_leaves_the_viewport() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(10), None);
    app.tree_area = Rect::new(0, 1, 20, 3);
    app.tree_visible_height = 3;
    app.tree_selected = 1;
    let mut terminal = workspace_terminal();

    app.dispatch(
        Action::MouseScroll {
            col: 1,
            row: 2,
            delta: 1,
        },
        &mut terminal,
    )
    .unwrap();

    assert_eq!(
        (app.tree_scroll, app.current_selection()),
        (3, Selection::Project(3))
    );
}

#[test]
fn workspace_header_wheel_changes_only_the_header_offset() {
    let config = GlobalConfig {
        groups: (0..10).map(|index| format!("group-{index}")).collect(),
        ..GlobalConfig::default()
    };
    let mut app = make_test_app(config, projects_for_tree(10), None);
    app.group_header_area = Rect::new(0, 0, 20, 1);
    app.tree_area = Rect::new(0, 1, 20, 4);
    app.tree_visible_height = 4;
    app.tree_scroll = 1;
    app.tree_selected = 2;
    let before = (app.tree_scroll, app.current_selection());
    let header_offset = app.group_header_scroll;
    let mut terminal = workspace_terminal();

    app.dispatch(
        Action::MouseScroll {
            col: 1,
            row: 0,
            delta: 1,
        },
        &mut terminal,
    )
    .unwrap();

    assert_eq!((app.tree_scroll, app.current_selection()), before);
    assert_ne!(app.group_header_scroll, header_offset);
}

#[test]
fn workspace_wheel_clamps_for_empty_and_bottom_viewports() {
    let mut empty = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    empty.tree_area = Rect::new(0, 1, 20, 0);
    empty.tree_visible_height = 0;
    let mut terminal = workspace_terminal();
    empty
        .dispatch(
            Action::MouseScroll {
                col: 1,
                row: 1,
                delta: 1,
            },
            &mut terminal,
        )
        .unwrap();

    let mut top = make_test_app(GlobalConfig::default(), projects_for_tree(4), None);
    top.tree_area = Rect::new(0, 1, 20, 3);
    top.tree_visible_height = 3;
    top.dispatch(
        Action::MouseScroll {
            col: 1,
            row: 2,
            delta: -1,
        },
        &mut terminal,
    )
    .unwrap();

    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(4), None);
    app.tree_area = Rect::new(0, 1, 20, 3);
    app.tree_visible_height = 3;
    app.tree_scroll = 1;
    app.tree_selected = 3;
    app.dispatch(
        Action::MouseScroll {
            col: 1,
            row: 2,
            delta: 1,
        },
        &mut terminal,
    )
    .unwrap();

    assert_eq!(
        (empty.tree_scroll, top.tree_scroll, app.tree_scroll),
        (0, 0, 1)
    );
}

fn project_with_available_session(name: &str) -> Project {
    let mut project = make_project(name);
    let mut worktree = make_worktree(&format!("/tmp/{name}"));
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Unknown)];
    project.worktrees = vec![worktree];
    project
}

#[test]
fn missing_activity_does_not_make_projects_immediately_stale() {
    let empty = make_project("empty");
    let mut exited = make_project("exited");
    let mut worktree = make_worktree("/tmp/exited");
    let mut session = make_sess(false, runtime::AgentState::Unknown);
    session.panes[0].exited = true;
    worktree.sessions = vec![session];
    exited.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![empty, exited],
        },
        None,
    );

    app.collapse_stale_projects();

    assert!(app
        .workspace
        .projects
        .iter()
        .all(|project| project.expanded));
    assert!(app.stale_project_indices().is_empty());
}

#[test]
fn expired_user_touch_collapses_a_project_without_sessions() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    let path = app.workspace.projects[0].path.clone();
    app.project_touched_unix_ms.insert(path, 0);

    app.collapse_stale_projects();

    assert!(!app.workspace.projects[0].expanded);
    assert_eq!(app.stale_project_indices(), HashSet::from([0]));
    assert!(app.cache_dirty);
}

#[test]
fn missing_activity_timestamps_do_not_stale_a_project_with_an_available_session() {
    let mut project = make_project("available");
    let mut worktree = make_worktree("/tmp/available");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Unknown)];
    project.worktrees = vec![worktree];
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
    assert!(!app.stale_project_indices().contains(&0));
}

#[test]
fn active_project_does_not_auto_collapse_or_project_stale_provenance() {
    let mut project = make_project("active");
    let mut worktree = make_worktree("/tmp/active");
    worktree.sessions = vec![make_sess(false, runtime::AgentState::Working)];
    project.worktrees = vec![worktree];
    project.last_agent_active_unix_ms = Some(0);
    project.last_terminal_active_unix_ms = Some(0);
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.collapse_stale_projects();
    assert!(app.workspace.projects[0].expanded);
    let path = app.workspace.projects[0].path.clone();
    app.workspace.projects[0].expanded = false;
    app.stale_collapsed_projects.insert(path.clone());
    app.collapse_stale_projects();
    assert!(
        !app.workspace.projects[0].expanded,
        "manual fold must remain"
    );
    assert!(app.stale_project_indices().is_empty());
    assert!(
        app.stale_collapsed_projects.contains(&path),
        "projection must not rewrite intent"
    );
}

#[test]
fn positive_auto_collapse_window_collapses_expired_activity_from_both_sources() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        projects_for_tree(1),
        None,
    );
    app.workspace.projects[0].last_agent_active_unix_ms = Some(0);
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(0);

    app.collapse_stale_projects();

    assert!(!app.workspace.projects[0].expanded);
}

#[test]
fn fresh_agent_activity_keeps_an_expanded_project_open() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project_with_available_session("agent-fresh")],
        },
        None,
    );
    let window_ms = app
        .config
        .auto_collapse_window_ms()
        .expect("positive automatic-collapse window");
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_millis() as u64;
    app.workspace.projects[0].last_agent_active_unix_ms = Some(now_unix_ms - window_ms / 2);

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
}

#[test]
fn fresh_terminal_activity_keeps_an_expanded_project_open() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project_with_available_session("terminal-fresh")],
        },
        None,
    );
    let window_ms = app
        .config
        .auto_collapse_window_ms()
        .expect("positive automatic-collapse window");
    let now_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_millis() as u64;
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(now_unix_ms - window_ms / 2);

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
}

#[test]
fn adaptive_window_credits_agent_terminal_and_explicit_user_activity_once_per_day() {
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project_with_available_session("adaptive-sources")],
        },
        None,
    );
    let path = app.workspace.projects[0].path.clone();
    let now = wsx_core::task::now_unix_ms();
    let day_ms = 24 * MILLIS_PER_HOUR;
    app.adaptive_collapse
        .insert(path.clone(), AdaptiveCollapseState::new(24, now));
    app.workspace.projects[0].last_agent_active_unix_ms = Some(now + day_ms);
    app.collapse_stale_projects();
    assert_eq!(app.adaptive_collapse[&path].window_hours, 36);

    app.workspace.projects[0].last_terminal_active_unix_ms = Some(now + 2 * day_ms);
    app.collapse_stale_projects();
    assert_eq!(app.adaptive_collapse[&path].window_hours, 48);

    let mut explicit = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    let explicit_path = explicit.workspace.projects[0].path.clone();
    let previous_utc_day_end = (now / day_ms).saturating_mul(day_ms).saturating_sub(1);
    explicit.adaptive_collapse.insert(
        explicit_path.clone(),
        AdaptiveCollapseState::new(24, previous_utc_day_end),
    );
    explicit.touch_project(0);
    assert_eq!(explicit.adaptive_collapse[&explicit_path].window_hours, 36);
    explicit.touch_project(0);
    assert_eq!(explicit.adaptive_collapse[&explicit_path].window_hours, 36);
}

#[test]
fn future_activity_keeps_an_expanded_project_open() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project_with_available_session("future")],
        },
        None,
    );
    app.workspace.projects[0].last_agent_active_unix_ms = Some(u64::MAX);

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
}

#[test]
fn zero_auto_collapse_window_leaves_an_expanded_inactive_project_open() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Disabled,
            ..GlobalConfig::default()
        },
        projects_for_tree(1),
        None,
    );
    app.workspace.projects[0].last_agent_active_unix_ms = Some(0);
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(0);

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
}

#[test]
fn already_collapsed_project_is_not_auto_expanded() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: 1 },
            ..GlobalConfig::default()
        },
        projects_for_tree(1),
        None,
    );
    app.workspace.projects[0].expanded = false;
    app.workspace.projects[0].last_agent_active_unix_ms = Some(u64::MAX);

    app.collapse_stale_projects();

    assert!(!app.workspace.projects[0].expanded);
}

#[test]
fn very_large_auto_collapse_window_does_not_overflow_and_keeps_old_activity_open() {
    let mut app = make_test_app(
        GlobalConfig {
            auto_collapse: AutoCollapsePolicy::Flat { hours: u64::MAX },
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project_with_available_session("large-window")],
        },
        None,
    );
    app.workspace.projects[0].last_agent_active_unix_ms = Some(0);

    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
}

#[test]
fn fresh_projects_keep_their_existing_expanded_state_from_either_activity_timestamp() {
    let mut agent_fresh = project_with_available_session("agent-fresh");
    agent_fresh.last_agent_active_unix_ms = Some(u64::MAX);
    agent_fresh.expanded = true;
    let mut terminal_fresh = project_with_available_session("terminal-fresh");
    terminal_fresh.last_terminal_active_unix_ms = Some(u64::MAX);
    terminal_fresh.expanded = false;
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![agent_fresh, terminal_fresh],
        },
        None,
    );

    app.collapse_stale_projects();

    assert_eq!(
        app.workspace
            .projects
            .iter()
            .map(|project| project.expanded)
            .collect::<Vec<_>>(),
        vec![true, false]
    );
}

#[test]
fn manually_expanded_project_refreshes_timer_and_clears_stale_cause() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    app.workspace.projects[0].expanded = false;
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(0);
    let path = app.workspace.projects[0].path.clone();
    app.stale_collapsed_projects.insert(path);
    let mut terminal = workspace_terminal();
    assert!(app.stale_project_indices().contains(&0));

    app.dispatch(Action::Select, &mut terminal).unwrap();
    app.collapse_stale_projects();
    app.collapse_stale_projects();

    assert!(app.workspace.projects[0].expanded);
    assert!(!app.stale_project_indices().contains(&0));
}

#[test]
fn stale_marker_records_collapse_cause_not_current_elapsed_time() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    let path = app.workspace.projects[0].path.clone();
    app.workspace.projects[0].expanded = false;
    app.project_touched_unix_ms.insert(path.clone(), 0);

    assert!(app.stale_project_indices().is_empty());

    app.stale_collapsed_projects.insert(path.clone());
    app.config.auto_collapse = AutoCollapsePolicy::Disabled;
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(u64::MAX);
    assert_eq!(app.stale_project_indices(), HashSet::from([0]));

    app.manually_expand_project(0);
    assert!(app.stale_project_indices().is_empty());
}

#[test]
fn stale_marker_survives_reconstruction_until_explicit_interaction() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    let path = app.workspace.projects[0].path.clone();
    app.workspace.projects[0].expanded = false;
    app.stale_collapsed_projects.insert(path.clone());

    let mut reconstructed = make_test_app(GlobalConfig::default(), app.workspace.clone(), None);
    reconstructed.stale_collapsed_projects = app.stale_collapsed_projects.clone();

    assert_eq!(reconstructed.stale_project_indices(), HashSet::from([0]));
    reconstructed.manually_expand_project(0);
    assert!(!reconstructed.stale_collapsed_projects.contains(&path));

    let mut after_interaction = make_test_app(
        GlobalConfig::default(),
        reconstructed.workspace.clone(),
        None,
    );
    after_interaction.stale_collapsed_projects = reconstructed.stale_collapsed_projects.clone();
    assert!(!after_interaction.stale_collapsed_projects.contains(&path));
}

#[test]
fn manual_collapse_refreshes_staleness_and_survives_reconstruction() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(0);
    let path = app.workspace.projects[0].path.clone();
    app.project_touched_unix_ms.insert(path.clone(), 0);
    let mut terminal = workspace_terminal();

    app.dispatch(Action::Select, &mut terminal).unwrap();
    let persisted_touches = app.project_touched_unix_ms.clone();
    let mut reconstructed = make_test_app(GlobalConfig::default(), app.workspace.clone(), None);
    reconstructed.project_touched_unix_ms = persisted_touches;
    reconstructed.collapse_stale_projects();

    assert!(!reconstructed.workspace.projects[0].expanded);
    assert!(!reconstructed.stale_project_indices().contains(&0));
    assert!(reconstructed.project_touched_unix_ms[&path] > 0);
}

#[test]
fn recent_project_touch_and_expansion_survive_app_reconstruction() {
    let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
    app.workspace.projects[0].expanded = false;
    app.workspace.projects[0].last_terminal_active_unix_ms = Some(0);
    let mut terminal = workspace_terminal();

    app.dispatch(Action::Select, &mut terminal).unwrap();
    let persisted_touches = app.project_touched_unix_ms.clone();
    let mut reconstructed = make_test_app(GlobalConfig::default(), app.workspace.clone(), None);
    reconstructed.project_touched_unix_ms = persisted_touches;
    reconstructed.collapse_stale_projects();

    assert!(reconstructed.workspace.projects[0].expanded);
    assert!(!reconstructed.stale_project_indices().contains(&0));
}

#[test]
fn global_settings_keys_navigate_typed_fields_without_mutating_config_until_save() {
    let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
    let mut terminal = workspace_terminal();

    app.dispatch(Action::EditGlobalConfig, &mut terminal)
        .unwrap();
    app.dispatch(Action::InputChar('j'), &mut terminal).unwrap();
    app.dispatch(Action::Select, &mut terminal).unwrap();
    let Mode::GlobalSettings { form } = &app.mode else {
        panic!("comma action must open global settings");
    };
    assert!(
        form.is_editing(),
        "j must move down to the editable path list"
    );
    app.dispatch(Action::InputEscape, &mut terminal).unwrap();

    app.dispatch(Action::InputChar('l'), &mut terminal).unwrap();
    app.dispatch(Action::InputChar(' '), &mut terminal).unwrap();
    let Mode::GlobalSettings { form } = &app.mode else {
        panic!("settings must stay open while changing sections");
    };
    assert!(!form.draft().show_release_status);
    assert!(app.config.show_release_status);

    app.dispatch(Action::InputEscape, &mut terminal).unwrap();
    assert!(matches!(app.mode, Mode::Workspace));
    assert!(app.config.show_release_status);
}

fn app_with_port_session(name: &str) -> App {
    let mut project = make_project("ports");
    let mut worktree = make_worktree("/tmp/ports");
    let mut session = make_sess(false, runtime::AgentState::Idle);
    session.display_name = name.into();
    session.panes[0].listening_ports = vec![3000];
    worktree.sessions.push(session);
    project.worktrees.push(worktree);
    make_test_app(
        GlobalConfig {
            port_visibility: wsx_core::config::global::PortVisibility::All,
            ..GlobalConfig::default()
        },
        WorkspaceState {
            projects: vec![project],
        },
        None,
    )
}

#[test]
fn rendered_session_port_ends_at_the_final_workspace_list_cell() {
    let mut app = app_with_port_session("identity");
    let backend = ratatui::backend::TestBackend::new(100, 16);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let port_end = (0..terminal.backend().buffer().area.height)
        .find_map(|y| {
            let row = (0..terminal.backend().buffer().area.width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<Vec<_>>();
            row.windows(4)
                .position(|cells| cells == ["3", "0", "0", "0"])
                .map(|start| (start + 3) as u16)
        })
        .expect("session row must render its port");
    assert_eq!(
        port_end,
        crate::ui::workspace_nav::SidebarLayout::bordered(app.tree_area)
            .list
            .right()
            .saturating_sub(1),
    );
}

#[test]
fn narrow_rendered_session_row_keeps_its_port_and_marks_truncated_identity() {
    let mut app = app_with_port_session("an-identity-too-wide-for-the-row");
    let backend = ratatui::backend::TestBackend::new(30, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();

    let port_row = (0..terminal.backend().buffer().area.height)
        .find(|&y| {
            let row = (0..terminal.backend().buffer().area.width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<Vec<_>>();
            row.windows(5)
                .any(|cells| cells == [":", "3", "0", "0", "0"])
        })
        .expect("session row must render its compact port");
    let row = (0..terminal.backend().buffer().area.width)
        .map(|x| terminal.backend().buffer()[(x, port_row)].symbol())
        .collect::<Vec<_>>();
    let list = crate::ui::workspace_nav::SidebarLayout::bordered(app.tree_area).list;
    let port_end = list.right().saturating_sub(1) as usize;
    let port_start = port_end - 4;

    assert_eq!(&row[port_start..=port_end], [":", "3", "0", "0", "0"]);
    assert!(!row.contains(&"·"));
    assert!(row[..port_start].contains(&"…"));
}

#[test]
fn worktree_move_reorders_siblings_and_records_stable_paths() {
    let mut project = make_project("ordering");
    project.worktrees = vec![
        make_worktree("/tmp/ordering-main"),
        make_worktree("/tmp/ordering-one"),
        make_worktree("/tmp/ordering-two"),
    ];
    let config = GlobalConfig {
        projects: vec![wsx_core::config::global::ProjectEntry {
            name: project.name.clone(),
            path: project.path.clone(),
            groups: Vec::new(),
            aliases: HashMap::new(),
            worktree_order: Vec::new(),
            profile: None,
        }],
        ..GlobalConfig::default()
    };
    let mut app = make_test_app(
        config,
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    select_rendered_navigation_entry(&mut app, Selection::Worktree(0, 1));

    app.action_enter_move();
    assert!(matches!(
        app.mode,
        Mode::MoveWorktree {
            project_idx: 0,
            worktree_idx: 1
        }
    ));
    let mut terminal = workspace_terminal();
    app.dispatch(Action::NavigateUp, &mut terminal).unwrap();

    assert_eq!(
        app.workspace.projects[0]
            .worktrees
            .iter()
            .map(|worktree| worktree.path.as_path())
            .collect::<Vec<_>>(),
        [
            Path::new("/tmp/ordering-one"),
            Path::new("/tmp/ordering-main"),
            Path::new("/tmp/ordering-two"),
        ]
    );
    assert_eq!(app.current_selection(), Selection::Worktree(0, 0));
    assert_eq!(
        app.config.projects[0].worktree_order,
        [
            PathBuf::from("/tmp/ordering-one"),
            PathBuf::from("/tmp/ordering-main"),
            PathBuf::from("/tmp/ordering-two"),
        ]
    );

    app.dispatch(Action::NavigateUp, &mut terminal).unwrap();
    assert_eq!(app.current_selection(), Selection::Worktree(0, 0));
}

#[test]
fn stale_snapshot_cannot_restore_an_acknowledged_session_order() {
    let mut project = make_project("ordering");
    let mut worktree = make_worktree("/tmp/ordering");
    let mut first = make_sess(false, runtime::AgentState::Idle);
    first.session_id = runtime::SessionId(1);
    first.revision = 7;
    let mut second = make_sess(false, runtime::AgentState::Idle);
    second.session_id = runtime::SessionId(2);
    second.revision = 7;
    worktree.sessions = vec![first, second];
    project.worktrees.push(worktree);
    let mut app = make_test_app(
        GlobalConfig::default(),
        WorkspaceState {
            projects: vec![project],
        },
        None,
    );
    app.pending_session_orders.insert(
        PathBuf::from("/tmp/ordering"),
        PendingSessionOrder {
            moved_session_id: runtime::SessionId(2),
            revision: 8,
            session_ids: vec![runtime::SessionId(2), runtime::SessionId(1)],
        },
    );

    app.reconcile_pending_session_orders();

    assert_eq!(
        app.workspace.projects[0].worktrees[0]
            .sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>(),
        [runtime::SessionId(2), runtime::SessionId(1)]
    );
    assert_eq!(app.pending_session_orders.len(), 1);

    app.workspace.projects[0].worktrees[0].sessions.swap(0, 1);
    app.workspace.projects[0].worktrees[0].sessions[1].revision = 8;
    app.reconcile_pending_session_orders();

    assert_eq!(
        app.workspace.projects[0].worktrees[0]
            .sessions
            .iter()
            .map(|session| session.session_id)
            .collect::<Vec<_>>(),
        [runtime::SessionId(1), runtime::SessionId(2)]
    );
    assert!(app.pending_session_orders.is_empty());
}

#[test]
fn pending_deletion_matches_exact_paths_only() {
    let mut pending = HashSet::from([PathBuf::from("/repo/issue")]);

    let visible = filter_pending_deletions(
        &mut pending,
        vec![(
            PathBuf::from("/repo"),
            vec![worktree_entry("/repo/issue-2")],
        )],
    );

    assert_eq!(visible[0].1.len(), 1);
    assert!(pending.is_empty());
}
