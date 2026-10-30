// App state machine and event loop.
// ref: ratatui app patterns — https://ratatui.rs/concepts/application-patterns/

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use ratatui::{
    layout::{Position, Rect},
    widgets::Clear,
};

use crate::{
    action::Action,
    event::{poll_event, EscapeSequence, EventMode},
    runtime_util::{api_error, api_error_text, expect_ack},
    search::SearchIndex,
    session_state::{self, AppSessionState},
    terminal_surface::{SurfaceUpdate, TerminalSurfaces},
    tui::{self, Tui},
    ui::{
        self,
        global_settings::GlobalSettingsForm,
        input::InputState,
        integration_manager::IntegrationManager,
        routine_editor::{RoutineForm, RoutinePreset},
        text_form::{dispatch_text_edit, RoutineTextEdit},
    },
};
use wsx_core::{
    cache::AdaptiveCollapseState,
    config::global::{
        project_has_activity_within, project_matches_group, AttentionPriority, AutoCollapsePolicy,
        GlobalConfig, GroupKey, TerminalSidebar, MILLIS_PER_HOUR,
    },
    git::{info as git_info, worktree as git_worktree},
    model::workspace::{
        flatten_tree_filtered, FlatEntry, GitInfo, Project, Selection, WorkspaceState,
    },
    ops, runtime,
};

use git_info::FetchOutcome;

mod actions;
mod dispatch;
mod dispatch_ops;
mod events;
mod groups;
mod input_confirm;
mod move_project;
mod navigation;
mod run_loop;
mod runtime_sync;
mod terminal_view;
mod upgrade_follow;

type WorktreeSnapshot = Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>;

enum RuntimeRefreshOutcome {
    Authoritative {
        availability: runtime::Availability,
        snapshot: Box<runtime::Snapshot>,
        worktrees: WorktreeSnapshot,
    },
    DiscoveryOnly {
        worktrees: WorktreeSnapshot,
        error: anyhow::Error,
    },
}

type RuntimeRefresh = Result<RuntimeRefreshOutcome>;

fn collect_runtime_refresh(
    client: &runtime::Client,
    config: &GlobalConfig,
    availability: fn(&runtime::Client) -> std::io::Result<runtime::Availability>,
) -> RuntimeRefresh {
    let discovery = ops::discover_workspace(config)?;
    let worktrees = discovery.clone().into_worktrees();
    let availability = match availability(client) {
        Ok(availability) => availability,
        Err(error) => {
            return Ok(RuntimeRefreshOutcome::DiscoveryOnly {
                worktrees,
                error: error.into(),
            })
        }
    };
    let snapshot = match ops::synchronize_discovery(client, &discovery) {
        Ok(snapshot) => snapshot,
        Err(error) => return Ok(RuntimeRefreshOutcome::DiscoveryOnly { worktrees, error }),
    };
    Ok(RuntimeRefreshOutcome::Authoritative {
        availability,
        snapshot: Box::new(snapshot),
        worktrees,
    })
}

// ^ [[wsx Architecture]] Runtime snapshots are authoritative; events invalidate them.
enum RuntimeResult {
    FullRefresh(RuntimeRefresh),
    SessionRefresh(Result<runtime::Snapshot>),
    ResumeRefresh {
        generation: u64,
        result: Result<runtime::Snapshot>,
    },
    Frame(Result<(u64, runtime::TerminalFrame)>),
    FollowCheck(Option<crate::follow::Target>),
    UpgradeAttempt(Result<runtime::Availability>),
    PluginView {
        generation: u64,
        plugin_id: String,
        pane_id: runtime::PaneId,
        result: Result<runtime::PluginSidecarView>,
    },
}

struct ActiveTerminalStream {
    epoch: u64,
    pane_id: runtime::PaneId,
    terminal_id: runtime::TerminalId,
    generation: u64,
    stream: runtime::TerminalStream,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingTerminalEntry {
    pane_id: runtime::PaneId,
    terminal_id: runtime::TerminalId,
    generation: u64,
    rows: u16,
    cols: u16,
}

impl PendingTerminalEntry {
    fn matches_frame(self, generation: u64, frame: &runtime::TerminalFrame) -> bool {
        self.generation == generation
            && self.pane_id == frame.pane_id
            && self.terminal_id == frame.terminal_id
            && self.rows == frame.rows
            && self.cols == frame.cols
    }
}

struct PendingTerminalResume {
    pane_id: runtime::PaneId,
    terminal_id: runtime::TerminalId,
    generation: u64,
    snapshot_ready: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalControlIntent {
    Explicit,
    Resume,
}

impl TerminalControlIntent {
    fn takeover(self) -> bool {
        matches!(self, Self::Explicit)
    }
}

// ── Git concurrency limiter ───────────────────────────────────────────────────

/// Counting semaphore — limits concurrent git-info subprocesses to CPU count.
#[derive(Clone)]
struct GitSemaphore(Arc<(Mutex<usize>, Condvar)>);

impl GitSemaphore {
    fn new(limit: usize) -> Self {
        Self(Arc::new((Mutex::new(limit), Condvar::new())))
    }

    /// Block until a permit is available, then return a guard that releases on drop.
    fn acquire(&self) -> GitPermit {
        let (lock, cvar) = &*self.0;
        let mut count = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *count == 0 {
            count = cvar.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count -= 1;
        GitPermit(self.0.clone())
    }
}

struct GitPermit(Arc<(Mutex<usize>, Condvar)>);

impl Drop for GitPermit {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        cvar.notify_one();
    }
}

// ── Timer ─────────────────────────────────────────────────────────────────────

struct Timer {
    last: Instant,
    interval: Duration,
}

impl Timer {
    fn new(interval_ms: u64) -> Self {
        Self {
            last: Instant::now(),
            interval: Duration::from_millis(interval_ms),
        }
    }

    fn ready(&mut self) -> bool {
        if self.last.elapsed() >= self.interval {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.last = Instant::now();
    }
}

#[derive(Clone, Copy, Debug)]
struct LifecycleClockSample {
    active: Duration,
    continuous: Duration,
}

struct SuspendDetector {
    previous: Option<LifecycleClockSample>,
}

impl SuspendDetector {
    fn new() -> Self {
        Self {
            previous: lifecycle_clock_sample(),
        }
    }

    fn resumed(&mut self) -> bool {
        lifecycle_clock_sample().is_some_and(|sample| self.observe(sample))
    }

