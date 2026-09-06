// App state machine and event loop.
// ref: ratatui app patterns — https://ratatui.rs/concepts/application-patterns/

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use ratatui::{
    layout::{Position, Rect},
    widgets::Clear,
};

use crate::{
    action::Action,
    event::{poll_event, EscapeSequence, EventMode},
    session_state::{self, AppSessionState},
    terminal_surface::{SurfaceUpdate, TerminalSurfaces},
    tui::{self, Tui},
    ui::{
        self,
        global_settings::GlobalSettingsForm,
        input::InputState,
        routine_editor::{RoutineForm, RoutinePreset},
    },
};
use wsx_core::{
    config::global::{
        project_has_activity_within, project_matches_group, GlobalConfig, GroupKey, TerminalSidebar,
    },
    git::{info as git_info, worktree as git_worktree},
    model::workspace::{
        flatten_tree_filtered, FlatEntry, GitInfo, Project, Selection, WorkspaceState,
    },
    ops, runtime,
};

use git_info::FetchOutcome;

type WorktreeSnapshot = Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>;
type RuntimeRefresh = Result<(runtime::Availability, runtime::Snapshot, WorktreeSnapshot)>;

fn collect_runtime_refresh(config: &GlobalConfig, background: bool) -> RuntimeRefresh {
    let availability = if background {
        runtime::ensure_background_available()?
    } else {
        runtime::ensure_available()?
    };
    let discovery = ops::discover_workspace(config)?;
    let snapshot = ops::synchronize_discovery(&discovery)?;
    Ok((availability, snapshot, discovery.into_worktrees()))
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
const TERMINAL_TICK_MS: u64 = 8;
const FAST_INTERVAL_MS: u64 = 500;
const GIT_SWEEP_INTERVAL_MS: u64 = 15_000;
const SLOW_INTERVAL_MS: u64 = 30_000;
const WORKSPACE_SCROLL_LINES: isize = 3;

fn unix_time_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn project_is_stale(
    project: &Project,
    freshened_projects: &HashSet<PathBuf>,
    now_unix_ms: u64,
    window_ms: u64,
) -> bool {
    !freshened_projects.contains(&project.path)
        && !project_has_activity_within(
            project.last_agent_active_unix_ms,
            project.last_terminal_active_unix_ms,
            now_unix_ms,
            window_ms,
        )
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
    if !in_bounds && !matches!(mouse.kind, MouseEventKind::Up(_)) {
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
    Move {
        project_idx: usize,
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
    },
}

// ── Background jobs ───────────────────────────────────────────────────────────

pub struct BgJob {
    pub label: String,
}

pub enum BgOutcome {
    WorktreeRemoved {
        label: String,
    },
    WorktreeCreated {
        label: String,
    },
    SessionKilled {
        session_id: runtime::SessionId,
        display_name: String,
    },
    IntegrationsInstalled {
        labels: Vec<&'static str>,
        failures: Vec<String>,
    },
}

struct BgResult {
    label: String,
    outcome: Result<BgOutcome>,
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
            "asched protocol mismatch; upgrade wsx and asched together, then restart asched".into()
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

fn current_integration_prompt_version() -> String {
    wsx_core::integration::prompt_version(env!("CARGO_PKG_VERSION"))
}

fn integration_prompt_label(targets: &[wsx_core::integration::IntegrationTarget]) -> String {
    let visible = targets
        .iter()
        .take(3)
        .map(|target| target.label())
        .collect::<Vec<_>>()
        .join(", ");
    match targets.len().saturating_sub(3) {
        0 => visible,
        remaining => format!("{visible}, and {remaining} more"),
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

#[derive(Debug, Clone)]
pub struct Notice {
    pub level: NoticeLevel,
    pub title: String,
    pub body: Option<String>,
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
            "A newer wsxd will be used after the current daemon stops".into(),
        )),
        runtime::Availability::NewerDaemon { daemon_version }
            if daemon_version == runtime::WSX_VERSION =>
        {
            Some((
                NoticeLevel::Warning,
                format!(
                    "A newer wsx {daemon_version} build owns wsxd; this TUI will keep using it"
                ),
            ))
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
                "wsxd upgraded from {previous_version} to {}; idle sessions restored",
                runtime::WSX_VERSION
            ),
        )),
        runtime::Availability::ReplacementDeferred {
            daemon_version,
            target_version,
            live_runtimes,
            blockers,
        } => {
            let mut reasons = Vec::new();
            if blockers.contains(&runtime::ReplacementBlocker::OtherTui) {
                reasons.push("older or different wsx TUI instances exit");
            }
            if blockers.contains(&runtime::ReplacementBlocker::WorkingAgent) {
                reasons.push("working agents become idle");
            }
            if blockers.contains(&runtime::ReplacementBlocker::PendingTarget) {
                reasons.push("the existing queued replacement is resolved");
            }
            if blockers.contains(&runtime::ReplacementBlocker::LegacyDaemon) {
                reasons.push("the older daemon reaches its safe replacement boundary");
            }
            if reasons.is_empty() {
                reasons.push("the daemon reaches its safe replacement boundary");
            }
            Some((
                NoticeLevel::Warning,
                format!(
                    "wsxd {daemon_version} will upgrade to {target_version} after {}; {live_runtimes} terminal runtime(s) remain open",
                    reasons.join(" and ")
                ),
            ))
        }
    }
}

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
    pub mode: Mode,
    pub config: GlobalConfig,
    pub active_group: GroupKey,
    pub group_header_scroll: usize,
    pub group_header_area: Rect,
    visible_projects: HashSet<usize>,
    freshened_projects: HashSet<PathBuf>,
    pub notice: Option<Notice>,
    notice_started: Option<Instant>,
    pub jobs: Vec<BgJob>,
    pub spinner_frame: usize,
    bg_tx: mpsc::Sender<BgResult>,
    bg_rx: mpsc::Receiver<BgResult>,
    needs_redraw: bool,
    should_quit: bool,
    force_terminal_redraw: bool,
    force_preview_redraw: bool,
    /// Tracks whether the last successful desktop frame contained captured terminal content.
    last_rendered_preview_was_session: bool,
    fast_timer: Timer,
    git_sweep_timer: Timer,
    slow_timer: Timer,
    cached_flat: Vec<FlatEntry>,
    flat_dirty: bool,
    /// Lowercase searchable text parallel to cached_flat; rebuilt with the flat cache.
    search_cache: Vec<String>,
    git_local_tx: mpsc::Sender<(PathBuf, Option<GitInfo>)>,
    git_local_rx: mpsc::Receiver<(PathBuf, Option<GitInfo>)>,
    git_local_pending: HashSet<PathBuf>,
    fetch_tx: mpsc::Sender<(PathBuf, FetchOutcome)>,
    fetch_rx: mpsc::Receiver<(PathBuf, FetchOutcome)>,
    fetch_pending: HashSet<PathBuf>,
    /// true when workspace state has changed since the last cache write.
    cache_dirty: bool,
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
    startup_cursor_identity: Option<wsx_core::cache::CursorIdentity>,
    /// An event requested a session refresh while one was in-flight.
    runtime_refresh_stale: bool,
    /// A full Git plus Runtime refresh takes priority over queued event refreshes.
    runtime_full_refresh_stale: bool,
    runtime_capture_pending: bool,
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
    suspend_detector: SuspendDetector,
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
    pending_integration_prompt: Vec<wsx_core::integration::IntegrationTarget>,
    integration_prompt_version: Option<String>,
    persist_group_selection: bool,
}

