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
    config: &GlobalConfig,
    availability: fn() -> std::io::Result<runtime::Availability>,
) -> RuntimeRefresh {
    let discovery = ops::discover_workspace(config)?;
    let worktrees = discovery.clone().into_worktrees();
    let availability = match availability() {
        Ok(availability) => availability,
        Err(error) => {
            return Ok(RuntimeRefreshOutcome::DiscoveryOnly {
                worktrees,
                error: error.into(),
            })
        }
    };
    let snapshot = match ops::synchronize_discovery(&discovery) {
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
        form: GlobalSettingsForm,
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
        ) = wsx_core::cache::apply_cache(&mut workspace)?;
        let stored_group = match wsx_core::cache::load_group_selection() {
            Ok(group) => group,
            Err(error) => {
                initial_notice.get_or_insert_with(|| {
                    format!("Could not read the saved group selection: {error}")
                });
                None
            }
        };
        let selected_group = initial_active_group(&config, stored_group.clone());
        if stored_group.as_ref() != Some(&selected_group) {
            if let Err(error) = wsx_core::cache::save_group_selection(&selected_group) {
                initial_notice
                    .get_or_insert_with(|| format!("Could not save the group selection: {error}"));
            }
        }
        let active_group = selected_group;
        let visible_projects = compute_visible_projects(&config, &workspace, Some(&active_group));
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
        self.visible_projects =
            compute_visible_projects(&self.config, &self.workspace, Some(&self.active_group));
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

    pub fn run(&mut self, terminal: &mut Tui, session: &tui::TerminalSession) -> Result<()> {
        // Kick off async git_info for all worktrees; render immediately with cached data.
        // Results arrive via drain_async_results() in the main loop without blocking.
        self.spawn_git_local_for_all();
        loop {
            if let Some(failure) = session.take_worker_failure() {
                self.set_error(failure);
            }
            if self.suspend_detector.resumed() {
                self.begin_resume_boundary();
                // The outer terminal can lose mouse capture across a sleep.
                self.reassert_terminal_modes = true;
            }
            // Drain async results every iteration so they're never blocked by UI events.
            self.drain_async_results();
            self.reconnect_terminal_after_resume(terminal);
            self.follow_installed_wsx(terminal)?;
            if let Err(error) = self.resume_followed_terminal(terminal) {
                self.set_error(format!("Terminal resume failed: {error}"));
            }

            // ^ docs/ui-state-ownership.md: resume, focus and resize write the modes again;
            // an idle window asks the terminal, because a reset sends no event.
            let mode_check = if std::mem::take(&mut self.reassert_terminal_modes) {
                tui::ModeCheck::Reassert
            } else if self.last_terminal_event.elapsed() >= MODE_PROBE_IDLE && session.probe_due() {
                tui::ModeCheck::Probe
            } else {
                tui::ModeCheck::Skip
            };
            if self.needs_redraw || mode_check != tui::ModeCheck::Skip {
                self.ensure_flat();
                let clear_terminal = self.force_terminal_redraw;
                let clear_preview = self.force_preview_redraw;
                self.force_terminal_redraw = false;
                self.force_preview_redraw = false;
                let preview_is_session = self.shows_preview()
                    && matches!(
                        self.current_selection(),
                        Selection::Session(..) | Selection::Pane(..)
                    );
                let cursor = self.terminal_cursor();
                match tui::draw_sync(
                    terminal,
                    session,
                    mode_check,
                    clear_terminal,
                    clear_preview,
                    cursor,
                    |frame, clear_preview| {
                        ui::render(frame, self);
                        if clear_preview {
                            frame.render_widget(Clear, self.preview_area);
                        }
                    },
                ) {
                    Ok(()) => self.last_rendered_preview_was_session = preview_is_session,
                    Err(e) => self.set_error(format!("Render failed: {e}")),
                }
                self.needs_redraw = false;
            }

            let event_mode = match &self.mode {
                _ if self.upgrade.is_some() => EventMode::Normal,
                Mode::Workspace => EventMode::Workspace,
                Mode::Terminal { .. } => EventMode::Terminal,
                Mode::Input { .. }
                | Mode::Search { .. }
                | Mode::GroupManager { .. }
                | Mode::GlobalSettings { .. }
                | Mode::IntegrationManager { .. }
                | Mode::RoutinePresetPicker { .. }
                | Mode::RoutineEditor { .. } => EventMode::Input,
                _ => EventMode::Normal,
            };
            let terminal_prefix_was_pending =
                event_mode == EventMode::Terminal && self.terminal_escape_chord.is_pending();
            let terminal_sidebar_before =
                (event_mode == EventMode::Terminal).then(|| self.effective_terminal_sidebar());
            let action = poll_event(
                Duration::from_millis(
                    if event_mode == EventMode::Terminal || self.pending_terminal_entry.is_some() {
                        TERMINAL_TICK_MS
                    } else {
                        TICK_MS
                    },
                ),
                event_mode,
                &mut self.terminal_escape_chord,
            )?;
            let action = action.map(|action| {
                self.last_terminal_event = Instant::now();
                if matches!(action, Action::Resize | Action::FocusGained) {
                    self.reassert_terminal_modes = true;
                    self.needs_redraw = true;
                }
                if action == Action::FocusGained {
                    Action::None
                } else {
                    action
                }
            });
            let mut terminal_sidebar_action = false;
            if self.upgrade.is_some() {
                match action {
                    Some(Action::Quit) => break,
                    Some(Action::Select) => self.open_on_running_daemon(),
                    Some(Action::None) | None => {}
                    // A resize or any other key repaints the same screen.
                    Some(_) => self.needs_redraw = true,
                }
                self.retry_upgrade();
                continue;
            }
            if let Some(action) = action {
                if action == Action::Quit && matches!(self.mode, Mode::Workspace) {
                    break;
                }
                if action != Action::None && action_needs_immediate_redraw(&action) {
                    self.needs_redraw = true;
                }
                terminal_sidebar_action = action == Action::ToggleTerminalSidebar;
                if let Err(e) = self.dispatch(action, terminal) {
                    if e.is::<tui::TerminalModeError>() {
                        return Err(e);
                    }
                    self.set_error(format!("Action failed: {e}"));
                }
            }
            if terminal_prefix_was_pending
                != (matches!(self.mode, Mode::Terminal { .. })
                    && self.terminal_escape_chord.is_pending())
            {
                self.needs_redraw = true;
            }
            if !terminal_sidebar_action {
                self.sync_terminal_sidebar_geometry(terminal_sidebar_before, terminal);
            }
            if self.should_quit {
                break;
            }
            if let Err(e) = self.tick() {
                self.set_error(format!("Background update failed: {e}"));
            }
        }
        Ok(())
    }

    fn drain_terminal_stream(&mut self) {
        let mut messages = Vec::new();
        if let Some(active) = self.terminal_stream.as_ref() {
            loop {
                match active.stream.try_recv() {
                    Ok(message) => messages.push((active.generation, active.epoch, message)),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        messages.push((
                            active.generation,
                            active.epoch,
                            runtime::TerminalServerMessage::Error(runtime::ApiError::new(
                                "stream_disconnected",
                                "terminal update stream disconnected",
                            )),
                        ));
                        break;
                    }
                }
            }
        }
        for (generation, epoch, message) in messages {
            if self
                .terminal_stream
                .as_ref()
                .is_none_or(|active| active.generation != generation)
            {
                continue;
            }
            match message {
                runtime::TerminalServerMessage::Update(update) => {
                    self.apply_terminal_update(generation, epoch, update)
                }
                runtime::TerminalServerMessage::ClipboardWrite(text) => {
                    if let Err(error) = crate::tui::copy_to_clipboard(&text) {
                        self.set_error(format!("Clipboard write failed: {error}"));
                    }
                }
                // ^ docs/daemon-live-upgrade.md: a replaced daemon keeps the PTY, so
                // reattach snapshot-first instead of leaving the terminal.
                runtime::TerminalServerMessage::Error(error)
                    if self.stream_loss_reattaches(&error) =>
                {
                    self.clear_active_terminal_selection();
                    self.begin_resume_boundary();
                    break;
                }
                runtime::TerminalServerMessage::Error(error) => {
                    self.clear_active_terminal_selection();
                    self.terminal_stream = None;
                    self.pending_terminal_entry = None;
                    self.mode = Mode::Workspace;
                    let (level, message) = terminal_stream_failure_notice(&error);
                    match level {
                        NoticeLevel::Warning => self.set_warning(message),
                        NoticeLevel::Error => self.set_error(message),
                        NoticeLevel::Info | NoticeLevel::Success => self.set_status(message),
                    }
                    break;
                }
                runtime::TerminalServerMessage::Exited => {
                    self.clear_active_terminal_selection();
                    // ^ The stream says it before the next snapshot does, and Enter on the
                    // pane must already start it again.
                    if let Some(pane_id) = self.terminal_stream.take().map(|active| active.pane_id)
                    {
                        self.terminal_surfaces.forget(pane_id);
                        if let Some(pane) = self.pane_mut(pane_id) {
                            pane.exited = true;
                        }
                    }
                    self.pending_terminal_entry = None;
                    self.mode = Mode::Workspace;
                    self.set_warning(
                        "Terminal process exited\nEnter starts it again; d closes the session",
                    );
                    break;
                }
            }
        }
    }

    fn apply_terminal_update(
        &mut self,
        generation: u64,
        epoch: u64,
        update: runtime::TerminalUpdate,
    ) {
        let (pane_id, terminal_id) = update.identity();
        let stream_identity = self.terminal_stream.as_ref().map(|active| {
            (
                active.generation,
                active.epoch,
                active.pane_id,
                active.terminal_id,
            )
        });
        if stream_identity != Some((generation, epoch, pane_id, terminal_id)) {
            return;
        }
        let pending_frame = match self.pending_terminal_entry {
            Some(pending) => match &update {
                runtime::TerminalUpdate::Full(frame)
                    if pending.matches_frame(generation, frame) =>
                {
                    Some((
                        pending,
                        self.terminal_surfaces.frame(pane_id, terminal_id) == Some(frame),
                    ))
                }
                _ => {
                    self.request_terminal_resync(generation);
                    return;
                }
            },
            None => None,
        };
        let new_epoch = self.terminal_surfaces.epoch() != Some(epoch);
        if new_epoch || !self.terminal_surfaces.contains(epoch, pane_id, terminal_id) {
            self.terminal_surfaces
                .activate_stream(epoch, pane_id, terminal_id);
        }
        match self.terminal_surfaces.apply(epoch, update) {
            SurfaceUpdate::Applied => {
                if let Some((pending, _)) = pending_frame {
                    self.activate_pending_terminal_entry(pending);
                }
                // Streamed terminal frames are complete projections. Let Ratatui diff one draw;
                // transition-only stale-glyph cleanup remains owned by update_scroll.
                self.needs_redraw = true;
            }
            SurfaceUpdate::Resync => self.request_terminal_resync(generation),
            SurfaceUpdate::Ignored => match pending_frame {
                Some((pending, true)) => {
                    self.activate_pending_terminal_entry(pending);
                    self.needs_redraw = true;
                }
                Some(_) => self.request_terminal_resync(generation),
                None => {}
            },
        }
    }

    fn request_terminal_resync(&self, generation: u64) {
        if let Some(active) = self
            .terminal_stream
            .as_ref()
            .filter(|active| active.generation == generation)
        {
            active.stream.request_resync();
        }
    }

    fn pending_terminal_entry_is_current(&self, pending: PendingTerminalEntry) -> bool {
        let mode_matches = matches!(self.mode, Mode::Workspace)
            || matches!(self.mode, Mode::Terminal { pane_id } if pane_id == pending.pane_id);
        mode_matches
            && self.selected_terminal_identity() == Some((pending.pane_id, pending.terminal_id))
    }

    fn activate_pending_terminal_entry(&mut self, pending: PendingTerminalEntry) {
        if self.pending_terminal_entry != Some(pending) {
            return;
        }
        if !self.pending_terminal_entry_is_current(pending) {
            self.pending_terminal_entry = None;
            self.terminal_stream = None;
            return;
        }
        self.pending_terminal_entry = None;
        self.mode = Mode::Terminal {
            pane_id: pending.pane_id,
        };
        self.terminal_escape_chord.reset();
    }

    fn cancel_pending_terminal_entry_if_stale(&mut self) {
        let stale = self
            .pending_terminal_entry
            .is_some_and(|pending| !self.pending_terminal_entry_is_current(pending));
        if stale {
            self.pending_terminal_entry = None;
            self.terminal_stream = None;
        }
    }

    fn drain_async_results(&mut self) {
        self.drain_terminal_stream();
        // Background repo discovery — survives modal opens. We dedup on the
        // spot so the list stays clean even across re-scans.
        if let Some(rx) = self.repo_scan_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(path) => {
                        if !self.scanned_repos.iter().any(|p| p == &path) {
                            self.scanned_repos.push(path);
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.repo_scan_rx = None;
                        break;
                    }
                }
            }
        }
        while let Ok((path, outcome)) = self.fetch_rx.try_recv() {
            self.apply_fetch_result(path, outcome);
        }
        while let Ok((path, info)) = self.git_local_rx.try_recv() {
            self.apply_git_local_result(path, info);
        }
        while let Ok(result) = self.bg_rx.try_recv() {
            self.apply_bg_result(result);
        }
        loop {
            let signal = self.runtime_event_rx.try_recv();
            match signal {
                Ok(signal) => self.apply_runtime_event(signal),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
        while let Ok(result) = self.runtime_rx.try_recv() {
            match result {
                RuntimeResult::FullRefresh(result) => self.apply_runtime_refresh(result),
                RuntimeResult::SessionRefresh(result) => self.apply_runtime_session_refresh(result),
                RuntimeResult::ResumeRefresh { generation, result } => {
                    self.apply_resume_refresh(generation, result)
                }
                RuntimeResult::Frame(frame) => self.apply_runtime_frame(frame),
                RuntimeResult::UpgradeAttempt(result) => self.apply_upgrade_attempt(result),
                RuntimeResult::FollowCheck(target) => {
                    self.follow_check_pending = false;
                    self.follow_target = target;
                }
                RuntimeResult::PluginView {
                    generation,
                    plugin_id,
                    pane_id,
                    result,
                } => self.apply_plugin_view(generation, &plugin_id, pane_id, result),
            }
        }
        while let Ok(result) = self.routine_rx.try_recv() {
            self.apply_routine_result(result);
        }
        if let Ok(v) = self.update_rx.try_recv() {
            self.update_available = Some(v);
            self.needs_redraw = true;
        }
        if let Ok(result) = self.integration_scan_rx.try_recv() {
            self.apply_integration_scan(result);
        }
    }

    // An unannounced disconnect reattaches once; a repeat within the window is a failure.
    fn stream_loss_reattaches(&mut self, error: &runtime::ApiError) -> bool {
        match error.code.as_str() {
            "daemon_handoff" => true,
            "stream_disconnected" => {
                let now = Instant::now();
                let repeated = self
                    .last_stream_reattach
                    .is_some_and(|last| now.duration_since(last) < STREAM_REATTACH_WINDOW);
                self.last_stream_reattach = Some(now);
                !repeated
            }
            _ => false,
        }
    }

    fn prepare_terminal_resume(&mut self) -> Option<u64> {
        if self.pending_terminal_entry.take().is_some() {
            self.terminal_stream = None;
            return None;
        }
        let active_identity = self
            .terminal_stream
            .take()
            .map(|active| (active.pane_id, active.terminal_id))
            .or_else(|| {
                self.pending_terminal_resume
                    .take()
                    .map(|pending| (pending.pane_id, pending.terminal_id))
            })?;
        self.terminal_surfaces.reset();
        self.terminal_stream_generation = self.terminal_stream_generation.wrapping_add(1);
        let generation = self.terminal_stream_generation;
        self.pending_terminal_resume = Some(PendingTerminalResume {
            pane_id: active_identity.0,
            terminal_id: active_identity.1,
            generation,
            snapshot_ready: false,
        });
        Some(generation)
    }

    fn begin_resume_boundary(&mut self) {
        self.fast_timer.reset();
        self.slow_timer.reset();
        let Some(generation) = self.prepare_terminal_resume() else {
            self.terminal_surfaces.reset();
            self.spawn_runtime_session_refresh();
            self.needs_redraw = true;
            return;
        };
        let tx = self.runtime_tx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                runtime::ensure_background_available()?;
                ops::runtime_snapshot()
            })();
            let _ = tx.send(RuntimeResult::ResumeRefresh { generation, result });
        });
        self.needs_redraw = true;
    }

    fn apply_resume_refresh(&mut self, generation: u64, result: Result<runtime::Snapshot>) {
        let Some(target) = self
            .pending_terminal_resume
            .as_ref()
            .filter(|target| target.generation == generation)
        else {
            return;
        };
        let (pane_id, terminal_id) = (target.pane_id, target.terminal_id);
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.pending_terminal_resume = None;
                self.mode = Mode::Workspace;
                self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()));
                return;
            }
        };
        let target_is_live = snapshot
            .panes
            .iter()
            .any(|pane| pane.id == pane_id && pane.terminal_id == terminal_id && !pane.exited);
        self.apply_projected_snapshot(snapshot, None);
        if target_is_live {
            if let Some(target) = self.pending_terminal_resume.as_mut() {
                target.snapshot_ready = true;
            }
        } else {
            self.pending_terminal_resume = None;
            self.mode = Mode::Workspace;
            self.set_warning("Terminal closed while wsx was reconnecting");
        }
    }

    fn reconnect_terminal_after_resume(&mut self, terminal: &Tui) {
        let Some(target) = self
            .pending_terminal_resume
            .as_ref()
            .filter(|target| target.snapshot_ready)
        else {
            return;
        };
        let (pane_id, terminal_id) = (target.pane_id, target.terminal_id);
        self.pending_terminal_resume = None;
        let (rows, cols) = self.terminal_pane_size(terminal);
        match runtime::TerminalStream::connect(
            &self.runtime_client,
            pane_id,
            self.terminal_controller_id,
            TerminalControlIntent::Resume.takeover(),
            rows,
            cols,
        ) {
            Ok(stream) => {
                self.terminal_stream_generation = self.terminal_stream_generation.wrapping_add(1);
                self.terminal_stream = Some(ActiveTerminalStream {
                    epoch: stream.epoch(),
                    pane_id,
                    terminal_id,
                    generation: self.terminal_stream_generation,
                    stream,
                });
                self.mode = Mode::Terminal { pane_id };
                self.terminal_escape_chord.reset();
            }
            Err(error) => {
                self.mode = Mode::Workspace;
                if error.to_string().starts_with("terminal_busy:") {
                    self.set_warning("Terminal control is active in another wsx instance");
                } else {
                    self.set_error(format!("Terminal resume failed: {error}"));
                }
            }
        }
    }

    fn apply_integration_scan(
        &mut self,
        result: Result<Vec<wsx_core::integration::IntegrationMetadata>>,
    ) {
        match result {
            Ok(metadata) => {
                if let Mode::IntegrationManager { manager, .. } = &mut self.mode {
                    manager.replace_metadata(metadata.clone());
                }
                self.integration_metadata = metadata;
                self.pending_integration_updates = self
                    .integration_metadata
                    .iter()
                    .filter(|metadata| {
                        metadata.available
                            && metadata.compatible
                            && metadata.install_status
                                == wsx_core::integration::InstallStatus::Outdated
                    })
                    .map(|metadata| metadata.target)
                    .collect();
                if let Some(target) = self.pending_integration_demand.take() {
                    self.prompt_for_integration_if_needed(target);
                }
                self.prompt_for_pending_integration_updates();
                self.needs_redraw = true;
            }
            Err(error) => self.set_warning(format!("Agent integration scan failed: {error}")),
        }
    }

    // ^ Startup and demand prompts share this modal-safe boundary and dismissal policy.
    fn prompt_for_integration_if_needed(
        &mut self,
        target: wsx_core::integration::IntegrationTarget,
    ) {
        use wsx_core::integration::InstallStatus;
        if !matches!(self.mode, Mode::Workspace) {
            self.pending_integration_demand = Some(target);
            return;
        }
        if self.integration_metadata.is_empty() {
            self.pending_integration_demand = Some(target);
            return;
        }
        self.pending_integration_demand = None;
        let Some(metadata) = self.integration_metadata.iter().find(|metadata| {
            metadata.target == target
                && metadata.available
                && metadata.compatible
                && metadata.install_status != InstallStatus::Current
        }) else {
            return;
        };
        let remember_dismissal = metadata.install_status == InstallStatus::Missing;
        if remember_dismissal && self.dismissed_integration_prompts.contains(&target) {
            return;
        }
        self.pending_integration_updates
            .retain(|pending| *pending != target);
        self.mode = Mode::Confirm {
            message: if remember_dismissal {
                format!(
                    "Install the {} status integration? Declining disables this prompt until you install it in Global Settings.",
                    target.label()
                )
            } else {
                format!("Update the {} status integration?", target.label())
            },
            pending: PendingAction::InstallIntegrations {
                targets: vec![target],
                remember_dismissal,
            },
        };
        self.needs_redraw = true;
    }

    fn prompt_for_pending_integration_updates(&mut self) {
        if !matches!(self.mode, Mode::Workspace) || self.pending_integration_updates.is_empty() {
            return;
        }
        let targets = std::mem::take(&mut self.pending_integration_updates);
        let labels = targets
            .iter()
            .map(|target| target.label())
            .collect::<Vec<_>>()
            .join(", ");
        self.mode = Mode::Confirm {
            message: format!("Update outdated agent integrations: {labels}?"),
            pending: PendingAction::InstallIntegrations {
                targets,
                remember_dismissal: false,
            },
        };
        self.needs_redraw = true;
    }

    pub fn upgrade_gate(&self) -> Option<&UpgradeGate> {
        self.upgrade.as_ref()
    }

    // The handoff blocks its caller until the successor answers, so it runs off the UI thread.
    fn spawn_upgrade_attempt(&mut self) {
        let tx = self.runtime_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::UpgradeAttempt(
                runtime::ensure_available().map_err(Into::into),
            ));
        });
    }

    fn apply_upgrade_attempt(&mut self, result: Result<runtime::Availability>) {
        // The user may have opened the old daemon while this attempt ran.
        let Some(gate) = self.upgrade.as_mut() else {
            return;
        };
        self.needs_redraw = true;
        match result {
            Ok(runtime::Availability::ReplacementDeferred {
                live_runtimes,
                blockers,
                ..
            }) => {
                gate.live_runtimes = live_runtimes;
                gate.step = UpgradeStep::Waiting(blockers);
                gate.retry_at = Some(Instant::now() + UPGRADE_RETRY);
            }
            Ok(availability) => {
                // A handoff that waited is finished by the daemon, not by this request.
                let availability = match availability {
                    runtime::Availability::Current => runtime::Availability::DaemonReplaced {
                        previous_version: gate.daemon_version.clone(),
                    },
                    other => other,
                };
                self.upgrade = None;
                self.announce_runtime_availability(&availability);
                self.spawn_runtime_refresh();
            }
            Err(error) => {
                let daemon_version = gate.daemon_version.clone();
                self.upgrade = None;
                self.set_warning(format!(
                    "Upgrade to wsxd {} did not finish; this window runs on wsxd {daemon_version}\n{error}",
                    runtime::WSX_VERSION
                ));
                // Do not request the handoff again on the way in: read the running daemon.
                self.spawn_runtime_refresh_with(|| Ok(runtime::Availability::Current));
            }
        }
    }

    fn retry_upgrade(&mut self) {
        let Some(gate) = self.upgrade.as_mut() else {
            return;
        };
        if gate
            .retry_at
            .is_some_and(|retry_at| retry_at <= Instant::now())
        {
            gate.retry_at = None;
            self.spawn_upgrade_attempt();
        }
    }

    // Only a waiting upgrade offers this; a running handoff cannot be left.
    fn open_on_running_daemon(&mut self) {
        if !matches!(
            self.upgrade.as_ref().map(|gate| &gate.step),
            Some(UpgradeStep::Waiting(_))
        ) {
            return;
        }
        self.upgrade = None;
        self.needs_redraw = true;
        // The normal start reports the waiting upgrade as a notice.
        self.spawn_runtime_refresh();
    }

    /// Show where this process came from and return to the terminal that was open.
    pub fn resume_after_follow(&mut self, from_version: &str, pane: Option<runtime::PaneId>) {
        self.follow_resume_pane = pane;
        self.set_notice(
            NoticeLevel::Success,
            format!(
                "wsx {from_version} became wsx {} with the upgraded wsxd\nSessions kept running",
                runtime::WSX_VERSION
            ),
        );
    }

    fn resume_followed_terminal(&mut self, terminal: &mut Tui) -> Result<()> {
        // The first authoritative snapshot restores the cursor from the cache.
        if self.startup_availability_pending {
            return Ok(());
        }
        let Some(pane_id) = self.follow_resume_pane.take() else {
            return Ok(());
        };
        self.ensure_flat();
        let target = self
            .cached_flat
            .iter()
            .enumerate()
            .find_map(|(index, entry)| match *entry {
                FlatEntry::Session {
                    project_idx,
                    worktree_idx,
                    session_idx,
                } => self
                    .workspace
                    .session(project_idx, worktree_idx, session_idx)
                    .filter(|session| session.pane_id == pane_id)
                    .map(|_| (index, project_idx, worktree_idx, session_idx)),
                _ => None,
            });
        let Some((index, project_idx, worktree_idx, session_idx)) = target else {
            self.set_warning("The terminal that was open is no longer listed");
            return Ok(());
        };
        self.tree_selected = index;
        self.update_scroll();
        self.switch_terminal_session(project_idx, worktree_idx, session_idx, terminal)
    }

    // ^ docs/daemon-live-upgrade.md: checked when the daemon connection changes and on
    // the slow tick. The file check is local; the daemon question runs off this thread.
    fn spawn_follow_check(&mut self) {
        if self.follow_check_pending || self.follow_target.is_some() {
            return;
        }
        let Some((path, identity)) = self.installed.as_ref().and_then(|wsx| wsx.replaced()) else {
            return;
        };
        self.follow_check_pending = true;
        let client = self.runtime_client.clone();
        let tx = self.runtime_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FollowCheck(crate::follow::target(
                &client, path, identity,
            )));
        });
    }

    // Unsent input, a pending operation or a half-typed prefix would be lost by a new process.
    fn follow_is_safe_now(&self) -> bool {
        matches!(self.mode, Mode::Workspace | Mode::Terminal { .. })
            && self.pending_terminal_entry.is_none()
            && !self.terminal_escape_chord.is_pending()
            && self.jobs.is_empty()
            && self.pending_deletions.is_empty()
            && self.pending_session_kills.is_empty()
            && self.pending_session_orders.is_empty()
    }

    fn follow_installed_wsx(&mut self, terminal: &mut Tui) -> Result<()> {
        if self.follow_target.is_none() || !self.follow_is_safe_now() {
            return Ok(());
        }
        // The new process restores the cursor from the cache.
        let saved = self.persist_state(true);
        let (Some(target), Some(installed)) = (self.follow_target.take(), self.installed.as_ref())
        else {
            return Ok(());
        };
        let pane = match self.mode {
            Mode::Terminal { pane_id } => Some(pane_id),
            _ => None,
        };
        let mobile = self.force_mobile;
        // Shell modes first: a failed exec then returns to this TUI.
        let exec = saved.and_then(|()| {
            tui::with_raw_mode_disabled(terminal, || -> Result<()> {
                Err(installed.exec(mobile, pane).into())
            })
        });
        let failure = match exec {
            Err(error) if error.is::<tui::TerminalModeError>() => return Err(error),
            Err(error) => error,
            Ok(()) => return Ok(()),
        };
        let daemon_version = target.daemon_version.clone();
        if let Some(installed) = self.installed.as_mut() {
            installed.refuse(target);
        }
        self.force_terminal_redraw = true;
        self.set_warning(format!(
            "Could not switch to the installed wsx for wsxd {daemon_version}; this window keeps wsx {}\n{failure}",
            runtime::WSX_VERSION
        ));
        Ok(())
    }

    fn apply_runtime_event(&mut self, signal: runtime::EventSignal) {
        if !matches!(signal, runtime::EventSignal::Dirty) {
            self.spawn_follow_check();
        }
        match signal {
            runtime::EventSignal::Dirty => self.spawn_runtime_session_refresh(),
            runtime::EventSignal::Connected(availability) => {
                // A new connection may belong to a different daemon epoch.
                self.spawn_runtime_session_refresh();
                if let Some(availability) = availability {
                    self.announce_runtime_availability(&availability);
                }
                self.needs_redraw = true;
            }
            runtime::EventSignal::Disconnected(error) => {
                // ^ A new reason replaces the notice: a handoff that did not finish follows
                // a plain disconnect (docs/daemon-live-upgrade.md "A stuck handoff").
                let new_reason = !matches!(
                    &self.runtime_health,
                    RuntimeHealth::Reconnecting { error: shown, .. } if *shown == error
                );
                let last_success = match self.runtime_health {
                    RuntimeHealth::Healthy { last_success } => Some(last_success),
                    RuntimeHealth::Reconnecting { last_success, .. } => last_success,
                    RuntimeHealth::Connecting => None,
                };
                self.runtime_health = RuntimeHealth::Reconnecting {
                    last_success,
                    error: error.clone(),
                };
                if new_reason {
                    self.set_notice(
                        NoticeLevel::Error,
                        format!("Runtime disconnected; retrying\n{error}"),
                    );
                }
                self.needs_redraw = true;
            }
        }
    }

    fn apply_routine_result(&mut self, result: RoutineRefreshResult) {
        let Some(project_idx) = self
            .workspace
            .projects
            .iter()
            .position(|project| project.path == result.project_path)
        else {
            return;
        };
        match (result.kind, result.response) {
            (
                RoutineResultKind::Refresh {
                    generation,
                    expand,
                    selection,
                },
                Ok(asched_core::routine::ipc::Response::Routines { revision, routines }),
            ) => {
                if self.routine_refresh_generation.get(&result.project_path) != Some(&generation) {
                    return;
                }
                let identity = matches!(selection, RoutineSelection::Preserve).then(|| {
                    wsx_core::cache::resolve_cursor_identity(
                        &self.workspace,
                        self.flat(),
                        self.tree_selected,
                    )
                });
                // ^ The snapshot may have removed the project while the request ran.
                let Some(project) = self.workspace.projects.get_mut(project_idx) else {
                    return;
                };
                project.routine_revision = revision;
                project.routines = routines;
                let expansion_changed = expand && !project.routines_expanded;
                project.routines_expanded |= expand;
                self.rebuild_flat();
                match selection {
                    RoutineSelection::Preserve => {
                        if let Some(Some(identity)) = identity {
                            if let Some(index) = wsx_core::cache::find_cursor_index(
                                &self.workspace,
                                self.flat(),
                                &identity,
                            ) {
                                self.tree_selected = index;
                            }
                        }
                    }
                    RoutineSelection::Header => self.select_routine(project_idx, None),
                    RoutineSelection::Named(name) => self.select_routine(project_idx, Some(&name)),
                }
                self.clamp_selected();
                if expansion_changed {
                    self.mark_dirty();
                    self.write_cache_if_dirty();
                }
                self.needs_redraw = true;
            }
            (RoutineResultKind::Refresh { generation, .. }, Err(error)) => {
                if self.routine_refresh_generation.get(&result.project_path) == Some(&generation) {
                    self.set_status(format!(
                        "Routines unavailable: {}",
                        routine_error_text(&error)
                    ));
                }
            }
            (
                RoutineResultKind::Save {
                    original_name: _,
                    can_rename: _,
                    form,
                    saved_name,
                },
                Ok(asched_core::routine::ipc::Response::Ok { .. }),
            ) => {
                self.spawn_routine_project_refresh(
                    result.project_path,
                    true,
                    RoutineSelection::Named(saved_name.clone()),
                );
                self.set_status(format!("Saved routine '{saved_name}'"));
                if let Some(target) = form.integration_target {
                    self.prompt_for_integration_if_needed(target);
                }
            }
            (
                RoutineResultKind::Save {
                    original_name,
                    can_rename,
                    form,
                    saved_name: _,
                },
                Err(error),
            ) => {
                if routine_error_kind(&error)
                    == Some(asched_core::routine::RoutineErrorKind::Conflict)
                {
                    self.spawn_routine_project_refresh(
                        result.project_path,
                        true,
                        RoutineSelection::Preserve,
                    );
                }
                self.mode = Mode::RoutineEditor {
                    project_idx,
                    original_name,
                    can_rename,
                    form,
                };
                self.set_status(format!("Routine not saved: {}", routine_error_text(&error)));
            }
            (
                RoutineResultKind::Delete { name },
                Ok(asched_core::routine::ipc::Response::Ok { .. }),
            ) => {
                self.spawn_routine_project_refresh(
                    result.project_path,
                    true,
                    RoutineSelection::Header,
                );
                self.set_status(format!("Deleted routine '{name}'"));
            }
            (RoutineResultKind::Delete { name }, Err(error)) => {
                if routine_error_kind(&error)
                    == Some(asched_core::routine::RoutineErrorKind::Conflict)
                {
                    self.spawn_routine_project_refresh(
                        result.project_path,
                        true,
                        RoutineSelection::Preserve,
                    );
                }
                self.restore_failed_routine_delete(project_idx, &name, routine_error_text(&error));
            }
            (_, Ok(response)) => self.set_status(format!(
                "Routine daemon returned an unexpected response: {response:?}"
            )),
        }
    }

    fn tick(&mut self) -> Result<()> {
        if self
            .review
            .as_ref()
            .is_some_and(|review| match self.current_selection() {
                Selection::Worktree(pi, wi) => self
                    .workspace
                    .worktree(pi, wi)
                    .is_none_or(|worktree| worktree.path != review.path),
                _ => true,
            })
        {
            self.review = None;
        }
        if self.review.as_mut().is_some_and(|review| review.poll()) {
            self.needs_redraw = true;
        }
        if let Mode::Tasks { list } = &mut self.mode {
            if list.poll() {
                self.needs_redraw = true;
            }
        }
        self.expire_notice(Instant::now());
        if let Some(target) = self.pending_integration_demand.take() {
            self.prompt_for_integration_if_needed(target);
        }
        self.prompt_for_pending_integration_updates();

        if self.slow_timer.ready() {
            // Staleness is wall-clock based and can change without a runtime event.
            let cursor = self.cursor_identity();
            self.collapse_stale_projects();
            self.rebuild_flat();
            self.restore_cursor(cursor);
            self.write_cache_if_dirty();
            self.spawn_follow_check();
            self.spawn_background_runtime_refresh();
            self.spawn_routine_refresh();
        }

        if self.git_sweep_timer.ready() {
            self.spawn_git_local_for_all();
        }

        if self.fast_timer.ready() {
            self.spawn_runtime_capture();
            self.spawn_plugin_view();
            self.spawn_git_local_for_selected();
            self.tick_git_fetch();
            if !self.jobs.is_empty() || self.has_working_agent() {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.needs_redraw = true;
            }
        }
        Ok(())
    }

    fn expire_notice(&mut self, now: Instant) {
        let Some(started) = self.notice_started else {
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        if !self.notice_queue.is_empty() && elapsed >= NOTICE_MIN_DWELL {
            self.promote_notice(now);
            return;
        }
        let timeout =
            Duration::from_secs(self.config.notification_timeout_seconds).max(NOTICE_MIN_DWELL);
        if elapsed >= timeout {
            self.notice = None;
            self.notice_started = None;
            self.needs_redraw = true;
        }
    }

    fn spawn_routine_refresh(&mut self) {
        let registered = match crate::cli::registered_routine_paths() {
            // ^ The scheduler's rule: without a registry it runs every project's routines.
            Ok(paths) => paths.map(|paths| paths.into_iter().collect::<HashSet<_>>()),
            Err(error) => {
                self.set_status(format!("Routines unavailable: {error}"));
                return;
            }
        };
        let mut paths = Vec::new();
        let mut cleared = false;
        for project in &mut self.workspace.projects {
            if registered
                .as_ref()
                .is_none_or(|paths| paths.contains(&project.path))
            {
                paths.push(project.path.clone());
            } else if !project.routines.is_empty() || project.routine_revision != 0 {
                project.routines.clear();
                project.routine_revision = 0;
                cleared = true;
            }
        }
        if cleared {
            self.rebuild_flat();
            self.needs_redraw = true;
        }
        for path in paths {
            self.spawn_routine_project_refresh(path, false, RoutineSelection::Preserve);
        }
    }

    /// Skip re-fetching git_info if the worktree is fresh and not currently selected.
    const GIT_INFO_CACHE_SECS: u64 = 15;

    fn spawn_git_local(&mut self, path: PathBuf, default_branch: String) {
        self.spawn_git_local_with_options(path, default_branch, false);
    }

    fn spawn_git_local_with_options(&mut self, path: PathBuf, default_branch: String, force: bool) {
        if self.git_local_pending.contains(&path) {
            return;
        }
        let is_selected = self
            .selected_worktree_indices()
            .and_then(|(project_idx, worktree_idx)| {
                self.workspace.worktree(project_idx, worktree_idx)
            })
            .is_some_and(|worktree| worktree.path == path);
        let cache_secs = if is_selected {
            1
        } else {
            Self::GIT_INFO_CACHE_SECS
        };
        if !force {
            if let Some(&(pi, wi)) = self.worktree_index.get(&path) {
                if let Some(wt) = self
                    .workspace
                    .projects
                    .get(pi)
                    .and_then(|p| p.worktrees.get(wi))
                {
                    let fresh = wt
                        .git_info_fetched_at
                        .map(|t| t.elapsed().as_secs() < cache_secs)
                        .unwrap_or(false);
                    if fresh {
                        return;
                    }
                }
            }
        }
        let subtrees = self
            .worktree_index
            .get(&path)
            .and_then(|(project_idx, _)| self.workspace.projects.get(*project_idx))
            .and_then(|project| project.config.as_ref())
            .map(|config| config.git_subtrees.clone())
            .unwrap_or_default();
        self.git_local_pending.insert(path.clone());
        let tx = self.git_local_tx.clone();
        let sem = self.git_semaphore.clone();
        std::thread::spawn(move || {
            let _permit = sem.acquire();
            let info = git_info::get_git_info(&path, &default_branch, &subtrees);
            let _ = tx.send((path, info));
        });
    }

    fn spawn_git_local_for_selected(&mut self) {
        let Some((project_idx, worktree_idx)) = self.selected_worktree_indices() else {
            return;
        };
        let Some(project) = self.workspace.projects.get(project_idx) else {
            return;
        };
        let Some(worktree) = project.worktrees.get(worktree_idx) else {
            return;
        };
        self.spawn_git_local(worktree.path.clone(), project.default_branch.clone());
    }

    /// Kick off async git_info refresh for all worktrees (periodic timer path).
    fn spawn_git_local_for_all(&mut self) {
        let targets: Vec<(PathBuf, String)> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| {
                let branch = p.default_branch.clone();
                p.worktrees
                    .iter()
                    .map(move |w| (w.path.clone(), branch.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (path, branch) in targets {
            self.spawn_git_local(path, branch);
        }
    }

    fn apply_fetch_result(&mut self, path: PathBuf, outcome: FetchOutcome) {
        let completed_at = Instant::now();
        self.fetch_pending.remove(&path);

        let mut spawn_branch: Option<String> = None;
        if let Some(&(pi, wi)) = self.worktree_index.get(&path) {
            if let Some(proj) = self.workspace.projects.get_mut(pi) {
                if let Some(wt) = proj.worktrees.get_mut(wi) {
                    // Throttle fetch attempts after both success and failure.
                    wt.last_fetched = Some(completed_at);
                    if outcome.success {
                        wt.fetch_failed = false;
                        wt.fetch_fail_count = 0;
                        wt.fetch_fail_reason = None;
                        spawn_branch = Some(proj.default_branch.clone());
                    } else {
                        wt.fetch_failed = true;
                        wt.fetch_fail_count = wt.fetch_fail_count.saturating_add(1);
                        wt.fetch_fail_reason = outcome.reason;
                    }
                    self.needs_redraw = true;
                }
            }
        }
        if let Some(branch) = spawn_branch {
            self.spawn_git_local(path, branch);
        }
    }

    fn apply_git_local_result(&mut self, path: PathBuf, info: Option<GitInfo>) {
        self.git_local_pending.remove(&path);
        if let Some(&(pi, wi)) = self.worktree_index.get(&path) {
            if let Some(wt) = self
                .workspace
                .projects
                .get_mut(pi)
                .and_then(|p| p.worktrees.get_mut(wi))
            {
                // ! timestamp unconditionally — throttles retries even on failed repos
                wt.git_info_fetched_at = Some(Instant::now());
                if let Some(gi) = info {
                    if wt.git_info.as_ref() != Some(&gi) {
                        wt.git_info = Some(gi);
                        self.needs_redraw = true;
                    }
                }
                // if info is None, leave existing git_info unchanged — old value stays visible
            }
        }
    }

    pub fn flush_cache(&mut self) -> Result<()> {
        self.persist_state(true)
    }

    // ^ docs/ui-state-ownership.md: observations cannot overwrite another client's intent.
    fn persist_state(&mut self, sync: bool) -> Result<()> {
        self.ensure_flat();
        let cursor = (
            self.tree_selected,
            wsx_core::cache::resolve_cursor_identity(
                &self.workspace,
                &self.cached_flat,
                self.tree_selected,
            ),
        );
        if cursor != self.cache_cursor {
            self.cache_changes.cursor = Some(cursor.clone());
        }
        let committed = match self.cache_changes.save(sync) {
            Ok(committed) => committed,
            Err(error) => {
                self.set_error(format!("Cache save failed: {error}"));
                self.cache_dirty = true;
                return Err(error.context("could not persist UI intent"));
            }
        };
        let projects_changed = !committed.is_empty();
        for (path, state) in committed {
            if let Some(touched) = state.touched_unix_ms {
                self.project_touched_unix_ms.insert(path.clone(), touched);
            }
            if state.stale {
                self.stale_collapsed_projects.insert(path.clone());
            } else {
                self.stale_collapsed_projects.remove(&path);
            }
            if let Some(adaptive) = state.adaptive {
                self.adaptive_collapse.insert(path.clone(), adaptive);
            } else {
                self.adaptive_collapse.remove(&path);
            }
            if let Some(project) = self
                .workspace
                .projects
                .iter_mut()
                .find(|project| project.path == path)
            {
                if let Some(expanded) = state.expanded {
                    project.expanded = expanded;
                }
                if let Some(expanded) = state.routines_expanded {
                    project.routines_expanded = expanded;
                }
            }
        }
        if projects_changed {
            self.recompute_visible();
            self.needs_redraw = true;
        }
        self.cache_changes = Default::default();
        self.cache_cursor = cursor;
        self.cache_dirty = false;
        Ok(())
    }

    fn mark_dirty(&mut self) {
        self.cache_dirty = true;
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        // ^ Keep one ordered refresh stream so older snapshots cannot clear mutation tombstones.
        self.spawn_runtime_refresh();
        Ok(())
    }

    fn write_cache_if_dirty(&mut self) {
        if self.cache_dirty {
            let _ = self.persist_state(false);
        }
    }

    fn filter_pending_deletions(
        &mut self,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
        filter_pending_deletions(&mut self.pending_deletions, worktrees)
    }

    fn spawn_runtime_refresh(&mut self) {
        self.spawn_runtime_refresh_with(runtime::ensure_available);
    }

    fn spawn_runtime_refresh_with(
        &mut self,
        availability: fn() -> std::io::Result<runtime::Availability>,
    ) {
        if self.runtime_refresh_pending {
            self.runtime_full_refresh_stale = true;
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FullRefresh(collect_runtime_refresh(
                &config,
                availability,
            )));
        });
    }

    fn spawn_background_runtime_refresh(&mut self) {
        if self.runtime_refresh_pending {
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FullRefresh(collect_runtime_refresh(
                &config,
                runtime::ensure_background_available,
            )));
        });
    }

    fn spawn_runtime_session_refresh(&mut self) {
        if self.runtime_refresh_pending {
            self.runtime_refresh_stale = true;
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::SessionRefresh(ops::runtime_snapshot()));
        });
    }

    fn apply_runtime_refresh(&mut self, result: RuntimeRefresh) {
        self.runtime_refresh_pending = false;
        match result {
            Ok(RuntimeRefreshOutcome::Authoritative {
                availability,
                snapshot,
                worktrees,
            }) => {
                self.apply_runtime_snapshot(*snapshot, worktrees);
                self.announce_startup_availability(&availability);
            }
            Ok(RuntimeRefreshOutcome::DiscoveryOnly { worktrees, error }) => {
                let worktrees = self.filter_pending_deletions(worktrees);
                if let Err(refresh_error) = ops::refresh_workspace_discovery_only(
                    &mut self.workspace,
                    &self.config,
                    worktrees,
                ) {
                    self.set_error(format!("Workspace discovery rejected: {refresh_error}"));
                } else {
                    self.collapse_stale_projects();
                    self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()));
                }
            }
            Err(error) => {
                self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()))
            }
        }
        self.start_runtime_monitor();
        self.spawn_queued_runtime_refresh();
    }

    fn start_runtime_monitor(&mut self) {
        if self._runtime_monitor.is_some() {
            return;
        }
        let monitor = if self.installed.is_some() {
            runtime::EventMonitor::start_following(self.runtime_client.clone())
        } else {
            runtime::EventMonitor::start(self.runtime_client.clone())
        };
        match monitor {
            Ok((monitor, receiver)) => {
                self._runtime_monitor = Some(monitor);
                self.runtime_event_rx = receiver;
            }
            Err(error) => self.set_error(format!("Runtime monitor unavailable: {error}")),
        }
    }

    fn announce_startup_availability(&mut self, availability: &runtime::Availability) {
        if !std::mem::take(&mut self.startup_availability_pending) {
            return;
        }
        self.announce_runtime_availability(availability);
    }

    fn announce_runtime_availability(&mut self, availability: &runtime::Availability) {
        if let Some((level, message)) = runtime_availability_notice(availability) {
            self.set_notice(level, message);
        }
    }

    fn apply_runtime_session_refresh(&mut self, result: Result<runtime::Snapshot>) {
        self.runtime_refresh_pending = false;
        match result {
            Ok(snapshot) => self.apply_projected_snapshot(snapshot, None),
            Err(error) => {
                self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()))
            }
        }
        self.spawn_queued_runtime_refresh();
    }

    fn spawn_queued_runtime_refresh(&mut self) {
        if std::mem::take(&mut self.runtime_full_refresh_stale) {
            self.runtime_refresh_stale = false;
            self.spawn_runtime_refresh();
        } else if std::mem::take(&mut self.runtime_refresh_stale) {
            self.spawn_runtime_session_refresh();
        }
    }

    fn apply_runtime_snapshot(
        &mut self,
        snapshot: runtime::Snapshot,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) {
        self.apply_projected_snapshot(snapshot, Some(worktrees));
        if let Some(identity) = self.startup_cursor_identity.as_ref() {
            if let Some(index) =
                wsx_core::cache::find_cursor_index(&self.workspace, self.flat(), identity)
            {
                self.tree_selected = index;
                self.startup_cursor_identity = None;
                self.update_scroll();
            }
        }
    }

    fn apply_projected_snapshot(
        &mut self,
        mut snapshot: runtime::Snapshot,
        worktrees: Option<Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>>,
    ) {
        if self.applied_revision.is_some_and(|(epoch, revision)| {
            epoch == snapshot.epoch && snapshot.revision < revision
        }) {
            return;
        }
        self.applied_revision = Some((snapshot.epoch, snapshot.revision));
        let cursor = self.cursor_identity();
        self.review_available = snapshot.capabilities.worktree_review_available;
        if self.plugin_sidecars != snapshot.plugin_sidecars {
            self.plugin_sidecars = snapshot.plugin_sidecars.clone();
            self.plugin_view_generation = self.plugin_view_generation.wrapping_add(1);
            self.plugin_view_pending = false;
            self.plugin_view_last_refresh = None;
            self.plugin_view_last_owner = None;
            if self.plugin_view.as_ref().is_some_and(|view| {
                !self
                    .plugin_sidecars
                    .iter()
                    .any(|sidecar| sidecar.plugin_id == view.plugin_id)
            }) {
                self.plugin_view = None;
            }
        }
        let live_sessions = snapshot
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        self.pending_session_kills
            .retain(|session_id| live_sessions.contains(session_id));
        snapshot
            .sessions
            .retain(|session| !self.pending_session_kills.contains(&session.id));
        let visible_sessions = snapshot
            .sessions
            .iter()
            .map(|session| session.id)
            .collect::<HashSet<_>>();
        snapshot
            .panes
            .retain(|pane| visible_sessions.contains(&pane.session_id));
        self.terminal_surfaces.reconcile(&snapshot);
        let stream_is_stale = self.terminal_stream.as_ref().is_some_and(|active| {
            !self
                .terminal_surfaces
                .contains(active.epoch, active.pane_id, active.terminal_id)
        });
        if stream_is_stale {
            self.terminal_stream = None;
            self.pending_terminal_entry = None;
        }
        let refresh = match worktrees {
            Some(worktrees) => {
                let worktrees = self.filter_pending_deletions(worktrees);
                ops::refresh_workspace_with_worktrees(
                    &mut self.workspace,
                    &self.config,
                    &snapshot,
                    worktrees,
                )
            }
            None => ops::refresh_sessions_from_snapshot(&mut self.workspace, &snapshot),
        };
        if let Err(error) = refresh {
            self.set_error(format!("Runtime snapshot rejected: {error}"));
            return;
        }
        for worktree in self
            .workspace
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
        {
            if let Some(expanded) = self
                .cached_worktree_expansion
                .remove(&worktree.path.to_string_lossy().into_owned())
            {
                // Apply saved presentation once when asynchronous discovery first creates this row.
                if !self
                    .cache_changes
                    .worktree_expanded
                    .contains_key(&worktree.path)
                {
                    worktree.expanded = expanded;
                }
            }
        }
        self.reconcile_pending_session_orders();
        self.collapse_stale_projects();
        if matches!(self.mode, Mode::Terminal { .. })
            && self.terminal_stream.is_none()
            && self.pending_terminal_resume.is_none()
        {
            self.mode = Mode::Workspace;
            self.set_warning("Terminal closed");
        }
        for session in self
            .workspace
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
            .flat_map(|worktree| &mut worktree.sessions)
        {
            session.muted = self
                .muted_terminal_ids
                .contains(&session.terminal_id.to_string());
            for pane in &mut session.panes {
                pane.outcome_acknowledged = pane.agent_status == runtime::AgentState::Done
                    && self
                        .acknowledged_outcomes
                        .get(&pane.terminal_id.to_string())
                        == Some(&pane.revision);
            }
            session.outcome_acknowledged = session
                .panes
                .iter()
                .find(|pane| pane.pane_id == session.pane_id)
                .is_some_and(|pane| pane.outcome_acknowledged);
        }
        self.rebuild_flat();
        self.restore_cursor(cursor);
        self.cancel_pending_terminal_entry_if_stale();
        self.runtime_health = RuntimeHealth::Healthy {
            last_success: Instant::now(),
        };
        self.mark_dirty();
        self.write_cache_if_dirty();
        self.needs_redraw = true;
    }

    fn reconcile_pending_session_orders(&mut self) {
        let workspace = &mut self.workspace;
        self.pending_session_orders.retain(|path, pending| {
            let Some(sessions) = workspace
                .projects
                .iter_mut()
                .flat_map(|project| &mut project.worktrees)
                .find(|worktree| worktree.path == *path)
                .map(|worktree| &mut worktree.sessions)
            else {
                return false;
            };
            let order_matches = sessions
                .iter()
                .map(|session| session.session_id)
                .eq(pending.session_ids.iter().copied());
            if order_matches {
                return false;
            }
            let snapshot_is_stale = sessions
                .iter()
                .find(|session| session.session_id == pending.moved_session_id)
                .is_some_and(|session| session.revision < pending.revision);
            if !snapshot_is_stale {
                return false;
            }
            let positions = pending
                .session_ids
                .iter()
                .enumerate()
                .map(|(index, session_id)| (*session_id, index))
                .collect::<HashMap<_, _>>();
            sessions.sort_by_key(|session| {
                positions
                    .get(&session.session_id)
                    .copied()
                    .unwrap_or(usize::MAX)
            });
            true
        });
    }

    fn selected_terminal_identity(&self) -> Option<(runtime::PaneId, runtime::TerminalId)> {
        match self.current_selection() {
            Selection::Session(pi, wi, si) => self
                .workspace
                .session(pi, wi, si)
                .map(|session| (session.pane_id, session.terminal_id)),
            Selection::Pane(pi, wi, si, pane_idx) => self
                .workspace
                .session(pi, wi, si)
                .and_then(|session| session.panes.get(pane_idx))
                .map(|pane| (pane.pane_id, pane.terminal_id)),
            _ => None,
        }
    }

    pub(crate) fn terminal_sidecar_descriptor(
        &self,
        total_width: u16,
        preview_width: u16,
    ) -> Option<runtime::PluginSidecarDescriptor> {
        if self.force_mobile || total_width < 60 {
            return None;
        }
        let descriptor = self
            .plugin_sidecars
            .iter()
            .find(|sidecar| {
                sidecar.spec.surface == runtime::PluginSurface::TerminalRight
                    && total_width >= sidecar.spec.minimum_columns
            })?
            .clone();
        let width = descriptor
            .spec
            .preferred_width
            .min(preview_width.saturating_sub(60));
        if width < 20 {
            return None;
        }
        self.selected_terminal_identity()?;
        Some(descriptor)
    }

    pub(crate) fn plugin_view(&self, plugin_id: &str) -> Option<&runtime::PluginSidecarView> {
        let pane_id = self.selected_terminal_identity()?.0;
        self.plugin_view
            .as_ref()
            .filter(|view| view.plugin_id == plugin_id && view.pane_id == pane_id)
    }

    fn spawn_plugin_view(&mut self) {
        if !matches!(self.mode, Mode::Terminal { .. })
            || self.plugin_view_pending
            || self.plugin_area.is_empty()
        {
            return;
        }
        let Some((pane_id, _)) = self.selected_terminal_identity() else {
            return;
        };
        let Some(descriptor) = self.plugin_sidecars.iter().find(|sidecar| {
            sidecar.spec.surface == runtime::PluginSurface::TerminalRight
                && sidecar.spec.preferred_width >= self.plugin_area.width
        }) else {
            return;
        };
        let owner = (descriptor.plugin_id.clone(), pane_id);
        if self.plugin_view_last_owner.as_ref() == Some(&owner)
            && self.plugin_view_last_refresh.is_some_and(|last| {
                last.elapsed() < Duration::from_millis(descriptor.spec.refresh_ms)
            })
        {
            return;
        }
        self.plugin_view_generation = self.plugin_view_generation.wrapping_add(1);
        let generation = self.plugin_view_generation;
        let plugin_id = descriptor.plugin_id.clone();
        let columns = self.plugin_area.width;
        let rows = self.plugin_area.height;
        let tx = self.runtime_tx.clone();
        let client = self.runtime_client.clone();
        self.plugin_view_pending = true;
        self.plugin_view_last_refresh = Some(Instant::now());
        self.plugin_view_last_owner = Some(owner);
        std::thread::spawn(move || {
            let result = match client.call(&runtime::Request::PluginRender {
                plugin_id: plugin_id.clone(),
                pane_id,
                columns,
                rows,
                generation,
            }) {
                Ok(runtime::Response::PluginView(view)) => Ok(view),
                Ok(runtime::Response::Error(error)) => Err(api_error(error)),
                Ok(_) => Err(anyhow::anyhow!("unexpected daemon plugin view response")),
                Err(error) => Err(error.into()),
            };
            let _ = tx.send(RuntimeResult::PluginView {
                generation,
                plugin_id,
                pane_id,
                result,
            });
        });
    }

    fn apply_plugin_view(
        &mut self,
        generation: u64,
        plugin_id: &str,
        pane_id: runtime::PaneId,
        result: Result<runtime::PluginSidecarView>,
    ) {
        if generation != self.plugin_view_generation {
            return;
        }
        self.plugin_view_pending = false;
        let current = self.selected_terminal_identity().map(|identity| identity.0);
        let descriptor_exists = self
            .plugin_sidecars
            .iter()
            .any(|sidecar| sidecar.plugin_id == plugin_id);
        if current != Some(pane_id) || !descriptor_exists {
            return;
        }
        if let Ok(view) = result {
            if view.generation == generation
                && view.plugin_id == plugin_id
                && view.pane_id == pane_id
                && self.terminal_surfaces.epoch() == Some(view.epoch)
            {
                self.plugin_view = Some(view);
                self.needs_redraw = true;
            }
        }
    }

    fn spawn_runtime_capture(&mut self) {
        if self.terminal_stream.is_some() || self.runtime_capture_pending || !self.shows_preview() {
            return;
        }
        let Some((pane_id, _)) = self.selected_terminal_identity() else {
            return;
        };
        self.runtime_capture_pending = true;
        let tx = self.runtime_tx.clone();
        let client = self.runtime_client.clone();
        std::thread::spawn(move || {
            let frame = match client.call(&runtime::Request::View {
                pane_ids: vec![pane_id],
            }) {
                Ok(runtime::Response::View {
                    snapshot,
                    mut frames,
                }) => frames
                    .pop()
                    .map(|frame| (snapshot.epoch, frame))
                    .ok_or_else(|| anyhow::anyhow!("pane frame is unavailable")),
                Ok(runtime::Response::Error(error)) => Err(api_error(error)),
                Ok(_) => Err(anyhow::anyhow!("unexpected daemon view response")),
                Err(error) => Err(error.into()),
            };
            let _ = tx.send(RuntimeResult::Frame(frame));
        });
    }

    fn apply_runtime_frame(&mut self, frame: Result<(u64, runtime::TerminalFrame)>) {
        self.runtime_capture_pending = false;
        let Ok((epoch, frame)) = frame else {
            self.set_error("Terminal frame unavailable; showing the last frame");
            return;
        };
        if self.terminal_stream.is_some()
            || self.selected_terminal_identity() != Some((frame.pane_id, frame.terminal_id))
        {
            return;
        }
        match self.terminal_surfaces.install_full(epoch, frame) {
            SurfaceUpdate::Applied => {
                self.needs_redraw = true;
                self.force_preview_redraw = self.shows_preview();
            }
            SurfaceUpdate::Resync => self.set_error("Terminal returned an invalid full frame"),
            SurfaceUpdate::Ignored => {}
        }
    }

    /// Git fetch trigger — called from the fast timer tick.
    fn tick_git_fetch(&mut self) {
        let Some((pi, wi)) = self.selected_worktree_indices() else {
            return;
        };
        let fetch_info = self.workspace.worktree(pi, wi).map(|wt| {
            let interval = FETCH_INTERVAL_SECS * 2u64.pow(wt.fetch_fail_count.min(4));
            let stale = wt
                .last_fetched
                .map(|t| t.elapsed().as_secs() >= interval)
                .unwrap_or(true);
            let in_flight = self.fetch_pending.contains(&wt.path);
            (stale && !in_flight, wt.path.clone())
        });
        if let Some((true, path)) = fetch_info {
            self.fetch_pending.insert(path.clone());
            let tx = self.fetch_tx.clone();
            std::thread::spawn(move || {
                let outcome = git_info::git_fetch(&path);
                let _ = tx.send((path, outcome));
            });
        }
    }

    pub fn terminal_escape_label(&self) -> &str {
        &self.terminal_escape_chord.label
    }

    pub fn terminal_literal_escape_label(&self) -> String {
        self.terminal_escape_chord.literal_label()
    }

    pub fn terminal_session_navigation_label(&self) -> &'static str {
        self.terminal_escape_chord.session_navigation_label()
    }

    pub fn terminal_workspace_hint(&self) -> String {
        format!("({})workspace", self.terminal_escape_chord.label)
    }

    pub fn terminal_prefix_pending(&self) -> bool {
        self.terminal_escape_chord.is_pending()
    }

    pub fn terminal_command_hints(&self) -> Vec<String> {
        let Some(workspace) = self.terminal_escape_chord.suffix_label() else {
            return vec![self.terminal_workspace_hint().to_ascii_lowercase()];
        };
        let prefix = self
            .terminal_escape_chord
            .prefix_label()
            .to_ascii_lowercase();
        let hint = |key: &str, action: &str| format!("({key}){action}");
        let mut hints = vec![hint(&prefix, "commands")];
        if self.terminal_prefix_pending() {
            hints.push(hint("esc", "cancel"));
        }
        hints.push(hint(&workspace.to_ascii_lowercase(), "workspace"));
        // A hint names a key only while the key has a target.
        if self.context_session_target(1).is_some() {
            hints.push(hint(self.terminal_session_navigation_label(), "session"));
        }
        if adjacent_group(&self.config.ordered_group_keys(), &self.active_group, 1).is_some() {
            hints.push(hint("{/}", "group"));
        }
        hints.extend([
            crate::ui::IDLE_ITERATION_HINT.to_string(),
            crate::ui::ACTIVE_ITERATION_HINT.to_string(),
            crate::ui::ATTENTION_ITERATION_HINT.to_string(),
        ]);
        if !self.is_mobile {
            hints.push(hint("b", "sidebar"));
        }
        hints.push(hint("q", "quit"));
        if self.terminal_prefix_pending() {
            hints.push(hint(&prefix, "send"));
        }
        hints
    }

    pub fn terminal_quit_label(&self) -> Option<String> {
        self.terminal_escape_chord.quit_label()
    }

    pub fn terminal_sidebar_hint(&self) -> Option<String> {
        (!self.is_mobile)
            .then(|| self.terminal_escape_chord.sidebar_label())
            .flatten()
            .map(|label| format!("({label})sidebar"))
    }

    fn terminal_sidebar_baseline(&self) -> TerminalSidebar {
        self.terminal_sidebar_override
            .unwrap_or(self.config.terminal_sidebar)
    }

    pub(crate) fn effective_terminal_sidebar(&self) -> TerminalSidebar {
        let baseline = self.terminal_sidebar_baseline();
        if !self.is_mobile
            && self.config.terminal_prefix_shows_sidebar
            && self.terminal_prefix_pending()
            && baseline == TerminalSidebar::Compact
        {
            TerminalSidebar::Expanded
        } else {
            baseline
        }
    }

    fn sync_terminal_sidebar_geometry(
        &mut self,
        previous: Option<TerminalSidebar>,
        terminal: &Tui,
    ) {
        let current =
            matches!(self.mode, Mode::Terminal { .. }).then(|| self.effective_terminal_sidebar());
        let (Some(previous), Some(current)) = (previous, current) else {
            return;
        };
        if previous == current {
            return;
        }
        let pane_id = match &self.mode {
            Mode::Terminal { pane_id } => *pane_id,
            _ => return,
        };
        if self.pending_terminal_entry.is_some() {
            self.resize_pending_terminal_entry(terminal);
        } else {
            self.resize_terminal_pane(pane_id, terminal);
        }
        self.force_terminal_redraw = true;
        self.needs_redraw = true;
    }

    pub fn current_selection(&self) -> Selection {
        self.workspace
            .get_selection(self.tree_selected, self.flat())
    }

    fn selected_worktree_indices(&self) -> Option<(usize, usize)> {
        match self.current_selection() {
            Selection::Worktree(pi, wi)
            | Selection::Session(pi, wi, _)
            | Selection::Pane(pi, wi, _, _) => Some((pi, wi)),
            _ => None,
        }
    }

    // ^ docs/ui-state-ownership.md "Selected row": a refresh keeps the cursor on its row by
    // identity, not by row number. Task sessions and worktrees come and go above it, and a
    // project above it collapses on a timer.
    fn cursor_identity(&mut self) -> Option<wsx_core::cache::CursorIdentity> {
        self.ensure_flat();
        wsx_core::cache::resolve_cursor_identity(&self.workspace, self.flat(), self.tree_selected)
    }

    /// After `rebuild_flat`: the row `cursor` named, else the same row number, clamped.
    fn restore_cursor(&mut self, cursor: Option<wsx_core::cache::CursorIdentity>) {
        if let Some(index) = cursor.and_then(|identity| {
            wsx_core::cache::find_cursor_index(&self.workspace, self.flat(), &identity)
        }) {
            self.tree_selected = index;
        }
        self.clamp_selected();
    }

    fn clamp_selected(&mut self) {
        let len = self.flat().len();
        if len == 0 {
            self.tree_selected = 0;
        } else {
            self.tree_selected = self.tree_selected.min(len - 1);
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn touch_project(&mut self, project_idx: usize) {
        let Some(path) = self
            .workspace
            .projects
            .get(project_idx)
            .map(|project| project.path.clone())
        else {
            return;
        };
        self.stale_collapsed_projects.remove(&path);
        let now_unix_ms = wsx_core::task::now_unix_ms();
        self.project_touched_unix_ms
            .insert(path.clone(), now_unix_ms);
        if let AutoCollapsePolicy::Adaptive { base_hours } = self.config.auto_collapse {
            self.adaptive_collapse
                .entry(path.clone())
                .and_modify(|state| {
                    state.observe(base_hours, now_unix_ms);
                })
                .or_insert_with(|| AdaptiveCollapseState::new(base_hours, now_unix_ms));
        }
        let change = self.cache_changes.projects.entry(path.clone()).or_default();
        change.observed_touch_unix_ms = now_unix_ms;
        change.touched_unix_ms = Some(now_unix_ms);
        change.stale = Some(false);
        change.adaptive = Some(self.adaptive_collapse.get(&path).copied().map(Into::into));
        self.mark_dirty();
    }

    fn set_worktree_expanded(&mut self, pi: usize, wi: usize, expanded: bool) {
        if let Some(worktree) = self.workspace.worktree_mut(pi, wi) {
            worktree.expanded = expanded;
            self.cache_changes
                .worktree_expanded
                .insert(worktree.path.clone(), expanded);
            self.mark_dirty();
        }
    }

    fn set_routines_expanded(&mut self, pi: usize, expanded: bool) {
        let project = &mut self.workspace.projects[pi];
        project.routines_expanded = expanded;
        self.cache_changes
            .projects
            .entry(project.path.clone())
            .or_default()
            .routines_expanded = Some(expanded);
        self.touch_project(pi);
    }

    fn manually_expand_project(&mut self, project_idx: usize) {
        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
            project.expanded = true;
            self.cache_changes
                .projects
                .entry(project.path.clone())
                .or_default()
                .expanded = Some(true);
            self.touch_project(project_idx);
        }
    }

    fn manually_collapse_project(&mut self, project_idx: usize) {
        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
            project.expanded = false;
            self.cache_changes
                .projects
                .entry(project.path.clone())
                .or_default()
                .expanded = Some(false);
            self.touch_project(project_idx);
        }
    }

    fn nav_up(&mut self) {
        if self.tree_selected > 0 {
            self.tree_selected -= 1;
            self.update_scroll();
        }
    }

    fn nav_down(&mut self) {
        let max = self.flat().len().saturating_sub(1);
        if self.tree_selected < max {
            self.tree_selected += 1;
            self.update_scroll();
        }
    }

    fn nav_left(&mut self) {
        let entry = self.flat().get(self.tree_selected).cloned();
        match entry {
            Some(FlatEntry::Project { idx }) => {
                if self.workspace.projects[idx].expanded {
                    self.manually_collapse_project(idx);
                    self.rebuild_flat();
                    self.clamp_selected();
                }
            }
            Some(FlatEntry::Worktree { project_idx: pi, worktree_idx: wi }) => {
                if self.workspace.projects[pi].worktrees[wi].expanded {
                    self.set_worktree_expanded(pi, wi, false);
                    self.touch_project(pi);
                    self.rebuild_flat();
                    self.clamp_selected();
                } else {
                    // Jump to parent project
                    if let Some(pos) = self.flat().iter().position(|e| matches!(e, FlatEntry::Project { idx } if *idx == pi)) {
                        self.tree_selected = pos;
                        self.update_scroll();
                    }
                }
            }
            Some(FlatEntry::Session { project_idx: pi, worktree_idx: wi, .. }) => {
                if let Some(pos) = self.flat().iter().position(|e| {
                    matches!(e, FlatEntry::Worktree { project_idx: p, worktree_idx: w } if *p == pi && *w == wi)
                }) {
                    self.tree_selected = pos;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::Pane {
                project_idx: pi,
                worktree_idx: wi,
                session_idx: si,
                ..
            }) => {
                if let Some(pos) = self.flat().iter().position(|entry| {
                    matches!(entry, FlatEntry::Session { project_idx, worktree_idx, session_idx } if *project_idx == pi && *worktree_idx == wi && *session_idx == si)
                }) {
                    self.tree_selected = pos;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::RoutinesHeader { project_idx: pi }) => {
                if self.workspace.projects[pi].routines_expanded {
                    self.set_routines_expanded(pi, false);
                    self.rebuild_flat();
                    self.clamp_selected();
                } else if let Some(pos) = self.flat().iter().position(|entry| matches!(entry, FlatEntry::Project { idx } if *idx == pi)) {
                    self.tree_selected = pos;
                }
            }
            Some(FlatEntry::Routine { project_idx: pi, .. }) => {
                if let Some(pos) = self.flat().iter().position(|entry| matches!(entry, FlatEntry::RoutinesHeader { project_idx } if *project_idx == pi)) {
                    self.tree_selected = pos;
                }
            }
            None => {}
        }
    }

    fn nav_right(&mut self) {
        let entry = self.flat().get(self.tree_selected).cloned();
        match entry {
            Some(FlatEntry::Project { idx: pi }) => {
                if !self.workspace.projects[pi].expanded {
                    self.manually_expand_project(pi);
                    self.mark_dirty();
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].worktrees.is_empty() {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::Worktree {
                project_idx: pi,
                worktree_idx: wi,
            }) => {
                if !self.workspace.projects[pi].worktrees[wi].expanded {
                    self.set_worktree_expanded(pi, wi, true);
                    self.touch_project(pi);
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].worktrees[wi]
                    .sessions
                    .is_empty()
                {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::RoutinesHeader { project_idx: pi }) => {
                if !self.workspace.projects[pi].routines_expanded {
                    self.set_routines_expanded(pi, true);
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].routines.is_empty() {
                    self.tree_selected += 1;
                }
            }
            _ => {}
        }
    }

    fn jump_project(&mut self, dir: isize) {
        let flat = self.flat();
        let current = self.tree_selected;
        let target = if dir > 0 {
            flat.iter()
                .enumerate()
                .find(|(i, e)| *i > current && matches!(e, FlatEntry::Project { .. }))
                .map(|(i, _)| i)
        } else {
            flat.iter()
                .enumerate()
                .rev()
                .find(|(i, e)| *i < current && matches!(e, FlatEntry::Project { .. }))
                .map(|(i, _)| i)
        };
        if let Some(pos) = target {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    fn update_scroll(&mut self) {
        // tree_visible_height is set each frame from actual terminal size; fall back to 20
        let visible = self.tree_visible_height.max(1);
        self.tree_scroll_manual = false;
        self.tree_scroll = crate::ui::workspace_tree::compute_scroll(
            self.tree_selected,
            visible,
            self.tree_scroll,
        );
        // ^ Captured terminal PUA glyphs can have widths that differ from ratatui's model.
        // Clear only when entering or leaving that content; ordinary previews diff cleanly.
        self.force_preview_redraw |= self.shows_preview()
            && (self.last_rendered_preview_was_session
                || matches!(self.current_selection(), Selection::Session(..)));
    }

    // ── Action dispatch ───────────────────────────────────────────────────────

    fn dispatch(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        self.ensure_flat();
        if matches!(
            action,
            Action::Quit | Action::HardQuit | Action::MouseClick { .. }
        ) {
            self.review = None;
        }
        if matches!(self.mode, Mode::Workspace) {
            if let Some(review) = &mut self.review {
                if !review.handle(&action) {
                    self.review = None;
                }
                self.needs_redraw = true;
                return Ok(());
            }
            if action == Action::InputTab && self.review_available {
                if let Selection::Worktree(pi, wi) = self.current_selection() {
                    let path = self.workspace.projects[pi].worktrees[wi].path.clone();
                    self.review = Some(crate::review::ReviewState::new(
                        path,
                        self.runtime_client.clone(),
                    ));
                    self.needs_redraw = true;
                }
                return Ok(());
            }
        }
        // Config mode handled first to avoid borrow conflicts
        if let Mode::Config { project_idx } = &self.mode {
            let pi = *project_idx;
            if matches!(action, Action::InputEscape | Action::Quit | Action::Help) {
                self.mode = Mode::Workspace;
            } else if action == Action::Edit {
                let Some(repo_path) = self
                    .workspace
                    .projects
                    .get(pi)
                    .map(|project| project.path.clone())
                else {
                    return Ok(());
                };
                let path =
                    match wsx_core::config::project::prepare_project_config_for_edit(&repo_path) {
                        Ok(path) => path,
                        Err(error) => {
                            self.set_error(format!("Could not prepare project config: {error}"));
                            return Ok(());
                        }
                    };
                if let Err(error) = edit_file(terminal, &path) {
                    self.set_error(format!("Could not edit project config: {error}"));
                    return Ok(());
                }
                let config = wsx_core::config::project::load_project_config(&repo_path);
                let notice = config.notice.clone();
                self.workspace.projects[pi].config = Some(config);
                if let Some(notice) = notice {
                    self.set_notice(NoticeLevel::Warning, notice);
                }
            }
            return Ok(());
        }

        if let Mode::Move { project_idx } = &self.mode {
            let pi = *project_idx;
            match action {
                Action::NavigateDown => self.move_project_down(pi),
                Action::NavigateUp => self.move_project_up(pi),
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.sync_config_project_order();
                    self.config.save()?;
                    self.mode = Mode::Workspace;
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::MoveWorktree {
            project_idx,
            worktree_idx,
        } = &self.mode
        {
            let (pi, wi) = (*project_idx, *worktree_idx);
            match action {
                Action::NavigateDown => self.move_worktree(pi, wi, 1),
                Action::NavigateUp => self.move_worktree(pi, wi, -1),
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.sync_config_worktree_order(pi);
                    self.config.save()?;
                    self.mode = Mode::Workspace;
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::GroupManager {
            selected,
            scroll,
            purpose,
        } = &self.mode
        {
            return self.dispatch_group_manager(*selected, *scroll, *purpose, action);
        }

        if matches!(self.mode, Mode::GlobalSettings { .. }) {
            return self.dispatch_global_settings(action, terminal);
        }
        if matches!(self.mode, Mode::IntegrationManager { .. }) {
            return self.dispatch_integration_manager(action);
        }
        if matches!(self.mode, Mode::RoutinePresetPicker { .. }) {
            return self.dispatch_routine_preset_picker(action);
        }
        if matches!(self.mode, Mode::RoutineEditor { .. }) {
            return self.dispatch_routine_editor(action);
        }
        if let Mode::Tasks { list } = &mut self.mode {
            match (action, list.detail.is_some()) {
                (Action::NavigateDown, false) => list.select(1),
                (Action::NavigateUp, false) => list.select(-1),
                (Action::Select, false) => list.open_detail(),
                (Action::NavigateDown, true) => list.scroll = list.scroll.saturating_add(1),
                (Action::NavigateUp, true) => list.scroll = list.scroll.saturating_sub(1),
                (Action::InputEscape | Action::Quit | Action::Select, true) => list.detail = None,
                (Action::InputEscape | Action::Quit | Action::Tasks, false) => {
                    self.mode = Mode::Workspace
                }
                _ => {}
            }
            return Ok(());
        }
        if matches!(self.mode, Mode::RoutineDetail { .. }) {
            match action {
                Action::InputEscape | Action::Quit | Action::Select => self.mode = Mode::Workspace,
                Action::NavigateDown => {
                    if let Mode::RoutineDetail { scroll, .. } = &mut self.mode {
                        *scroll = scroll.saturating_add(1);
                    }
                }
                Action::NavigateUp => {
                    if let Mode::RoutineDetail { scroll, .. } = &mut self.mode {
                        *scroll = scroll.saturating_sub(1);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::MoveSession {
            project_idx,
            worktree_idx,
            session_idx,
        } = &self.mode
        {
            let (pi, wi, si) = (*project_idx, *worktree_idx, *session_idx);
            match action {
                Action::NavigateDown => self.move_session(pi, wi, si, 1)?,
                Action::NavigateUp => self.move_session(pi, wi, si, -1)?,
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.mark_dirty();
                    self.write_cache_if_dirty();
                    self.mode = Mode::Workspace;
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::Terminal { pane_id } = self.mode {
            self.dispatch_terminal(action, pane_id, terminal)?;
            self.write_cache_if_dirty();
            return Ok(());
        }

        match &self.mode {
            Mode::Workspace => {
                self.dispatch_normal(action, terminal)?;
                self.cancel_pending_terminal_entry_if_stale();
            }
            Mode::Terminal { .. } => unreachable!(),
            Mode::Input { .. } => self.dispatch_input(action, terminal)?,
            Mode::Confirm { .. } => self.dispatch_confirm(action, terminal)?,
            Mode::Help => {
                if matches!(action, Action::InputEscape | Action::Quit | Action::Help) {
                    self.mode = Mode::Workspace;
                }
            }
            Mode::Search { .. } => self.dispatch_search(action, terminal)?,
            Mode::Config { .. }
            | Mode::GlobalSettings { .. }
            | Mode::IntegrationManager { .. }
            | Mode::Move { .. }
            | Mode::MoveWorktree { .. }
            | Mode::MoveSession { .. }
            | Mode::GroupManager { .. } => unreachable!(),
            Mode::RoutinePresetPicker { .. }
            | Mode::RoutineEditor { .. }
            | Mode::RoutineDetail { .. }
            | Mode::Tasks { .. } => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_terminal(
        &mut self,
        action: Action,
        pane_id: runtime::PaneId,
        terminal: &mut Tui,
    ) -> Result<()> {
        if self.pending_terminal_entry.is_some()
            && !matches!(
                &action,
                Action::InputEscape
                    | Action::Quit
                    | Action::Resize
                    | Action::ToggleTerminalSidebar
                    | Action::NextIdle
                    | Action::PrevIdle
                    | Action::NextActive
                    | Action::PrevActive
                    | Action::NextAttention
                    | Action::PrevAttention
                    | Action::NextSession
                    | Action::PrevSession
                    | Action::GroupNext
                    | Action::GroupPrev
            )
        {
            return Ok(());
        }
        match action {
            Action::InputEscape => self.leave_terminal_mode(pane_id),
            Action::Quit => self.should_quit = true,
            Action::Resize => {
                if self.pending_terminal_entry.is_some() {
                    self.resize_pending_terminal_entry(terminal);
                } else {
                    self.resize_terminal_pane(pane_id, terminal);
                }
                self.force_terminal_redraw = true;
                self.needs_redraw = true;
            }
            Action::NextIdle => self.action_switch_idle(1, terminal)?,
            Action::PrevIdle => self.action_switch_idle(-1, terminal)?,
            Action::NextActive => self.action_switch_active(1, terminal)?,
            Action::PrevActive => self.action_switch_active(-1, terminal)?,
            Action::NextAttention => self.action_switch_attention(1, terminal)?,
            Action::PrevAttention => self.action_switch_attention(-1, terminal)?,
            Action::NextSession => self.action_switch_context_session(1, terminal)?,
            Action::PrevSession => self.action_switch_context_session(-1, terminal)?,
            Action::GroupNext => self.action_switch_group(1, terminal)?,
            Action::GroupPrev => self.action_switch_group(-1, terminal)?,
            Action::TerminalKey(key) => self.send_terminal_keys([key]),
            Action::TerminalKeys(keys) => self.send_terminal_keys(keys),
            Action::TerminalPaste(text) => {
                self.send_terminal_stream(runtime::TerminalClientMessage::Paste(text))
            }
            Action::TerminalPrefixedPaste(prefix, text) => {
                self.send_terminal_keys([prefix]);
                self.send_terminal_stream(runtime::TerminalClientMessage::Paste(text));
            }
            Action::TerminalMouse(mouse) => {
                if self.handle_terminal_group_header_scroll(mouse) {
                    return Ok(());
                }
                if self.is_workspace_click(mouse) {
                    let compact_tree =
                        self.effective_terminal_sidebar() == TerminalSidebar::Compact;
                    self.leave_terminal_mode(pane_id);
                    self.handle_mouse_click(mouse.column, mouse.row, terminal, compact_tree)?;
                    self.needs_redraw = true;
                } else {
                    self.send_terminal_mouse(mouse);
                }
            }
            Action::TerminalPrefixedMouse(prefix, mouse) => {
                if self.handle_terminal_group_header_scroll(mouse) {
                    return Ok(());
                }
                if self.is_workspace_click(mouse) {
                    let compact_tree =
                        self.effective_terminal_sidebar() == TerminalSidebar::Compact;
                    self.leave_terminal_mode(pane_id);
                    self.handle_mouse_click(mouse.column, mouse.row, terminal, compact_tree)?;
                    self.needs_redraw = true;
                } else {
                    self.send_terminal_keys([prefix]);
                    self.send_terminal_mouse(mouse);
                }
            }
            Action::ToggleTerminalSidebar if !self.is_mobile => {
                self.terminal_sidebar_override = Some(match self.terminal_sidebar_baseline() {
                    TerminalSidebar::Compact => TerminalSidebar::Expanded,
                    TerminalSidebar::Expanded => TerminalSidebar::Compact,
                });
                if self.pending_terminal_entry.is_some() {
                    self.resize_pending_terminal_entry(terminal);
                } else {
                    self.resize_terminal_pane(pane_id, terminal);
                }
                self.force_terminal_redraw = true;
                self.needs_redraw = true;
            }
            Action::ToggleTerminalSidebar => {}
            _ => {}
        }
        Ok(())
    }

    fn send_terminal_keys(&mut self, keys: impl IntoIterator<Item = crossterm::event::KeyEvent>) {
        for key in keys {
            if let Some(key) = runtime_key_event(key) {
                self.send_terminal_stream(runtime::TerminalClientMessage::Key(key));
            }
        }
    }

    fn send_terminal_stream(&mut self, message: runtime::TerminalClientMessage) {
        let Some(active) = self.terminal_stream.as_ref() else {
            // Input typed while the terminal reattaches is dropped, as during entry.
            if self.pending_terminal_resume.is_none() {
                self.set_error("Terminal stream is unavailable");
            }
            return;
        };
        match active.stream.try_send(message) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.set_error("Terminal input queue is full; input was not sent")
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.clear_active_terminal_selection();
                self.mode = Mode::Workspace;
                self.terminal_stream = None;
                self.pending_terminal_entry = None;
                self.set_error("Terminal stream disconnected");
            }
        }
    }

    /// Queues one-shot bytes for the terminal input worker. The worker runs the
    /// lease round trips off the UI thread, in queue order, so input to one pane
    /// keeps its order. A failure comes back as `BgOutcome::TerminalInputFailed`.
    fn send_terminal_bytes_once(&self, pane_id: runtime::PaneId, bytes: Vec<u8>) -> Result<()> {
        self.terminal_input_tx
            .send((pane_id, bytes))
            .map_err(|_| anyhow::anyhow!("terminal input worker stopped"))
    }

    fn is_workspace_click(&self, mouse: crossterm::event::MouseEvent) -> bool {
        matches!(
            mouse.kind,
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
        ) && (self
            .tree_area
            .contains(Position::new(mouse.column, mouse.row))
            || self
                .group_header_area
                .contains(Position::new(mouse.column, mouse.row)))
    }

    fn handle_terminal_group_header_scroll(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        if !self
            .group_header_area
            .contains(Position::new(mouse.column, mouse.row))
        {
            return false;
        }
        let delta = match mouse.kind {
            crossterm::event::MouseEventKind::ScrollUp
            | crossterm::event::MouseEventKind::ScrollLeft => -1,
            crossterm::event::MouseEventKind::ScrollDown
            | crossterm::event::MouseEventKind::ScrollRight => 1,
            _ => return false,
        };
        self.scroll_group_header(delta);
        true
    }

    fn send_terminal_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let Some(mouse) = runtime_mouse_event(mouse, self.terminal_area) else {
            return;
        };
        self.send_terminal_stream(runtime::TerminalClientMessage::Mouse(mouse));
    }

    fn dispatch_normal(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        match action {
            Action::HardQuit => self.action_hard_quit(),
            Action::NavigateUp => self.nav_up(),
            Action::NavigateDown => self.nav_down(),
            Action::NavigateLeft => self.nav_left(),
            Action::NavigateRight => self.nav_right(),
            Action::Select => self.action_select(terminal)?,
            Action::LiteralEscape => {
                let key = self.terminal_escape_chord.literal_key_event();
                self.action_select(terminal)?;
                if matches!(self.mode, Mode::Terminal { .. }) {
                    self.send_terminal_keys([key]);
                }
            }
            Action::AddProject => self.action_add_project()?,
            Action::AddWorktree => self.action_add_worktree()?,
            Action::AddSession => self.action_add_session()?,
            Action::SplitPaneVertical => self.action_split_pane(runtime::SplitAxis::Vertical)?,
            Action::SplitPaneHorizontal => {
                self.action_split_pane(runtime::SplitAxis::Horizontal)?
            }
            Action::AddRoutine => self.action_add_routine()?,
            Action::Delete => self.action_delete()?,
            Action::Edit => self.action_edit()?,
            Action::EditGlobalConfig => self.action_edit_global_config(terminal),
            Action::SetAlias => self.action_set_alias()?,
            Action::Refresh => self.refresh_all()?,
            Action::Resize => {
                if self.pending_terminal_entry.is_some() {
                    self.resize_pending_terminal_entry(terminal);
                }
                self.force_terminal_redraw = true;
                self.needs_redraw = true;
            }
            Action::Help => {
                self.mode = Mode::Help;
            }
            Action::NextAttention => self.action_next_attention(1),
            Action::PrevAttention => self.action_next_attention(-1),
            Action::DismissAttention => self.action_dismiss_attention(),
            Action::NextActive => self.action_move_active(1),
            Action::PrevActive => self.action_move_active(-1),
            Action::NextIdle => self.action_move_idle(1),
            Action::PrevIdle => self.action_move_idle(-1),
            Action::SendCtrlC => self.action_send_ctrl_c()?,
            Action::AssignGroup => self.action_assign_group(),
            Action::EnterMove => self.action_enter_move(),
            Action::JumpProjectDown => self.jump_project(1),
            Action::JumpProjectUp => self.jump_project(-1),
            Action::SearchStart => {
                self.mode = Mode::Search {
                    query: String::new(),
                    match_idx: 0,
                };
                // ^ An index left from an earlier search holds that search's counts.
                self.search_index = None;
                self.ensure_search_index();
            }
            Action::GroupNext => self.action_group_next(),
            Action::GroupPrev => self.action_group_prev(),
            Action::GroupManager => self.action_group_manager(),
            Action::Tasks => self.action_tasks(),
            Action::MouseClick { col, row } => {
                self.handle_mouse_click(col, row, terminal, false)?
            }
            Action::MouseScroll { col, row, delta } => {
                self.handle_workspace_mouse_scroll(col, row, delta)
            }
            _ => {}
        }
        // ^ Expansion and other explicit local intent must be durable before the
        // next blocking read because an SSH disconnect can terminate this TUI.
        self.write_cache_if_dirty();
        Ok(())
    }

    fn handle_mouse_click(
        &mut self,
        col: u16,
        row: u16,
        terminal: &mut Tui,
        compact_tree: bool,
    ) -> Result<()> {
        let pos = Position { x: col, y: row };
        if matches!(self.mode, Mode::Workspace) && self.group_header_area.contains(pos) {
            let strip = crate::ui::workspace_nav::fit_group_strip(
                &self.config.ordered_group_keys(),
                &self.active_group,
                usize::from(self.group_header_area.width),
                self.group_header_scroll,
            );
            match strip.target_at(usize::from(col - self.group_header_area.x)) {
                Some(crate::ui::workspace_nav::GroupStripTarget::Group(key)) => {
                    self.toggle_active_group(key)
                }
                Some(crate::ui::workspace_nav::GroupStripTarget::ScrollLeft) => {
                    self.scroll_group_header(-1)
                }
                Some(crate::ui::workspace_nav::GroupStripTarget::ScrollRight) => {
                    self.scroll_group_header(1)
                }
                None => {}
            }
            return Ok(());
        }
        if self.tree_area.contains(pos) {
            let layout = if compact_tree {
                crate::ui::workspace_nav::SidebarLayout::compact_rail(self.tree_area)
            } else {
                crate::ui::workspace_nav::SidebarLayout::bordered(self.tree_area)
            };
            if let Some(flat_idx) = layout.item_at(pos, self.tree_scroll, self.flat().len()) {
                if flat_idx == self.tree_selected {
                    self.action_select(terminal)?;
                } else {
                    self.tree_selected = flat_idx;
                    self.update_scroll();
                }
            }
        } else if self.preview_area.contains(pos)
            && matches!(
                self.current_selection(),
                Selection::Session(..) | Selection::Pane(..)
            )
        {
            self.action_select(terminal)?;
        }
        Ok(())
    }

    fn handle_workspace_mouse_scroll(&mut self, col: u16, row: u16, delta: i8) {
        if !matches!(self.mode, Mode::Workspace) {
            return;
        }
        let position = Position::new(col, row);
        if self.group_header_area.contains(position) {
            self.scroll_group_header(delta);
            return;
        }
        if !self.tree_area.contains(position) {
            return;
        }
        let previous_selection = self.tree_selected;
        let (scroll, selected) = crate::ui::workspace_tree::scroll_viewport(
            self.tree_scroll,
            self.tree_selected,
            self.tree_visible_height.max(1),
            self.flat().len(),
            isize::from(delta) * WORKSPACE_SCROLL_LINES,
        );
        self.tree_scroll = scroll;
        self.tree_selected = selected;
        self.tree_scroll_manual = true;
        if self.tree_selected != previous_selection {
            self.force_preview_redraw |= self.shows_preview()
                && (self.last_rendered_preview_was_session
                    || matches!(self.current_selection(), Selection::Session(..)));
        }
        self.needs_redraw = true;
    }

    fn scroll_group_header(&mut self, delta: i8) {
        let last = self.config.ordered_group_keys().len().saturating_sub(1);
        self.group_header_scroll = if delta < 0 {
            self.group_header_scroll.saturating_sub(1)
        } else {
            self.group_header_scroll.saturating_add(1).min(last)
        };
        self.needs_redraw = true;
    }

    fn dispatch_input(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        if let Mode::Input { state, .. } = &mut self.mode {
            if dispatch_text_edit(state, &action) {
                return Ok(());
            }
        }
        match action {
            Action::InputEscape | Action::Quit => {
                let group_selection = match &self.mode {
                    Mode::Input {
                        context: InputContext::AddGroup,
                        ..
                    } => Some(0),
                    Mode::Input {
                        context: InputContext::RenameGroup { group_idx },
                        ..
                    } => Some(group_idx + 1),
                    _ => None,
                };
                self.mode =
                    group_selection.map_or(Mode::Workspace, |selected| Mode::GroupManager {
                        selected,
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    });
            }
            Action::Select => {
                self.confirm_input(terminal)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch_confirm(&mut self, action: Action, _terminal: &mut Tui) -> Result<()> {
        match action {
            Action::ConfirmYes | Action::Select => self.confirm_action()?,
            Action::NextAttention | Action::InputEscape | Action::Quit => {
                let (group_selection, dismissed_integrations) = match &self.mode {
                    Mode::Confirm {
                        pending: PendingAction::DeleteGroup { group_idx },
                        ..
                    } => (Some(group_idx + 1), None),
                    Mode::Confirm {
                        pending:
                            PendingAction::InstallIntegrations {
                                targets,
                                remember_dismissal: true,
                            },
                        ..
                    } => (None, Some(targets.clone())),
                    _ => (None, None),
                };
                self.mode =
                    group_selection.map_or(Mode::Workspace, |selected| Mode::GroupManager {
                        selected,
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    });
                if let Some(targets) = dismissed_integrations {
                    for target in targets {
                        self.dismissed_integration_prompts.insert(target);
                        self.cache_changes
                            .dismissed_integration_prompts
                            .insert(target, true);
                    }
                    self.mark_dirty();
                    self.write_cache_if_dirty();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch_search(&mut self, action: Action, _terminal: &mut Tui) -> Result<()> {
        match action {
            Action::InputEscape | Action::Quit => {
                self.mode = Mode::Workspace;
            }
            Action::InputChar(c) => {
                if let Mode::Search {
                    ref mut query,
                    ref mut match_idx,
                } = self.mode
                {
                    query.push(c);
                    *match_idx = 0;
                }
                self.search_apply();
            }
            Action::InputBackspace => {
                if let Mode::Search {
                    ref mut query,
                    ref mut match_idx,
                } = self.mode
                {
                    query.pop();
                    *match_idx = 0;
                }
                self.search_apply();
            }
            Action::Select => self.search_advance(),
            _ => {}
        }
        Ok(())
    }

    fn dispatch_routine_preset_picker(&mut self, action: Action) -> Result<()> {
        let Mode::RoutinePresetPicker {
            project_idx,
            selected,
        } = &mut self.mode
        else {
            return Ok(());
        };
        match action {
            Action::InputEscape | Action::Quit => self.mode = Mode::Workspace,
            Action::NavigateDown | Action::InputChar('j') => {
                *selected = (*selected + 1) % RoutinePreset::ALL.len();
            }
            Action::NavigateUp | Action::InputChar('k') => {
                *selected = (*selected + RoutinePreset::ALL.len() - 1) % RoutinePreset::ALL.len();
            }
            Action::Select => {
                let project_idx = *project_idx;
                let form = RoutinePreset::ALL[*selected].form();
                self.mode = Mode::RoutineEditor {
                    project_idx,
                    original_name: None,
                    can_rename: true,
                    form,
                };
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch_routine_editor(&mut self, action: Action) -> Result<()> {
        if let Mode::RoutineEditor {
            form, can_rename, ..
        } = &mut self.mode
        {
            let mut edit = RoutineTextEdit {
                form,
                can_rename: *can_rename,
            };
            if dispatch_text_edit(&mut edit, &action) {
                return Ok(());
            }
        }
        match action {
            Action::InputEscape | Action::Quit => self.mode = Mode::Workspace,
            Action::Select => self.save_routine_form()?,
            _ => {}
        }
        Ok(())
    }

    fn save_routine_form(&mut self) -> Result<()> {
        let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
        let Mode::RoutineEditor {
            project_idx,
            original_name,
            can_rename,
            mut form,
        } = mode
        else {
            return Ok(());
        };
        if !can_rename {
            if let Some(old_name) = &original_name {
                form.name.clone_from(old_name);
            }
        }
        let routine = match form.routine() {
            Ok(routine) => routine,
            Err(error) => {
                self.mode = Mode::RoutineEditor {
                    project_idx,
                    original_name,
                    can_rename,
                    form,
                };
                self.set_status(error);
                return Ok(());
            }
        };
        let Some(project) = self.workspace.projects.get(project_idx) else {
            self.set_status("Project is no longer available");
            return Ok(());
        };
        let project_path = project.path.clone();
        let revision = project.routine_revision;
        let action = if let Some(old_name) = original_name.clone() {
            asched_core::routine::ipc::Action::Edit {
                revision,
                old_name,
                routine: routine.clone(),
            }
        } else {
            asched_core::routine::ipc::Action::Add {
                revision,
                routine: routine.clone(),
            }
        };
        let saved_name = routine.name.clone();
        self.spawn_routine_request(
            project_path,
            action,
            RoutineResultKind::Save {
                original_name,
                can_rename,
                form,
                saved_name: saved_name.clone(),
            },
        );
        self.set_status(format!("Saving routine '{saved_name}'…"));
        Ok(())
    }

    fn select_routine(&mut self, project_idx: usize, name: Option<&str>) {
        let position = self.flat().iter().position(|entry| match entry {
            FlatEntry::Routine {
                project_idx: pi,
                routine_idx,
            } if *pi == project_idx => name.is_none_or(|name| {
                self.workspace.projects[*pi].routines[*routine_idx]
                    .routine
                    .name
                    == name
            }),
            FlatEntry::RoutinesHeader { project_idx: pi } if *pi == project_idx => name.is_none(),
            _ => false,
        });
        if let Some(position) = position {
            self.tree_selected = position;
            self.update_scroll();
        }
    }

    fn search_matches(&self, query: &str) -> Vec<usize> {
        match &self.search_index {
            Some(index) => index.matches(query),
            // ^ Outside Search mode nothing keeps an index; build a throwaway one.
            None => SearchIndex::new(&self.workspace, &self.cached_flat).matches(query),
        }
    }

    pub(crate) fn search_counts(&self) -> Option<&[usize]> {
        matches!(self.mode, Mode::Search { .. })
            .then(|| {
                self.search_index
                    .as_ref()
                    .map(|index| index.counts.as_slice())
            })
            .flatten()
    }

    pub(crate) fn search_total(&self) -> usize {
        self.search_index.as_ref().map_or(0, |index| index.total)
    }

    fn search_apply(&mut self) {
        let query = match &self.mode {
            Mode::Search { query, .. } => query.clone(),
            _ => return,
        };
        self.ensure_search_index();
        if let Some(index) = self.search_index.as_mut() {
            index.update(&query);
        }
        let matches = self.search_matches(&query);
        if matches.is_empty() {
            return;
        }
        self.tree_selected = matches[0];
        self.update_scroll();
    }

    /// Enter: finish a sole match or cycle to the next of multiple matches.
    fn search_advance(&mut self) {
        let (query, match_idx) = match &self.mode {
            Mode::Search { query, match_idx } => (query.clone(), *match_idx),
            _ => return,
        };
        let matches = self.search_matches(&query);
        if matches.len() <= 1 {
            self.mode = Mode::Workspace;
            return;
        }
        let next = (match_idx + 1) % matches.len();
        if let Mode::Search {
            ref mut match_idx, ..
        } = self.mode
        {
            *match_idx = next;
        }
        self.tree_selected = matches[next];
        self.update_scroll();
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    fn action_select(&mut self, terminal: &mut Tui) -> Result<()> {
        match self.current_selection() {
            Selection::Session(pi, wi, si) => {
                self.attach_session(pi, wi, si, terminal)?;
            }
            Selection::Pane(pi, wi, si, pane_idx) => {
                self.attach_pane(pi, wi, si, pane_idx, terminal)?;
            }
            Selection::Project(pi) => {
                if self.workspace.projects[pi].expanded {
                    self.manually_collapse_project(pi);
                } else {
                    self.manually_expand_project(pi);
                }
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::Worktree(pi, wi) => {
                let expanded = !self.workspace.projects[pi].worktrees[wi].expanded;
                self.set_worktree_expanded(pi, wi, expanded);
                self.touch_project(pi);
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::RoutinesHeader(pi) => {
                let expanded = !self.workspace.projects[pi].routines_expanded;
                self.set_routines_expanded(pi, expanded);
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::Routine(pi, ri) => {
                self.mode = Mode::RoutineDetail {
                    project_path: self.workspace.projects[pi].path.clone(),
                    routine_name: self.workspace.projects[pi].routines[ri]
                        .routine
                        .name
                        .clone(),
                    scroll: 0,
                };
            }
            Selection::None => {}
        }
        Ok(())
    }

    /// Explicit session interaction clears local mute and acknowledges the exact
    /// provider-completion revision. Cursor navigation and pane output do neither.
    /// Entering Terminal mode and leaving it both count: a session that finishes while
    /// the user is inside it was seen when the user leaves.
    fn unmute_on_interaction(&mut self, pane_id: runtime::PaneId) {
        let result = self
            .workspace
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
            .flat_map(|worktree| &mut worktree.sessions)
            .find(|session| {
                session.pane_id == pane_id
                    || session.panes.iter().any(|pane| pane.pane_id == pane_id)
            })
            .map(|session| {
                let was_muted = session.muted;
                session.muted = false;
                let muted_ids = session
                    .panes
                    .iter()
                    .map(|pane| pane.terminal_id.to_string())
                    .chain(std::iter::once(session.terminal_id.to_string()))
                    .collect::<Vec<_>>();
                let acknowledged = session
                    .panes
                    .iter_mut()
                    .find(|pane| pane.pane_id == pane_id)
                    .and_then(|pane| {
                        (pane.agent_status == runtime::AgentState::Done).then(|| {
                            pane.outcome_acknowledged = true;
                            (pane.terminal_id.to_string(), pane.revision)
                        })
                    });
                if session.pane_id == pane_id && acknowledged.is_some() {
                    session.outcome_acknowledged = true;
                }
                (was_muted, muted_ids, acknowledged)
            });
        let Some((was_muted, muted_ids, acknowledged)) = result else {
            return;
        };
        if was_muted {
            for terminal_id in muted_ids {
                self.muted_terminal_ids.remove(&terminal_id);
                self.cache_changes
                    .muted_terminals
                    .insert(terminal_id, false);
            }
        }
        if let Some((terminal_id, revision)) = acknowledged.as_ref() {
            self.acknowledged_outcomes
                .insert(terminal_id.clone(), *revision);
            self.cache_changes
                .acknowledged_outcomes
                .insert(terminal_id.clone(), *revision);
        }
        if was_muted || acknowledged.is_some() {
            self.mark_dirty();
        }
    }

    fn attach_session(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        terminal: &mut Tui,
    ) -> Result<()> {
        let Some((pane_id, terminal_id)) = self
            .workspace
            .session(pi, wi, si)
            .map(|session| (session.pane_id, session.terminal_id))
        else {
            self.set_status("Session not found");
            return Ok(());
        };
        let target = self
            .terminal_target_label(pi, wi, si, pane_id)
            .unwrap_or_else(|| format!("pane {}", pane_id.0));
        self.touch_project(pi);
        self.enter_terminal(pane_id, terminal_id, target, terminal)
    }

    fn attach_pane(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        pane_idx: usize,
        terminal: &mut Tui,
    ) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session not found");
            return Ok(());
        };
        let session_id = session.session_id;
        let Some((pane_id, terminal_id)) = session
            .panes
            .get(pane_idx)
            .map(|pane| (pane.pane_id, pane.terminal_id))
        else {
            self.set_status("Pane not found");
            return Ok(());
        };
        let focus_revision = match self.runtime_client.call(&runtime::Request::PaneFocus {
            session_id,
            pane_id,
        })? {
            runtime::Response::Ack { revision } => revision,
            runtime::Response::Error(error) => {
                self.set_error(api_error_text(&error));
                return Ok(());
            }
            _ => {
                self.set_error("Unexpected pane focus response");
                return Ok(());
            }
        };
        if let Some(session) = self.workspace.session_mut(pi, wi, si) {
            if let Some(pane) = session.panes.get(pane_idx) {
                session.pane_id = pane.pane_id;
                session.terminal_id = pane.terminal_id;
                session.agent = pane.agent.clone();
                session.agent_status = pane.agent_status;
                session.revision = focus_revision;
            }
        }
        let target = self
            .terminal_target_label(pi, wi, si, pane_id)
            .unwrap_or_else(|| format!("pane {}", pane_id.0));
        self.touch_project(pi);
        self.enter_terminal(pane_id, terminal_id, target, terminal)
    }

    fn terminal_target_label(
        &self,
        pi: usize,
        wi: usize,
        si: usize,
        pane_id: runtime::PaneId,
    ) -> Option<String> {
        let project = self.workspace.projects.get(pi)?;
        let worktree = project.worktrees.get(wi)?;
        let session = worktree.sessions.get(si)?;
        let pane = session.panes.iter().find(|pane| pane.pane_id == pane_id)?;
        Some(format!(
            "{} › {} › {} › {}",
            project.name,
            worktree.display_name(),
            session.display_name,
            pane.label
        ))
    }

    fn enter_terminal(
        &mut self,
        pane_id: runtime::PaneId,
        terminal_id: runtime::TerminalId,
        target: String,
        terminal: &Tui,
    ) -> Result<()> {
        self.pending_terminal_entry = None;
        self.pending_terminal_resume = None;
        self.clear_active_terminal_selection();
        if self.terminal_surfaces.clear_selection(pane_id, terminal_id) {
            self.needs_redraw = true;
        }
        // A switch from another session leaves that session.
        if let Some(left) = self.terminal_stream.take().map(|active| active.pane_id) {
            self.unmute_on_interaction(left);
        }
        self.unmute_on_interaction(pane_id);
        // ^ docs/terminal-exit-ordering.md: an exited pane keeps its last screen, often empty
        // after an agent quits. Opening it starts its saved command again (an agent resumes
        // its conversation), so the session is usable at once.
        if self.pane_mut(pane_id).is_some_and(|pane| pane.exited) {
            if let Err(error) = self.restart_exited_pane(pane_id) {
                self.set_error(format!("Cannot start {target} again: {error:#}"));
                return Ok(());
            }
        }
        let (rows, cols) = self.terminal_pane_size(terminal);
        match runtime::TerminalStream::connect(
            &self.runtime_client,
            pane_id,
            self.terminal_controller_id,
            TerminalControlIntent::Explicit.takeover(),
            rows,
            cols,
        ) {
            Ok(stream) => {
                self.terminal_stream_generation = self.terminal_stream_generation.wrapping_add(1);
                let generation = self.terminal_stream_generation;
                self.terminal_stream = Some(ActiveTerminalStream {
                    epoch: stream.epoch(),
                    pane_id,
                    terminal_id,
                    generation,
                    stream,
                });
                self.pending_terminal_entry = Some(PendingTerminalEntry {
                    pane_id,
                    terminal_id,
                    generation,
                    rows,
                    cols,
                });
                self.terminal_escape_chord.reset();
            }
            Err(error) => self.set_error(terminal_stream_error_notice(&error, &target)),
        }
        Ok(())
    }

    /// `SessionRestart` with the pane's revision as wsxd has it now: the exit itself moved it.
    fn restart_exited_pane(&mut self, pane_id: runtime::PaneId) -> Result<()> {
        let revision = ops::restart_session(pane_id)?;
        // ^ A refresh read before the restart still says exited and would end the new stream.
        if let Some((_, applied)) = self.applied_revision.as_mut() {
            *applied = (*applied).max(revision);
        }
        Ok(())
    }

    fn pane_mut(
        &mut self,
        pane_id: runtime::PaneId,
    ) -> Option<&mut wsx_core::model::workspace::PaneInfo> {
        self.workspace
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
            .flat_map(|worktree| &mut worktree.sessions)
            .flat_map(|session| &mut session.panes)
            .find(|pane| pane.pane_id == pane_id)
    }

    fn leave_terminal_mode(&mut self, pane_id: runtime::PaneId) {
        self.unmute_on_interaction(pane_id);
        self.clear_active_terminal_selection();
        self.terminal_stream = None;
        self.pending_terminal_entry = None;
        self.pending_terminal_resume = None;
        self.terminal_escape_chord.reset();
        self.mode = Mode::Workspace;
    }

    fn clear_active_terminal_selection(&mut self) {
        let Some(active) = self.terminal_stream.as_ref() else {
            return;
        };
        if self
            .terminal_surfaces
            .clear_selection(active.pane_id, active.terminal_id)
        {
            self.needs_redraw = true;
        }
    }

    fn terminal_cursor(&self) -> Option<runtime::Cursor> {
        if !matches!(self.mode, Mode::Terminal { .. }) || self.pending_terminal_entry.is_some() {
            return None;
        }
        let active = self.terminal_stream.as_ref()?;
        self.terminal_surfaces
            .frame(active.pane_id, active.terminal_id)
            .map(|frame| frame.cursor)
    }

    pub(crate) fn terminal_surface(
        &self,
        pane_id: runtime::PaneId,
        terminal_id: runtime::TerminalId,
    ) -> Option<&runtime::TerminalFrame> {
        self.terminal_surfaces.frame(pane_id, terminal_id)
    }

    fn terminal_pane_size(&self, terminal: &Tui) -> (u16, u16) {
        let size = terminal
            .size()
            .unwrap_or(ratatui::layout::Size::new(80, 24));
        let area = Rect::new(0, 0, size.width, size.height);
        let mobile = self.force_mobile || size.width < 60;
        let sidebar = self.effective_terminal_sidebar();
        let preview_width = if mobile {
            size.width
        } else {
            size.width
                .saturating_sub(crate::ui::layout::terminal_sidebar_width(sidebar))
        };
        let sidecar_width = self
            .terminal_sidecar_descriptor(size.width, preview_width)
            .map(|descriptor| {
                descriptor
                    .spec
                    .preferred_width
                    .min(preview_width.saturating_sub(60))
            });
        let viewport = crate::ui::layout::terminal_viewport(
            area,
            mobile,
            sidebar,
            self.config.terminal_title_position,
            sidecar_width,
        );
        (viewport.height.max(1), viewport.width.max(1))
    }

    fn resize_terminal_pane(&mut self, _pane_id: runtime::PaneId, terminal: &Tui) {
        let (rows, cols) = self.terminal_pane_size(terminal);
        self.send_terminal_stream(runtime::TerminalClientMessage::Resize { rows, cols });
    }

    fn resize_pending_terminal_entry(&mut self, terminal: &Tui) {
        let (rows, cols) = self.terminal_pane_size(terminal);
        let Some(pending) = self.pending_terminal_entry.as_mut() else {
            return;
        };
        pending.rows = rows;
        pending.cols = cols;
        self.send_terminal_stream(runtime::TerminalClientMessage::Resize { rows, cols });
    }

    fn action_hard_quit(&mut self) {
        self.mode = Mode::Confirm {
            message: "Hard quit? Stops wsxd and all live processes. Saved sessions restart on next launch.".into(),
            pending: PendingAction::ShutdownDaemon,
        };
    }

    fn action_add_project(&mut self) -> Result<()> {
        // Snapshot the cached repos for the modal. Trigger a refresh in the
        // background so the next open picks up newly created repos.
        let cached = self.scanned_repos.clone();
        self.spawn_repo_scan();
        self.mode = Mode::Input {
            context: InputContext::AddProject,
            state: InputState::new_project_search("project: ", cached),
        };
        Ok(())
    }

    fn action_add_worktree(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi)
            | Selection::Worktree(pi, _)
            | Selection::Session(pi, _, _)
            | Selection::Pane(pi, _, _, _)
            | Selection::RoutinesHeader(pi)
            | Selection::Routine(pi, _) => pi,
            Selection::None => {
                self.set_status("Select a project first (press p to add one)");
                return Ok(());
            }
        };
        let prefix = self.workspace.projects[pi]
            .config
            .as_ref()
            .map(|config| config.branch_prefix.clone())
            .unwrap_or_default();
        self.mode = Mode::Input {
            context: InputContext::AddWorktree { project_idx: pi },
            state: InputState::with_value("branch: ", prefix),
        };
        Ok(())
    }

    fn action_add_session(&mut self) -> Result<()> {
        let (pi, wi) = match self.current_selection() {
            Selection::Worktree(pi, wi)
            | Selection::Session(pi, wi, _)
            | Selection::Pane(pi, wi, _, _) => (pi, wi),
            _ => {
                self.set_status("Select a worktree first");
                return Ok(());
            }
        };
        self.mode = Mode::Input {
            context: InputContext::AddSession {
                project_idx: pi,
                worktree_idx: wi,
            },
            state: InputState::new("name (optional): "),
        };
        Ok(())
    }

    fn action_split_pane(&mut self, axis: runtime::SplitAxis) -> Result<()> {
        let (pi, wi, si, target) = match self.current_selection() {
            Selection::Session(pi, wi, si) => {
                let session = &self.workspace.projects[pi].worktrees[wi].sessions[si];
                (pi, wi, si, session.pane_id)
            }
            Selection::Pane(pi, wi, si, pane_idx) => {
                let pane_id =
                    self.workspace.projects[pi].worktrees[wi].sessions[si].panes[pane_idx].pane_id;
                (pi, wi, si, pane_id)
            }
            _ => {
                self.set_status("Select a session or pane first");
                return Ok(());
            }
        };
        let session = &self.workspace.projects[pi].worktrees[wi].sessions[si];
        let rows = self.terminal_area.height.max(1);
        let cols = self.terminal_area.width.max(1);
        match self.runtime_client.call(&runtime::Request::PaneSplit {
            session_id: session.session_id,
            target,
            axis,
            label: format!("terminal-{}", session.panes.len() + 1),
            command: Vec::new(),
            initial_input: None,
            rows,
            cols,
            expected_revision: session.revision,
        })? {
            runtime::Response::Created { .. } => {
                self.spawn_runtime_session_refresh();
                self.set_status("Pane created");
            }
            runtime::Response::Error(error) => {
                self.set_error(api_error_text(&error));
            }
            _ => self.set_error("Unexpected pane split response"),
        }
        Ok(())
    }

    fn action_tasks(&mut self) {
        let project = match self.current_selection() {
            Selection::Project(pi)
            | Selection::Worktree(pi, _)
            | Selection::Session(pi, _, _)
            | Selection::Pane(pi, _, _, _)
            | Selection::RoutinesHeader(pi)
            | Selection::Routine(pi, _) => &self.workspace.projects[pi],
            Selection::None => {
                self.set_status("Select a project first");
                return;
            }
        };
        self.mode = Mode::Tasks {
            list: Box::new(crate::task_list::TaskList::new(
                project.name.clone(),
                project.path.clone(),
            )),
        };
    }

    fn action_add_routine(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi)
            | Selection::Worktree(pi, _)
            | Selection::Session(pi, _, _)
            | Selection::Pane(pi, _, _, _)
            | Selection::RoutinesHeader(pi)
            | Selection::Routine(pi, _) => pi,
            Selection::None => {
                self.set_status("Select a project first");
                return Ok(());
            }
        };
        self.mode = Mode::RoutinePresetPicker {
            project_idx: pi,
            selected: 0,
        };
        Ok(())
    }

    fn action_delete(&mut self) -> Result<()> {
        match self.current_selection() {
            Selection::Session(pi, wi, si) => {
                let display_name = self.workspace.projects[pi].worktrees[wi].sessions[si]
                    .display_name
                    .clone();
                self.mode = Mode::Confirm {
                    message: format!("Kill session '{}'?", display_name),
                    pending: PendingAction::DeleteSession {
                        project_idx: pi,
                        worktree_idx: wi,
                        session_idx: si,
                    },
                };
            }
            Selection::Pane(pi, wi, si, pane_idx) => {
                let pane = &self.workspace.projects[pi].worktrees[wi].sessions[si].panes[pane_idx];
                self.mode = Mode::Confirm {
                    message: format!("Close pane '{}' ?", pane.label),
                    pending: PendingAction::ClosePane {
                        pane_id: pane.pane_id,
                        revision: pane.revision,
                    },
                };
            }
            Selection::Worktree(pi, wi) => {
                let wt = &self.workspace.projects[pi].worktrees[wi];
                if wt.is_main {
                    self.set_status("Cannot delete main worktree");
                    return Ok(());
                }
                // ^ skip synchronous merge check — always warn; deletion logic is identical
                let msg = format!(
                    "Delete worktree '{}'? Branch may have unmerged changes.",
                    wt.name
                );
                self.mode = Mode::Confirm {
                    message: msg,
                    pending: PendingAction::DeleteWorktree {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                };
            }
            Selection::Project(pi) => {
                let name = self.workspace.projects[pi].name.clone();
                self.mode = Mode::Confirm {
                    message: format!("Unregister project '{}'? (files not deleted)", name),
                    pending: PendingAction::DeleteProject { project_idx: pi },
                };
            }
            Selection::RoutinesHeader(_) => {}
            Selection::Routine(pi, ri) => {
                let view = &self.workspace.projects[pi].routines[ri];
                self.mode = Mode::Confirm {
                    message: if view.capabilities.can_cancel {
                        format!(
                            "Cancel active run and delete routine '{}' ?",
                            view.routine.name
                        )
                    } else {
                        format!("Delete routine '{}' ?", view.routine.name)
                    },
                    pending: PendingAction::DeleteRoutine {
                        project_path: self.workspace.projects[pi].path.clone(),
                        name: view.routine.name.clone(),
                        revision: self.workspace.projects[pi].routine_revision,
                    },
                };
            }
            Selection::None => {}
        }
        Ok(())
    }

    fn action_edit_global_config(&mut self, _terminal: &mut Tui) {
        self.mode = Mode::GlobalSettings {
            form: GlobalSettingsForm::new(self.config.clone()),
        };
    }

    fn dispatch_global_settings(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        if let Mode::GlobalSettings { form } = &mut self.mode {
            if dispatch_text_edit(form, &action) {
                return Ok(());
            }
        }
        match action {
            Action::InputEscape | Action::Quit => {
                let cancelled_editor = match &mut self.mode {
                    Mode::GlobalSettings { form } => form.cancel_editor(),
                    _ => false,
                };
                if !cancelled_editor {
                    self.mode = Mode::Workspace;
                }
            }
            Action::Select => {
                let opens_integrations = matches!(
                    &self.mode,
                    Mode::GlobalSettings { form } if form.opens_agent_integrations()
                );
                if opens_integrations {
                    let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
                    if let Mode::GlobalSettings { form } = mode {
                        self.mode = Mode::IntegrationManager {
                            manager: IntegrationManager::new(self.integration_metadata.clone()),
                            return_form: Box::new(form),
                        };
                    }
                } else {
                    let result = match &mut self.mode {
                        Mode::GlobalSettings { form } => form.begin_or_commit(),
                        _ => Ok(()),
                    };
                    if let Err(error) = result {
                        self.set_error(error);
                    }
                }
            }
            Action::InputChar('j') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.next_field(false);
                }
            }
            Action::InputChar('k') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.next_field(true);
                }
            }
            Action::InputChar('h') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.left();
                }
            }
            Action::InputChar('l') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.right();
                }
            }
            Action::InputChar(' ') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.toggle();
                }
            }
            Action::InputChar('a') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.add_list_item();
                }
            }
            Action::InputChar('d') => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.delete_list_items();
                }
            }
            Action::InputChar('e') => {
                let list_is_open = matches!(
                    &self.mode,
                    Mode::GlobalSettings { form } if form.is_editing()
                );
                if list_is_open {
                    if let Mode::GlobalSettings { form } = &mut self.mode {
                        form.edit_list_item();
                    }
                } else {
                    self.edit_raw_global_config(terminal);
                }
            }
            Action::InputChar('s') => self.save_global_settings()?,
            _ => {}
        }
        Ok(())
    }

    fn dispatch_integration_manager(&mut self, action: Action) -> Result<()> {
        match action {
            Action::InputEscape | Action::Quit => {
                let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
                if let Mode::IntegrationManager { return_form, .. } = mode {
                    self.mode = Mode::GlobalSettings { form: *return_form };
                }
            }
            Action::NavigateDown | Action::InputChar('j') => {
                if let Mode::IntegrationManager { manager, .. } = &mut self.mode {
                    manager.navigate(false);
                }
            }
            Action::NavigateUp | Action::InputChar('k') => {
                if let Mode::IntegrationManager { manager, .. } = &mut self.mode {
                    manager.navigate(true);
                }
            }
            Action::InputChar(' ') => {
                if let Mode::IntegrationManager { manager, .. } = &mut self.mode {
                    manager.toggle();
                }
            }
            Action::Select => {
                let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
                if let Mode::IntegrationManager {
                    manager,
                    return_form,
                } = mode
                {
                    let targets = manager.targets();
                    self.mode = Mode::GlobalSettings { form: *return_form };
                    if targets.is_empty() {
                        self.set_status("Select at least one available integration");
                    } else {
                        self.spawn_install_integrations(targets);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn spawn_install_integrations(
        &mut self,
        targets: Vec<wsx_core::integration::IntegrationTarget>,
    ) {
        self.spawn_bg("install agent integrations", move || {
            let mut installed = Vec::new();
            let mut failures = Vec::new();
            for target in targets {
                match wsx_core::integration::install(target) {
                    Ok(_) => installed.push(target),
                    Err(error) => failures.push(format!("{}: {error}", target.label())),
                }
            }
            Ok(BgOutcome::IntegrationsInstalled {
                installed,
                failures,
            })
        });
    }

    fn save_global_settings(&mut self) -> Result<()> {
        let Some(config) = (match &self.mode {
            Mode::GlobalSettings { form } if !form.is_editing() => Some(form.draft().clone()),
            _ => None,
        }) else {
            return Ok(());
        };
        let Some(escape) = EscapeSequence::parse(&config.terminal_escape_chord) else {
            self.set_error(format!(
                "Invalid terminal_escape_chord: {}",
                config.terminal_escape_chord
            ));
            return Ok(());
        };
        config.save()?;
        self.terminal_escape_chord = escape;
        self.config = config;
        self.terminal_sidebar_override = None;
        self.mode = Mode::Workspace;
        self.set_status("Global settings saved");
        self.reload_plugins();
        Ok(())
    }

    fn reload_plugins(&mut self) {
        if matches!(
            self.runtime_client.call(&runtime::Request::PluginReload),
            Ok(runtime::Response::Plugins(_))
        ) {
            self.spawn_runtime_session_refresh();
        }
    }

    fn edit_raw_global_config(&mut self, terminal: &mut Tui) {
        let is_dirty = matches!(
            &self.mode,
            Mode::GlobalSettings { form } if form.is_dirty()
        );
        if is_dirty {
            self.set_status("Save or discard settings before opening raw TOML");
            return;
        }
        let path = match self.config.prepare_for_edit() {
            Ok(path) => path,
            Err(error) => {
                self.set_error(format!("Could not prepare global config: {error}"));
                return;
            }
        };
        if let Err(error) = edit_file(terminal, &path) {
            self.set_error(format!("Could not edit global config: {error}"));
            return;
        }
        match GlobalConfig::load() {
            Err(error) => self.set_error(format!("Could not validate global config: {error}")),
            Ok((_, Some(warning))) => self.set_error(warning),
            Ok((config, None)) => {
                let Some(escape) = EscapeSequence::parse(&config.terminal_escape_chord) else {
                    self.set_error(format!(
                        "Invalid terminal_escape_chord: {}",
                        config.terminal_escape_chord
                    ));
                    return;
                };
                self.terminal_escape_chord = escape;
                self.config = config.clone();
                self.terminal_sidebar_override = None;
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.reset_saved(config);
                }
                self.set_status("Global config reloaded");
                self.reload_plugins();
            }
        }
    }

    fn action_edit(&mut self) -> Result<()> {
        if let Selection::Routine(pi, ri) = self.current_selection() {
            let view = self.workspace.projects[pi].routines[ri].clone();
            if !view.capabilities.can_edit {
                self.set_status("Routine cannot be edited in its current state");
                return Ok(());
            }
            let mut form = RoutineForm::from_routine(view.routine);
            if !view.capabilities.can_rename {
                form.field = 1;
                form.cursor = form.cron.len();
            }
            self.mode = Mode::RoutineEditor {
                project_idx: pi,
                original_name: Some(form.name.clone()),
                can_rename: view.capabilities.can_rename,
                form,
            };
            return Ok(());
        }
        let pi = match self.current_selection() {
            Selection::Project(pi)
            | Selection::Worktree(pi, _)
            | Selection::Session(pi, _, _)
            | Selection::Pane(pi, _, _, _)
            | Selection::RoutinesHeader(pi) => pi,
            Selection::None => {
                self.set_status("Select a project or worktree");
                return Ok(());
            }
            Selection::Routine(_, _) => unreachable!(),
        };
        self.mode = Mode::Config { project_idx: pi };
        Ok(())
    }

    fn session_candidates(
        &self,
        matches: impl Fn(&wsx_core::model::workspace::SessionInfo) -> bool,
    ) -> Vec<usize> {
        self.flat()
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let FlatEntry::Session {
                    project_idx,
                    worktree_idx,
                    session_idx,
                } = entry
                else {
                    return None;
                };
                self.workspace
                    .session(*project_idx, *worktree_idx, *session_idx)
                    .filter(|session| matches(session))
                    .map(|_| index)
            })
            .collect()
    }

    fn active_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session).app_state() == AppSessionState::Active
        })
    }

    fn idle_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session.agent.is_some()
                && session_state::derive(session).app_state() == AppSessionState::Idle
        })
    }

    fn candidate_target(&self, candidates: &[usize], dir: isize) -> Option<usize> {
        if dir >= 0 {
            candidates
                .iter()
                .find(|&&index| index > self.tree_selected)
                .or_else(|| candidates.first())
                .copied()
        } else {
            candidates
                .iter()
                .rev()
                .find(|&&index| index < self.tree_selected)
                .or_else(|| candidates.last())
                .copied()
        }
    }

    fn idle_target(&self, dir: isize) -> Option<usize> {
        self.candidate_target(&self.idle_candidates(), dir)
    }

    fn action_move_idle(&mut self, dir: isize) {
        let Some(target) = self.idle_target(dir) else {
            self.set_status("No idle sessions");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    fn action_send_ctrl_c(&mut self) -> Result<()> {
        let pane_id = match self.current_selection() {
            Selection::Session(pi, wi, si) => self
                .workspace
                .session(pi, wi, si)
                .map(|session| session.pane_id),
            Selection::Pane(pi, wi, si, pane_idx) => self
                .workspace
                .session(pi, wi, si)
                .and_then(|session| session.panes.get(pane_idx))
                .map(|pane| pane.pane_id),
            _ => None,
        };
        if let Some(pane_id) = pane_id {
            self.unmute_on_interaction(pane_id);
            self.send_terminal_bytes_once(pane_id, vec![3])?;
        }
        Ok(())
    }

    fn active_target(&self, dir: isize) -> Option<usize> {
        self.candidate_target(&self.active_candidates(), dir)
    }

    fn action_move_active(&mut self, dir: isize) {
        let Some(target) = self.active_target(dir) else {
            self.set_status("No active sessions");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    fn attention_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session).app_state() == AppSessionState::NeedsAttention
        })
    }

    fn blocked_attention_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session) == session_state::SessionHeuristic::Blocked
        })
    }

    fn attention_target(&self, dir: isize) -> Option<usize> {
        if self.config.attention_priority == AttentionPriority::BlockedFirst {
            let blocked = self.blocked_attention_candidates();
            if !blocked.is_empty() {
                return self.candidate_target(&blocked, dir);
            }
        }
        self.candidate_target(&self.attention_candidates(), dir)
    }

    fn action_next_attention(&mut self, dir: isize) {
        let Some(target) = self.attention_target(dir) else {
            self.set_status("No sessions need attention");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    fn action_switch_idle(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.idle_target(dir) else {
            self.set_status("No idle sessions");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    fn action_switch_active(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.active_target(dir) else {
            self.set_status("No active sessions");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    fn action_switch_attention(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.attention_target(dir) else {
            self.set_status("No sessions need attention");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    fn action_switch_group(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let groups = self.config.ordered_group_keys();
        let Some(group) = adjacent_group(&groups, &self.active_group, dir) else {
            self.set_status("No other groups");
            return Ok(());
        };
        let Some((project_idx, worktree_idx, session_idx)) =
            preferred_group_session(&self.config, &self.workspace, &group)
        else {
            self.set_status(format!(
                "No attention or idle agent sessions in group '{}'",
                crate::ui::workspace_nav::group_label(&group)
            ));
            return Ok(());
        };

        self.set_active_group(group);
        let Some(target) = self.reveal_session(project_idx, worktree_idx, session_idx) else {
            self.set_error("Group session could not be selected");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    // ^ docs/terminal-context.md: cycle the shared project ring, not flat tree rows
    // or the first visible peer; both would miss collapsed worktrees or starve peers.
    fn context_session_target(&self, dir: isize) -> Option<(usize, usize, usize)> {
        let (pi, wi, si) = match self.current_selection() {
            Selection::Session(pi, wi, si) | Selection::Pane(pi, wi, si, _) => (pi, wi, si),
            _ => return None,
        };
        let project = self.workspace.projects.get(pi)?;
        let order: Vec<_> = session_state::context_sessions(project)
            .map(|(wi, si, _)| (pi, wi, si))
            .collect();
        if order.len() <= 1 {
            return None;
        }
        let current = order
            .iter()
            .position(|&position| position == (pi, wi, si))?;
        let next = cyclic_sibling_index(current, order.len(), dir)?;
        Some(order[next])
    }

    fn action_switch_context_session(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some((pi, wi, si)) = self.context_session_target(dir) else {
            self.set_status("No other sessions in this project");
            return Ok(());
        };
        let Some(target) = self.reveal_session(pi, wi, si) else {
            self.set_status("Project session could not be selected");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    fn reveal_session(
        &mut self,
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
    ) -> Option<usize> {
        self.workspace
            .session(project_idx, worktree_idx, session_idx)?;
        self.manually_expand_project(project_idx);
        self.set_worktree_expanded(project_idx, worktree_idx, true);
        self.rebuild_flat();
        self.flat().iter().position(|entry| {
            matches!(entry,
                FlatEntry::Session { project_idx: pi, worktree_idx: wi, session_idx: si }
                if *pi == project_idx && *wi == worktree_idx && *si == session_idx
            )
        })
    }

    fn switch_to_flat_session(&mut self, target: usize, terminal: &mut Tui) -> Result<()> {
        let Some(FlatEntry::Session {
            project_idx,
            worktree_idx,
            session_idx,
        }) = self.flat().get(target).cloned()
        else {
            self.set_status("Session not found");
            return Ok(());
        };
        self.tree_selected = target;
        self.update_scroll();
        self.switch_terminal_session(project_idx, worktree_idx, session_idx, terminal)
    }

    fn switch_terminal_session(
        &mut self,
        project_idx: usize,
        worktree_idx: usize,
        session_idx: usize,
        terminal: &mut Tui,
    ) -> Result<()> {
        // ^ [[Modal Terminal Navigation]] A target stream must pass the normal
        // dimension-first baseline gate before Terminal input resumes.
        self.mode = Mode::Workspace;
        self.attach_session(project_idx, worktree_idx, session_idx, terminal)?;
        if let Some(pending) = self.pending_terminal_entry {
            self.mode = Mode::Terminal {
                pane_id: pending.pane_id,
            };
            self.force_terminal_redraw = true;
            self.needs_redraw = true;
        }
        Ok(())
    }

    fn action_dismiss_attention(&mut self) {
        if let Selection::Session(pi, wi, si) = self.current_selection() {
            // ^ [[Session Model]] Dismiss the exact done revision before allowing
            // a later press to apply the separate local mute state.
            let done_pane = self.workspace.session(pi, wi, si).and_then(|session| {
                (session.agent_status == runtime::AgentState::Done && !session.outcome_acknowledged)
                    .then_some(session.pane_id)
            });
            if let Some(pane_id) = done_pane {
                self.unmute_on_interaction(pane_id);
                return;
            }
            if let Some(sess) = self.workspace.session_mut(pi, wi, si) {
                if session_state::derive(sess).app_state() == AppSessionState::Active {
                    return;
                }
                sess.muted = !sess.muted;
                let muted = sess.muted;
                let terminal_id = sess.terminal_id.to_string();
                if muted {
                    self.muted_terminal_ids.insert(terminal_id.clone());
                } else {
                    self.muted_terminal_ids.remove(&terminal_id);
                }
                self.cache_changes
                    .muted_terminals
                    .insert(terminal_id, muted);
                self.mark_dirty();
                return;
            }
        }
        self.set_status("No session selected");
    }

    fn action_set_alias(&mut self) -> Result<()> {
        match self.current_selection() {
            Selection::Worktree(pi, wi) => {
                let current = self.workspace.projects[pi].worktrees[wi]
                    .alias
                    .clone()
                    .unwrap_or_default();
                self.mode = Mode::Input {
                    context: InputContext::SetAlias {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                    state: InputState::with_value("alias: ", current),
                };
            }
            Selection::Session(pi, wi, si) => {
                let current = self.workspace.projects[pi].worktrees[wi].sessions[si]
                    .display_name
                    .clone();
                self.mode = Mode::Input {
                    context: InputContext::RenameSession {
                        project_idx: pi,
                        worktree_idx: wi,
                        session_idx: si,
                    },
                    state: InputState::with_value("name: ", current),
                };
            }
            _ => {
                self.set_status("Select a worktree or session");
            }
        }
        Ok(())
    }

    // ── Input confirm ─────────────────────────────────────────────────────────

    fn confirm_input(&mut self, _terminal: &mut Tui) -> Result<()> {
        let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
        if let Mode::Input { context, state } = mode {
            let value = state.value().trim().to_string();
            match context {
                InputContext::AddProject => self.do_register_project(ops::expand_path(&value))?,
                InputContext::AddWorktree { project_idx } => {
                    if !value.is_empty() {
                        self.mode = Mode::Confirm {
                            message: format!("Create worktree '{}'?", value),
                            pending: PendingAction::CreateWorktree {
                                project_idx,
                                branch: value,
                            },
                        };
                        return Ok(());
                    }
                }
                InputContext::AddSession {
                    project_idx,
                    worktree_idx,
                } => {
                    // Step 1: got name, now ask for command
                    self.mode = Mode::Input {
                        context: InputContext::AddSessionCmd {
                            project_idx,
                            worktree_idx,
                            session_label: value,
                        },
                        state: InputState::new("command (optional): "),
                    };
                    return Ok(());
                }
                InputContext::AddSessionCmd {
                    project_idx,
                    worktree_idx,
                    session_label,
                } => {
                    let cmd = if value.is_empty() { None } else { Some(value) };
                    self.do_create_session(project_idx, worktree_idx, session_label, cmd)?;
                }
                InputContext::SetAlias {
                    project_idx,
                    worktree_idx,
                } => {
                    self.do_apply_alias(project_idx, worktree_idx, value)?;
                }
                InputContext::RenameSession {
                    project_idx,
                    worktree_idx,
                    session_idx,
                } => {
                    if !value.is_empty() {
                        self.do_rename_session(project_idx, worktree_idx, session_idx, value)?;
                    }
                }
                InputContext::AddGroup => {
                    let trimmed = value.trim().to_string();
                    if trimmed.is_empty() || GroupKey::named(trimmed.clone()).is_err() {
                        self.set_status("Group name is empty or reserved");
                    } else if self.config.groups.contains(&trimmed) {
                        self.set_status(format!("Group '{}' already exists", trimmed));
                    } else {
                        self.config.groups.push(trimmed);
                        self.config.save()?;
                    }
                    self.mode = Mode::GroupManager {
                        selected: self.config.groups.len().saturating_add(1),
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    };
                }
                InputContext::RenameGroup { group_idx } => {
                    let trimmed = value.trim().to_string();
                    if !trimmed.is_empty()
                        && GroupKey::named(trimmed.clone()).is_ok()
                        && self.config.groups.get(group_idx) != Some(&trimmed)
                        && !self.config.groups.contains(&trimmed)
                    {
                        let old_name =
                            std::mem::replace(&mut self.config.groups[group_idx], trimmed.clone());
                        for project in &mut self.config.projects {
                            for membership in &mut project.groups {
                                if *membership == old_name {
                                    *membership = trimmed.clone();
                                }
                            }
                        }
                        if self.active_group == GroupKey::Named(old_name) {
                            self.active_group = GroupKey::Named(trimmed.clone());
                            self.persist_active_group();
                        }
                        self.config.save()?;
                        self.recompute_visible();
                        self.mark_dirty();
                    } else if !trimmed.is_empty()
                        && self.config.groups.get(group_idx) != Some(&trimmed)
                    {
                        self.set_status("Group name is reserved or already exists");
                    }
                    self.mode = Mode::GroupManager {
                        selected: group_idx + 1,
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    };
                }
            }
        }
        Ok(())
    }

    fn confirm_action(&mut self) -> Result<()> {
        if self.is_busy() {
            self.set_status("Operation in progress");
            return Ok(());
        }
        let mode = std::mem::replace(&mut self.mode, Mode::Workspace);
        if let Mode::Confirm { pending, .. } = mode {
            match pending {
                PendingAction::DeleteProject { project_idx } => {
                    self.do_delete_project(project_idx)?;
                }
                PendingAction::DeleteSession {
                    project_idx,
                    worktree_idx,
                    session_idx,
                } => {
                    self.do_delete_session(project_idx, worktree_idx, session_idx)?;
                }
                PendingAction::ClosePane { pane_id, revision } => {
                    match self.runtime_client.call(&runtime::Request::PaneClose {
                        pane_id,
                        expected_revision: revision,
                    })? {
                        runtime::Response::Ack { .. } => {
                            self.spawn_runtime_session_refresh();
                            self.set_status("Pane closed");
                        }
                        runtime::Response::Error(error) => {
                            self.set_error(api_error_text(&error));
                        }
                        _ => self.set_error("Unexpected pane close response"),
                    }
                }
                PendingAction::DeleteWorktree {
                    project_idx: pi,
                    worktree_idx: wi,
                } => {
                    // ^ A snapshot may have removed the target while the prompt was open.
                    let Some((repo, wt_path, branch)) =
                        self.workspace.projects.get(pi).and_then(|p| {
                            let wt = p.worktrees.get(wi)?;
                            Some((p.path.clone(), wt.path.clone(), wt.branch.clone()))
                        })
                    else {
                        self.set_status("Worktree is no longer available");
                        return Ok(());
                    };
                    let label = format!("Deleted: {}", branch);
                    // Optimistically remove before spawning so the tree updates this frame
                    self.pending_deletions.insert(wt_path.clone());
                    self.workspace.projects[pi].worktrees.remove(wi);
                    self.rebuild_flat();
                    self.clamp_selected();
                    self.spawn_bg(format!("delete {}", branch), move || {
                        ops::delete_worktree(&repo, &wt_path, &branch)?;
                        Ok(BgOutcome::WorktreeRemoved { label })
                    });
                }
                PendingAction::CreateWorktree {
                    project_idx: pi,
                    branch,
                } => {
                    let Some(p) = self.workspace.projects.get(pi) else {
                        self.set_status("Project is no longer available");
                        return Ok(());
                    };
                    let (config, project_name, repo_path, default_branch, proj_config) = (
                        self.config.clone(),
                        p.name.clone(),
                        p.path.clone(),
                        p.default_branch.clone(),
                        p.config.clone().unwrap_or_default(),
                    );
                    let label = format!("Created worktree: {branch}");
                    self.spawn_bg(format!("create {branch}"), move || {
                        let created = ops::create_configured_worktree(
                            &config,
                            &project_name,
                            &repo_path,
                            &default_branch,
                            &proj_config,
                            &branch,
                            wsx_core::model::workspace::WorktreeInitialSession::Disabled,
                        )?;
                        Ok(BgOutcome::WorktreeCreated {
                            label,
                            warning: created.warning,
                            session_error: created.session_error,
                        })
                    });
                }
                PendingAction::DeleteGroup { group_idx } => {
                    let group_name = self.config.groups.remove(group_idx);
                    for project in &mut self.config.projects {
                        project
                            .groups
                            .retain(|membership| membership != &group_name);
                    }
                    if self.active_group == GroupKey::Named(group_name.clone()) {
                        self.active_group = GroupKey::Ungrouped;
                        self.persist_active_group();
                    }
                    self.config.save()?;
                    self.recompute_visible();
                    self.mark_dirty();
                    self.mode = Mode::GroupManager {
                        selected: 0,
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    };
                    self.set_status(format!("Deleted group '{}'", group_name));
                }
                PendingAction::ShutdownDaemon => {
                    self.persist_state(true)?;
                    self.runtime_client.shutdown()?;
                    self.should_quit = true;
                }
                PendingAction::InstallIntegrations { targets, .. } => {
                    self.spawn_install_integrations(targets);
                }
                PendingAction::DeleteRoutine {
                    project_path,
                    name,
                    revision,
                } => {
                    if !self
                        .workspace
                        .projects
                        .iter()
                        .any(|project| project.path == project_path)
                    {
                        self.mode = Mode::Workspace;
                        self.set_status(format!(
                            "Routine '{name}' not deleted: project is no longer registered"
                        ));
                        return Ok(());
                    }
                    self.spawn_routine_request(
                        project_path,
                        asched_core::routine::ipc::Action::Delete {
                            revision,
                            name: name.clone(),
                        },
                        RoutineResultKind::Delete { name: name.clone() },
                    );
                    self.set_status(format!("Deleting routine '{name}'…"));
                }
            }
        }
        Ok(())
    }

    fn restore_failed_routine_delete(&mut self, project_idx: usize, name: &str, error: String) {
        self.mode = Mode::Workspace;
        self.select_routine(project_idx, Some(name));
        self.set_status(format!("Routine '{name}' not deleted: {error}"));
    }

    // ── Dispatch to ops ───────────────────────────────────────────────────────

    fn do_register_project(&mut self, path: PathBuf) -> Result<()> {
        // Surface duplicate/invalid-path rejections as status rather than
        // propagating up the event loop (which would tear down the TUI).
        let project = match ops::register_project(path, &mut self.config) {
            Ok(project) => project,
            Err(e) => {
                self.set_status(e.to_string());
                return Ok(());
            }
        };
        // Virtual filters are views; only active named groups become memberships.
        if let Some(entry) = self.config.projects.last_mut() {
            entry.groups = match &self.active_group {
                GroupKey::Named(name) => vec![name.clone()],
                GroupKey::Ungrouped => Vec::new(),
            };
        }
        self.workspace.projects.push(project);
        self.recompute_visible();
        self.config.save()?;
        self.set_status("Project registered");
        Ok(())
    }

    fn do_create_session(
        &mut self,
        pi: usize,
        wi: usize,
        session_label: String,
        command: Option<String>,
    ) -> Result<()> {
        let Some((proj_name, wt_path, wt_slug)) = self.workspace.projects.get(pi).and_then(|p| {
            let wt = p.worktrees.get(wi)?;
            Some((p.name.clone(), wt.path.clone(), wt.session_slug(&p.name)))
        }) else {
            self.set_status("Worktree is no longer available");
            return Ok(());
        };
        let explicit_name = if session_label.is_empty() {
            None
        } else {
            Some(session_label)
        };
        let (_pane_id, display_name) = ops::create_session(
            &self.config,
            &proj_name,
            &wt_slug,
            &wt_path,
            explicit_name,
            command,
        )?;
        self.set_status(format!("Session '{}' created", display_name));
        // Expand before the authoritative Runtime refresh reveals the new pane.
        self.set_worktree_expanded(pi, wi, true);
        self.spawn_runtime_refresh();
        Ok(())
    }

    fn do_delete_project(&mut self, pi: usize) -> Result<()> {
        let Some((name, path)) = self
            .workspace
            .projects
            .get(pi)
            .map(|p| (p.name.clone(), p.path.clone()))
        else {
            self.set_status("Project is no longer available");
            return Ok(());
        };
        self.workspace.projects.remove(pi);
        self.rebuild_flat();
        ops::unregister_project(&path, &mut self.config);
        self.config.save()?;
        self.clamp_selected();
        self.set_status(format!("Unregistered: {}", name));
        Ok(())
    }

    fn do_delete_session(&mut self, pi: usize, wi: usize, si: usize) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session is no longer available");
            return Ok(());
        };
        let session_id = session.session_id;
        let display_name = session.display_name.clone();
        self.spawn_bg(format!("kill {display_name}"), move || {
            ops::kill_session(session_id)?;
            Ok(BgOutcome::SessionKilled {
                session_id,
                display_name,
            })
        });
        Ok(())
    }

    fn do_apply_alias(&mut self, pi: usize, wi: usize, alias: String) -> Result<()> {
        let Some((branch, proj_path)) = self
            .workspace
            .worktree(pi, wi)
            .map(|wt| (wt.branch.clone(), self.workspace.projects[pi].path.clone()))
        else {
            self.set_status("Worktree is no longer available");
            return Ok(());
        };

        ops::set_alias(&mut self.config, &proj_path, &branch, &alias);
        self.config.save()?;

        if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
            wt.alias = (!alias.is_empty()).then(|| alias.clone());
        }

        self.set_status(if alias.is_empty() {
            format!("Alias cleared for '{}'", branch)
        } else {
            format!("Alias '{}' set for '{}'", alias, branch)
        });
        Ok(())
    }

    fn do_rename_session(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        new_name: String,
    ) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session is no longer available");
            return Ok(());
        };
        let session_id = session.session_id;
        let pane_id = session.pane_id;
        self.unmute_on_interaction(pane_id);
        ops::rename_session(session_id, &new_name)?;
        if let Some(session) = self.workspace.session_mut(pi, wi, si) {
            session.display_name = new_name.clone();
        }
        self.mark_dirty();
        self.set_status(format!("Session renamed to '{}'", new_name));
        Ok(())
    }

    // ── Move project ──────────────────────────────────────────────────────────

    fn action_enter_move(&mut self) {
        match self.current_selection() {
            Selection::Project(pi) => {
                self.mode = Mode::Move { project_idx: pi };
            }
            Selection::Worktree(pi, wi) => {
                self.mode = Mode::MoveWorktree {
                    project_idx: pi,
                    worktree_idx: wi,
                };
            }
            Selection::Session(pi, wi, si) => {
                self.mode = Mode::MoveSession {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                };
            }
            _ => self.set_status("Select a project, worktree, or session to move"),
        }
    }

    fn move_project(&mut self, pi: usize, delta: isize) {
        let Some(new_pi) =
            adjacent_visible_project_index(&self.workspace, &self.visible_projects, pi, delta)
        else {
            return;
        };
        self.workspace.projects.swap(pi, new_pi);
        self.mode = Mode::Move {
            project_idx: new_pi,
        };
        self.rebuild_flat();
        if let Some(pos) = self
            .flat()
            .iter()
            .position(|e| matches!(e, FlatEntry::Project { idx } if *idx == new_pi))
        {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    fn move_project_down(&mut self, pi: usize) {
        self.move_project(pi, 1);
    }

    fn move_project_up(&mut self, pi: usize) {
        self.move_project(pi, -1);
    }

    fn move_worktree(&mut self, pi: usize, wi: usize, delta: isize) {
        let new_wi = wi as isize + delta;
        if new_wi < 0 {
            return;
        }
        let new_wi = new_wi as usize;
        let Some(project) = self.workspace.projects.get_mut(pi) else {
            return;
        };
        if wi >= project.worktrees.len() || new_wi >= project.worktrees.len() {
            return;
        }
        project.worktrees.swap(wi, new_wi);
        self.sync_config_worktree_order(pi);
        self.mode = Mode::MoveWorktree {
            project_idx: pi,
            worktree_idx: new_wi,
        };
        self.rebuild_flat();
        if let Some(pos) = self.flat().iter().position(|entry| {
            matches!(entry, FlatEntry::Worktree { project_idx, worktree_idx }
                if *project_idx == pi && *worktree_idx == new_wi)
        }) {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    fn move_session(&mut self, pi: usize, wi: usize, si: usize, delta: isize) -> Result<()> {
        let new_si = si as isize + delta;
        if new_si < 0 {
            return Ok(());
        }
        let new_si = new_si as usize;
        let Some(worktree) = self.workspace.worktree_mut(pi, wi) else {
            return Ok(());
        };
        let sessions = &mut worktree.sessions;
        if new_si >= sessions.len() || si >= sessions.len() {
            return Ok(());
        }
        let session_id = sessions[si].session_id;
        let target_session_id = sessions[new_si].session_id;
        let placement = if delta < 0 {
            runtime::SessionPlacement::Before
        } else {
            runtime::SessionPlacement::After
        };
        let revision = ops::reorder_session(
            session_id,
            target_session_id,
            placement,
            sessions[si].revision,
        )?;
        sessions.swap(si, new_si);
        sessions[new_si].revision = revision;
        let session_ids = sessions.iter().map(|session| session.session_id).collect();
        let worktree_path = self.workspace.projects[pi].worktrees[wi].path.clone();
        self.pending_session_orders.insert(
            worktree_path,
            PendingSessionOrder {
                moved_session_id: session_id,
                revision,
                session_ids,
            },
        );
        self.mode = Mode::MoveSession {
            project_idx: pi,
            worktree_idx: wi,
            session_idx: new_si,
        };
        self.rebuild_flat();
        if let Some(pos) = self.flat().iter().position(|e| {
            matches!(e, FlatEntry::Session { project_idx: p, worktree_idx: w, session_idx: s }
                if *p == pi && *w == wi && *s == new_si)
        }) {
            self.tree_selected = pos;
            self.update_scroll();
        }
        Ok(())
    }

    fn sync_config_worktree_order(&mut self, project_idx: usize) {
        let Some(project) = self.workspace.projects.get(project_idx) else {
            return;
        };
        self.config.set_worktree_order(
            &project.path,
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect(),
        );
    }

    fn sync_config_project_order(&mut self) {
        let ordered: Vec<_> = self
            .workspace
            .projects
            .iter()
            .filter_map(|wp| {
                self.config
                    .projects
                    .iter()
                    .find(|c| c.path == wp.path)
                    .cloned()
            })
            .collect();
        self.config.projects = ordered;
    }

    // ── Project-group navigation ─────────────────────────────────────────────

    fn persist_active_group(&mut self) {
        if !self.persist_group_selection {
            return;
        }
        if let Err(error) = wsx_core::cache::save_group_selection(&self.active_group) {
            self.set_warning(format!("Could not save group selection: {error}"));
        }
    }

    fn set_active_group(&mut self, key: GroupKey) {
        let groups = self.config.ordered_group_keys();
        self.group_header_scroll = groups
            .iter()
            .position(|candidate| candidate == &key)
            .unwrap_or(0);
        if self.active_group == key {
            return;
        }
        self.active_group = key;
        self.persist_active_group();
        self.recompute_visible();
        self.update_scroll();
        self.mark_dirty();
    }

    fn toggle_active_group(&mut self, key: GroupKey) {
        self.set_active_group(key);
    }

    fn action_group_next(&mut self) {
        let groups = self.config.ordered_group_keys();
        let current = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        let next = (current + 1) % groups.len();
        self.set_active_group(groups[next].clone());
    }

    fn action_group_prev(&mut self) {
        let groups = self.config.ordered_group_keys();
        let current = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        let previous = current.checked_sub(1).unwrap_or(groups.len() - 1);
        self.set_active_group(groups[previous].clone());
    }

    fn action_group_manager(&mut self) {
        let groups = self.config.ordered_group_keys();
        let selected = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        self.mode = Mode::GroupManager {
            selected,
            scroll: 0,
            purpose: GroupManagerPurpose::Switch,
        };
    }

    fn action_assign_group(&mut self) {
        let Selection::Project(project_idx) = self.current_selection() else {
            return;
        };
        if self.config.groups.is_empty() {
            self.set_status("Create a group first with T");
            return;
        }
        self.mode = Mode::GroupManager {
            selected: 0,
            scroll: 0,
            purpose: GroupManagerPurpose::Assign { project_idx },
        };
    }

    fn dispatch_group_manager(
        &mut self,
        selected: usize,
        scroll: usize,
        purpose: GroupManagerPurpose,
        action: Action,
    ) -> Result<()> {
        let group_count = match purpose {
            GroupManagerPurpose::Switch => self.config.groups.len() + 1,
            GroupManagerPurpose::Assign { .. } => self.config.groups.len(),
        };
        let layout = crate::ui::workspace_nav::SidebarLayout::new(self.tree_area);
        let visible_height = layout.list.height.max(1) as usize;
        let manager_mode = |selected: usize, scroll: usize| Mode::GroupManager {
            selected,
            scroll: crate::ui::workspace_tree::compute_scroll(selected, visible_height, scroll),
            purpose,
        };
        let toggle = |app: &mut Self, index: usize| -> Result<()> {
            match purpose {
                GroupManagerPurpose::Switch => {
                    if let Some(key) = app.config.ordered_group_keys().get(index).cloned() {
                        app.toggle_active_group(key);
                    }
                }
                GroupManagerPurpose::Assign { project_idx } => {
                    let Some(project) = app.workspace.projects.get(project_idx) else {
                        app.set_status("Project is no longer available");
                        return Ok(());
                    };
                    let Some(group) = app.config.groups.get(index).cloned() else {
                        return Ok(());
                    };
                    let path = project.path.clone();
                    if app.config.project_groups(&path).contains(&group) {
                        app.config.remove_project_from_group(&path, &group);
                    } else {
                        app.config.add_project_to_group(&path, &group);
                    }
                    app.config.save()?;
                    app.recompute_visible();
                }
            }
            Ok(())
        };
        match action {
            Action::InputEscape | Action::Quit | Action::GroupManager => {
                self.mode = Mode::Workspace
            }
            Action::InputChar('j') | Action::NavigateDown if group_count > 0 => {
                self.mode = manager_mode((selected + 1) % group_count, scroll)
            }
            Action::InputChar('k') | Action::NavigateUp if group_count > 0 => {
                self.mode = manager_mode(selected.checked_sub(1).unwrap_or(group_count - 1), scroll)
            }
            Action::MouseClick { col, row } => {
                if let Some(index) = layout.item_at(Position::new(col, row), scroll, group_count) {
                    toggle(self, index)?;
                    self.mode = manager_mode(index, scroll);
                }
            }
            Action::Select | Action::InputChar(' ') => {
                toggle(self, selected)?;
                self.mode = manager_mode(selected, scroll);
            }
            Action::InputChar('a') if purpose == GroupManagerPurpose::Switch => {
                self.mode = Mode::Input {
                    context: InputContext::AddGroup,
                    state: InputState::new("group name: "),
                };
            }
            Action::InputChar('r') if purpose == GroupManagerPurpose::Switch => {
                if selected < 1 {
                    self.set_status("Virtual groups cannot be renamed");
                } else {
                    let group_idx = selected - 1;
                    if let Some(name) = self.config.groups.get(group_idx) {
                        self.mode = Mode::Input {
                            context: InputContext::RenameGroup { group_idx },
                            state: InputState::with_value("new name: ", name.clone()),
                        };
                    }
                }
            }
            Action::InputChar('d') if purpose == GroupManagerPurpose::Switch => {
                if selected < 1 {
                    self.set_status("Virtual groups cannot be deleted");
                } else {
                    let group_idx = selected - 1;
                    if let Some(name) = self.config.groups.get(group_idx) {
                        let count = self
                            .config
                            .projects
                            .iter()
                            .filter(|project| project.groups.contains(name))
                            .count();
                        self.mode = Mode::Confirm {
                            message: format!("Delete group '{name}'? Removes {count} memberships"),
                            pending: PendingAction::DeleteGroup { group_idx },
                        };
                    }
                }
            }
            Action::InputChar('J') if purpose == GroupManagerPurpose::Switch && selected >= 1 => {
                let group_idx = selected - 1;
                if group_idx + 1 < self.config.groups.len() {
                    self.config.groups.swap(group_idx, group_idx + 1);
                    self.config.save()?;
                    self.mode = manager_mode(selected + 1, scroll);
                }
            }
            Action::InputChar('K') if purpose == GroupManagerPurpose::Switch && selected > 1 => {
                let group_idx = selected - 1;
                self.config.groups.swap(group_idx - 1, group_idx);
                self.config.save()?;
                self.mode = manager_mode(selected - 1, scroll);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Projects can have multiple memberships, but the Workspace applies one optional group filter.
// ^ [[wsx Architecture]] Project grouping and filtering have separate cardinality.
fn initial_active_group(config: &GlobalConfig, stored: Option<GroupKey>) -> GroupKey {
    stored
        .filter(|candidate| config.ordered_group_keys().contains(candidate))
        .unwrap_or(GroupKey::Ungrouped)
}

fn compute_visible_projects(
    config: &GlobalConfig,
    workspace: &WorkspaceState,
    active_group: Option<&GroupKey>,
) -> HashSet<usize> {
    workspace
        .projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            project_matches_group(config.project_groups(&project.path), active_group)
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