    fn observe(&mut self, sample: LifecycleClockSample) -> bool {
        let previous = self.previous.replace(sample);
        let Some(previous) = previous else {
            return false;
        };
        let Some(active_elapsed) = sample.active.checked_sub(previous.active) else {
            return false;
        };
        let Some(continuous_elapsed) = sample.continuous.checked_sub(previous.continuous) else {
            return false;
        };
        continuous_elapsed.saturating_sub(active_elapsed) >= Duration::from_millis(250)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn clock_duration(clock_id: libc::clockid_t) -> Option<Duration> {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the supplied timespec on success.
    if unsafe { libc::clock_gettime(clock_id, value.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the successful call initialized value.
    let value = unsafe { value.assume_init() };
    let seconds = u64::try_from(value.tv_sec).ok()?;
    let nanos = u32::try_from(value.tv_nsec).ok()?;
    (nanos < 1_000_000_000).then(|| Duration::new(seconds, nanos))
}

#[cfg(target_os = "linux")]
fn lifecycle_clock_sample() -> Option<LifecycleClockSample> {
    // ^ https://man7.org/linux/man-pages/man2/clock_gettime.2.html
    // CLOCK_MONOTONIC excludes suspend while CLOCK_BOOTTIME includes it.
    Some(LifecycleClockSample {
        active: clock_duration(libc::CLOCK_MONOTONIC)?,
        continuous: clock_duration(libc::CLOCK_BOOTTIME)?,
    })
}

#[cfg(target_os = "macos")]
fn lifecycle_clock_sample() -> Option<LifecycleClockSample> {
    // ^ https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/clock_gettime.3.html
    // UPTIME_RAW excludes sleep while MONOTONIC_RAW uses continuous time.
    Some(LifecycleClockSample {
        active: clock_duration(libc::CLOCK_UPTIME_RAW)?,
        continuous: clock_duration(libc::CLOCK_MONOTONIC_RAW)?,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn lifecycle_clock_sample() -> Option<LifecycleClockSample> {
    None
}

const TICK_MS: u64 = 100;
const TERMINAL_TICK_MS: u64 = 4;
const MODE_PROBE_IDLE: Duration = Duration::from_secs(1);
const FAST_INTERVAL_MS: u64 = 500;
const GIT_SWEEP_INTERVAL_MS: u64 = 15_000;
const SLOW_INTERVAL_MS: u64 = 30_000;
const WORKSPACE_SCROLL_LINES: isize = 3;
const NOTICE_MIN_DWELL: Duration = Duration::from_secs(2);
const STREAM_REATTACH_WINDOW: Duration = Duration::from_secs(5);
const NOTICE_QUEUE_CAPACITY: usize = 8;

fn project_latest_activity_unix_ms(
    project: &Project,
    project_touched_unix_ms: &HashMap<PathBuf, u64>,
) -> Option<u64> {
    project
        .last_agent_active_unix_ms
        .into_iter()
        .chain(project.last_terminal_active_unix_ms)
        .chain(project_touched_unix_ms.get(&project.path).copied())
        .max()
}

fn project_has_active_sessions(project: &Project) -> bool {
    session_state::folded_status(
        project
            .worktrees
            .iter()
            .flat_map(|worktree| &worktree.sessions),
        &HashSet::new(),
    )
    .active_sessions
        > 0
}

fn project_is_stale(
    project: &Project,
    project_touched_unix_ms: &HashMap<PathBuf, u64>,
    now_unix_ms: u64,
    window_ms: u64,
) -> bool {
    // ^ docs/ui-state-ownership.md: live activity fences automatic collapse,
    // without inventing trusted activity timestamps or changing manual folding.
    if project_has_active_sessions(project) {
        return false;
    }
    let latest = project_latest_activity_unix_ms(project, project_touched_unix_ms);
    latest.is_some() && !project_has_activity_within(latest, None, now_unix_ms, window_ms)
}

fn action_needs_immediate_redraw(action: &Action) -> bool {
    !matches!(
        action,
        Action::TerminalKey(_)
            | Action::TerminalKeys(_)
            | Action::TerminalPaste(_)
            | Action::TerminalPrefixedPaste(_, _)
            | Action::TerminalMouse(_)
            | Action::TerminalPrefixedMouse(_, _)
    )
}

fn runtime_key_event(key: crossterm::event::KeyEvent) -> Option<runtime::KeyEvent> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let (code, text) = match key.code {
        KeyCode::Char(ch) => (runtime::KeyCode::Text, ch.to_string()),
        KeyCode::Enter => (runtime::KeyCode::Enter, String::new()),
        KeyCode::Backspace => (runtime::KeyCode::Backspace, String::new()),
        KeyCode::Tab | KeyCode::BackTab => (runtime::KeyCode::Tab, String::new()),
        KeyCode::Esc => (runtime::KeyCode::Escape, String::new()),
        KeyCode::Insert => (runtime::KeyCode::Insert, String::new()),
        KeyCode::Delete => (runtime::KeyCode::Delete, String::new()),
        KeyCode::Home => (runtime::KeyCode::Home, String::new()),
        KeyCode::End => (runtime::KeyCode::End, String::new()),
        KeyCode::PageUp => (runtime::KeyCode::PageUp, String::new()),
        KeyCode::PageDown => (runtime::KeyCode::PageDown, String::new()),
        KeyCode::Left => (runtime::KeyCode::Left, String::new()),
        KeyCode::Right => (runtime::KeyCode::Right, String::new()),
        KeyCode::Up => (runtime::KeyCode::Up, String::new()),
        KeyCode::Down => (runtime::KeyCode::Down, String::new()),
        KeyCode::F(number) => (runtime::KeyCode::Function(number), String::new()),
        KeyCode::Null => (runtime::KeyCode::Text, "\0".into()),
        _ => return None,
    };
    Some(runtime::KeyEvent {
        code,
        text,
        shift: key.modifiers.contains(KeyModifiers::SHIFT) || matches!(key.code, KeyCode::BackTab),
        control: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        super_key: key.modifiers.contains(KeyModifiers::SUPER),
        repeat: key.kind == KeyEventKind::Repeat,
    })
}

fn runtime_mouse_event(
    mouse: crossterm::event::MouseEvent,
    viewport: Rect,
) -> Option<runtime::MouseEvent> {
    use crossterm::event::{KeyModifiers, MouseEventKind};
    if viewport.is_empty() {
        return None;
    }
    let in_bounds = viewport.contains(Position::new(mouse.column, mouse.row));
    let vertical_drag_outside = matches!(mouse.kind, MouseEventKind::Drag(_))
        && (mouse.row < viewport.y || mouse.row >= viewport.bottom());
    if !in_bounds && !matches!(mouse.kind, MouseEventKind::Up(_)) && !vertical_drag_outside {
        return None;
    }
    let (action, button) = match mouse.kind {
        MouseEventKind::Down(button) => (runtime::MouseAction::Press, runtime_mouse_button(button)),
        MouseEventKind::Up(button) => (runtime::MouseAction::Release, runtime_mouse_button(button)),
        MouseEventKind::Drag(button) => {
            (runtime::MouseAction::Motion, runtime_mouse_button(button))
        }
        MouseEventKind::Moved => (runtime::MouseAction::Motion, runtime::MouseButton::None),
        MouseEventKind::ScrollUp => (runtime::MouseAction::Press, runtime::MouseButton::WheelUp),
        MouseEventKind::ScrollDown => {
            (runtime::MouseAction::Press, runtime::MouseButton::WheelDown)
        }
        MouseEventKind::ScrollLeft => {
            (runtime::MouseAction::Press, runtime::MouseButton::WheelLeft)
        }
        MouseEventKind::ScrollRight => (
            runtime::MouseAction::Press,
            runtime::MouseButton::WheelRight,
        ),
    };
    Some(runtime::MouseEvent {
        action,
        button,
        x: mouse
            .column
            .saturating_sub(viewport.x)
            .min(viewport.width.saturating_sub(1)),
        y: mouse
            .row
            .saturating_sub(viewport.y)
            .min(viewport.height.saturating_sub(1)),
        in_bounds,
        shift: mouse.modifiers.contains(KeyModifiers::SHIFT),
        control: mouse.modifiers.contains(KeyModifiers::CONTROL),
        alt: mouse.modifiers.contains(KeyModifiers::ALT),
        super_key: mouse.modifiers.contains(KeyModifiers::SUPER),
    })
}

fn runtime_mouse_button(button: crossterm::event::MouseButton) -> runtime::MouseButton {
    match button {
        crossterm::event::MouseButton::Left => runtime::MouseButton::Left,
        crossterm::event::MouseButton::Middle => runtime::MouseButton::Middle,
        crossterm::event::MouseButton::Right => runtime::MouseButton::Right,
    }
}
const FETCH_INTERVAL_SECS: u64 = 60;

// ── Modes ─────────────────────────────────────────────────────────────────────

struct PendingSessionOrder {
    moved_session_id: runtime::SessionId,
    revision: u64,
    session_ids: Vec<runtime::SessionId>,
}

pub enum Mode {
    Workspace,
    Terminal {
        pane_id: runtime::PaneId,
    },
    Input {
        context: InputContext,
        state: InputState,
    },
    Confirm {
        message: String,
        pending: PendingAction,
    },
    Config {
        project_idx: usize,
    },
    GlobalSettings {
        form: Box<GlobalSettingsForm>,
    },
    IntegrationManager {
        manager: IntegrationManager,
        return_form: Box<GlobalSettingsForm>,
    },
    Move {
        project_idx: usize,
    },
    MoveWorktree {
        project_idx: usize,
        worktree_idx: usize,
    },
    MoveSession {
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
    },
    Help,
    Search {
        query: String,
        match_idx: usize,
    },
    GroupManager {
        selected: usize,
        scroll: usize,
        purpose: GroupManagerPurpose,
    },
    RoutinePresetPicker {
        project_idx: usize,
        selected: usize,
    },
    RoutineEditor {
        project_idx: usize,
        original_name: Option<String>,
        can_rename: bool,
        form: RoutineForm,
    },
    RoutineDetail {
        project_path: PathBuf,
        routine_name: String,
        scroll: u16,
    },
    Tasks {
        list: Box<crate::task_list::TaskList>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupManagerPurpose {
    Switch,
    Assign { project_idx: usize },
}

pub enum InputContext {
    AddProject,
    AddWorktree {
        project_idx: usize,
    },
    AddSession {
        project_idx: usize,
        worktree_idx: usize,
    },
    AddSessionCmd {
        project_idx: usize,
        worktree_idx: usize,
        session_label: String,
    },
    SetAlias {
        project_idx: usize,
        worktree_idx: usize,
    },
    RenameSession {
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
    },
    AddGroup,
    RenameGroup {
        group_idx: usize,
    },
}

impl InputContext {
    pub fn title(&self) -> &'static str {
        match self {
            InputContext::AddProject => "Add Project (git repos)",
            InputContext::AddWorktree { .. } => "Add Worktree",
            InputContext::AddSession { .. } => "New Session — name",
            InputContext::AddSessionCmd { .. } => "New Session — command",
            InputContext::SetAlias { .. } => "Set Alias",
            InputContext::RenameSession { .. } => "Rename Session",
            InputContext::AddGroup => "New Group",
            InputContext::RenameGroup { .. } => "Rename Group",
        }
    }
}

pub enum PendingAction {
    DeleteProject {
        project_idx: usize,
    },
    DeleteWorktree {
        project_idx: usize,
        worktree_idx: usize,
    },
    DeleteSession {
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
    },
    ClosePane {
        pane_id: runtime::PaneId,
        revision: u64,
    },
    CreateWorktree {
        project_idx: usize,
        branch: String,
    },
    DeleteGroup {
        group_idx: usize,
    },
    DeleteRoutine {
        project_path: PathBuf,
        name: String,
        revision: u64,
    },
    ShutdownDaemon,
    InstallIntegrations {
        targets: Vec<wsx_core::integration::IntegrationTarget>,
        remember_dismissal: bool,
    },
}

// ── Background jobs ───────────────────────────────────────────────────────────

pub struct BgJob {
    /// Unique per `spawn_bg` call, so a result clears only its own job.
    pub id: u64,
    pub label: String,
}

pub enum BgOutcome {
    WorktreeRemoved {
        label: String,
    },
    WorktreeCreated {
        label: String,
        warning: Option<String>,
        session_error: Option<String>,
    },
    SessionKilled {
        session_id: runtime::SessionId,
        display_name: String,
    },
    IntegrationsInstalled {
        installed: Vec<wsx_core::integration::IntegrationTarget>,
        failures: Vec<String>,
    },
    /// A one-shot terminal input failed on the worker thread (`TerminalInputWorker`).
    TerminalInputFailed(String),
}

/// Result id for work that never had a `BgJob`, so it clears no job.
const NO_JOB_ID: u64 = u64::MAX;

struct BgResult {
    id: u64,
    label: String,
    outcome: Result<BgOutcome>,
}

/// Starts the one thread that sends one-shot terminal bytes (Workspace actions
/// such as Ctrl-C). It ends when the App drops the sender.
fn spawn_terminal_input_worker(
    client: runtime::Client,
    results: mpsc::Sender<BgResult>,
) -> mpsc::Sender<(runtime::PaneId, Vec<u8>)> {
    let (tx, rx) = mpsc::channel::<(runtime::PaneId, Vec<u8>)>();
    std::thread::spawn(move || {
        while let Ok((pane_id, bytes)) = rx.recv() {
            if let Err(error) = send_terminal_bytes_once(&client, pane_id, bytes) {
                let _ = results.send(BgResult {
                    id: NO_JOB_ID,
                    label: "terminal input".into(),
                    outcome: Ok(BgOutcome::TerminalInputFailed(error.to_string())),
                });
            }
        }
    });
    tx
}

fn send_terminal_bytes_once(
    client: &runtime::Client,
    pane_id: runtime::PaneId,
    bytes: Vec<u8>,
) -> Result<()> {
    // ^ A one-shot Workspace action must never share identity with a live or
    // reconnecting Terminal stream, whose lease has a separate incarnation. A key
    // press is explicit, so it takes the pane from another controller.
    let client_id = runtime::new_client_id();
    expect_ack(
        client.call(&runtime::Request::TerminalAcquire {
            pane_id,
            client_id,
            takeover: TerminalControlIntent::Explicit.takeover(),
        })?,
        "unexpected terminal lease response",
    )?;
    let result = expect_ack(
        client.call(&runtime::Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        })?,
        "unexpected terminal input response",
    )
    .map(|_| ());
    let _ = client.call(&runtime::Request::TerminalRelease { pane_id, client_id });
    result
}

enum RoutineSelection {
    Preserve,
    Header,
    Named(String),
}

enum RoutineResultKind {
    Refresh {
        generation: u64,
        expand: bool,
        selection: RoutineSelection,
    },
    Save {
        original_name: Option<String>,
        can_rename: bool,
        form: RoutineForm,
        saved_name: String,
    },
    Delete {
        name: String,
    },
}

struct RoutineRefreshResult {
    project_path: PathBuf,
    kind: RoutineResultKind,
    response: Result<asched_core::routine::ipc::Response>,
}

fn routine_error_kind(error: &anyhow::Error) -> Option<asched_core::routine::RoutineErrorKind> {
    error
        .downcast_ref::<asched_core::routine::RoutineError>()
        .map(asched_core::routine::RoutineError::kind)
}

fn routine_error_text(error: &anyhow::Error) -> String {
    match routine_error_kind(error) {
        Some(asched_core::routine::RoutineErrorKind::ProtocolMismatch) => {
            "routine scheduler is newer than wsx; upgrade wsx and retry".into()
        }
        Some(asched_core::routine::RoutineErrorKind::Conflict) => {
            "routine changed in asched; refreshed the latest revision".into()
        }
        Some(asched_core::routine::RoutineErrorKind::AlreadyRunning) => {
            "routine is already running".into()
        }
        _ => error.to_string(),
    }
}

fn edit_file(terminal: &mut Tui, path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    tui::with_raw_mode_disabled(terminal, || {
        let status = std::process::Command::new(&editor)
            .arg(path)
            .status()
            .with_context(|| format!("launching {editor}"))?;
        if !status.success() {
            anyhow::bail!("{editor} exited with {status}");
        }
        Ok(())
    })
}

pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn terminal_stream_error_notice(error: &std::io::Error, target: &str) -> String {
    let detail = error.to_string();
    let title = detail
        .strip_prefix("terminal_busy: ")
        .map(|reason| {
            format!(
                "Terminal busy: {}",
                reason.strip_prefix("pane has ").unwrap_or(reason)
            )
        })
        .unwrap_or_else(|| format!("Terminal stream failed: {detail}"));
    format!("{title}\nTarget: {target}")
}

fn terminal_stream_failure_notice(error: &runtime::ApiError) -> (NoticeLevel, String) {
    if error.code == "lease_transferred" {
        (
            NoticeLevel::Warning,
            "Terminal control moved to another wsx instance".into(),
        )
    } else {
        (
            NoticeLevel::Error,
            format!("Terminal stream: {}: {}", error.code, error.message),
        )
    }
}

fn filter_pending_deletions(
    pending: &mut HashSet<PathBuf>,
    worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
    pending.retain(|path| {
        worktrees
            .iter()
            .any(|(_, entries)| entries.iter().any(|entry| entry.path == *path))
    });
    worktrees
        .into_iter()
        .map(|(project_path, entries)| {
            let entries = entries
                .into_iter()
                .filter(|entry| !pending.contains(&entry.path))
                .collect();
            (project_path, entries)
        })
        .collect()
}

// ── App ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

fn notice_priority(level: NoticeLevel) -> u8 {
    match level {
        NoticeLevel::Info => 0,
        NoticeLevel::Success => 1,
        NoticeLevel::Warning => 2,
        NoticeLevel::Error => 3,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub title: String,
    pub body: Option<String>,
}

fn replacement_blocker_label(blocker: runtime::ReplacementBlocker) -> &'static str {
    match blocker {
        runtime::ReplacementBlocker::OtherTui => "another wsx TUI",
        runtime::ReplacementBlocker::WorkingAgent => "a working agent",
        runtime::ReplacementBlocker::ListenerScanPending => "a pending listener scan",
        runtime::ReplacementBlocker::ForegroundJob => "a foreground job",
        runtime::ReplacementBlocker::ListeningPort => "a listening port",
        runtime::ReplacementBlocker::LegacyDaemon => "a legacy daemon",
        runtime::ReplacementBlocker::PendingTarget => "another pending upgrade",
        runtime::ReplacementBlocker::HandoffUnavailable => "unavailable live handoff",
        runtime::ReplacementBlocker::Unknown => "a blocker this wsx does not know",
    }
}

// ^ docs/daemon-live-upgrade.md: say what the user can do, not only what blocks.
pub(crate) fn replacement_wait_reason(blockers: &[runtime::ReplacementBlocker]) -> String {
    let other_tui = blockers.contains(&runtime::ReplacementBlocker::OtherTui);
    let rest = blockers
        .iter()
        .filter(|blocker| **blocker != runtime::ReplacementBlocker::OtherTui)
        .map(|blocker| replacement_blocker_label(*blocker))
        .collect::<Vec<_>>()
        .join(", ");
    match (other_tui, rest.is_empty()) {
        (true, true) => "Quit the other wsx window (an older version).".into(),
        (true, false) => {
            format!("Quit the other wsx window (an older version). Also waiting for: {rest}.")
        }
        (false, false) => format!("Waiting for: {rest}."),
        (false, true) => "Waiting for the running wsxd.".into(),
    }
}

pub(crate) fn runtime_availability_notice(
    availability: &runtime::Availability,
) -> Option<(NoticeLevel, String)> {
    match availability {
        runtime::Availability::Current => None,
        runtime::Availability::RecoveredFromBackup => Some((
            NoticeLevel::Warning,
            "wsxd recovered a corrupt primary state from its last-known-good backup".into(),
        )),
        runtime::Availability::LegacyCompatible => Some((
            NoticeLevel::Warning,
            "Legacy wsxd is still running\nAutomatic upgrade is unavailable; `wsx daemon stop --force` ends its sessions and lets the next wsx start the new daemon"
                .into(),
        )),
        runtime::Availability::NewerDaemon { daemon_version }
            if daemon_version == runtime::WSX_VERSION =>
        {
            None
        }
        runtime::Availability::NewerDaemon { daemon_version } => Some((
            NoticeLevel::Warning,
            format!(
                "wsxd {daemon_version} is newer than this wsx {}; open wsx {daemon_version}",
                runtime::WSX_VERSION
            ),
        )),
        runtime::Availability::DaemonReplaced { previous_version } => Some((
            NoticeLevel::Success,
            format!(
                "Runtime upgraded to wsxd {}\nPrevious version: {previous_version}",
                runtime::WSX_VERSION
            ),
        )),
        runtime::Availability::DaemonRestarted => Some((
            NoticeLevel::Success,
            format!("Runtime restarted with wsxd {}", runtime::WSX_VERSION),
        )),
        runtime::Availability::ReplacementDeferred {
            daemon_version,
            target_version,
            live_runtimes,
            blockers,
        } => {
            let reason = replacement_wait_reason(blockers);
            // Only a daemon without live handoff reports work in its sessions as a blocker.
            let restarts_sessions = blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    runtime::ReplacementBlocker::WorkingAgent
                        | runtime::ReplacementBlocker::ListenerScanPending
                        | runtime::ReplacementBlocker::ForegroundJob
                        | runtime::ReplacementBlocker::ListeningPort
                        | runtime::ReplacementBlocker::LegacyDaemon
                )
            });
            let safety = if restarts_sessions {
                format!("This older wsxd restarts its {live_runtimes} live session(s) when it upgrades, so it waits.")
            } else {
                format!("Sessions keep running ({live_runtimes} live). The upgrade then continues by itself.")
            };
            Some((
                NoticeLevel::Warning,
                format!(
                    "Upgrade from wsxd {daemon_version} to {target_version} is waiting\n{reason}\n{safety}"
                ),
            ))
        }
        // ^ docs/daemon-live-upgrade.md "A failed automatic upgrade": this wsx works with the
        // running daemon; `wsx daemon restart` tries again.
        runtime::Availability::UpgradeFailed { reason } => Some((
            NoticeLevel::Warning,
            format!(
                "wsxd upgrade failed: {reason}; sessions keep running\n`wsx daemon restart` tries again."
            ),
        )),
    }
}

// ^ docs/daemon-live-upgrade.md: a starting TUI waits for the daemon upgrade before it opens.
pub struct UpgradeGate {
    pub daemon_version: String,
    pub daemon_revision: u32,
    pub live_runtimes: usize,
    pub step: UpgradeStep,
    retry_at: Option<Instant>,
}

pub enum UpgradeStep {
    Upgrading,
    Waiting(Vec<runtime::ReplacementBlocker>),
}

const UPGRADE_RETRY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub enum RuntimeHealth {
    Connecting,
    Healthy {
        last_success: Instant,
    },
    Reconnecting {
        last_success: Option<Instant>,
        error: String,
    },
}

pub struct App {
    pub workspace: WorkspaceState,
    pub tree_selected: usize,
    pub tree_scroll: usize,
    pub tree_visible_height: usize,
    pub tree_scroll_manual: bool,
    pub tree_area: Rect,
    pub preview_area: Rect,
    pub terminal_area: Rect,
    pub plugin_area: Rect,
    pub review: Option<crate::review::ReviewState>,
    pub review_available: bool,
    pub mode: Mode,
    pub config: GlobalConfig,
    pub active_group: GroupKey,
    /// None without `[[profiles]]`: every project shows. ref: README "Profiles"
    pub active_profile: Option<String>,
    pub group_header_scroll: usize,
    pub group_header_area: Rect,
    visible_projects: HashSet<usize>,
    project_touched_unix_ms: HashMap<PathBuf, u64>,
    stale_collapsed_projects: HashSet<PathBuf>,
    adaptive_collapse: HashMap<PathBuf, AdaptiveCollapseState>,
    pub notice: Option<Notice>,
    notice_started: Option<Instant>,
    notice_queue: VecDeque<Notice>,
    pub jobs: Vec<BgJob>,
    next_job_id: u64,
    pub spinner_frame: usize,
    bg_tx: mpsc::Sender<BgResult>,
    bg_rx: mpsc::Receiver<BgResult>,
    terminal_input_tx: mpsc::Sender<(runtime::PaneId, Vec<u8>)>,
    needs_redraw: bool,
    should_quit: bool,
    force_terminal_redraw: bool,
    /// Write the outer terminal modes again with the next frame.
    reassert_terminal_modes: bool,
    last_terminal_event: Instant,
    force_preview_redraw: bool,
    /// Tracks whether the last successful desktop frame contained captured terminal content.
    last_rendered_preview_was_session: bool,
    fast_timer: Timer,
    git_sweep_timer: Timer,
    slow_timer: Timer,
    cached_flat: Vec<FlatEntry>,
    flat_dirty: bool,
    /// Logical hits projected onto visible ancestors; rebuilt with the flat cache.
    /// Built when Search mode needs it; `ensure_flat` drops it on every rebuild.
    search_index: Option<SearchIndex>,
    git_local_tx: mpsc::Sender<(PathBuf, Option<GitInfo>)>,
    git_local_rx: mpsc::Receiver<(PathBuf, Option<GitInfo>)>,
    git_local_pending: HashSet<PathBuf>,
    fetch_tx: mpsc::Sender<(PathBuf, FetchOutcome)>,
    fetch_rx: mpsc::Receiver<(PathBuf, FetchOutcome)>,
    fetch_pending: HashSet<PathBuf>,
    /// Retry pending commands without publishing the surrounding presentation snapshot.
    cache_dirty: bool,
    /// ^ docs/ui-state-ownership.md "Test isolation": every cache read and write uses this file.
    cache_path: PathBuf,
    cache_changes: wsx_core::cache::WorkspaceCacheChanges,
    cache_cursor: (usize, Option<wsx_core::cache::CursorIdentity>),
    cached_worktree_expansion: HashMap<String, bool>,
    /// Limits concurrent git-info threads to available CPU count.
    git_semaphore: GitSemaphore,
    /// path → (project_idx, worktree_idx) for O(1) async-result application.
    worktree_index: std::collections::HashMap<PathBuf, (usize, usize)>,
    runtime_client: runtime::Client,
    _runtime_monitor: Option<runtime::EventMonitor>,
    runtime_event_rx: mpsc::Receiver<runtime::EventSignal>,
    pub runtime_health: RuntimeHealth,
    runtime_tx: mpsc::Sender<RuntimeResult>,
    runtime_rx: mpsc::Receiver<RuntimeResult>,
    runtime_refresh_pending: bool,
    startup_availability_pending: bool,
    startup_cursor_identity: Option<wsx_core::cache::CursorIdentity>,
    /// An event requested a session refresh while one was in-flight.
    runtime_refresh_stale: bool,
    /// A full Git plus Runtime refresh takes priority over queued event refreshes.
    runtime_full_refresh_stale: bool,
    runtime_capture_pending: bool,
    plugin_sidecars: Vec<runtime::PluginSidecarDescriptor>,
    plugin_view: Option<runtime::PluginSidecarView>,
    plugin_view_generation: u64,
    plugin_view_pending: bool,
    plugin_view_last_refresh: Option<Instant>,
    plugin_view_last_owner: Option<(String, runtime::PaneId)>,
    /// Worktree paths pending bg deletion; filtered from refresh results until bg confirms.
    pending_deletions: HashSet<PathBuf>,
    /// Pane closes awaiting confirmation by a newer Runtime snapshot.
    pending_session_kills: HashSet<runtime::SessionId>,
    /// Session orders awaiting confirmation by an equal-or-newer Runtime snapshot.
    pending_session_orders: HashMap<PathBuf, PendingSessionOrder>,
    /// Mute is wsx-local and keyed by stable terminal ID.
    muted_terminal_ids: HashSet<String>,
    /// Exact done revisions acknowledged by explicit interaction, keyed by stable terminal ID.
    acknowledged_outcomes: HashMap<String, u64>,
    terminal_controller_id: u64,
    terminal_surfaces: TerminalSurfaces,
    terminal_stream: Option<ActiveTerminalStream>,
    terminal_stream_generation: u64,
    pending_terminal_entry: Option<PendingTerminalEntry>,
    pending_terminal_resume: Option<PendingTerminalResume>,
    /// A shell the Tasks view created, entered once a refresh lists it, and when it asked.
    pending_shell: Option<(runtime::SessionId, Instant)>,
    last_stream_reattach: Option<Instant>,
    // ^ docs/daemon-live-upgrade.md: this TUI re-executes into a replaced install.
    installed: Option<crate::follow::Installed>,
    follow_check_pending: bool,
    follow_target: Option<crate::follow::Target>,
    follow_resume_pane: Option<runtime::PaneId>,
    upgrade: Option<UpgradeGate>,
    suspend_detector: SuspendDetector,
    /// Epoch and revision of the newest daemon state this window applied or caused: a
    /// snapshot read before it (a refresh still in flight) must not undo what came after.
    applied_revision: Option<(u64, u64)>,
    terminal_escape_chord: EscapeSequence,
    terminal_sidebar_override: Option<TerminalSidebar>,
    update_rx: mpsc::Receiver<String>,
    pub update_available: Option<String>,
    /// Effective responsive mode for the current frame.
    pub is_mobile: bool,
    /// Explicit --mobile override; otherwise width drives the mode.
    pub force_mobile: bool,
    /// Git repos discovered under `$HOME` — populated from app start by a
    /// background walker. Survives modal opens so the add-project prompt
    /// is instant on the second and later opens, and picks up newly
    /// created repos between opens.
    scanned_repos: Vec<String>,
    repo_scan_rx: Option<mpsc::Receiver<String>>,
    routine_tx: mpsc::Sender<RoutineRefreshResult>,
    routine_rx: mpsc::Receiver<RoutineRefreshResult>,
    routine_refresh_generation: HashMap<PathBuf, u64>,
    integration_scan_rx: mpsc::Receiver<Result<Vec<wsx_core::integration::IntegrationMetadata>>>,
    integration_metadata: Vec<wsx_core::integration::IntegrationMetadata>,
    pending_integration_demand: Option<wsx_core::integration::IntegrationTarget>,
    pending_integration_updates: Vec<wsx_core::integration::IntegrationTarget>,
    dismissed_integration_prompts: HashSet<wsx_core::integration::IntegrationTarget>,
    persist_group_selection: bool,
}

impl App {
    pub fn muted_terminals(&self) -> &HashSet<String> {
        &self.muted_terminal_ids
    }

    pub fn new(mobile: bool) -> Result<Self> {
        let (config, config_warn) = GlobalConfig::load()?;
        let mut workspace = ops::workspace_from_config(&config);
        let project_config_notice = workspace.projects.iter().find_map(|project| {
            project
                .config
                .as_ref()
                .and_then(|config| config.notice.clone())
        });
        let mut initial_notice = config_warn.or(project_config_notice);
        let cache_path = wsx_core::cache::workspace_cache_path()?;
        let (
            raw_selected,
            cursor_identity,
            project_touched_unix_ms,
            stale_collapsed_projects,
            adaptive_collapse,
            cached_muted,
            acknowledged_outcomes,
            dismissed_integration_prompts,
            cached_worktree_expansion,
        ) = wsx_core::cache::apply_cache(&cache_path, &mut workspace)?;
        let stored = match wsx_core::cache::load_group_selection() {
            Ok(selection) => selection,
            Err(error) => {
                initial_notice.get_or_insert_with(|| {
                    format!("Could not read the saved group selection: {error}")
                });
                None
            }
        };
        let selection = wsx_core::cache::GroupSelection {
            selected: initial_active_group(
                &config,
                stored.as_ref().map(|stored| stored.selected.clone()),
            ),
            profile: initial_active_profile(
                &config,
                stored.as_ref().and_then(|stored| stored.profile.clone()),
            ),
        };
        if stored.as_ref() != Some(&selection) {
            if let Err(error) = wsx_core::cache::save_group_selection(&selection) {
                initial_notice
                    .get_or_insert_with(|| format!("Could not save the group selection: {error}"));
            }
        }
        let wsx_core::cache::GroupSelection {
            selected: active_group,
            profile: active_profile,
        } = selection;
        let visible_projects = compute_visible_projects(
            &config,
            &workspace,
            Some(&active_group),
            active_profile.as_deref(),
        );
        let group_header_scroll = config
            .ordered_group_keys()
            .iter()
            .position(|candidate| candidate == &active_group)
            .unwrap_or(0);
        let cached_flat = flatten_tree_filtered(&workspace, &visible_projects);
        let tree_selected = cursor_identity
            .as_ref()
            .and_then(|id| wsx_core::cache::find_cursor_index(&workspace, &cached_flat, id))
            .unwrap_or_else(|| raw_selected.min(cached_flat.len().saturating_sub(1)));
        let (git_local_tx, git_local_rx) = mpsc::channel();
        let (fetch_tx, fetch_rx) = mpsc::channel();
        let (bg_tx, bg_rx) = mpsc::channel();
        let (runtime_tx, runtime_rx) = mpsc::channel();
        let runtime_client = runtime::Client::local();
        let terminal_input_tx = spawn_terminal_input_worker(runtime_client.clone(), bg_tx.clone());
        // ^ Register TUI presence only after initial ensure_available, or this TUI blocks its own handoff.
        let (_runtime_event_tx, runtime_event_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel::<String>();
        let (routine_tx, routine_rx) = mpsc::channel();
        let (integration_scan_tx, integration_scan_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = wsx_core::integration::scan().map_err(Into::into);
            let _ = integration_scan_tx.send(result);
        });
        std::thread::spawn(move || {
            if let Some(v) = crate::update::fetch_latest_version() {
                let _ = update_tx.send(v);
            }
        });
        let worktree_index = build_worktree_index(&workspace);
        let search_index = None;
        let terminal_controller_id = runtime::new_client_id();
        let terminal_escape_chord = EscapeSequence::parse(&config.terminal_escape_chord)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid terminal_escape_chord: {}",
                    config.terminal_escape_chord
                )
            })?;

        let mut app = Self {
            workspace,
            tree_selected,
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
            active_profile,
            group_header_scroll,
            group_header_area: Rect::default(),
            visible_projects,
            project_touched_unix_ms,
            stale_collapsed_projects,
            adaptive_collapse,
            review: None,
            review_available: false,
            notice: initial_notice.clone().map(|title| Notice {
                level: NoticeLevel::Warning,
                title,
                body: None,
            }),
            notice_started: initial_notice.as_ref().map(|_| Instant::now()),
            notice_queue: VecDeque::new(),
            jobs: vec![],
            next_job_id: 0,
            spinner_frame: 0,
            terminal_input_tx,
            bg_tx,
            bg_rx,
            needs_redraw: true,
            should_quit: false,
            force_terminal_redraw: false,
            reassert_terminal_modes: false,
            last_terminal_event: Instant::now(),
            force_preview_redraw: false,
            last_rendered_preview_was_session: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            git_sweep_timer: Timer::new(GIT_SWEEP_INTERVAL_MS),
            slow_timer: Timer::new(SLOW_INTERVAL_MS + (std::process::id() % 500) as u64),
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
            cache_path,
            cache_changes: Default::default(),
            cache_cursor: (raw_selected, cursor_identity.clone()),
            cached_worktree_expansion,
            worktree_index,
            git_semaphore: GitSemaphore::new(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            ),
            runtime_client,
            _runtime_monitor: None,
            runtime_event_rx,
            runtime_health: RuntimeHealth::Connecting,
            runtime_tx,
            runtime_rx,
            runtime_refresh_pending: false,
            startup_availability_pending: true,
            startup_cursor_identity: cursor_identity.clone(),
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
            muted_terminal_ids: cached_muted,
            acknowledged_outcomes,
            terminal_controller_id,
            terminal_surfaces: TerminalSurfaces::default(),
            terminal_stream: None,
            terminal_stream_generation: 0,
            pending_terminal_entry: None,
            pending_terminal_resume: None,
            pending_shell: None,
            last_stream_reattach: None,
            installed: crate::follow::Installed::capture(),
            follow_check_pending: false,
            follow_target: None,
            follow_resume_pane: None,
            // One daemon question before the first frame decides which screen opens.
            upgrade: runtime::pending_upgrade()
                .ok()
                .flatten()
                .map(|pending| UpgradeGate {
                    daemon_version: pending.daemon_version,
                    daemon_revision: pending.daemon_revision,
                    live_runtimes: pending.live_runtimes,
                    step: UpgradeStep::Upgrading,
                    retry_at: None,
                }),
            suspend_detector: SuspendDetector::new(),
            applied_revision: None,
            terminal_escape_chord,
            terminal_sidebar_override: None,
            update_rx,
            update_available: None,
            is_mobile: mobile,
            force_mobile: mobile,
            scanned_repos: Vec::new(),
            repo_scan_rx: None,
            routine_tx,
            routine_rx,
            routine_refresh_generation: HashMap::new(),
            integration_scan_rx,
            integration_metadata: Vec::new(),
            pending_integration_demand: None,
            pending_integration_updates: Vec::new(),
            dismissed_integration_prompts,
            persist_group_selection: true,
        };
        if let Some(identity) = cursor_identity.as_ref() {
            if let Some(index) =
                wsx_core::cache::find_cursor_index(&app.workspace, app.flat(), identity)
            {
                app.tree_selected = index;
            }
        }
        if app.upgrade.is_some() {
            app.spawn_upgrade_attempt();
        } else {
            app.spawn_runtime_refresh();
        }
        app.spawn_routine_refresh();
        app.spawn_repo_scan();
        Ok(app)
    }