impl App {
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
            cached_muted,
            acknowledged_outcomes,
            integration_prompt_version,
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
        let (runtime_monitor, runtime_event_rx) =
            runtime::EventMonitor::start(runtime_client.clone())?;
        let (update_tx, update_rx) = mpsc::channel::<String>();
        let (routine_tx, routine_rx) = mpsc::channel();
        let (integration_scan_tx, integration_scan_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = wsx_core::integration::scan_needing_install().map_err(Into::into);
            let _ = integration_scan_tx.send(result);
        });
        std::thread::spawn(move || {
            if let Some(v) = crate::update::fetch_latest_version() {
                let _ = update_tx.send(v);
            }
        });
        let worktree_index = build_worktree_index(&workspace);
        let search_cache = build_search_cache(&workspace, &cached_flat);
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
            mode: Mode::Workspace,
            config,
            active_group,
            group_header_scroll,
            group_header_area: Rect::default(),
            visible_projects,
            freshened_projects: HashSet::new(),
            notice: initial_notice.clone().map(|title| Notice {
                level: NoticeLevel::Warning,
                title,
                body: None,
            }),
            notice_started: initial_notice.as_ref().map(|_| Instant::now()),
            jobs: vec![],
            spinner_frame: 0,
            bg_tx,
            bg_rx,
            needs_redraw: true,
            should_quit: false,
            force_terminal_redraw: false,
            force_preview_redraw: false,
            last_rendered_preview_was_session: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            git_sweep_timer: Timer::new(GIT_SWEEP_INTERVAL_MS),
            slow_timer: Timer::new(SLOW_INTERVAL_MS + (std::process::id() % 500) as u64),
            cached_flat,
            flat_dirty: false,
            search_cache,
            git_local_tx,
            git_local_rx,
            git_local_pending: HashSet::new(),
            fetch_tx,
            fetch_rx,
            fetch_pending: HashSet::new(),
            cache_dirty: false,
            worktree_index,
            git_semaphore: GitSemaphore::new(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            ),
            runtime_client,
            _runtime_monitor: Some(runtime_monitor),
            runtime_event_rx,
            runtime_health: RuntimeHealth::Connecting,
            runtime_tx,
            runtime_rx,
            runtime_refresh_pending: false,
            startup_cursor_identity: cursor_identity.clone(),
            runtime_refresh_stale: false,
            runtime_full_refresh_stale: false,
            runtime_capture_pending: false,
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
            suspend_detector: SuspendDetector::new(),
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
            pending_integration_prompt: Vec::new(),
            integration_prompt_version,
            persist_group_selection: true,
        };
        if let Some(identity) = cursor_identity.as_ref() {
            if let Some(index) =
                wsx_core::cache::find_cursor_index(&app.workspace, app.flat(), identity)
            {
                app.tree_selected = index;
            }
        }
        app.spawn_runtime_refresh();
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
        let message = msg.into();
        let mut lines = message.lines();
        let title = lines.next().unwrap_or_default().to_string();
        let body = {
            let rest = lines.collect::<Vec<_>>().join("\n");
            (!rest.is_empty()).then_some(rest)
        };
        self.notice = Some(Notice { level, title, body });
        self.notice_started = Some(Instant::now());
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
        self.jobs.push(BgJob {
            label: label.clone(),
        });
        self.needs_redraw = true;
        let tx = self.bg_tx.clone();
        std::thread::spawn(move || {
            let outcome = f();
            let _ = tx.send(BgResult { label, outcome });
        });
    }

    fn apply_bg_result(&mut self, result: BgResult) {
        self.jobs.retain(|j| j.label != result.label);
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
            Ok(BgOutcome::WorktreeCreated { label }) => {
                self.spawn_runtime_refresh();
                self.set_status(label);
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
            Ok(BgOutcome::IntegrationsInstalled { labels, failures }) => {
                if failures.is_empty() {
                    self.integration_prompt_version = Some(current_integration_prompt_version());
                    self.mark_dirty();
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
            self.search_cache = build_search_cache(&self.workspace, &self.cached_flat);
            self.flat_dirty = false;
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

    // ^ [[wsx UI Patterns]] Staleness can only collapse an expanded project.
    // Explicit expansion clears stale presentation for this process; activity never opens a row.
    fn collapse_stale_projects(&mut self) {
        let Some(window_ms) = self.config.auto_collapse_window_ms() else {
            return;
        };
        let now_unix_ms = unix_time_millis();
        for project in &mut self.workspace.projects {
            if !project.expanded {
                continue;
            }
            if project_is_stale(project, &self.freshened_projects, now_unix_ms, window_ms) {
                project.expanded = false;
            }
        }
    }

    pub(crate) fn stale_project_indices(&self) -> HashSet<usize> {
        let Some(window_ms) = self.config.auto_collapse_window_ms() else {
            return HashSet::new();
        };
        let now_unix_ms = unix_time_millis();
        self.workspace
            .projects
            .iter()
            .enumerate()
            .filter_map(|(index, project)| {
                project_is_stale(project, &self.freshened_projects, now_unix_ms, window_ms)
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

    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        // Kick off async git_info for all worktrees; render immediately with cached data.
        // Results arrive via drain_async_results() in the main loop without blocking.
        self.spawn_git_local_for_all();
        loop {
            if self.suspend_detector.resumed() {
                self.begin_resume_boundary();
            }
            // Drain async results every iteration so they're never blocked by UI events.
            self.drain_async_results();
            self.reconnect_terminal_after_resume(terminal);

            if self.needs_redraw {
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
                Mode::Workspace => EventMode::Workspace,
                Mode::Terminal { .. } => EventMode::Terminal,
                Mode::Input { .. }
                | Mode::Search { .. }
                | Mode::GroupManager { .. }
                | Mode::GlobalSettings { .. }
                | Mode::RoutinePresetPicker { .. }
                | Mode::RoutineEditor { .. } => EventMode::Input,
                _ => EventMode::Normal,
            };
            let terminal_prefix_was_pending =
                event_mode == EventMode::Terminal && self.terminal_escape_chord.is_pending();
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
            if terminal_prefix_was_pending
                != (event_mode == EventMode::Terminal && self.terminal_escape_chord.is_pending())
            {
                self.needs_redraw = true;
            }
            if let Some(action) = action {
                if action == Action::Quit && matches!(self.mode, Mode::Workspace) {
                    break;
                }
                if action != Action::None && action_needs_immediate_redraw(&action) {
                    self.needs_redraw = true;
                }
                if let Err(e) = self.dispatch(action, terminal) {
                    self.set_error(format!("Action failed: {e}"));
                }
                if self.should_quit {
                    break;
                }
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
                runtime::TerminalServerMessage::Error(error) => {
                    self.clear_active_terminal_selection();
                    self.terminal_stream = None;
                    self.pending_terminal_entry = None;
                    self.mode = Mode::Workspace;
                    self.set_error(format!(
                        "Terminal stream: {}: {}",
                        error.code, error.message
                    ));
                    break;
                }
                runtime::TerminalServerMessage::Exited => {
                    self.clear_active_terminal_selection();
                    self.terminal_stream = None;
                    self.pending_terminal_entry = None;
                    self.mode = Mode::Workspace;
                    self.set_warning("Terminal process exited");
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
        self.show_integration_prompt_if_ready();
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
            self.set_warning("Terminal closed while the system was suspended");
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
            true,
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
                self.set_error(format!("Terminal resume failed: {error}"));
            }
        }
    }

    fn apply_integration_scan(
        &mut self,
        result: Result<Vec<wsx_core::integration::IntegrationMetadata>>,
    ) {
        if self.integration_prompt_version.as_deref()
            == Some(current_integration_prompt_version().as_str())
        {
            return;
        }
        match result {
            Ok(metadata) => {
                self.pending_integration_prompt =
                    metadata.into_iter().map(|item| item.target).collect();
            }
            Err(error) => self.set_warning(format!("Agent integration scan failed: {error}")),
        }
    }

    fn show_integration_prompt_if_ready(&mut self) {
        if !matches!(self.mode, Mode::Workspace)
            || self.is_busy()
            || self.pending_integration_prompt.is_empty()
        {
            return;
        }
        let targets = std::mem::take(&mut self.pending_integration_prompt);
        let labels = integration_prompt_label(&targets);
        self.mode = Mode::Confirm {
            message: format!(
                "Install status integrations for detected agents: {labels}? This updates their user configuration."
            ),
            pending: PendingAction::InstallIntegrations { targets },
        };
        self.needs_redraw = true;
    }

    fn apply_runtime_event(&mut self, signal: runtime::EventSignal) {
        match signal {
            runtime::EventSignal::Dirty => self.spawn_runtime_session_refresh(),
            runtime::EventSignal::Connected => {
                // A new connection may belong to a different daemon epoch.
                self.spawn_runtime_session_refresh();
                self.needs_redraw = true;
            }
            runtime::EventSignal::Disconnected(error) => {
                let last_success = match self.runtime_health {
                    RuntimeHealth::Healthy { last_success } => Some(last_success),
                    RuntimeHealth::Reconnecting { last_success, .. } => last_success,
                    RuntimeHealth::Connecting => None,
                };
                self.runtime_health = RuntimeHealth::Reconnecting {
                    last_success,
                    error,
                };
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
                let project = &mut self.workspace.projects[project_idx];
                project.routine_revision = revision;
                project.routines = routines;
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
                    form: _,
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
        self.expire_notice(Instant::now());

        if self.slow_timer.ready() {
            // Staleness is wall-clock based and can change without a runtime event.
            self.collapse_stale_projects();
            self.recompute_visible();
            self.spawn_background_runtime_refresh();
            self.spawn_routine_refresh();
        }

        if self.git_sweep_timer.ready() {
            self.spawn_git_local_for_all();
        }

        if self.fast_timer.ready() {
            self.spawn_runtime_capture();
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
        let timeout = Duration::from_secs(self.config.notification_timeout_seconds);
        if self
            .notice_started
            .is_some_and(|started| now.saturating_duration_since(started) >= timeout)
        {
            self.notice = None;
            self.notice_started = None;
            self.needs_redraw = true;
        }
    }

    fn spawn_routine_refresh(&mut self) {
        let registered = match crate::cli::registered_routine_paths() {
            Ok(paths) => paths.into_iter().collect::<HashSet<_>>(),
            Err(error) => {
                self.set_status(format!("Routines unavailable: {error}"));
                return;
            }
        };
        let mut paths = Vec::new();
        let mut cleared = false;
        for project in &mut self.workspace.projects {
            if registered.contains(&project.path) {
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

    pub fn flush_cache(&mut self) {
        self.persist_state(true);
    }

    /// Single write point — both cache and session snapshot always written together.
    /// `sync=true` on quit (fsync), `sync=false` on periodic writes.
    fn persist_state(&mut self, sync: bool) {
        if let Some(error) = wsx_core::cache::save_cache(
            &self.workspace,
            self.tree_selected,
            self.flat(),
            self.integration_prompt_version.as_deref(),
            sync,
        ) {
            self.set_error(error);
            self.cache_dirty = true;
        } else {
            self.cache_dirty = false;
        }
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
            self.persist_state(false);
        }
    }

    fn filter_pending_deletions(
        &mut self,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
        filter_pending_deletions(&mut self.pending_deletions, worktrees)
    }

    fn spawn_runtime_refresh(&mut self) {
        if self.runtime_refresh_pending {
            self.runtime_full_refresh_stale = true;
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FullRefresh(collect_runtime_refresh(
                &config, false,
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
                &config, true,
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
            Ok((availability, snapshot, worktrees)) => {
                self.apply_runtime_snapshot(snapshot, worktrees);
                if let Some((level, message)) = runtime_availability_notice(&availability) {
                    self.set_notice(level, message);
                }
            }
            Err(error) => {
                self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()))
            }
        }
        self.spawn_queued_runtime_refresh();
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
        self.clamp_selected();
        self.cancel_pending_terminal_entry_if_stale();
        let recovered = matches!(self.runtime_health, RuntimeHealth::Reconnecting { .. });
        self.runtime_health = RuntimeHealth::Healthy {
            last_success: Instant::now(),
        };
        if recovered {
            self.set_status("Runtime reconnected; workspace refreshed");
        }
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
                Ok(runtime::Response::Error(error)) => {
                    Err(anyhow::anyhow!("{}: {}", error.code, error.message))
                }
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
        hints.extend([
            hint(&workspace.to_ascii_lowercase(), "workspace"),
            hint("j/k/↑↓", "session"),
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

    pub(crate) fn effective_terminal_sidebar(&self) -> TerminalSidebar {
        self.terminal_sidebar_override
            .unwrap_or(self.config.terminal_sidebar)
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

    fn clamp_selected(&mut self) {
        let len = self.flat().len();
        if len == 0 {
            self.tree_selected = 0;
        } else {
            self.tree_selected = self.tree_selected.min(len - 1);
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    fn manually_expand_project(&mut self, project_idx: usize) {
        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
            project.expanded = true;
            self.freshened_projects.insert(project.path.clone());
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
                self.workspace.projects[idx].expanded = false;
                self.rebuild_flat();
                self.clamp_selected();
            }
            Some(FlatEntry::Worktree { project_idx: pi, worktree_idx: wi }) => {
                if self.workspace.projects[pi].worktrees[wi].expanded {
                    self.workspace.projects[pi].worktrees[wi].expanded = false;
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
                    self.workspace.projects[pi].routines_expanded = false;
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
                    self.workspace.projects[pi].worktrees[wi].expanded = true;
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
                    self.workspace.projects[pi].routines_expanded = true;
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
        if matches!(self.mode, Mode::RoutinePresetPicker { .. }) {
            return self.dispatch_routine_preset_picker(action);
        }
        if matches!(self.mode, Mode::RoutineEditor { .. }) {
            return self.dispatch_routine_editor(action);
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
            | Mode::Move { .. }
            | Mode::MoveSession { .. }
            | Mode::GroupManager { .. } => unreachable!(),
            Mode::RoutinePresetPicker { .. }
            | Mode::RoutineEditor { .. }
            | Mode::RoutineDetail { .. } => unreachable!(),
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
            Action::NextSession => self.action_switch_sibling_session(1, terminal)?,
            Action::PrevSession => self.action_switch_sibling_session(-1, terminal)?,
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
                self.terminal_sidebar_override = Some(match self.effective_terminal_sidebar() {
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
            self.set_error("Terminal stream is unavailable");
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

    fn send_terminal_bytes_once(&self, pane_id: runtime::PaneId, bytes: Vec<u8>) -> Result<()> {
        // ^ A one-shot Workspace action must never share identity with a live or
        // reconnecting Terminal stream, whose lease has a separate incarnation.
        let client_id = runtime::new_client_id();
        match self
            .runtime_client
            .call(&runtime::Request::TerminalAcquire {
                pane_id,
                client_id,
                takeover: false,
            })? {
            runtime::Response::Ack { .. } => {}
            runtime::Response::Error(error) => anyhow::bail!("{}: {}", error.code, error.message),
            _ => anyhow::bail!("unexpected terminal lease response"),
        }
        let result = match self.runtime_client.call(&runtime::Request::TerminalInput {
            pane_id,
            client_id,
            bytes,
        })? {
            runtime::Response::Ack { .. } => Ok(()),
            runtime::Response::Error(error) => {
                Err(anyhow::anyhow!("{}: {}", error.code, error.message))
            }
            _ => Err(anyhow::anyhow!("unexpected terminal input response")),
        };
        let _ = self
            .runtime_client
            .call(&runtime::Request::TerminalRelease { pane_id, client_id });
        result
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
            }
            Action::GroupNext => self.action_group_next(),
            Action::GroupPrev => self.action_group_prev(),
            Action::GroupManager => self.action_group_manager(),
            Action::MouseClick { col, row } => {
                self.handle_mouse_click(col, row, terminal, false)?
            }
            Action::MouseScroll { col, row, delta } => {
                self.handle_workspace_mouse_scroll(col, row, delta)
            }
            _ => {}
        }
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
            Action::InputChar(c) => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.insert_char(c);
                }
            }
            Action::InputBackspace => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.backspace();
                }
            }
            Action::NavigateLeft => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.cursor_left();
                }
            }
            Action::NavigateRight => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.cursor_right();
                }
            }
            Action::InputTab => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.select_next();
                }
            }
            Action::InputBackTab => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.select_prev();
                }
            }
            Action::NavigateDown => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.select_next();
                }
            }
            Action::NavigateUp => {
                if let Mode::Input { state, .. } = &mut self.mode {
                    state.select_prev();
                }
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
                    } => (Some(group_idx + 1), false),
                    Mode::Confirm {
                        pending: PendingAction::InstallIntegrations { .. },
                        ..
                    } => (None, true),
                    _ => (None, false),
                };
                self.mode =
                    group_selection.map_or(Mode::Workspace, |selected| Mode::GroupManager {
                        selected,
                        scroll: 0,
                        purpose: GroupManagerPurpose::Switch,
                    });
                if dismissed_integrations {
                    self.integration_prompt_version = Some(current_integration_prompt_version());
                    self.mark_dirty();
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
        match action {
            Action::InputEscape | Action::Quit => self.mode = Mode::Workspace,
            Action::InputChar(c) => {
                if let Mode::RoutineEditor {
                    form, can_rename, ..
                } = &mut self.mode
                {
                    if *can_rename || form.field != 0 {
                        form.insert(c);
                    }
                }
            }
            Action::InputBackspace => {
                if let Mode::RoutineEditor {
                    form, can_rename, ..
                } = &mut self.mode
                {
                    if *can_rename || form.field != 0 {
                        form.backspace();
                    }
                }
            }
            Action::NavigateLeft => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.left();
                }
            }
            Action::NavigateRight => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.right();
                }
            }
            Action::InputTab | Action::NavigateDown => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.next(false);
                }
            }
            Action::InputBackTab | Action::NavigateUp => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.next(true);
                }
            }
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
        let project = &self.workspace.projects[project_idx];
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
        search_matches_in(&self.search_cache, query)
    }

    fn search_apply(&mut self) {
        let query = match &self.mode {
            Mode::Search { query, .. } => query.clone(),
            _ => return,
        };
        let matches = self.search_matches(&query);
        if matches.is_empty() {
            return;
        }
        self.tree_selected = matches[0];
        self.update_scroll();
    }

    /// Enter: cycle to next match. Exits search when wrapping back to start.
    fn search_advance(&mut self) {
        let (query, match_idx) = match &self.mode {
            Mode::Search { query, match_idx } => (query.clone(), *match_idx),
            _ => return,
        };
        let matches = self.search_matches(&query);
        if matches.is_empty() {
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
                    self.workspace.projects[pi].expanded = false;
                } else {
                    self.manually_expand_project(pi);
                }
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::Worktree(pi, wi) => {
                self.workspace.projects[pi].worktrees[wi].expanded =
                    !self.workspace.projects[pi].worktrees[wi].expanded;
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::RoutinesHeader(pi) => {
                self.workspace.projects[pi].routines_expanded =
                    !self.workspace.projects[pi].routines_expanded;
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
            }
        }
        if let Some((terminal_id, revision)) = acknowledged.as_ref() {
            self.acknowledged_outcomes
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
                self.set_error(format!("{}: {}", error.code, error.message));
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
        self.terminal_stream = None;
        self.unmute_on_interaction(pane_id);
        let (rows, cols) = self.terminal_pane_size(terminal);
        match runtime::TerminalStream::connect(
            &self.runtime_client,
            pane_id,
            self.terminal_controller_id,
            false,
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

    fn leave_terminal_mode(&mut self, _pane_id: runtime::PaneId) {
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
        let viewport =
            crate::ui::layout::terminal_viewport(area, mobile, self.effective_terminal_sidebar());
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
        self.mode = Mode::Input {
            context: InputContext::AddWorktree { project_idx: pi },
            state: InputState::new("branch: "),
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
                self.set_error(format!("{}: {}", error.code, error.message));
            }
            _ => self.set_error("Unexpected pane split response"),
        }
        Ok(())
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
        let accepts_text = matches!(
            &self.mode,
            Mode::GlobalSettings { form } if form.accepts_text()
        );
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
                let result = match &mut self.mode {
                    Mode::GlobalSettings { form } => form.begin_or_commit(),
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    self.set_error(error);
                }
            }
            Action::NavigateUp | Action::InputBackTab => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.next_field(true);
                }
            }
            Action::NavigateDown | Action::InputTab => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.next_field(false);
                }
            }
            Action::NavigateLeft => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.left();
                }
            }
            Action::NavigateRight => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.right();
                }
            }
            Action::InputBackspace => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.backspace();
                }
            }
            Action::InputChar(character) if accepts_text => {
                if let Mode::GlobalSettings { form } = &mut self.mode {
                    form.insert(character);
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
        Ok(())
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

    fn active_candidates(&self) -> Vec<usize> {
        self.flat()
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let FlatEntry::Session {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                } = entry
                else {
                    return None;
                };
                let sess = self.workspace.session(*pi, *wi, *si)?;
                if session_state::derive(sess).app_state() == AppSessionState::Active {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn idle_candidates(&self) -> Vec<usize> {
        self.flat()
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let FlatEntry::Session {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                } = entry
                else {
                    return None;
                };
                let sess = self.workspace.session(*pi, *wi, *si)?;
                if sess.agent.is_some()
                    && session_state::derive(sess).app_state() == AppSessionState::Idle
                {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
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
        self.flat()
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let FlatEntry::Session {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                } = entry
                else {
                    return None;
                };
                let sess = self.workspace.session(*pi, *wi, *si)?;
                if session_state::derive(sess).app_state() == AppSessionState::NeedsAttention {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn attention_target(&self, dir: isize) -> Option<usize> {
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

    fn action_switch_sibling_session(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let (project_idx, worktree_idx, session_idx) = match self.current_selection() {
            Selection::Session(pi, wi, si) | Selection::Pane(pi, wi, si, _) => (pi, wi, si),
            _ => {
                self.set_status("No session selected");
                return Ok(());
            }
        };
        let count = self.workspace.projects[project_idx].worktrees[worktree_idx]
            .sessions
            .len();
        if count <= 1 {
            self.set_status("No other sessions in this worktree");
            return Ok(());
        }
        let Some(target_session) = cyclic_sibling_index(session_idx, count, dir) else {
            self.set_status("No other sessions in this worktree");
            return Ok(());
        };
        let target = self.flat().iter().position(|entry| {
            matches!(
                entry,
                FlatEntry::Session {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                } if *pi == project_idx && *wi == worktree_idx && *si == target_session
            )
        });
        let Some(target) = target else {
            self.set_status("Sibling session is not visible");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
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
                    self.muted_terminal_ids.insert(terminal_id);
                } else {
                    self.muted_terminal_ids.remove(&terminal_id);
                }
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
                            self.set_error(format!("{}: {}", error.code, error.message));
                        }
                        _ => self.set_error("Unexpected pane close response"),
                    }
                }
                PendingAction::DeleteWorktree {
                    project_idx: pi,
                    worktree_idx: wi,
                } => {
                    let (repo, wt_path, branch) = {
                        let p = &self.workspace.projects[pi];
                        let wt = &p.worktrees[wi];
                        (p.path.clone(), wt.path.clone(), wt.branch.clone())
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
                    let (repo_path, default_branch, proj_config) = {
                        let p = &self.workspace.projects[pi];
                        (
                            p.path.clone(),
                            p.default_branch.clone(),
                            p.config.clone().unwrap_or_default(),
                        )
                    };
                    let label = format!("Created worktree: {}", branch);
                    self.spawn_bg(format!("create {}", branch), move || {
                        ops::create_worktree(&repo_path, &default_branch, &proj_config, &branch)?;
                        Ok(BgOutcome::WorktreeCreated { label })
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
                    self.persist_state(true);
                    self.runtime_client.shutdown()?;
                    self.should_quit = true;
                }
                PendingAction::InstallIntegrations { targets } => {
                    self.spawn_bg("install agent integrations", move || {
                        let mut labels = Vec::new();
                        let mut failures = Vec::new();
                        for target in targets {
                            match wsx_core::integration::install(target) {
                                Ok(_) => labels.push(target.label()),
                                Err(error) => failures.push(format!("{}: {error}", target.label())),
                            }
                        }
                        Ok(BgOutcome::IntegrationsInstalled { labels, failures })
                    });
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
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug(&p.name))
        };
        let explicit_name = if session_label.is_empty() {
            None
        } else {
            Some(session_label)
        };
        let (_pane_id, display_name) =
            ops::create_session(&proj_name, &wt_slug, &wt_path, explicit_name, command)?;
        self.set_status(format!("Session '{}' created", display_name));
        // Expand before the authoritative Runtime refresh reveals the new pane.
        if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
            wt.expanded = true;
        }
        self.spawn_runtime_refresh();
        Ok(())
    }

    fn do_delete_project(&mut self, pi: usize) -> Result<()> {
        let (name, path) = {
            let p = &self.workspace.projects[pi];
            (p.name.clone(), p.path.clone())
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
        let session = &self.workspace.projects[pi].worktrees[wi].sessions[si];
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
        let branch = self.workspace.projects[pi].worktrees[wi].branch.clone();
        let proj_path = self.workspace.projects[pi].path.clone();

        ops::set_alias(&mut self.config, &proj_path, &branch, &alias);
        self.config.save()?;

        let wt = &mut self.workspace.projects[pi].worktrees[wi];
        wt.alias = (!alias.is_empty()).then(|| alias.clone());

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
        let session = &self.workspace.projects[pi].worktrees[wi].sessions[si];
        let session_id = session.session_id;
        let pane_id = session.pane_id;
        self.unmute_on_interaction(pane_id);
        ops::rename_session(session_id, &new_name)?;
        self.workspace.projects[pi].worktrees[wi].sessions[si].display_name = new_name.clone();
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
            Selection::Session(pi, wi, si) => {
                self.mode = Mode::MoveSession {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                };
            }
            _ => self.set_status("Select a project or session to move"),
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

    fn move_session(&mut self, pi: usize, wi: usize, si: usize, delta: isize) -> Result<()> {
        let new_si = si as isize + delta;
        if new_si < 0 {
            return Ok(());
        }
        let new_si = new_si as usize;
        let sessions = &mut self.workspace.projects[pi].worktrees[wi].sessions;
        if new_si >= sessions.len() {
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

fn search_text_for(workspace: &WorkspaceState, entry: &FlatEntry) -> String {
    match entry {
        FlatEntry::Project { idx } => workspace.projects[*idx].name.to_lowercase(),
        FlatEntry::Worktree {
            project_idx: pi,
            worktree_idx: wi,
        } => {
            let wt = &workspace.projects[*pi].worktrees[*wi];
            let alias = wt.alias.as_deref().unwrap_or("");
            format!("{} {} {}", wt.branch, alias, wt.name).to_lowercase()
        }
        FlatEntry::Session {
            project_idx: pi,
            worktree_idx: wi,
            session_idx: si,
        } => workspace.projects[*pi].worktrees[*wi].sessions[*si]
            .display_name
            .to_lowercase(),
        FlatEntry::Pane {
            project_idx: pi,
            worktree_idx: wi,
            session_idx: si,
            pane_idx,
        } => {
            let pane = &workspace.projects[*pi].worktrees[*wi].sessions[*si].panes[*pane_idx];
            format!(
                "{} {}",
                pane.label,
                pane.agent.as_deref().unwrap_or("terminal")
            )
            .to_lowercase()
        }
        FlatEntry::RoutinesHeader { project_idx } => {
            format!("{} routines", workspace.projects[*project_idx].name).to_lowercase()
        }
        FlatEntry::Routine {
            project_idx,
            routine_idx,
        } => {
            let routine = &workspace.projects[*project_idx].routines[*routine_idx].routine;
            let trigger = format!("{:?}", routine.trigger);
            format!(
                "{} {} {} {}",
                routine.name,
                trigger,
                routine.command.join(" "),
                routine.prompt
            )
            .to_lowercase()
        }
    }
}

#[cfg(test)]
fn session_needs_attention(sess: &wsx_core::model::workspace::SessionInfo) -> bool {
    session_state::derive(sess).app_state() == AppSessionState::NeedsAttention
}

fn search_matches_in(cache: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return vec![];
    }
    let q = query.to_lowercase();
    cache
        .iter()
        .enumerate()
        .filter(|(_, text)| text.contains(&q))
        .map(|(i, _)| i)
        .collect()
}

fn build_search_cache(workspace: &WorkspaceState, flat: &[FlatEntry]) -> Vec<String> {
    flat.iter().map(|e| search_text_for(workspace, e)).collect()
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
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};

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
    fn search_matches_empty_query_returns_nothing() {
        let cache = vec!["main".to_string(), "feat/foo".to_string()];
        assert!(search_matches_in(&cache, "").is_empty());
    }

    #[test]
    fn search_matches_case_insensitive() {
        // cache is always pre-lowercased by build_search_cache; query is lowercased at match time
        let cache = vec!["main".to_string(), "feature".to_string(), "fix".to_string()];
        let hits = search_matches_in(&cache, "FEAT");
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn search_matches_multiple_hits() {
        let cache = vec![
            "feat/a".to_string(),
            "other".to_string(),
            "feat/b".to_string(),
        ];
        let hits = search_matches_in(&cache, "feat");
        assert_eq!(hits, vec![0, 2]);
    }

    #[test]
    fn search_matches_no_match_returns_empty() {
        let cache = vec!["main".to_string(), "fix".to_string()];
        assert!(search_matches_in(&cache, "xyz").is_empty());
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

    fn terminal_stream_listener(name: &str) -> (PathBuf, UnixListener) {
        let directory = std::env::current_dir().unwrap().join(".work/s");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "{name}-{}-{}.sock",
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

    fn make_test_app(
        config: GlobalConfig,
        workspace: WorkspaceState,
        active_group: Option<String>,
    ) -> App {
        let active_group = active_group
            .map(GroupKey::Named)
            .unwrap_or(GroupKey::Ungrouped);
        let visible_projects = compute_visible_projects(&config, &workspace, Some(&active_group));
        let group_header_scroll = config
            .ordered_group_keys()
            .iter()
            .position(|candidate| candidate == &active_group)
            .unwrap_or(0);
        let cached_flat = flatten_tree_filtered(&workspace, &visible_projects);
        let search_cache = build_search_cache(&workspace, &cached_flat);
        let worktree_index = build_worktree_index(&workspace);
        let (bg_tx, bg_rx) = std::sync::mpsc::channel();
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
            mode: Mode::Workspace,
            config,
            active_group,
            group_header_scroll,
            group_header_area: Rect::default(),
            visible_projects,
            freshened_projects: HashSet::new(),
            notice: None,
            notice_started: None,
            jobs: Vec::new(),
            spinner_frame: 0,
            bg_tx,
            bg_rx,
            needs_redraw: false,
            should_quit: false,
            force_terminal_redraw: false,
            force_preview_redraw: false,
            last_rendered_preview_was_session: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            git_sweep_timer: Timer::new(GIT_SWEEP_INTERVAL_MS),
            slow_timer: Timer::new(SLOW_INTERVAL_MS),
            cached_flat,
            flat_dirty: false,
            search_cache,
            git_local_tx,
            git_local_rx,
            git_local_pending: HashSet::new(),
            fetch_tx,
            fetch_rx,
            fetch_pending: HashSet::new(),
            cache_dirty: false,
            git_semaphore: GitSemaphore::new(1),
            worktree_index,
            runtime_client: test_runtime_client(),
            _runtime_monitor: None,
            runtime_event_rx,
            runtime_health: RuntimeHealth::Connecting,
            runtime_tx,
            runtime_rx,
            runtime_refresh_pending: false,
            startup_cursor_identity: None,
            runtime_refresh_stale: false,
            runtime_full_refresh_stale: false,
            runtime_capture_pending: false,
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
            suspend_detector: SuspendDetector::new(),
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
            pending_integration_prompt: Vec::new(),
            integration_prompt_version: None,
            persist_group_selection: false,
        }
    }

    fn make_project_entry(
        name: &str,
        group: Option<&str>,
    ) -> wsx_core::config::global::ProjectEntry {
        wsx_core::config::global::ProjectEntry {
            name: name.to_string(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            groups: group.into_iter().map(str::to_string).collect(),
            aliases: Default::default(),
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
            compute_visible_projects(&config, &workspace, Some(&active)),
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

        let terminal = workspace_terminal();
        app.enter_terminal(pane_id, terminal_id, "baseline entry".into(), &terminal)
            .unwrap();
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
        let baseline_deadline = Instant::now() + Duration::from_secs(1);
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
            let expanded = read_terminal_client_message(&mut reader);
            let compact = read_terminal_client_message(&mut reader);
            drop(listener);
            let _ = std::fs::remove_file(server_path);
            ((rows, cols), expanded, compact)
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

        app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
            .unwrap();
        assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Expanded);
        app.dispatch(Action::ToggleTerminalSidebar, &mut terminal)
            .unwrap();
        assert_eq!(app.effective_terminal_sidebar(), TerminalSidebar::Compact);
        assert_eq!(app.config.terminal_sidebar, TerminalSidebar::Compact);

        let (initial, expanded, compact) = server.join().unwrap();
        app.terminal_stream = None;
        assert_eq!(
            expanded,
            runtime::TerminalClientMessage::Resize {
                rows: initial.0,
                cols: initial.1.saturating_sub(30),
            }
        );
        assert_eq!(
            compact,
            runtime::TerminalClientMessage::Resize {
                rows: initial.0,
                cols: initial.1,
            }
        );
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

        app.dispatch(Action::PrevActive, &mut terminal).unwrap();
        assert!(matches!(app.mode, Mode::Terminal { .. }));
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
            std::thread::sleep(Duration::from_millis(20));
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
    fn integration_prompt_label_bounds_long_target_lists() {
        assert_eq!(
            integration_prompt_label(&[
                wsx_core::integration::IntegrationTarget::Pi,
                wsx_core::integration::IntegrationTarget::Claude,
                wsx_core::integration::IntegrationTarget::Codex,
                wsx_core::integration::IntegrationTarget::Kimi,
                wsx_core::integration::IntegrationTarget::Opencode,
            ]),
            "Pi, Claude Code, Codex, and 2 more"
        );
    }

    #[test]
    fn startup_integration_prompt_lists_detected_missing_targets() {
        let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
        app.pending_integration_prompt = vec![
            wsx_core::integration::IntegrationTarget::Pi,
            wsx_core::integration::IntegrationTarget::Claude,
        ];

        app.show_integration_prompt_if_ready();

        assert!(matches!(
            &app.mode,
            Mode::Confirm {
                message,
                pending: PendingAction::InstallIntegrations { targets },
            } if message.contains("Pi")
                && message.contains("Claude Code")
                && targets.len() == 2
        ));
    }

    #[test]
    fn current_prompt_version_suppresses_and_legacy_app_version_does_not() {
        let mut app = make_test_app(GlobalConfig::default(), WorkspaceState::empty(), None);
        app.integration_prompt_version = Some(current_integration_prompt_version());
        let metadata = wsx_core::integration::IntegrationMetadata {
            target: wsx_core::integration::IntegrationTarget::Pi,
            cli_value: "pi",
            label: "Pi",
            lifecycle: wsx_core::integration::LifecycleCapability::Authoritative,
            available: true,
            install_status: wsx_core::integration::InstallStatus::Missing,
            installed_version: None,
            expected_version: 8,
        };

        app.apply_integration_scan(Ok(vec![metadata.clone()]));

        assert!(app.pending_integration_prompt.is_empty());
        assert!(matches!(app.mode, Mode::Workspace));

        app.integration_prompt_version = Some(env!("CARGO_PKG_VERSION").into());
        app.apply_integration_scan(Ok(vec![metadata]));

        assert_eq!(
            app.pending_integration_prompt,
            vec![wsx_core::integration::IntegrationTarget::Pi]
        );
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
    fn protocol_mismatch_surfaces_joint_upgrade_and_restart_guidance() {
        let error = anyhow::Error::new(asched_core::routine::RoutineError::ProtocolMismatch {
            client: 3,
            daemon: 2,
        });
        assert_eq!(
            routine_error_text(&error),
            "asched protocol mismatch; upgrade wsx and asched together, then restart asched"
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
            vec!["pi", "-p", "{prompt}"]
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
            Mode::RoutinePresetPicker { selected: 3, .. }
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
    fn given_projects_with_visibility_gaps_when_moving_backward_then_returns_previous_visible_index(
    ) {
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
    fn given_multi_step_positive_delta_when_target_visible_exists_then_returns_that_visible_index()
    {
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
    fn given_multi_step_negative_delta_when_target_visible_exists_then_returns_that_visible_index()
    {
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
            auto_collapse_after_hours: 24,
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
    fn given_navigation_boundary_when_navigation_is_noop_then_selection_and_redraw_stay_unchanged()
    {
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
        let project = make_project("demo");
        let config = GlobalConfig {
            groups: vec!["work".into(), "personal".into()],
            terminal_escape_chord: "alt+g z".into(),
            projects: vec![wsx_core::config::global::ProjectEntry {
                name: "demo".into(),
                path: project.path.clone(),
                groups: vec!["work".into()],
                aliases: HashMap::new(),
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
                "(j/k/↑↓)session",
                crate::ui::IDLE_ITERATION_HINT,
                crate::ui::ACTIVE_ITERATION_HINT,
                crate::ui::ATTENTION_ITERATION_HINT,
                "(b)sidebar",
                concat!("(q)", "quit"),
            ]
        );
        for hint in ["(alt+g)commands", "(z)workspace", "(j/k/↑↓)session"] {
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
                "(j/k/↑↓)session",
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
    }

    #[test]
    fn compact_terminal_sidebar_mirrors_session_status_and_preserves_terminal_geometry() {
        let mut project = make_project("compact");
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
        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        assert_eq!(app.tree_area, Rect::new(0, 1, 2, 8));
        assert_eq!(app.preview_area, Rect::new(2, 1, 78, 8));
        assert_eq!(app.terminal_area, Rect::new(2, 2, 78, 7));
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
        for ((x, y), symbol) in [((0, 1), "┌"), ((39, 1), "┐"), ((0, 6), "└"), ((39, 6), "┘")]
        {
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
        assert!(
            footer.contains(&format!("(q)uit  ☕︎v{}", env!("CARGO_PKG_VERSION"))),
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
    fn mobile_terminal_mode_keeps_global_header_and_breadcrumb_above_viewport() {
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
        assert_eq!(app.terminal_area, Rect::new(0, 2, 56, 13));
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("workspace"), "{rendered:?}");
        assert!(rendered.contains("mobile-terminal ›"), "{rendered:?}");
        assert!(rendered.contains("hello"), "{rendered:?}");
        assert!(!rendered.contains(")sidebar"), "{rendered:?}");
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
            GlobalConfig::default(),
            WorkspaceState {
                projects: vec![project],
            },
            None,
        );
        let session_position = |app: &App, session_idx| {
            app.flat()
                .iter()
                .position(|entry| {
                    matches!(
                        entry,
                        FlatEntry::Session {
                            session_idx: si,
                            ..
                        } if *si == session_idx
                    )
                })
                .unwrap()
        };

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
            label: "kill session".into(),
        });

        app.apply_bg_result(BgResult {
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
    fn runtime_version_notices_explain_direction_and_blockers() {
        let newer = runtime_availability_notice(&runtime::Availability::NewerDaemon {
            daemon_version: "0.22.0".into(),
        })
        .unwrap()
        .1;
        assert!(newer.contains("open wsx 0.22.0"), "{newer}");

        let deferred = runtime_availability_notice(&runtime::Availability::ReplacementDeferred {
            daemon_version: "0.20.0".into(),
            target_version: "0.21.0".into(),
            live_runtimes: 4,
            blockers: vec![
                runtime::ReplacementBlocker::OtherTui,
                runtime::ReplacementBlocker::WorkingAgent,
            ],
        })
        .unwrap()
        .1;
        assert!(deferred.contains("0.20.0"), "{deferred}");
        assert!(deferred.contains("0.21.0"), "{deferred}");
        assert!(deferred.contains("older or different wsx TUI instances exit"));
        assert!(deferred.contains("working agents become idle"));
        assert!(deferred.contains("4 terminal runtime(s) remain open"));
        let lowercase = deferred.to_ascii_lowercase();
        for unsupported_claim in [
            "exact pty survives",
            "pty will survive",
            "exact process survives",
            "process will survive",
            "exact terminal buffer survives",
            "terminal buffer will survive",
        ] {
            assert!(!lowercase.contains(unsupported_claim), "{deferred}");
        }
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
            app.set_notice(level, "notice");
            app.notice_started = Some(started);
            app.expire_notice(started + Duration::from_secs(1));
            assert!(app.notice.is_some());
            app.expire_notice(started + Duration::from_secs(2));
            assert!(app.notice.is_none());
        }
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
        app.apply_runtime_event(runtime::EventSignal::Connected);
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
                capabilities: runtime::Capabilities::default(),
            },
            None,
        );
        assert!(matches!(app.runtime_health, RuntimeHealth::Healthy { .. }));
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
    fn terminal_mouse_release_outside_the_panel_is_forwarded_without_a_cell_reference() {
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
        assert!(runtime_mouse_event(drag, Rect::new(36, 1, 60, 20)).is_none());
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

    fn workspace_terminal() -> ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>
    {
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

    #[test]
    fn missing_activity_timestamps_collapse_a_stale_project() {
        let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);

        app.collapse_stale_projects();

        assert!(!app.workspace.projects[0].expanded);
    }

    #[test]
    fn positive_auto_collapse_window_collapses_expired_activity_from_both_sources() {
        let mut app = make_test_app(
            GlobalConfig {
                auto_collapse_after_hours: 1,
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
                auto_collapse_after_hours: 1,
                ..GlobalConfig::default()
            },
            projects_for_tree(1),
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
                auto_collapse_after_hours: 1,
                ..GlobalConfig::default()
            },
            projects_for_tree(1),
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
    fn future_activity_keeps_an_expanded_project_open() {
        let mut app = make_test_app(
            GlobalConfig {
                auto_collapse_after_hours: 1,
                ..GlobalConfig::default()
            },
            projects_for_tree(1),
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
                auto_collapse_after_hours: 0,
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
                auto_collapse_after_hours: 1,
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
                auto_collapse_after_hours: u64::MAX,
                ..GlobalConfig::default()
            },
            projects_for_tree(1),
            None,
        );
        app.workspace.projects[0].last_agent_active_unix_ms = Some(0);

        app.collapse_stale_projects();

        assert!(app.workspace.projects[0].expanded);
    }

    #[test]
    fn fresh_projects_keep_their_existing_expanded_state_from_either_activity_timestamp() {
        let mut agent_fresh = make_project("agent-fresh");
        agent_fresh.last_agent_active_unix_ms = Some(u64::MAX);
        agent_fresh.expanded = true;
        let mut terminal_fresh = make_project("terminal-fresh");
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
    fn manually_expanded_stale_projects_are_fresh_until_exit() {
        let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
        app.workspace.projects[0].expanded = false;
        let mut terminal = workspace_terminal();
        assert!(app.stale_project_indices().contains(&0));

        app.dispatch(Action::Select, &mut terminal).unwrap();
        app.collapse_stale_projects();
        app.collapse_stale_projects();

        assert!(app.workspace.projects[0].expanded);
        assert!(!app.stale_project_indices().contains(&0));
    }

    #[test]
    fn manually_expanded_stale_project_override_resets_after_app_reconstruction() {
        let mut app = make_test_app(GlobalConfig::default(), projects_for_tree(1), None);
        app.workspace.projects[0].expanded = false;
        let mut terminal = workspace_terminal();

        app.dispatch(Action::Select, &mut terminal).unwrap();
        app.collapse_stale_projects();
        assert!(app.workspace.projects[0].expanded);
        assert!(!app.stale_project_indices().contains(&0));

        let mut reconstructed = make_test_app(GlobalConfig::default(), app.workspace.clone(), None);
        assert!(reconstructed.stale_project_indices().contains(&0));
        reconstructed.collapse_stale_projects();

        assert!(!reconstructed.workspace.projects[0].expanded);
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
}
