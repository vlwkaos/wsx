use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wsx_core::config::global::GlobalConfig;
use wsx_core::git::worktree::WorktreeEntry;
use wsx_core::model::workspace::{
    ForegroundKind, Project, SessionInfo, WorkspaceState, WorktreeInfo,
};
use wsx_core::ops::{refresh_workspace_with_worktrees, update_activity};
use wsx_core::tmux::monitor::{AgentTailSample, SessionStatus};

fn session(name: &str) -> SessionInfo {
    SessionInfo {
        name: name.to_string(),
        display_name: name.to_string(),
        has_activity: false,
        pane_capture: None,
        last_activity: None,
        agent_tail: None,
        tmux_activity_ts: 0,
        foreground: ForegroundKind::Unknown,
        is_running_wsx: false,
        muted: false,
    }
}

fn workspace_with_session(session: SessionInfo) -> WorkspaceState {
    WorkspaceState {
        projects: vec![Project {
            name: "project".to_string(),
            path: PathBuf::from("/tmp/project"),
            default_branch: "main".to_string(),
            worktrees: vec![WorktreeInfo {
                name: "main".to_string(),
                branch: "main".to_string(),
                path: PathBuf::from("/tmp/project"),
                is_main: true,
                alias: None,
                sessions: vec![session],
                expanded: false,
                git_info: None,
                fetch_failed: false,
                fetch_fail_count: 0,
                fetch_fail_reason: None,
                last_fetched: None,
                git_info_fetched_at: None,
            }],
            routines: Vec::new(),
            routine_revision: 0,
            routines_expanded: false,
            config: None,
            expanded: false,
            missing: false,
        }],
    }
}

fn status(
    has_bell: bool,
    last_activity_ts: u64,
    foreground: ForegroundKind,
    agent_tail: AgentTailSample,
    is_running_wsx: bool,
    wsx_muted: bool,
) -> SessionStatus {
    SessionStatus {
        has_bell,
        last_activity_ts,
        foreground,
        agent_tail,
        is_running_wsx,
        wsx_muted,
    }
}

fn now_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs()
}