    /// Kick off a background walk of `$HOME` for git repos. Idempotent
    /// while a previous scan is still draining — only the second open of
    /// the modal forks a refresh, not every render tick.
    fn spawn_repo_scan(&mut self) {
        if self.repo_scan_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.repo_scan_rx = Some(rx);
        std::thread::spawn(move || crate::repo_scan::scan_git_repos(tx));
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.set_notice(NoticeLevel::Success, msg);
    }

    fn set_warning(&mut self, msg: impl Into<String>) {
        self.set_notice(NoticeLevel::Warning, msg);
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        self.set_notice(NoticeLevel::Error, msg);
    }

    fn set_notice(&mut self, level: NoticeLevel, msg: impl Into<String>) {
        self.set_notice_at(level, msg, Instant::now());
    }

    fn set_notice_at(&mut self, level: NoticeLevel, msg: impl Into<String>, now: Instant) {
        let message = msg.into();
        let mut lines = message.lines();
        let title = lines.next().unwrap_or_default().to_string();
        let body = {
            let rest = lines.collect::<Vec<_>>().join("\n");
            (!rest.is_empty()).then_some(rest)
        };
        let notice = Notice { level, title, body };
        if self.notice.as_ref() == Some(&notice) || self.notice_queue.contains(&notice) {
            return;
        }
        if self.notice.is_none() {
            self.notice = Some(notice);
            self.notice_started = Some(now);
            self.needs_redraw = true;
            return;
        }
        if self.notice_queue.len() >= NOTICE_QUEUE_CAPACITY {
            let incoming_priority = notice_priority(notice.level);
            let Some(index) = self
                .notice_queue
                .iter()
                .position(|queued| notice_priority(queued.level) <= incoming_priority)
            else {
                return;
            };
            self.notice_queue.remove(index);
        }
        if notice.level == NoticeLevel::Error {
            let index = self
                .notice_queue
                .iter()
                .position(|queued| queued.level != NoticeLevel::Error)
                .unwrap_or(self.notice_queue.len());
            self.notice_queue.insert(index, notice);
        } else {
            self.notice_queue.push_back(notice);
        }
        if self
            .notice_started
            .is_some_and(|started| now.saturating_duration_since(started) >= NOTICE_MIN_DWELL)
        {
            self.promote_notice(now);
        }
    }

    fn promote_notice(&mut self, now: Instant) {
        let Some(notice) = self.notice_queue.pop_front() else {
            return;
        };
        self.notice = Some(notice);
        self.notice_started = Some(now);
        self.needs_redraw = true;
    }

    pub fn is_busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    fn has_working_agent(&self) -> bool {
        self.workspace.projects.iter().any(|project| {
            project.worktrees.iter().any(|worktree| {
                worktree.sessions.iter().any(|session| {
                    !session.muted
                        && (session.agent_status == runtime::AgentState::Working
                            || session.panes.iter().any(|pane| {
                                pane.agent_status == runtime::AgentState::Working
                                    && pane.agent.is_some()
                            }))
                })
            })
        })
    }

    fn shows_preview(&self) -> bool {
        !self.is_mobile || matches!(self.mode, Mode::Terminal { .. })
    }

    fn spawn_bg<F>(&mut self, label: impl Into<String>, f: F)
    where
        F: FnOnce() -> Result<BgOutcome> + Send + 'static,
    {
        let label = label.into();
        let id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push(BgJob {
            id,
            label: label.clone(),
        });
        self.needs_redraw = true;
        let tx = self.bg_tx.clone();
        std::thread::spawn(move || {
            let outcome = f();
            let _ = tx.send(BgResult { id, label, outcome });
        });
    }