#[test]
fn given_identical_captured_agent_tail_when_activity_updates_then_old_recency_is_preserved() {
    let old_activity = Instant::now() - Duration::from_secs(60);
    let mut current = session("agent");
    current.foreground = ForegroundKind::Agent;
    current.agent_tail = Some("same visible cells".to_string());
    current.last_activity = Some(old_activity);
    current.tmux_activity_ts = 10;
    let mut workspace = workspace_with_session(current);

    let mut statuses = HashMap::new();
    statuses.insert(
        "agent".to_string(),
        status(
            false,
            10,
            ForegroundKind::Agent,
            AgentTailSample::Captured("same visible cells".to_string()),
            false,
            false,
        ),
    );

    assert!(!update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert_eq!(updated.last_activity, Some(old_activity));
}

#[test]
fn given_different_captured_agent_tail_when_activity_updates_then_recency_is_refreshed_and_tail_stored(
) {
    let old_activity = Instant::now() - Duration::from_secs(60);
    let before_update = Instant::now();
    let mut current = session("agent");
    current.foreground = ForegroundKind::Agent;
    current.agent_tail = Some("old visible cells".to_string());
    current.last_activity = Some(old_activity);
    current.tmux_activity_ts = 10;
    let mut workspace = workspace_with_session(current);

    let mut statuses = HashMap::new();
    statuses.insert(
        "agent".to_string(),
        status(
            false,
            10,
            ForegroundKind::Agent,
            AgentTailSample::Captured("new visible cells".to_string()),
            false,
            false,
        ),
    );

    assert!(update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert_eq!(updated.agent_tail.as_deref(), Some("new visible cells"));
    assert!(updated.last_activity.expect("activity timestamp") >= before_update);
}

#[test]
fn given_unavailable_agent_capture_when_activity_updates_then_raw_tmux_timestamp_is_used() {
    let current_timestamp = now_unix_timestamp();
    let mut current = session("agent");
    current.foreground = ForegroundKind::Agent;
    current.agent_tail = Some("previous visible cells".to_string());
    current.tmux_activity_ts = current_timestamp.saturating_sub(1);
    let mut workspace = workspace_with_session(current);
    let before_update = Instant::now();

    let mut statuses = HashMap::new();
    statuses.insert(
        "agent".to_string(),
        status(
            false,
            current_timestamp,
            ForegroundKind::Agent,
            AgentTailSample::Unavailable,
            false,
            false,
        ),
    );

    assert!(update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert_eq!(updated.tmux_activity_ts, current_timestamp);
    assert!(updated.last_activity.expect("activity timestamp") >= before_update);
}

#[test]
fn given_muted_agent_activity_when_activity_updates_then_baselines_move_without_recency_activity() {
    let old_activity = Instant::now() - Duration::from_secs(60);
    let current_timestamp = now_unix_timestamp();
    let mut current = session("agent");
    current.foreground = ForegroundKind::Agent;
    current.agent_tail = Some("old visible cells".to_string());
    current.last_activity = Some(old_activity);
    current.tmux_activity_ts = current_timestamp.saturating_sub(1);
    current.muted = true;
    let mut workspace = workspace_with_session(current);

    let mut statuses = HashMap::new();
    statuses.insert(
        "agent".to_string(),
        status(
            true,
            current_timestamp,
            ForegroundKind::Agent,
            AgentTailSample::Captured("new visible cells".to_string()),
            false,
            true,
        ),
    );

    assert!(!update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert!(!updated.has_activity);
    assert_eq!(updated.last_activity, Some(old_activity));
    assert_eq!(updated.agent_tail.as_deref(), Some("new visible cells"));
    assert_eq!(updated.tmux_activity_ts, current_timestamp);
}

#[test]
fn given_runtime_activity_when_activity_updates_then_raw_activity_and_classification_are_kept() {
    let current_timestamp = now_unix_timestamp();
    let before_update = Instant::now();
    let mut current = session("runtime");
    current.foreground = ForegroundKind::Shell;
    current.agent_tail = Some("same visible cells".to_string());
    current.tmux_activity_ts = current_timestamp.saturating_sub(1);
    let mut workspace = workspace_with_session(current);

    let mut statuses = HashMap::new();
    statuses.insert(
        "runtime".to_string(),
        status(
            false,
            current_timestamp,
            ForegroundKind::Runtime,
            AgentTailSample::Captured("same visible cells".to_string()),
            true,
            false,
        ),
    );

    assert!(update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert!(!updated.has_activity);
    assert_eq!(updated.foreground, ForegroundKind::Runtime);
    assert!(updated.last_activity.expect("activity timestamp") >= before_update);
    assert_eq!(updated.agent_tail, None);
    assert!(updated.is_running_wsx);
}

#[test]
fn given_empty_workspace_when_activity_updates_then_it_reports_no_change() {
    let mut workspace = WorkspaceState::empty();
    let statuses = HashMap::new();

    assert!(!update_activity(&mut workspace, &statuses));
}

#[test]
fn given_missing_session_status_when_activity_updates_then_live_process_classification_is_cleared()
{
    let mut current = session("missing");
    current.foreground = ForegroundKind::Runtime;
    current.is_running_wsx = true;
    let mut workspace = workspace_with_session(current);

    let statuses = HashMap::new();
    assert!(update_activity(&mut workspace, &statuses));

    let updated = &workspace.projects[0].worktrees[0].sessions[0];
    assert_eq!(updated.foreground, ForegroundKind::Unknown);
    assert!(!updated.is_running_wsx);
}

#[test]
fn given_stable_agent_when_workspace_refreshes_then_semantic_recency_and_tail_are_preserved() {
    let old_activity = Instant::now() - Duration::from_secs(60);
    let mut current = session("agent");
    current.foreground = ForegroundKind::Agent;
    current.last_activity = Some(old_activity);
    current.agent_tail = Some("same visible cells".to_string());
    let project_path = PathBuf::from("/tmp/project");
    let worktree_path = project_path.clone();
    let current_timestamp = now_unix_timestamp();
    current.tmux_activity_ts = current_timestamp.saturating_sub(1);
    let mut workspace = workspace_with_session(current);

    let mut statuses = HashMap::new();
    statuses.insert(
        "agent".to_string(),
        status(
            false,
            current_timestamp,
            ForegroundKind::Agent,
            AgentTailSample::Captured("same visible cells".to_string()),
            false,
            false,
        ),
    );

    let sessions_with_paths = vec![("agent".to_string(), worktree_path.clone())];
    refresh_workspace_with_worktrees(
        &mut workspace,
        &GlobalConfig::default(),
        &sessions_with_paths,
        &statuses,
        vec![(
            project_path,
            vec![WorktreeEntry {
                name: "main".to_string(),
                path: worktree_path,
                branch: "main".to_string(),
                is_main: true,
            }],
        )],
    );

    let refreshed = &workspace.projects[0].worktrees[0].sessions[0];
    assert_eq!(refreshed.name, "agent");
    assert_eq!(refreshed.last_activity, Some(old_activity));
    assert_eq!(refreshed.agent_tail.as_deref(), Some("same visible cells"));
}