    fn apply_bg_result(&mut self, result: BgResult) {
        self.jobs.retain(|j| j.id != result.id);
        self.needs_redraw = true;
        match result.outcome {
            Err(e) => {
                // ! clear pending_deletions and refresh so any optimistic removals are restored
                if !self.pending_deletions.is_empty() {
                    self.pending_deletions.clear();
                    self.spawn_runtime_refresh();
                }
                self.set_error(format!("{}: {}", result.label, e));
            }
            Ok(BgOutcome::WorktreeRemoved { label }) => {
                // ^ A live refresh clears the tombstone after Git stops reporting the path.
                self.spawn_runtime_refresh();
                self.set_status(label);
            }
            Ok(BgOutcome::WorktreeCreated {
                label,
                warning,
                session_error,
            }) => {
                self.spawn_runtime_refresh();
                if let Some(error) = session_error {
                    let detail = match warning {
                        Some(warning) => format!("{error}\n{warning}"),
                        None => error,
                    };
                    self.set_error(format!("{label}; default session failed\n{detail}"));
                } else if let Some(warning) = warning {
                    self.set_warning(format!("{label}\n{warning}"));
                } else {
                    self.set_status(label);
                }
            }
            Ok(BgOutcome::SessionKilled {
                session_id,
                display_name,
            }) => {
                // ^ Filter snapshots captured before the successful close.
                self.pending_session_kills.insert(session_id);
                for worktree in self
                    .workspace
                    .projects
                    .iter_mut()
                    .flat_map(|project| &mut project.worktrees)
                {
                    worktree
                        .sessions
                        .retain(|session| session.session_id != session_id);
                }
                self.rebuild_flat();
                self.clamp_selected();
                self.mark_dirty();
                self.spawn_runtime_refresh();
                self.set_status(format!("Killed session: {display_name}"));
            }
            Ok(BgOutcome::TerminalInputFailed(error)) => {
                self.set_error(format!("Action failed: {error}"));
            }
            Ok(BgOutcome::IntegrationsInstalled {
                installed,
                failures,
            }) => {
                let labels = installed
                    .iter()
                    .map(|target| target.label())
                    .collect::<Vec<_>>();
                for target in &installed {
                    self.dismissed_integration_prompts.remove(target);
                    self.cache_changes
                        .dismissed_integration_prompts
                        .insert(*target, false);
                    if let Ok(metadata) = wsx_core::integration::metadata(*target) {
                        if let Some(current) = self
                            .integration_metadata
                            .iter_mut()
                            .find(|item| item.target == *target)
                        {
                            *current = metadata;
                        } else {
                            self.integration_metadata.push(metadata);
                        }
                    }
                }
                self.mark_dirty();
                self.write_cache_if_dirty();
                if failures.is_empty() {
                    self.set_status(format!(
                        "Installed agent integrations: {}. Restart those agents to enable status.",
                        labels.join(", ")
                    ));
                } else {
                    let installed = if labels.is_empty() {
                        String::new()
                    } else {
                        format!("Installed {}. ", labels.join(", "))
                    };
                    self.set_error(format!(
                        "{installed}Some agent integrations failed: {}",
                        failures.join("; ")
                    ));
                }
            }
        }
    }

    fn ensure_flat(&mut self) {
        if self.flat_dirty {
            self.cached_flat = flatten_tree_filtered(&self.workspace, &self.visible_projects);
            self.search_index = None;
            self.flat_dirty = false;
            self.ensure_search_index();
        }
    }

    /// Builds the search index while Search mode is active and none is current.
    fn ensure_search_index(&mut self) {
        let Mode::Search { query, .. } = &self.mode else {
            return;
        };
        if self.search_index.is_none() {
            let mut index = SearchIndex::new(&self.workspace, &self.cached_flat);
            index.update(query);
            self.search_index = Some(index);
        }
    }

    fn refresh_visible_projects(&mut self) {
        // ^ A config reload may remove the active profile or add the first one.
        self.active_profile = initial_active_profile(&self.config, self.active_profile.take());
        self.visible_projects = compute_visible_projects(
            &self.config,
            &self.workspace,
            Some(&self.active_group),
            self.active_profile.as_deref(),
        );
    }

    /// Recompute visible project set from active groups, then rebuild flat and clamp cursor.
    fn recompute_visible(&mut self) {
        self.rebuild_flat();
        self.clamp_selected();
    }

    // ^ [[wsx UI Patterns]] Expansion and the inactivity timer are durable user state.
    // stale_collapsed_projects records a timer-driven collapse until explicit interaction.
    fn collapse_stale_projects(&mut self) {
        let policy = self.config.auto_collapse;
        if !matches!(policy, AutoCollapsePolicy::Adaptive { .. })
            && !self.adaptive_collapse.is_empty()
        {
            for (path, _) in std::mem::take(&mut self.adaptive_collapse) {
                let change = self.cache_changes.projects.entry(path.clone()).or_default();
                change.observed_touch_unix_ms = self
                    .project_touched_unix_ms
                    .get(&path)
                    .copied()
                    .unwrap_or(0);
                change.adaptive = Some(None);
            }
            self.mark_dirty();
        }
        let now_unix_ms = wsx_core::task::now_unix_ms();
        let mut changed = false;
        for project in &mut self.workspace.projects {
            let previous_adaptive = self.adaptive_collapse.get(&project.path).copied();
            let window_ms = match policy {
                AutoCollapsePolicy::Disabled => continue,
                AutoCollapsePolicy::Flat { hours } => hours.saturating_mul(MILLIS_PER_HOUR),
                AutoCollapsePolicy::Adaptive { base_hours } => {
                    let Some(activity_unix_ms) =
                        project_latest_activity_unix_ms(project, &self.project_touched_unix_ms)
                    else {
                        continue;
                    };
                    let state = match self.adaptive_collapse.entry(project.path.clone()) {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            changed = true;
                            entry.insert(AdaptiveCollapseState::new(base_hours, activity_unix_ms))
                        }
                    };
                    changed |= state.observe(base_hours, activity_unix_ms);
                    state.window_ms()
                }
            };
            if self.adaptive_collapse.get(&project.path).copied() != previous_adaptive {
                let change = self
                    .cache_changes
                    .projects
                    .entry(project.path.clone())
                    .or_default();
                change.observed_touch_unix_ms = self
                    .project_touched_unix_ms
                    .get(&project.path)
                    .copied()
                    .unwrap_or(0);
                change.adaptive = Some(
                    self.adaptive_collapse
                        .get(&project.path)
                        .copied()
                        .map(Into::into),
                );
            }
            if !project.expanded {
                continue;
            }
            if project_is_stale(
                project,
                &self.project_touched_unix_ms,
                now_unix_ms,
                window_ms,
            ) {
                project.expanded = false;
                self.stale_collapsed_projects.insert(project.path.clone());
                if let AutoCollapsePolicy::Adaptive { base_hours } = policy {
                    if let Some(state) = self.adaptive_collapse.get_mut(&project.path) {
                        state.reset_after_expiry(base_hours);
                    }
                }
                let change = self
                    .cache_changes
                    .projects
                    .entry(project.path.clone())
                    .or_default();
                change.observed_touch_unix_ms = self
                    .project_touched_unix_ms
                    .get(&project.path)
                    .copied()
                    .unwrap_or(0);
                change.expanded = Some(false);
                change.stale = Some(true);
                change.adaptive = Some(
                    self.adaptive_collapse
                        .get(&project.path)
                        .copied()
                        .map(Into::into),
                );
                changed = true;
            }
        }
        if changed {
            self.mark_dirty();
        }
    }

    pub(crate) fn stale_project_indices(&self) -> HashSet<usize> {
        self.workspace
            .projects
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                (self.stale_collapsed_projects.contains(&project.path)
                    && !project_has_active_sessions(project))
                .then_some(index)
            })
            .collect()
    }

    fn rebuild_flat(&mut self) {
        self.refresh_visible_projects();
        self.flat_dirty = true;
        self.ensure_flat();
        self.worktree_index = build_worktree_index(&self.workspace);
        self.close_stale_routine_detail();
    }

    fn close_stale_routine_detail(&mut self) {
        let is_stale = match &self.mode {
            Mode::RoutineDetail {
                project_path,
                routine_name,
                ..
            } => !self.workspace.projects.iter().any(|project| {
                project.path == *project_path
                    && project
                        .routines
                        .iter()
                        .any(|view| view.routine.name == *routine_name)
            }),
            _ => false,
        };
        if is_stale {
            self.mode = Mode::Workspace;
        }
    }

    pub fn flat(&self) -> &[FlatEntry] {
        debug_assert!(!self.flat_dirty, "flat() called with dirty cache");
        &self.cached_flat
    }

    fn spawn_routine_request(
        &mut self,
        project_path: PathBuf,
        action: asched_core::routine::ipc::Action,
        kind: RoutineResultKind,
    ) {
        let tx = self.routine_tx.clone();
        std::thread::spawn(move || {
            let response = crate::cli::send_routine(&project_path, action);
            let _ = tx.send(RoutineRefreshResult {
                project_path,
                kind,
                response,
            });
        });
    }

    fn spawn_routine_project_refresh(
        &mut self,
        project_path: PathBuf,
        expand: bool,
        selection: RoutineSelection,
    ) {
        let generation = self.invalidate_routine_refresh(&project_path);
        self.spawn_routine_request(
            project_path,
            asched_core::routine::ipc::Action::List,
            RoutineResultKind::Refresh {
                generation,
                expand,
                selection,
            },
        );
    }

    fn invalidate_routine_refresh(&mut self, path: &Path) -> u64 {
        let generation = self
            .routine_refresh_generation
            .entry(path.to_path_buf())
            .or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }
}

/// Projects can have multiple memberships, but the Workspace applies one optional group filter.
// ^ [[wsx Architecture]] Project grouping and filtering have separate cardinality.
fn initial_active_group(config: &GlobalConfig, stored: Option<GroupKey>) -> GroupKey {
    stored
        .filter(|candidate| config.ordered_group_keys().contains(candidate))
        .unwrap_or(GroupKey::Ungrouped)
}

/// The stored profile while the config declares it, else the first; None without
/// `[[profiles]]`. ref: README "Profiles"
fn initial_active_profile(config: &GlobalConfig, stored: Option<String>) -> Option<String> {
    if config.profiles.is_empty() {
        return None;
    }
    let names = config.profile_names();
    stored
        .filter(|name| names.contains(name))
        .or_else(|| names.first().cloned())
}

/// Groups filter inside the active profile.
fn compute_visible_projects(
    config: &GlobalConfig,
    workspace: &WorkspaceState,
    active_group: Option<&GroupKey>,
    active_profile: Option<&str>,
) -> HashSet<usize> {
    workspace
        .projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            (active_profile.is_none_or(|profile| config.project_profile(&project.path) == profile)
                && project_matches_group(config.project_groups(&project.path), active_group))
            .then_some(index)
        })
        .collect()
}

fn adjacent_group(groups: &[GroupKey], current: &GroupKey, dir: isize) -> Option<GroupKey> {
    if groups.len() <= 1 {
        return None;
    }
    let current = groups
        .iter()
        .position(|group| group == current)
        .unwrap_or(0);
    let target = if dir >= 0 {
        (current + 1) % groups.len()
    } else {
        current.checked_sub(1).unwrap_or(groups.len() - 1)
    };
    groups.get(target).cloned()
}

fn first_group_session(
    config: &GlobalConfig,
    workspace: &WorkspaceState,
    group: &GroupKey,
    matches: impl Fn(&wsx_core::model::workspace::SessionInfo) -> bool,
) -> Option<(usize, usize, usize)> {
    for (project_idx, project) in workspace.projects.iter().enumerate() {
        if !project_matches_group(config.project_groups(&project.path), Some(group)) {
            continue;
        }
        for (worktree_idx, worktree) in project.worktrees.iter().enumerate() {
            for (session_idx, session) in worktree.sessions.iter().enumerate() {
                if matches(session) {
                    return Some((project_idx, worktree_idx, session_idx));
                }
            }
        }
    }
    None
}

fn preferred_group_session(
    config: &GlobalConfig,
    workspace: &WorkspaceState,
    group: &GroupKey,
) -> Option<(usize, usize, usize)> {
    first_group_session(config, workspace, group, |session| {
        session_state::derive(session).app_state() == AppSessionState::NeedsAttention
    })
    .or_else(|| {
        first_group_session(config, workspace, group, |session| {
            session.agent.is_some()
                && session_state::derive(session).app_state() == AppSessionState::Idle
        })
    })
}

fn cyclic_sibling_index(current: usize, count: usize, dir: isize) -> Option<usize> {
    if count <= 1 || current >= count {
        return None;
    }
    Some(if dir >= 0 {
        (current + 1) % count
    } else {
        (current + count - 1) % count
    })
}

fn adjacent_visible_project_index(
    workspace: &WorkspaceState,
    visible: &HashSet<usize>,
    pi: usize,
    delta: isize,
) -> Option<usize> {
    let ordered_visible: Vec<usize> = workspace
        .projects
        .iter()
        .enumerate()
        .filter_map(|(idx, _)| visible.contains(&idx).then_some(idx))
        .collect();
    let current = ordered_visible.iter().position(|&idx| idx == pi)? as isize;
    let target = current + delta;
    if target < 0 || target >= ordered_visible.len() as isize {
        return None;
    }
    ordered_visible.get(target as usize).copied()
}

#[cfg(test)]
fn session_needs_attention(sess: &wsx_core::model::workspace::SessionInfo) -> bool {
    session_state::derive(sess).app_state() == AppSessionState::NeedsAttention
}

fn build_worktree_index(
    workspace: &WorkspaceState,
) -> std::collections::HashMap<PathBuf, (usize, usize)> {
    let mut idx = std::collections::HashMap::new();
    for (pi, proj) in workspace.projects.iter().enumerate() {
        for (wi, wt) in proj.worktrees.iter().enumerate() {
            idx.insert(wt.path.clone(), (pi, wi));
        }
    }
    idx
}

#[cfg(test)]
mod tests;
