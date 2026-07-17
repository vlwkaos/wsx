// App state machine and event loop.
// ref: ratatui app patterns — https://ratatui.rs/concepts/application-patterns/

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use ratatui::layout::{Position, Rect};

use crate::{
    action::Action,
    event::poll_event,
    session_state::{self, AppSessionState},
    tui::{self, Tui},
    ui::{self, ansi, input::InputState, routine_editor::RoutineForm},
};
use wsx_core::{
    config::global::GlobalConfig,
    git::{info as git_info, ops as git_ops, worktree as git_worktree},
    model::workspace::{flatten_tree_filtered, FlatEntry, GitInfo, Selection, WorkspaceState},
    ops,
    tmux::{capture, monitor, session},
};

use git_info::FetchOutcome;
use monitor::SessionStatus;

// ── Tmux async results ────────────────────────────────────────────────────────

enum TmuxResult {
    FullRefresh {
        sessions: Vec<(String, PathBuf)>,
        activity: HashMap<String, SessionStatus>,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    },
    Activity(HashMap<String, SessionStatus>),
    Capture {
        session_name: String,
        content: Option<String>,
    },
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
}

const TICK_MS: u64 = 100;
const FAST_INTERVAL_MS: u64 = 500;
const ACTIVITY_INTERVAL_MS: u64 = 1000;
const SLOW_INTERVAL_MS: u64 = 3000;
const FETCH_INTERVAL_SECS: u64 = 60;

// ── Modes ─────────────────────────────────────────────────────────────────────

pub enum Mode {
    Normal,
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
    GitPopup {
        project_idx: usize,
        worktree_idx: usize,
    },
    TabManager {
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
        session_name: String,
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
    SendCommand {
        session_name: String,
    },
    GitPullRebase {
        project_idx: usize,
        worktree_idx: usize,
    },
    GitMergeFrom {
        project_idx: usize,
        worktree_idx: usize,
    },
    GitMergeInto {
        project_idx: usize,
        worktree_idx: usize,
    },
    AddTab,
    RenameTab {
        tab_idx: usize, // index into config.tabs (0-based, not ordered_tabs)
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
            InputContext::SendCommand { .. } => "Send Command",
            InputContext::GitPullRebase { .. } => "Pull Rebase — branch",
            InputContext::GitMergeFrom { .. } => "Merge From — branch",
            InputContext::GitMergeInto { .. } => "Merge Into — branch",
            InputContext::AddTab => "New Tab",
            InputContext::RenameTab { .. } => "Rename Tab",
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
    CreateWorktree {
        project_idx: usize,
        branch: String,
    },
    DeleteTab {
        tab_idx: usize, // index into config.tabs (0-based)
    },
    DeleteRoutine {
        project_path: PathBuf,
        name: String,
        revision: u64,
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
    CleanAborted {
        wt_path: std::path::PathBuf,
        msg: String,
    },
    ProjectsCleaned {
        sessions_to_kill: Vec<String>,
        msg: String,
    },
    GitOp {
        pi: usize,
        wi: usize,
        msg: String,
    },
    WorktreeCreated {
        label: String,
    },
}

struct BgResult {
    label: String,
    outcome: Result<BgOutcome>,
}

struct RoutineRefreshResult {
    project_path: PathBuf,
    generation: u64,
    response: wsx_core::routine::ipc::Response,
}

/// Tracks user-initiated session mutations so stale refresh results don't overwrite intent.
/// Entries are filtered in apply_tmux_refresh before rebuilding workspace state.
enum SessionOp {
    Killed,
    Renamed { new_name: String },
}

pub const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Filter/remap stale tmux session data so user-initiated mutations win over in-flight refreshes.
/// - Killed: remove from sessions list; clear entry once tmux confirms it's gone.
/// - Renamed: replace old name with new name so the snapshot lookup preserves pane state.
fn filter_pending_session_ops(
    pending: &mut HashMap<String, SessionOp>,
    sessions: Vec<(String, PathBuf)>,
) -> Vec<(String, PathBuf)> {
    if pending.is_empty() {
        return sessions;
    }
    // Build live name set for cleanup; retain only unconfirmed ops
    let live_names: HashSet<&str> = sessions.iter().map(|(n, _)| n.as_str()).collect();
    pending.retain(|old_name, op| match op {
        SessionOp::Killed => live_names.contains(old_name.as_str()), // keep while still alive
        SessionOp::Renamed { new_name } => !live_names.contains(new_name.as_str()), // keep until new name appears
    });
    // Apply filters: drop killed, remap renamed
    sessions
        .into_iter()
        .filter_map(|(name, path)| match pending.get(&name) {
            Some(SessionOp::Killed) => None,
            Some(SessionOp::Renamed { new_name }) => Some((new_name.clone(), path)),
            None => Some((name, path)),
        })
        .collect()
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

pub struct App {
    pub workspace: WorkspaceState,
    pub tree_selected: usize,
    pub tree_scroll: usize,
    pub tree_visible_height: usize,
    pub tree_area: Rect,
    pub preview_area: Rect,
    pub mode: Mode,
    pub config: GlobalConfig,
    pub active_tab: Option<String>,
    visible_projects: HashSet<usize>,
    send_command_history: Vec<String>,
    pub status_message: Option<String>,
    status_message_expires: Option<Instant>,
    pub jobs: Vec<BgJob>,
    pub spinner_frame: usize,
    bg_tx: mpsc::Sender<BgResult>,
    bg_rx: mpsc::Receiver<BgResult>,
    needs_redraw: bool,
    force_redraw: bool,
    fast_timer: Timer,
    activity_timer: Timer,
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
    last_session_snapshot: HashMap<String, Vec<String>>,
    /// Limits concurrent git-info threads to available CPU count.
    git_semaphore: GitSemaphore,
    /// path → (project_idx, worktree_idx) for O(1) async-result application.
    worktree_index: std::collections::HashMap<PathBuf, (usize, usize)>,
    /// session_name → parsed ANSI preview; invalidated when pane_capture changes.
    pub parsed_preview: std::collections::HashMap<String, ratatui::text::Text<'static>>,
    tmux_tx: mpsc::Sender<TmuxResult>,
    tmux_rx: mpsc::Receiver<TmuxResult>,
    tmux_refresh_pending: bool,
    /// A user action requested a refresh while one was in-flight; fire another when it lands.
    tmux_refresh_stale: bool,
    tmux_activity_pending: bool,
    tmux_capture_pending: bool,
    /// Worktree paths pending bg deletion; filtered from refresh results until bg confirms.
    pending_deletions: HashSet<PathBuf>,
    /// Session mutations pending confirmation in tmux; filtered/remapped in apply_tmux_refresh.
    pending_session_ops: HashMap<String, SessionOp>,
    update_rx: mpsc::Receiver<String>,
    pub update_available: Option<String>,
    /// Set by --mobile flag; collapses preview panel for portrait SSH sessions.
    pub is_mobile: bool,
    /// Git repos discovered under `$HOME` — populated from app start by a
    /// background walker. Survives modal opens so the add-project prompt
    /// is instant on the second and later opens, and picks up newly
    /// created repos between opens.
    scanned_repos: Vec<String>,
    repo_scan_rx: Option<mpsc::Receiver<String>>,
    routine_tx: mpsc::Sender<RoutineRefreshResult>,
    routine_rx: mpsc::Receiver<RoutineRefreshResult>,
    routine_refresh_generation: HashMap<PathBuf, u64>,
}

impl App {
    pub fn new(mobile: bool) -> Result<Self> {
        let (config, config_warn) = GlobalConfig::load()?;
        let mut workspace = ops::load_workspace(&config);
        let (
            raw_selected,
            cursor_identity,
            command_history,
            cached_active_tab,
            cached_tmux_pid,
            cached_muted,
        ) = wsx_core::cache::apply_cache(&mut workspace);
        // One-time migration: write cached muted names to tmux user options.
        wsx_core::cache::migrate_flags_to_tmux(&cached_muted);
        let restored = ops::restore_cached_sessions(&workspace, cached_tmux_pid);
        let visible_projects =
            compute_visible_projects(&config, &workspace, cached_active_tab.as_deref());
        let cached_flat = flatten_tree_filtered(&workspace, &visible_projects);
        let tree_selected = cursor_identity
            .as_ref()
            .and_then(|id| wsx_core::cache::find_cursor_index(&workspace, &cached_flat, id))
            .unwrap_or_else(|| raw_selected.min(cached_flat.len().saturating_sub(1)));
        let (git_local_tx, git_local_rx) = mpsc::channel();
        let (fetch_tx, fetch_rx) = mpsc::channel();
        let (bg_tx, bg_rx) = mpsc::channel();
        let (tmux_tx, tmux_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel::<String>();
        let (routine_tx, routine_rx) = mpsc::channel();
        std::thread::spawn(move || {
            if let Some(v) = crate::update::fetch_latest_version() {
                let _ = update_tx.send(v);
            }
        });
        let worktree_index = build_worktree_index(&workspace);
        let search_cache = build_search_cache(&workspace, &cached_flat);
        let last_session_snapshot = wsx_core::cache::collect_session_names(&workspace);

        let mut app = Self {
            workspace,
            tree_selected,
            tree_scroll: 0,
            tree_visible_height: 20,
            tree_area: Rect::default(),
            preview_area: Rect::default(),
            mode: Mode::Normal,
            config,
            active_tab: cached_active_tab,
            visible_projects,
            send_command_history: command_history,
            status_message: if restored > 0 {
                Some(format!(
                    "tmux restarted: {restored} session{} restored",
                    if restored == 1 { "" } else { "s" }
                ))
            } else {
                config_warn.clone()
            },
            status_message_expires: if restored > 0 || config_warn.is_some() {
                Some(Instant::now() + Duration::from_secs(10))
            } else {
                None
            },
            jobs: vec![],
            spinner_frame: 0,
            bg_tx,
            bg_rx,
            needs_redraw: true,
            force_redraw: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            activity_timer: Timer::new(ACTIVITY_INTERVAL_MS),
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
            last_session_snapshot,
            worktree_index,
            parsed_preview: std::collections::HashMap::new(),
            git_semaphore: GitSemaphore::new(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            ),
            tmux_tx,
            tmux_rx,
            tmux_refresh_pending: false,
            tmux_refresh_stale: false,
            tmux_activity_pending: false,
            tmux_capture_pending: false,
            pending_deletions: HashSet::new(),
            pending_session_ops: HashMap::new(),
            update_rx,
            update_available: None,
            is_mobile: mobile,
            scanned_repos: Vec::new(),
            repo_scan_rx: None,
            routine_tx,
            routine_rx,
            routine_refresh_generation: HashMap::new(),
        };
        if let Err(error) = app.refresh_routines_all() {
            app.set_status(format!("Routines unavailable: {error}"));
        } else if let Some(identity) = cursor_identity.as_ref() {
            if let Some(index) =
                wsx_core::cache::find_cursor_index(&app.workspace, app.flat(), identity)
            {
                app.tree_selected = index;
            }
        }
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
        self.status_message = Some(msg.into());
        self.status_message_expires = Some(Instant::now() + Duration::from_secs(4));
    }

    pub fn is_busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    fn shows_preview(&self) -> bool {
        !self.is_mobile
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
                    self.spawn_tmux_refresh();
                }
                self.set_status(format!("{}: {}", result.label, e));
            }
            Ok(BgOutcome::WorktreeRemoved { label }) => {
                // ^ A live refresh clears the tombstone after Git stops reporting the path.
                self.spawn_tmux_refresh();
                self.set_status(label);
            }
            Ok(BgOutcome::CleanAborted { wt_path, msg }) => {
                // Merge check failed off-thread; restore the optimistically removed worktree
                self.pending_deletions.remove(&wt_path);
                self.spawn_tmux_refresh();
                self.set_status(msg);
            }
            Ok(BgOutcome::ProjectsCleaned {
                sessions_to_kill,
                msg,
            }) => {
                if !sessions_to_kill.is_empty() {
                    std::thread::spawn(move || {
                        for sess in sessions_to_kill {
                            let _ = session::kill_session(&sess);
                        }
                    });
                }
                self.spawn_tmux_refresh();
                self.set_status(msg);
            }
            Ok(BgOutcome::GitOp { pi, wi, msg }) => {
                let path = self.git_worktree_path(pi, wi).unwrap_or_default();
                let branch = self.default_branch_for_project(pi);
                self.spawn_git_local_force(path, branch);
                self.set_status(msg);
            }
            Ok(BgOutcome::WorktreeCreated { label }) => {
                self.spawn_tmux_refresh();
                self.set_status(label);
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
            compute_visible_projects(&self.config, &self.workspace, self.active_tab.as_deref());
    }

    /// Recompute visible project set from active_tab + config, then rebuild flat + clamp cursor.
    fn recompute_visible(&mut self) {
        self.rebuild_flat();
        self.clamp_selected();
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
            self.mode = Mode::Normal;
        }
    }

    pub fn flat(&self) -> &[FlatEntry] {
        debug_assert!(!self.flat_dirty, "flat() called with dirty cache");
        &self.cached_flat
    }

    fn refresh_routines_all(&mut self) -> Result<()> {
        for project in &mut self.workspace.projects {
            let response =
                crate::cli::send_routine(&project.path, wsx_core::routine::ipc::Action::List)?;
            if let wsx_core::routine::ipc::Response::Routines { revision, routines } = response {
                project.routine_revision = revision;
                project.routines = routines;
            }
        }
        self.rebuild_flat();
        Ok(())
    }

    fn refresh_routine_project(&mut self, project_idx: usize) -> Result<()> {
        let path = self
            .workspace
            .projects
            .get(project_idx)
            .ok_or_else(|| anyhow::anyhow!("project not found"))?
            .path
            .clone();
        self.invalidate_routine_refresh(&path);
        let response = crate::cli::send_routine(&path, wsx_core::routine::ipc::Action::List)?;
        if let wsx_core::routine::ipc::Response::Routines { revision, routines } = response {
            let project = self
                .workspace
                .projects
                .iter_mut()
                .find(|project| project.path == path)
                .ok_or_else(|| anyhow::anyhow!("project removed during routine refresh"))?;
            project.routine_revision = revision;
            project.routines = routines;
            project.routines_expanded = true;
            self.rebuild_flat();
        }
        Ok(())
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
            // Drain async results every iteration so they're never blocked by UI events.
            self.drain_async_results();

            if self.needs_redraw {
                self.ensure_flat();
                let force = self.force_redraw;
                self.force_redraw = false;
                if let Err(e) = tui::draw_sync(terminal, force, |frame| ui::render(frame, self)) {
                    self.set_status(format!("Error: {}", e));
                }
                self.needs_redraw = false;
            }

            let in_input = matches!(
                self.mode,
                Mode::Input { .. }
                    | Mode::Search { .. }
                    | Mode::GitPopup { .. }
                    | Mode::TabManager { .. }
                    | Mode::RoutineEditor { .. }
            );
            if let Some(action) = poll_event(Duration::from_millis(TICK_MS), in_input)? {
                if action == Action::Quit && matches!(self.mode, Mode::Normal) {
                    break;
                }
                if action != Action::None {
                    self.needs_redraw = true;
                }
                if let Err(e) = self.dispatch(action, terminal) {
                    self.set_status(format!("Error: {}", e));
                }
            }
            if let Err(e) = self.tick() {
                self.set_status(format!("Error: {}", e));
            }
        }
        Ok(())
    }

    fn drain_async_results(&mut self) {
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
        while let Ok(result) = self.tmux_rx.try_recv() {
            match result {
                TmuxResult::FullRefresh {
                    sessions,
                    activity,
                    worktrees,
                } => {
                    self.apply_tmux_refresh(sessions, activity, worktrees);
                }
                TmuxResult::Activity(activity) => {
                    self.apply_tmux_activity(activity);
                }
                TmuxResult::Capture {
                    session_name,
                    content,
                } => {
                    self.apply_tmux_capture(session_name, content);
                }
            }
        }
        while let Ok(result) = self.routine_rx.try_recv() {
            if self.routine_refresh_generation.get(&result.project_path) != Some(&result.generation)
            {
                continue;
            }
            if let wsx_core::routine::ipc::Response::Routines { revision, routines } =
                result.response
            {
                let identity = wsx_core::cache::resolve_cursor_identity(
                    &self.workspace,
                    self.flat(),
                    self.tree_selected,
                );
                if let Some(project) = self
                    .workspace
                    .projects
                    .iter_mut()
                    .find(|project| project.path == result.project_path)
                {
                    project.routine_revision = revision;
                    project.routines = routines;
                    self.rebuild_flat();
                    if let Some(identity) = identity.as_ref() {
                        if let Some(index) = wsx_core::cache::find_cursor_index(
                            &self.workspace,
                            self.flat(),
                            identity,
                        ) {
                            self.tree_selected = index;
                        }
                    }
                    self.clamp_selected();
                    self.needs_redraw = true;
                }
            }
        }
        if let Ok(v) = self.update_rx.try_recv() {
            self.update_available = Some(v);
            self.needs_redraw = true;
        }
    }

    fn tick(&mut self) -> Result<()> {
        if let Some(expires) = self.status_message_expires {
            if Instant::now() >= expires {
                self.status_message = None;
                self.status_message_expires = None;
                self.needs_redraw = true;
            }
        }

        if self.slow_timer.ready() {
            self.spawn_tmux_refresh();
            self.spawn_git_local_for_all();
            self.activity_timer.last = Instant::now(); // rescan subsumes activity check
            self.spawn_routine_refresh();
        } else if self.activity_timer.ready() {
            self.spawn_tmux_activity();
            self.needs_redraw = true; // ^ idle time display changes every second
        }

        if self.fast_timer.ready() {
            self.spawn_tmux_capture();
            self.tick_git_fetch();
            if !self.jobs.is_empty() {
                self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
                self.needs_redraw = true;
            }
        }

        Ok(())
    }

    fn spawn_routine_refresh(&mut self) {
        let paths = self
            .workspace
            .projects
            .iter()
            .map(|project| project.path.clone())
            .collect::<Vec<_>>();
        for path in paths {
            let generation = self.invalidate_routine_refresh(&path);
            let tx = self.routine_tx.clone();
            std::thread::spawn(move || {
                if let Ok(response) =
                    crate::cli::send_routine(&path, wsx_core::routine::ipc::Action::List)
                {
                    let _ = tx.send(RoutineRefreshResult {
                        project_path: path,
                        generation,
                        response,
                    });
                }
            });
        }
    }

    /// Skip re-fetching git_info if the worktree is fresh and not currently selected.
    const GIT_INFO_CACHE_SECS: u64 = 15;

    fn spawn_git_local(&mut self, path: PathBuf, default_branch: String) {
        self.spawn_git_local_with_options(path, default_branch, false);
    }

    fn spawn_git_local_force(&mut self, path: PathBuf, default_branch: String) {
        self.spawn_git_local_with_options(path, default_branch, true);
    }

    fn spawn_git_local_with_options(&mut self, path: PathBuf, default_branch: String, force: bool) {
        if self.git_local_pending.contains(&path) {
            return;
        }
        let is_selected = matches!(
            self.current_selection(),
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _)
            if self.workspace.projects.get(pi)
                .and_then(|p| p.worktrees.get(wi))
                .map(|wt| wt.path == path)
                .unwrap_or(false)
        );
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
        self.git_local_pending.insert(path.clone());
        let tx = self.git_local_tx.clone();
        let sem = self.git_semaphore.clone();
        std::thread::spawn(move || {
            let _permit = sem.acquire();
            let info = git_info::get_git_info(&path, &default_branch);
            let _ = tx.send((path, info));
        });
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
        if let Some(e) = wsx_core::cache::save_cache(
            &self.workspace,
            self.tree_selected,
            self.flat(),
            &self.send_command_history,
            self.active_tab.as_deref(),
            sync,
        ) {
            self.set_status(e);
        }
        let session_snapshot = wsx_core::cache::collect_session_names(&self.workspace);
        if sync || session_snapshot != self.last_session_snapshot {
            wsx_core::cache::save_session_snapshot(&self.workspace, true);
            self.last_session_snapshot = session_snapshot;
        }
        self.cache_dirty = false;
    }

    fn mark_dirty(&mut self) {
        self.cache_dirty = true;
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        let sessions_with_paths = session::list_sessions_with_paths();
        let activity = monitor::session_activity();
        let worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> = self
            .workspace
            .projects
            .iter()
            .map(|p| {
                let entries = git_worktree::list_worktrees(&p.path).unwrap_or_default();
                (p.path.clone(), entries)
            })
            .collect();
        let worktrees = self.filter_pending_deletions(worktrees);
        ops::refresh_workspace_with_worktrees(
            &mut self.workspace,
            &self.config,
            &sessions_with_paths,
            &activity,
            worktrees,
        );
        self.rebuild_flat();
        self.clamp_selected();
        // Prune parsed_preview entries for sessions that no longer exist
        let live_sessions: std::collections::HashSet<&str> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| p.worktrees.iter())
            .flat_map(|w| w.sessions.iter())
            .map(|s| s.name.as_str())
            .collect();
        self.parsed_preview
            .retain(|k, _| live_sessions.contains(k.as_str()));
        self.mark_dirty();
        self.write_cache_if_dirty();
        Ok(())
    }

    /// Write cache only when state has changed since last write.
    fn write_cache_if_dirty(&mut self) {
        if !self.cache_dirty {
            return;
        }
        self.persist_state(false);
    }

    fn filter_pending_deletions(
        &mut self,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
        filter_pending_deletions(&mut self.pending_deletions, worktrees)
    }

    fn apply_pending_session_ops(
        &mut self,
        sessions: Vec<(String, PathBuf)>,
    ) -> Vec<(String, PathBuf)> {
        filter_pending_session_ops(&mut self.pending_session_ops, sessions)
    }

    fn spawn_tmux_refresh(&mut self) {
        if self.tmux_refresh_pending {
            // ^ caller needs fresh data; remember to fire another refresh when current lands
            self.tmux_refresh_stale = true;
            return;
        }
        self.tmux_refresh_pending = true;
        let tx = self.tmux_tx.clone();
        let project_paths: Vec<PathBuf> = self
            .workspace
            .projects
            .iter()
            .map(|p| p.path.clone())
            .collect();
        std::thread::spawn(move || {
            let sessions = session::list_sessions_with_paths();
            let activity = monitor::session_activity();
            let worktrees = project_paths
                .into_iter()
                .map(|path| {
                    let entries = git_worktree::list_worktrees(&path).unwrap_or_default();
                    (path, entries)
                })
                .collect();
            let _ = tx.send(TmuxResult::FullRefresh {
                sessions,
                activity,
                worktrees,
            });
        });
    }

    fn apply_tmux_refresh(
        &mut self,
        sessions: Vec<(String, PathBuf)>,
        activity: HashMap<String, SessionStatus>,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) {
        self.tmux_refresh_pending = false;
        let worktrees = self.filter_pending_deletions(worktrees);
        // Apply pending session ops: filter killed sessions and remap renamed ones so
        // stale tmux snapshots don't clobber user actions in progress.
        let sessions = self.apply_pending_session_ops(sessions);
        ops::refresh_workspace_with_worktrees(
            &mut self.workspace,
            &self.config,
            &sessions,
            &activity,
            worktrees,
        );
        self.rebuild_flat();
        self.clamp_selected();
        let live_sessions: HashSet<&str> = self
            .workspace
            .projects
            .iter()
            .flat_map(|p| p.worktrees.iter())
            .flat_map(|w| w.sessions.iter())
            .map(|s| s.name.as_str())
            .collect();
        self.parsed_preview
            .retain(|k, _| live_sessions.contains(k.as_str()));
        self.mark_dirty();
        self.write_cache_if_dirty();
        self.needs_redraw = true;
        // If a user action queued a refresh while this one was in-flight, fire it now.
        if self.tmux_refresh_stale {
            self.tmux_refresh_stale = false;
            self.spawn_tmux_refresh();
        }
    }

    fn spawn_tmux_activity(&mut self) {
        if self.tmux_activity_pending || self.tmux_refresh_pending {
            return;
        }
        self.tmux_activity_pending = true;
        let tx = self.tmux_tx.clone();
        std::thread::spawn(move || {
            let activity = monitor::session_activity();
            let _ = tx.send(TmuxResult::Activity(activity));
        });
    }

    fn apply_tmux_activity(&mut self, activity: HashMap<String, SessionStatus>) {
        self.tmux_activity_pending = false;
        ops::update_activity(&mut self.workspace, &activity);
        self.needs_redraw = true; // ^ idle counter ticks every second regardless of state change
    }

    fn spawn_tmux_capture(&mut self) {
        if self.tmux_capture_pending || !self.shows_preview() {
            return;
        }
        let (name, is_wsx) = match self.current_selection() {
            Selection::Session(pi, wi, si) => self
                .workspace
                .session(pi, wi, si)
                .map(|s| (s.name.clone(), s.is_running_wsx))
                .unwrap_or_default(),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        if is_wsx {
            // Suppress capture — no thread needed, stable placeholder breaks the render loop.
            self.apply_tmux_capture(name, Some(capture::NESTED_WSX_MSG.to_string()));
            return;
        }
        self.tmux_capture_pending = true;
        let tx = self.tmux_tx.clone();
        std::thread::spawn(move || {
            let content = capture::capture_pane(&name).map(|raw| capture::trim_capture(&raw));
            let _ = tx.send(TmuxResult::Capture {
                session_name: name,
                content,
            });
        });
    }

    fn apply_tmux_capture(&mut self, session_name: String, content: Option<String>) {
        self.tmux_capture_pending = false;
        let trimmed = match content {
            Some(t) => t,
            None => return,
        };
        // Find the session by name and update if content changed
        let mut found: Option<(usize, usize, usize)> = None;
        'outer: for (pi, proj) in self.workspace.projects.iter().enumerate() {
            for (wi, wt) in proj.worktrees.iter().enumerate() {
                for (si, s) in wt.sessions.iter().enumerate() {
                    if s.name == session_name {
                        found = Some((pi, wi, si));
                        break 'outer;
                    }
                }
            }
        }
        let Some((pi, wi, si)) = found else { return };
        if let Some(s) = self.workspace.session_mut(pi, wi, si) {
            if s.pane_capture.as_deref() != Some(&trimmed) {
                let mut parsed = ansi::parse(&trimmed);
                while parsed
                    .lines
                    .last()
                    .map(|l| l.spans.iter().all(|sp| sp.content.trim().is_empty()))
                    .unwrap_or(false)
                {
                    parsed.lines.pop();
                }
                self.parsed_preview.insert(s.name.clone(), parsed);
                s.pane_capture = Some(trimmed);
                self.needs_redraw = true;
                // ! PUA chars (powerline) are width-1 to ratatui but width-2 in terminal;
                // ! clear the buffer on every content change so diffs never leave stale cells.
                self.force_redraw = self.shows_preview();
            }
        }
    }

    /// Git fetch trigger — called from fast_timer tick (already was in refresh_captures).
    fn tick_git_fetch(&mut self) {
        let Some((pi, wi)) = self.selected_worktree_indices() else {
            return;
        };
        let fetch_info = self.workspace.worktree(pi, wi).map(|wt| {
            let interval = FETCH_INTERVAL_SECS * 2u64.pow(wt.fetch_fail_count.min(4) as u32);
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

    pub fn current_selection(&self) -> Selection {
        self.workspace
            .get_selection(self.tree_selected, self.flat())
    }

    fn selected_worktree_indices(&self) -> Option<(usize, usize)> {
        match self.current_selection() {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => Some((pi, wi)),
            _ => None,
        }
    }

    fn default_branch_for_project(&self, project_idx: usize) -> String {
        self.workspace
            .projects
            .get(project_idx)
            .map(|p| p.default_branch.clone())
            .unwrap_or_else(|| "main".to_string())
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
                    self.workspace.projects[pi].expanded = true;
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
        self.tree_scroll = crate::ui::workspace_tree::compute_scroll(
            self.tree_selected,
            visible,
            self.tree_scroll,
        );
        // ^ PUA width mismatch can leave ghost cells when the preview content changes;
        // force a full redraw on every navigation to flush them.
        self.force_redraw = self.shows_preview();
    }

    // ── Action dispatch ───────────────────────────────────────────────────────

    fn dispatch(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        self.ensure_flat();
        // Config mode handled first to avoid borrow conflicts
        if let Mode::Config { project_idx } = &self.mode {
            let pi = *project_idx;
            if matches!(action, Action::InputEscape | Action::Quit | Action::Help) {
                self.mode = Mode::Normal;
            } else if action == Action::Edit {
                let path = self
                    .workspace
                    .projects
                    .get(pi)
                    .map(|p| p.path.join(".gtrignore"));
                if let Some(path) = path {
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                    tui::with_raw_mode_disabled(terminal, || {
                        std::process::Command::new(&editor).arg(&path).status()?;
                        Ok(())
                    })?;
                }
            }
            return Ok(());
        }

        if let Mode::Move { project_idx } = &self.mode {
            let pi = *project_idx;
            match action {
                Action::NavigateDown => self.move_project_down(pi),
                Action::NavigateUp => self.move_project_up(pi),
                Action::NavigateLeft => {
                    if !self.config.tabs.is_empty() {
                        self.move_project_to_adjacent_tab(pi, -1)?;
                        return Ok(());
                    }
                }
                Action::NavigateRight => {
                    if !self.config.tabs.is_empty() {
                        self.move_project_to_adjacent_tab(pi, 1)?;
                        return Ok(());
                    }
                }
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.sync_config_project_order();
                    self.config.save()?;
                    self.mode = Mode::Normal;
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::TabManager { selected } = &self.mode {
            let sel = *selected;
            return self.dispatch_tab_manager(sel, action);
        }

        if matches!(self.mode, Mode::RoutineEditor { .. }) {
            return self.dispatch_routine_editor(action);
        }
        if matches!(self.mode, Mode::RoutineDetail { .. }) {
            match action {
                Action::InputEscape | Action::Quit | Action::Select => self.mode = Mode::Normal,
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
                Action::NavigateDown => self.move_session(pi, wi, si, 1),
                Action::NavigateUp => self.move_session(pi, wi, si, -1),
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.mark_dirty();
                    self.write_cache_if_dirty();
                    self.mode = Mode::Normal;
                }
                _ => {}
            }
            return Ok(());
        }

        if let Mode::GitPopup {
            project_idx,
            worktree_idx,
        } = &self.mode
        {
            let (pi, wi) = (*project_idx, *worktree_idx);
            return self.dispatch_git_popup(pi, wi, action, terminal);
        }

        match &self.mode {
            Mode::Normal => self.dispatch_normal(action, terminal)?,
            Mode::Input { .. } => self.dispatch_input(action, terminal)?,
            Mode::Confirm { .. } => self.dispatch_confirm(action, terminal)?,
            Mode::Help => {
                if matches!(action, Action::InputEscape | Action::Quit | Action::Help) {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Search { .. } => self.dispatch_search(action, terminal)?,
            Mode::Config { .. }
            | Mode::Move { .. }
            | Mode::MoveSession { .. }
            | Mode::GitPopup { .. }
            | Mode::TabManager { .. } => unreachable!(),
            Mode::RoutineEditor { .. } | Mode::RoutineDetail { .. } => unreachable!(),
        }
        Ok(())
    }

    fn dispatch_normal(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        match action {
            Action::NavigateUp => self.nav_up(),
            Action::NavigateDown => self.nav_down(),
            Action::NavigateLeft => self.nav_left(),
            Action::NavigateRight => self.nav_right(),
            Action::Select => self.action_select(terminal)?,
            Action::AddProject => self.action_add_project()?,
            Action::AddWorktree => self.action_add_worktree()?,
            Action::AddSession => self.action_add_session()?,
            Action::AddRoutine => self.action_add_routine()?,
            Action::Delete => self.action_delete()?,
            Action::Clean => self.action_clean()?,
            Action::Edit => self.action_edit()?,
            Action::SetAlias => self.action_set_alias()?,
            Action::Refresh => self.refresh_all()?,
            Action::Resize => {
                self.force_redraw = true;
                self.needs_redraw = true;
            }
            Action::Help => {
                self.mode = Mode::Help;
            }
            Action::NextAttention => self.action_next_attention(1),
            Action::PrevAttention => self.action_next_attention(-1),
            Action::DismissAttention => self.action_dismiss_attention(),
            Action::NextActive => self.action_next_active(),
            Action::PrevActive => self.action_prev_active(),
            Action::NextIdle => self.action_next_idle(),
            Action::PrevIdle => self.action_prev_idle(),
            Action::SendCommand => self.action_send_command(),
            Action::SendCtrlC => self.action_send_ctrl_c()?,
            Action::EnterMove => self.action_enter_move(),
            Action::JumpProjectDown => self.jump_project(1),
            Action::JumpProjectUp => self.jump_project(-1),
            Action::SearchStart => {
                self.mode = Mode::Search {
                    query: String::new(),
                    match_idx: 0,
                };
            }
            Action::GitPopup => self.action_git_popup(),
            Action::TabNext => self.action_tab_next(),
            Action::TabPrev => self.action_tab_prev(),
            Action::TabManager => self.action_tab_manager(),
            Action::MouseClick { col, row } => self.handle_mouse_click(col, row, terminal)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_click(&mut self, col: u16, row: u16, terminal: &mut Tui) -> Result<()> {
        let pos = Position { x: col, y: row };
        if self.tree_area.contains(pos) {
            // Content starts after top border (y+1), ends before bottom border (y+height-1)
            let content_top = self.tree_area.y + 1;
            let content_bottom = self.tree_area.y + self.tree_area.height.saturating_sub(1);
            if row >= content_top && row < content_bottom {
                let flat_idx = (row - content_top) as usize + self.tree_scroll;
                if flat_idx < self.flat().len() {
                    if flat_idx == self.tree_selected {
                        self.action_select(terminal)?;
                    } else {
                        self.tree_selected = flat_idx;
                        self.update_scroll();
                    }
                }
            }
        } else if self.preview_area.contains(pos) {
            if matches!(self.current_selection(), Selection::Session(..)) {
                self.action_select(terminal)?;
            }
        }
        Ok(())
    }

    fn dispatch_input(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        match action {
            Action::InputEscape | Action::Quit => {
                self.mode = Mode::Normal;
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
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch_search(&mut self, action: Action, _terminal: &mut Tui) -> Result<()> {
        match action {
            Action::InputEscape | Action::Quit => {
                self.mode = Mode::Normal;
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

    fn dispatch_routine_editor(&mut self, action: Action) -> Result<()> {
        match action {
            Action::InputEscape | Action::Quit => self.mode = Mode::Normal,
            Action::RoutineCodexPreset => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.apply_preset(false);
                }
            }
            Action::RoutineClaudePreset => {
                if let Mode::RoutineEditor { form, .. } = &mut self.mode {
                    form.apply_preset(true);
                }
            }
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
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
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
        let action = if let Some(old_name) = original_name.clone() {
            wsx_core::routine::ipc::Action::Edit {
                revision: project.routine_revision,
                old_name,
                routine: routine.clone(),
            }
        } else {
            wsx_core::routine::ipc::Action::Add {
                revision: project.routine_revision,
                routine: routine.clone(),
            }
        };
        if let Err(error) = crate::cli::send_routine(&project.path, action) {
            self.refresh_routine_project(project_idx).ok();
            self.mode = Mode::RoutineEditor {
                project_idx,
                original_name,
                can_rename,
                form,
            };
            self.set_status(format!("Routine not saved: {error}"));
            return Ok(());
        }
        self.refresh_routine_project(project_idx)?;
        self.select_routine(project_idx, Some(&routine.name));
        self.set_status(format!("Saved routine '{}'", routine.name));
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
            self.mode = Mode::Normal;
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
            Selection::Project(pi) => {
                self.workspace.projects[pi].expanded = !self.workspace.projects[pi].expanded;
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

    /// User interacted with a session in wsx (attach, send command, rename,
    /// ctrl-c). Mute is sticky — only an explicit interaction unmutes; pane
    /// output alone does not. Cursor navigation must NOT call this.
    fn unmute_on_interaction(&mut self, session_name: &str) {
        for project in &mut self.workspace.projects {
            for wt in &mut project.worktrees {
                for sess in &mut wt.sessions {
                    if sess.name == session_name {
                        if sess.muted {
                            sess.muted = false;
                            session::set_session_opt(session_name, session::OPT_MUTED, "0");
                        }
                        return;
                    }
                }
            }
        }
    }

    fn attach_to_session(&self, name: &str, terminal: &mut Tui) -> Result<()> {
        let detach_key = if self.is_mobile {
            self.config.mobile_detach_key.as_deref()
        } else {
            None
        };
        session::apply_session_defaults(name, detach_key);
        match session::attach_session_cmd(name) {
            session::AttachCommand::SwitchClient(n) => session::switch_client(&n)?,
            session::AttachCommand::Attach(n) => {
                tui::with_raw_mode_disabled(terminal, || session::attach_foreground(&n))?;
            }
        }
        Ok(())
    }

    fn attach_session(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        terminal: &mut Tui,
    ) -> Result<()> {
        let name = self.workspace.session(pi, wi, si).map(|s| s.name.clone());

        let Some(name) = name else {
            self.set_status("Session not found");
            return Ok(());
        };

        self.unmute_on_interaction(&name);

        let proj = &self.workspace.projects[pi];
        let wt = &proj.worktrees[wi];
        let alias = wt.alias.as_deref().unwrap_or(&wt.branch);
        session::set_session_opt(&name, "@wsx_project", &proj.name);
        session::set_session_opt(&name, "@wsx_alias", alias);
        if !session::user_has_tmux_config() {
            let label = format!(" {}/{} ", proj.name, alias);
            session::set_session_opt(&name, "status-right", &label);
        }

        self.attach_to_session(&name, terminal)?;

        // Trigger a refresh after returning, but keep old git_info visible until result arrives.
        self.spawn_git_local(
            self.workspace
                .worktree(pi, wi)
                .map(|w| w.path.clone())
                .unwrap_or_default(),
            self.default_branch_for_project(pi),
        );
        Ok(())
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
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
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

    fn action_add_routine(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi)
            | Selection::Worktree(pi, _)
            | Selection::Session(pi, _, _)
            | Selection::RoutinesHeader(pi)
            | Selection::Routine(pi, _) => pi,
            Selection::None => {
                self.set_status("Select a project first");
                return Ok(());
            }
        };
        self.mode = Mode::RoutineEditor {
            project_idx: pi,
            original_name: None,
            can_rename: true,
            form: RoutineForm::codex(),
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

    fn action_clean(&mut self) -> Result<()> {
        if self.is_busy() {
            self.set_status("Operation in progress");
            return Ok(());
        }
        match self.current_selection() {
            Selection::Worktree(pi, wi) => {
                let (repo, wt_path, branch, default_branch, is_main, session_names) = {
                    let p = &self.workspace.projects[pi];
                    let wt = &p.worktrees[wi];
                    let names: Vec<String> = wt.sessions.iter().map(|s| s.name.clone()).collect();
                    (
                        p.path.clone(),
                        wt.path.clone(),
                        wt.branch.clone(),
                        p.default_branch.clone(),
                        wt.is_main,
                        names,
                    )
                };
                if is_main {
                    self.set_status("Cannot clean main worktree");
                    return Ok(());
                }
                // Optimistic removal — merge check moved to bg thread to avoid blocking UI
                let label = format!("Cleaned: {}", branch);
                let abort_msg = format!("'{}' not merged into {}", branch, default_branch);
                self.pending_deletions.insert(wt_path.clone());
                self.workspace.projects[pi].worktrees.remove(wi);
                self.rebuild_flat();
                self.clamp_selected();
                self.spawn_bg(format!("clean {}", branch), move || {
                    if !git_worktree::is_branch_merged(&repo, &branch, &default_branch) {
                        return Ok(BgOutcome::CleanAborted {
                            wt_path,
                            msg: abort_msg,
                        });
                    }
                    ops::delete_worktree(&repo, &wt_path, &branch, &session_names)?;
                    Ok(BgOutcome::WorktreeRemoved { label })
                });
            }
            Selection::Project(pi) | Selection::Session(pi, _, _) => {
                let (path, default_branch, branch_sessions) = {
                    let p = &self.workspace.projects[pi];
                    (
                        p.path.clone(),
                        p.default_branch.clone(),
                        p.branch_session_names(),
                    )
                };
                self.spawn_bg("clean project", move || {
                    let removed = git_worktree::clean_merged(&path, &default_branch)?;
                    let sessions_to_kill: Vec<String> = removed
                        .iter()
                        .flat_map(|b| branch_sessions.get(b).cloned().unwrap_or_default())
                        .collect();
                    let msg = if removed.is_empty() {
                        "No merged worktrees to clean".into()
                    } else {
                        format!("Cleaned: {}", removed.join(", "))
                    };
                    Ok(BgOutcome::ProjectsCleaned {
                        sessions_to_kill,
                        msg,
                    })
                });
            }
            Selection::None => {
                let snapshots: Vec<_> = self
                    .workspace
                    .projects
                    .iter()
                    .map(|p| {
                        (
                            p.path.clone(),
                            p.default_branch.clone(),
                            p.branch_session_names(),
                        )
                    })
                    .collect();
                self.spawn_bg("clean all", move || {
                    let mut sessions_to_kill = Vec::new();
                    let mut total = 0usize;
                    for (path, branch, branch_sessions) in &snapshots {
                        if let Ok(removed) = git_worktree::clean_merged(path, branch) {
                            for b in &removed {
                                if let Some(s) = branch_sessions.get(b) {
                                    sessions_to_kill.extend(s.iter().cloned());
                                }
                            }
                            total += removed.len();
                        }
                    }
                    let msg = format!("Cleaned {} merged worktrees", total);
                    Ok(BgOutcome::ProjectsCleaned {
                        sessions_to_kill,
                        msg,
                    })
                });
            }
            Selection::RoutinesHeader(_) | Selection::Routine(_, _) => {
                self.set_status("Clean applies to projects and worktrees")
            }
        }
        Ok(())
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
                if session_state::derive(sess).app_state() == AppSessionState::Idle {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn action_next_idle(&mut self) {
        let candidates = self.idle_candidates();
        if candidates.is_empty() {
            self.set_status("No idle sessions");
            return;
        }
        let next = candidates
            .iter()
            .find(|&&i| i > self.tree_selected)
            .or_else(|| candidates.first())
            .copied()
            .unwrap();
        self.tree_selected = next;
        self.update_scroll();
    }

    fn action_prev_idle(&mut self) {
        let candidates = self.idle_candidates();
        if candidates.is_empty() {
            self.set_status("No idle sessions");
            return;
        }
        let prev = candidates
            .iter()
            .rev()
            .find(|&&i| i < self.tree_selected)
            .or_else(|| candidates.last())
            .copied()
            .unwrap();
        self.tree_selected = prev;
        self.update_scroll();
    }

    fn action_send_command(&mut self) {
        if let Selection::Session(pi, wi, si) = self.current_selection() {
            if let Some(sess) = self.workspace.session(pi, wi, si) {
                let name = sess.name.clone();
                self.mode = Mode::Input {
                    context: InputContext::SendCommand { session_name: name },
                    state: InputState::with_history("cmd: ", self.send_command_history.clone()),
                };
            }
        }
    }

    fn action_send_ctrl_c(&mut self) -> Result<()> {
        if let Selection::Session(pi, wi, si) = self.current_selection() {
            let name = self.workspace.session(pi, wi, si).map(|s| s.name.clone());
            if let Some(name) = name {
                self.unmute_on_interaction(&name);
                session::send_ctrl_c(&name)?;
            }
        }
        Ok(())
    }

    fn action_next_active(&mut self) {
        let candidates = self.active_candidates();
        if candidates.is_empty() {
            self.set_status("No active sessions");
            return;
        }
        let next = candidates
            .iter()
            .find(|&&i| i > self.tree_selected)
            .or_else(|| candidates.first())
            .copied()
            .unwrap_or(candidates[0]);
        self.tree_selected = next;
        self.update_scroll();
    }

    fn action_prev_active(&mut self) {
        let candidates = self.active_candidates();
        if candidates.is_empty() {
            self.set_status("No active sessions");
            return;
        }
        let prev = candidates
            .iter()
            .rev()
            .find(|&&i| i < self.tree_selected)
            .or_else(|| candidates.last())
            .copied()
            .unwrap_or(candidates[0]);
        self.tree_selected = prev;
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

    fn action_next_attention(&mut self, dir: isize) {
        let candidates = self.attention_candidates();

        if candidates.is_empty() {
            self.set_status("No sessions need attention");
            return;
        }

        let next = if dir >= 0 {
            candidates
                .iter()
                .find(|&&i| i > self.tree_selected)
                .or_else(|| candidates.first())
                .copied()
                .unwrap()
        } else {
            candidates
                .iter()
                .rev()
                .find(|&&i| i < self.tree_selected)
                .or_else(|| candidates.last())
                .copied()
                .unwrap()
        };

        self.tree_selected = next;
        self.update_scroll();
    }

    fn action_dismiss_attention(&mut self) {
        if let Selection::Session(pi, wi, si) = self.current_selection() {
            if let Some(sess) = self.workspace.session_mut(pi, wi, si) {
                if session_state::derive(sess).app_state() == AppSessionState::Active {
                    return;
                }
                if sess.has_activity {
                    sess.has_activity = false;
                    self.set_status("Dismissed");
                    return;
                }
                // Running app or idle session — toggle mute
                sess.muted = !sess.muted;
                let muted_val = if sess.muted { "1" } else { "0" };
                session::set_session_opt(&sess.name, session::OPT_MUTED, muted_val);
                let msg = if sess.muted { "Muted" } else { "Unmuted" };
                self.set_status(msg);
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
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
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
                            session_name: value,
                        },
                        state: InputState::new("command (optional): "),
                    };
                    return Ok(());
                }
                InputContext::AddSessionCmd {
                    project_idx,
                    worktree_idx,
                    session_name,
                } => {
                    let cmd = if value.is_empty() { None } else { Some(value) };
                    self.do_create_session(project_idx, worktree_idx, session_name, cmd)?;
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
                InputContext::SendCommand { session_name } => {
                    if !value.is_empty() {
                        self.unmute_on_interaction(&session_name);
                        session::send_keys(&session_name, &value)?;
                        self.send_command_history.retain(|cmd| cmd != &value);
                        self.send_command_history.push(value);
                        if self.send_command_history.len() > 50 {
                            let overflow = self.send_command_history.len() - 50;
                            self.send_command_history.drain(0..overflow);
                        }
                    }
                }
                InputContext::GitPullRebase {
                    project_idx,
                    worktree_idx,
                } => {
                    if !value.is_empty() {
                        self.do_git_pull_rebase(project_idx, worktree_idx, value)?;
                        return Ok(());
                    }
                }
                InputContext::GitMergeFrom {
                    project_idx,
                    worktree_idx,
                } => {
                    if !value.is_empty() {
                        self.do_git_merge_from(project_idx, worktree_idx, value)?;
                        return Ok(());
                    }
                }
                InputContext::GitMergeInto {
                    project_idx,
                    worktree_idx,
                } => {
                    if !value.is_empty() {
                        self.do_git_merge_into(project_idx, worktree_idx, value)?;
                        return Ok(());
                    }
                }
                InputContext::AddTab => {
                    let trimmed = value.trim().to_string();
                    if trimmed.is_empty() {
                        self.mode = Mode::TabManager { selected: 0 };
                    } else if self.config.tabs.contains(&trimmed) {
                        self.set_status(format!("Tab '{}' already exists", trimmed));
                        self.mode = Mode::TabManager { selected: 0 };
                    } else {
                        self.config.tabs.push(trimmed);
                        self.config.save()?;
                        let sel = self.config.tabs.len(); // last in ordered_tabs (1-indexed)
                        self.mode = Mode::TabManager { selected: sel };
                    }
                }
                InputContext::RenameTab { tab_idx } => {
                    let trimmed = value.trim().to_string();
                    if trimmed.is_empty()
                        || self
                            .config
                            .tabs
                            .get(tab_idx)
                            .map_or(false, |t| t == &trimmed)
                    {
                        self.mode = Mode::TabManager {
                            selected: tab_idx + 1,
                        };
                    } else if self.config.tabs.contains(&trimmed) {
                        self.set_status(format!("Tab '{}' already exists", trimmed));
                        self.mode = Mode::TabManager {
                            selected: tab_idx + 1,
                        };
                    } else {
                        let old_name = self.config.tabs[tab_idx].clone();
                        self.config.tabs[tab_idx] = trimmed.clone();
                        // Update all projects assigned to this tab
                        for proj in &mut self.config.projects {
                            if proj.tab.as_deref() == Some(&old_name) {
                                proj.tab = Some(trimmed.clone());
                            }
                        }
                        // Update active_tab if it was the renamed one
                        if self.active_tab.as_deref() == Some(&old_name) {
                            self.active_tab = Some(trimmed);
                        }
                        self.config.save()?;
                        self.mode = Mode::TabManager {
                            selected: tab_idx + 1,
                        };
                    }
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
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
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
                PendingAction::DeleteWorktree {
                    project_idx: pi,
                    worktree_idx: wi,
                } => {
                    let (repo, wt_path, branch, session_names) = {
                        let p = &self.workspace.projects[pi];
                        let wt = &p.worktrees[wi];
                        let names: Vec<String> =
                            wt.sessions.iter().map(|s| s.name.clone()).collect();
                        (p.path.clone(), wt.path.clone(), wt.branch.clone(), names)
                    };
                    let label = format!("Deleted: {}", branch);
                    // Optimistically remove before spawning so the tree updates this frame
                    self.pending_deletions.insert(wt_path.clone());
                    self.workspace.projects[pi].worktrees.remove(wi);
                    self.rebuild_flat();
                    self.clamp_selected();
                    self.spawn_bg(format!("delete {}", branch), move || {
                        ops::delete_worktree(&repo, &wt_path, &branch, &session_names)?;
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
                PendingAction::DeleteTab { tab_idx } => {
                    let tab_name = self.config.tabs[tab_idx].clone();
                    // Move projects from deleted tab to default
                    for proj in &mut self.config.projects {
                        if proj.tab.as_deref() == Some(&tab_name) {
                            proj.tab = None;
                        }
                    }
                    self.config.tabs.remove(tab_idx);
                    // If active tab was deleted, switch to default
                    if self.active_tab.as_deref() == Some(&tab_name) {
                        self.active_tab = None;
                    }
                    self.config.save()?;
                    self.recompute_visible();
                    self.mark_dirty();
                    self.mode = Mode::TabManager { selected: 0 };
                    self.set_status(format!("Deleted tab '{}'", tab_name));
                }
                PendingAction::DeleteRoutine {
                    project_path,
                    name,
                    revision,
                } => {
                    let Some(project_idx) = self
                        .workspace
                        .projects
                        .iter()
                        .position(|project| project.path == project_path)
                    else {
                        self.mode = Mode::Normal;
                        self.set_status(format!(
                            "Routine '{name}' not deleted: project is no longer registered"
                        ));
                        return Ok(());
                    };
                    let path = project_path;
                    if let Err(error) = crate::cli::send_routine(
                        &path,
                        wsx_core::routine::ipc::Action::Delete {
                            revision,
                            name: name.clone(),
                        },
                    ) {
                        let _ = self.refresh_routine_project(project_idx);
                        self.restore_failed_routine_delete(project_idx, &name, error.to_string());
                        return Ok(());
                    }
                    if let Err(error) = self.refresh_routine_project(project_idx) {
                        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
                            project.routines.retain(|view| view.routine.name != name);
                            project.routines_expanded = true;
                        }
                        self.rebuild_flat();
                        self.select_routine(project_idx, None);
                        self.set_status(format!(
                            "Deleted routine '{name}', but refresh failed: {error}"
                        ));
                        return Ok(());
                    }
                    self.select_routine(project_idx, None);
                    self.set_status(format!("Deleted routine '{name}'"));
                }
            }
        }
        Ok(())
    }

    fn restore_failed_routine_delete(&mut self, project_idx: usize, name: &str, error: String) {
        self.mode = Mode::Normal;
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
        // Assign new project to the currently active tab
        if !self.config.tabs.is_empty() {
            if let Some(entry) = self.config.projects.last_mut() {
                entry.tab = self.active_tab.clone();
            }
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
        session_name: String,
        command: Option<String>,
    ) -> Result<()> {
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug(&p.name))
        };
        let explicit_name = if session_name.is_empty() {
            None
        } else {
            Some(session_name)
        };
        let (_tmux_name, display_name) =
            ops::create_session(&proj_name, &wt_slug, &wt_path, explicit_name, command)?;
        self.set_status(format!("Session '{}' created", display_name));
        // Expand so the new session is visible; set before spawn_tmux_refresh so the
        // snapshot in apply_tmux_refresh captures it before rebuilding workspace state
        if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
            wt.expanded = true;
        }
        self.spawn_tmux_refresh();
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
        let sess = &self.workspace.projects[pi].worktrees[wi].sessions[si];
        let tmux_name = sess.name.clone();
        let display_name = sess.display_name.clone();
        // Optimistic removal — if kill fails, next periodic refresh re-adds the session.
        // Register pending op so stale in-flight refreshes don't resurrect it before kill confirms.
        self.pending_session_ops
            .insert(tmux_name.clone(), SessionOp::Killed);
        self.workspace.projects[pi].worktrees[wi]
            .sessions
            .remove(si);
        self.rebuild_flat();
        self.clamp_selected();
        self.mark_dirty();
        self.set_status(format!("Killed session: {}", display_name));
        std::thread::spawn(move || {
            let _ = session::kill_session(&tmux_name);
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
        let old_tmux_name = self.workspace.projects[pi].worktrees[wi].sessions[si]
            .name
            .clone();
        self.unmute_on_interaction(&old_tmux_name);
        let proj_name = self.workspace.projects[pi].name.clone();
        let wt_slug = self.workspace.projects[pi].worktrees[wi].session_slug(&proj_name);
        let new_tmux_name = format!("{}-{}-{}", proj_name, wt_slug, new_name);
        ops::rename_session(&old_tmux_name, &new_tmux_name)?;
        let sess = &mut self.workspace.projects[pi].worktrees[wi].sessions[si];
        sess.name = new_tmux_name.clone();
        sess.display_name = new_name.clone();
        // Register pending rename so a stale in-flight refresh remaps old name → new name
        // and preserves pane_capture/muted state instead of losing them.
        self.pending_session_ops.insert(
            old_tmux_name,
            SessionOp::Renamed {
                new_name: new_tmux_name,
            },
        );
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

    fn move_session(&mut self, pi: usize, wi: usize, si: usize, delta: isize) {
        let new_si = si as isize + delta;
        if new_si < 0 {
            return;
        }
        let new_si = new_si as usize;
        let sessions = &mut self.workspace.projects[pi].worktrees[wi].sessions;
        if new_si >= sessions.len() {
            return;
        }
        sessions.swap(si, new_si);
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

    // ── Git popup ─────────────────────────────────────────────────────────────

    fn action_git_popup(&mut self) {
        let (pi, wi) = match self.current_selection() {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
            _ => {
                self.set_status("Select a worktree");
                return;
            }
        };
        self.mode = Mode::GitPopup {
            project_idx: pi,
            worktree_idx: wi,
        };
    }

    fn dispatch_git_popup(
        &mut self,
        pi: usize,
        wi: usize,
        action: Action,
        _terminal: &mut Tui,
    ) -> Result<()> {
        match action {
            Action::InputChar('p') => self.do_git_pull(pi, wi),
            Action::InputChar('P') => self.do_git_push(pi, wi),
            Action::InputChar('r') => {
                let default = self.workspace.projects[pi].default_branch.clone();
                self.mode = Mode::Input {
                    context: InputContext::GitPullRebase {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                    state: InputState::with_value("branch: ", default),
                };
            }
            Action::InputChar('m') => {
                let default = self.workspace.projects[pi].default_branch.clone();
                self.mode = Mode::Input {
                    context: InputContext::GitMergeFrom {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                    state: InputState::with_value("branch: ", default),
                };
            }
            Action::InputChar('M') => {
                let default = self.workspace.projects[pi].default_branch.clone();
                self.mode = Mode::Input {
                    context: InputContext::GitMergeInto {
                        project_idx: pi,
                        worktree_idx: wi,
                    },
                    state: InputState::with_value("branch: ", default),
                };
            }
            Action::InputEscape | Action::Quit => self.mode = Mode::Normal,
            _ => {}
        }
        Ok(())
    }

    // ── Tab navigation ────────────────────────────────────────────────────────

    fn action_tab_next(&mut self) {
        if self.config.tabs.is_empty() {
            return;
        }
        let tabs = self.config.ordered_tabs();
        let cur = tabs
            .iter()
            .position(|t| t.as_deref() == self.active_tab.as_deref())
            .unwrap_or(0);
        let next = (cur + 1) % tabs.len();
        self.active_tab = tabs[next].map(|s| s.to_string());
        self.recompute_visible();
        self.update_scroll();
        self.mark_dirty();
    }

    fn action_tab_prev(&mut self) {
        if self.config.tabs.is_empty() {
            return;
        }
        let tabs = self.config.ordered_tabs();
        let cur = tabs
            .iter()
            .position(|t| t.as_deref() == self.active_tab.as_deref())
            .unwrap_or(0);
        let prev = if cur == 0 { tabs.len() - 1 } else { cur - 1 };
        self.active_tab = tabs[prev].map(|s| s.to_string());
        self.recompute_visible();
        self.update_scroll();
        self.mark_dirty();
    }

    fn action_tab_manager(&mut self) {
        let tabs = self.config.ordered_tabs();
        let selected = tabs
            .iter()
            .position(|t| t.as_deref() == self.active_tab.as_deref())
            .unwrap_or(0);
        self.mode = Mode::TabManager { selected };
    }

    fn dispatch_tab_manager(&mut self, selected: usize, action: Action) -> Result<()> {
        match action {
            Action::InputEscape => {
                self.mode = Mode::Normal;
            }
            Action::InputChar('j') | Action::NavigateDown => {
                let len = self.config.tabs.len() + 1; // +1 for default
                self.mode = Mode::TabManager {
                    selected: (selected + 1) % len,
                };
            }
            Action::InputChar('k') | Action::NavigateUp => {
                let len = self.config.tabs.len() + 1;
                let prev = if selected == 0 { len - 1 } else { selected - 1 };
                self.mode = Mode::TabManager { selected: prev };
            }
            Action::Select => {
                // Switch active tab to the selected one
                let tabs = self.config.ordered_tabs();
                if let Some(&tab) = tabs.get(selected) {
                    self.active_tab = tab.map(|s| s.to_string());
                    self.recompute_visible();
                    self.mark_dirty();
                }
                self.mode = Mode::Normal;
            }
            Action::InputChar('a') => {
                self.mode = Mode::Input {
                    context: InputContext::AddTab,
                    state: InputState::new("tab name: "),
                };
            }
            Action::InputChar('r') => {
                if selected == 0 {
                    self.set_status("Cannot rename default tab");
                    return Ok(());
                }
                let tab_idx = selected - 1;
                if let Some(name) = self.config.tabs.get(tab_idx) {
                    let name = name.clone();
                    self.mode = Mode::Input {
                        context: InputContext::RenameTab { tab_idx },
                        state: InputState::with_value("new name: ", name),
                    };
                }
            }
            Action::InputChar('d') => {
                if selected == 0 {
                    self.set_status("Cannot delete default tab");
                    return Ok(());
                }
                let tab_idx = selected - 1;
                if let Some(name) = self.config.tabs.get(tab_idx) {
                    let name = name.clone();
                    let count = self
                        .config
                        .projects
                        .iter()
                        .filter(|p| p.tab.as_deref() == Some(&name))
                        .count();
                    let msg = if count > 0 {
                        format!("Delete tab '{}'? {} projects move to default", name, count)
                    } else {
                        format!("Delete tab '{}'?", name)
                    };
                    self.mode = Mode::Confirm {
                        message: msg,
                        pending: PendingAction::DeleteTab { tab_idx },
                    };
                }
            }
            Action::InputChar('J') => {
                // Reorder: move tab down (later in list)
                if selected == 0 {
                    return Ok(());
                }
                let idx = selected - 1;
                if idx + 1 < self.config.tabs.len() {
                    self.config.tabs.swap(idx, idx + 1);
                    self.mode = Mode::TabManager {
                        selected: selected + 1,
                    };
                    self.config.save()?;
                }
            }
            Action::InputChar('K') => {
                // Reorder: move tab up (earlier in list)
                if selected == 0 {
                    return Ok(());
                }
                let idx = selected - 1;
                if idx > 0 {
                    self.config.tabs.swap(idx - 1, idx);
                    self.mode = Mode::TabManager {
                        selected: selected - 1,
                    };
                    self.config.save()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn move_project_to_adjacent_tab(&mut self, pi: usize, dir: isize) -> Result<()> {
        let proj_path = self.workspace.projects[pi].path.clone();
        let proj_name = self.workspace.projects[pi].name.clone();
        let tabs = self.config.ordered_tabs();
        let current_tab = self
            .config
            .projects
            .iter()
            .find(|c| c.path == proj_path)
            .and_then(|c| c.tab.as_deref());
        let cur_idx = tabs
            .iter()
            .position(|t| t.as_deref() == current_tab)
            .unwrap_or(0);
        let target_idx = (cur_idx as isize + dir).rem_euclid(tabs.len() as isize) as usize;
        let target_tab = tabs[target_idx].map(|s| s.to_string());
        let target_name = target_tab.clone().unwrap_or_else(|| "default".to_string());
        self.config.move_project_tab(&proj_path, target_tab);
        self.config.save()?;
        self.mode = Mode::Normal;
        self.recompute_visible();
        self.set_status(format!("Moved '{}' to '{}'", proj_name, target_name));
        Ok(())
    }

    fn git_worktree_path(&self, pi: usize, wi: usize) -> Option<std::path::PathBuf> {
        self.workspace
            .projects
            .get(pi)?
            .worktrees
            .get(wi)
            .map(|wt| wt.path.clone())
    }

    /// Spawn a background git operation. Closes the current popup mode immediately.
    fn spawn_git_op<F>(&mut self, pi: usize, wi: usize, op_name: &str, f: F)
    where
        F: FnOnce(&std::path::Path) -> anyhow::Result<String> + Send + 'static,
    {
        if self.is_busy() {
            self.set_status("Operation in progress");
            return;
        }
        let Some(path) = self.git_worktree_path(pi, wi) else {
            self.set_status("Worktree not found");
            return;
        };
        self.mode = Mode::Normal;
        let label = op_name.to_string();
        self.spawn_bg(label.clone(), move || {
            let out = f(&path).map_err(|e| anyhow::anyhow!("{} failed: {}", label, e))?;
            let msg = format!("{}: {}", label, first_line(&out));
            Ok(BgOutcome::GitOp { pi, wi, msg })
        });
    }

    fn do_git_pull(&mut self, pi: usize, wi: usize) {
        self.spawn_git_op(pi, wi, "pull", |p| git_ops::pull(p));
    }

    fn do_git_push(&mut self, pi: usize, wi: usize) {
        self.spawn_git_op(pi, wi, "push", |p| git_ops::push(p));
    }

    fn do_git_pull_rebase(&mut self, pi: usize, wi: usize, branch: String) -> Result<()> {
        self.spawn_git_op(pi, wi, "rebase", move |p| git_ops::pull_rebase(p, &branch));
        Ok(())
    }

    fn do_git_merge_from(&mut self, pi: usize, wi: usize, branch: String) -> Result<()> {
        self.spawn_git_op(pi, wi, "merge", move |p| git_ops::merge_from(p, &branch));
        Ok(())
    }

    fn do_git_merge_into(&mut self, pi: usize, wi: usize, branch: String) -> Result<()> {
        self.spawn_git_op(pi, wi, "merge-into", move |p| {
            git_ops::merge_into(p, &branch)
        });
        Ok(())
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// Compute the set of project indices visible in `active_tab`.
/// When config.tabs is empty, all projects are visible (single implicit tab).
fn compute_visible_projects(
    config: &GlobalConfig,
    workspace: &WorkspaceState,
    active_tab: Option<&str>,
) -> HashSet<usize> {
    if config.tabs.is_empty() {
        return (0..workspace.projects.len()).collect();
    }
    workspace
        .projects
        .iter()
        .enumerate()
        .filter_map(|(i, wp)| {
            let tab = config
                .projects
                .iter()
                .find(|c| c.path == wp.path)
                .and_then(|c| c.tab.as_deref());
            if tab == active_tab {
                Some(i)
            } else {
                None
            }
        })
        .collect()
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
        FlatEntry::RoutinesHeader { project_idx } => {
            format!("{} routines", workspace.projects[*project_idx].name).to_lowercase()
        }
        FlatEntry::Routine {
            project_idx,
            routine_idx,
        } => {
            let routine = &workspace.projects[*project_idx].routines[*routine_idx].routine;
            format!(
                "{} {} {} {}",
                routine.name,
                routine.cron,
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

    fn make_git_info() -> GitInfo {
        GitInfo {
            recent_commits: vec![],
            modified_files: vec![],
            ahead: 1,
            behind: 0,
            remote_branch: Some("origin/main".to_string()),
        }
    }

    fn make_test_app(
        config: GlobalConfig,
        workspace: WorkspaceState,
        active_tab: Option<String>,
    ) -> App {
        let visible_projects = compute_visible_projects(&config, &workspace, active_tab.as_deref());
        let cached_flat = flatten_tree_filtered(&workspace, &visible_projects);
        let search_cache = build_search_cache(&workspace, &cached_flat);
        let worktree_index = build_worktree_index(&workspace);
        let last_session_snapshot = wsx_core::cache::collect_session_names(&workspace);
        let (bg_tx, bg_rx) = std::sync::mpsc::channel();
        let (git_local_tx, git_local_rx) = std::sync::mpsc::channel();
        let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
        let (tmux_tx, tmux_rx) = std::sync::mpsc::channel();
        let (_update_tx, update_rx) = std::sync::mpsc::channel();
        let (routine_tx, routine_rx) = std::sync::mpsc::channel();

        App {
            workspace,
            tree_selected: 0,
            tree_scroll: 0,
            tree_visible_height: 20,
            tree_area: Rect::default(),
            preview_area: Rect::default(),
            mode: Mode::Normal,
            config,
            active_tab,
            visible_projects,
            send_command_history: Vec::new(),
            status_message: None,
            status_message_expires: None,
            jobs: Vec::new(),
            spinner_frame: 0,
            bg_tx,
            bg_rx,
            needs_redraw: false,
            force_redraw: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            activity_timer: Timer::new(ACTIVITY_INTERVAL_MS),
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
            last_session_snapshot,
            git_semaphore: GitSemaphore::new(1),
            worktree_index,
            parsed_preview: std::collections::HashMap::new(),
            tmux_tx,
            tmux_rx,
            tmux_refresh_pending: false,
            tmux_refresh_stale: false,
            tmux_activity_pending: false,
            tmux_capture_pending: false,
            pending_deletions: HashSet::new(),
            pending_session_ops: HashMap::new(),
            update_rx,
            update_available: None,
            is_mobile: false,
            scanned_repos: Vec::new(),
            repo_scan_rx: None,
            routine_tx,
            routine_rx,
            routine_refresh_generation: HashMap::new(),
        }
    }

    fn make_project_entry(name: &str, tab: Option<&str>) -> wsx_core::config::global::ProjectEntry {
        wsx_core::config::global::ProjectEntry {
            name: name.to_string(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            tab: tab.map(|tab| tab.to_string()),
            aliases: Default::default(),
        }
    }

    fn routine_view(name: &str) -> wsx_core::routine::ipc::RoutineView {
        wsx_core::routine::ipc::RoutineView {
            routine: wsx_core::routine::Routine {
                name: name.into(),
                cron: "0 9 * * *".into(),
                command: vec!["echo".into(), "{prompt}".into()],
                prompt: "hello".into(),
                enabled: true,
            },
            capabilities: wsx_core::routine::Capabilities::for_running(false),
            next_run_epoch: None,
            latest_run: None,
            recent_runs: Vec::new(),
        }
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
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(app.current_selection(), Selection::Routine(0, 0)));
        assert_eq!(
            app.status_message.as_deref(),
            Some("Routine 'morning' not deleted: stale config revision")
        );
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
                generation,
                response: wsx_core::routine::ipc::Response::Routines {
                    revision: 1,
                    routines: vec![routine_view("fresh")],
                },
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
                generation,
                response: wsx_core::routine::ipc::Response::Routines {
                    revision: 0,
                    routines: vec![routine_view("stale")],
                },
            })
            .unwrap();
        app.drain_async_results();
        assert_eq!(app.workspace.projects[1].routines[0].routine.name, "fresh");
        assert_eq!(app.workspace.projects[1].routine_revision, 1);
    }

    #[test]
    fn empty_project_has_no_routines_header_but_can_open_first_creation_form() {
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
            Mode::RoutineEditor {
                project_idx: 0,
                original_name: None,
                can_rename: true,
                ..
            }
        ));
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
                generation,
                response: wsx_core::routine::ipc::Response::Routines {
                    revision: 2,
                    routines: Vec::new(),
                },
            })
            .unwrap();

        app.drain_async_results();

        assert!(matches!(app.mode, Mode::Normal));
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
    fn given_project_removed_from_tab_when_flat_rebuilt_then_shifted_project_does_not_leak() {
        let config = GlobalConfig {
            tabs: vec!["work".to_string(), "personal".to_string()],
            projects: vec![
                make_project_entry("work-a", Some("work")),
                make_project_entry("personal", Some("personal")),
                make_project_entry("work-b", Some("work")),
            ],
            exclude_worktree_paths: vec![],
            mobile_detach_key: None,
        };
        let workspace = WorkspaceState {
            projects: vec![
                make_project("work-a"),
                make_project("personal"),
                make_project("work-b"),
            ],
        };
        let visible_projects = compute_visible_projects(&config, &workspace, Some("work"));

        let (bg_tx, bg_rx) = std::sync::mpsc::channel();
        let (git_local_tx, git_local_rx) = std::sync::mpsc::channel();
        let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
        let (tmux_tx, tmux_rx) = std::sync::mpsc::channel();
        let (_update_tx, update_rx) = std::sync::mpsc::channel();
        let (routine_tx, routine_rx) = std::sync::mpsc::channel();
        let mut app = App {
            workspace,
            tree_selected: 0,
            tree_scroll: 0,
            tree_visible_height: 20,
            tree_area: Rect::default(),
            preview_area: Rect::default(),
            mode: Mode::Normal,
            config,
            active_tab: Some("work".to_string()),
            visible_projects,
            send_command_history: Vec::new(),
            status_message: None,
            status_message_expires: None,
            jobs: Vec::new(),
            spinner_frame: 0,
            bg_tx,
            bg_rx,
            needs_redraw: false,
            force_redraw: false,
            fast_timer: Timer::new(FAST_INTERVAL_MS),
            activity_timer: Timer::new(ACTIVITY_INTERVAL_MS),
            slow_timer: Timer::new(SLOW_INTERVAL_MS),
            cached_flat: Vec::new(),
            flat_dirty: false,
            search_cache: Vec::new(),
            git_local_tx,
            git_local_rx,
            git_local_pending: HashSet::new(),
            fetch_tx,
            fetch_rx,
            fetch_pending: HashSet::new(),
            cache_dirty: false,
            last_session_snapshot: HashMap::new(),
            git_semaphore: GitSemaphore::new(1),
            worktree_index: std::collections::HashMap::new(),
            parsed_preview: std::collections::HashMap::new(),
            tmux_tx,
            tmux_rx,
            tmux_refresh_pending: false,
            tmux_refresh_stale: false,
            tmux_activity_pending: false,
            tmux_capture_pending: false,
            pending_deletions: HashSet::new(),
            pending_session_ops: HashMap::new(),
            update_rx,
            update_available: None,
            is_mobile: false,
            scanned_repos: Vec::new(),
            repo_scan_rx: None,
            routine_tx,
            routine_rx,
            routine_refresh_generation: HashMap::new(),
        };

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

        app.spawn_git_local_force(path.clone(), "main".to_string());
        assert!(app.git_local_pending.contains(&path));
        assert!(app.workspace.projects[0].worktrees[0].git_info.is_some());
    }

    // Regression guard for the attention_candidates logic (commit c118ea2).
    // Ensures active sessions never trigger attention, and muted/suppressed are ignored.

    fn make_sess(
        muted: bool,
        has_activity: bool,
        foreground: wsx_core::model::workspace::ForegroundKind,
    ) -> wsx_core::model::workspace::SessionInfo {
        wsx_core::model::workspace::SessionInfo {
            name: String::new(),
            display_name: String::new(),
            has_activity,
            pane_capture: None,
            last_activity: None,
            foreground,
            is_running_wsx: false,
            muted,
        }
    }

    #[test]
    fn attention_bell_inactive_triggers() {
        let s = make_sess(
            false,
            true,
            wsx_core::model::workspace::ForegroundKind::Shell,
        );
        assert!(session_needs_attention(&s));
    }

    #[test]
    fn attention_muted_does_not_trigger() {
        let s = make_sess(
            true,
            true,
            wsx_core::model::workspace::ForegroundKind::Agent,
        );
        assert!(!session_needs_attention(&s));
    }

    #[test]
    fn attention_bell_active_still_triggers() {
        // bell always needs attention regardless of foreground
        let s = make_sess(
            false,
            true,
            wsx_core::model::workspace::ForegroundKind::Shell,
        );
        assert!(session_needs_attention(&s));
    }

    // ── filter_pending_session_ops ─────────────────────────────────────────────

    fn sess(name: &str) -> (String, std::path::PathBuf) {
        (name.to_string(), std::path::PathBuf::from("/tmp"))
    }

    #[test]
    fn pending_ops_empty_returns_sessions_unchanged() {
        let mut pending = HashMap::new();
        let input = vec![sess("a"), sess("b")];
        let out = filter_pending_session_ops(&mut pending, input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn killed_session_suppressed_while_still_alive() {
        let mut pending = HashMap::new();
        pending.insert("sess-a".to_string(), SessionOp::Killed);
        let input = vec![sess("sess-a"), sess("sess-b")];
        let out = filter_pending_session_ops(&mut pending, input);
        // sess-a still in tmux → suppressed; sess-b passes through
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            !names.contains(&"sess-a"),
            "killed session should be filtered"
        );
        assert!(
            names.contains(&"sess-b"),
            "other sessions should pass through"
        );
        // Entry stays because tmux still reports it (kill not confirmed)
        assert!(pending.contains_key("sess-a"));
    }

    #[test]
    fn killed_session_entry_cleared_when_tmux_confirms() {
        let mut pending = HashMap::new();
        pending.insert("sess-dead".to_string(), SessionOp::Killed);
        // tmux no longer reports sess-dead
        let input = vec![sess("sess-other")];
        let out = filter_pending_session_ops(&mut pending, input);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"sess-other"));
        // Entry cleared (kill confirmed)
        assert!(!pending.contains_key("sess-dead"));
    }

    #[test]
    fn renamed_session_remapped_to_new_name() {
        let mut pending = HashMap::new();
        pending.insert(
            "old-name".to_string(),
            SessionOp::Renamed {
                new_name: "new-name".to_string(),
            },
        );
        // tmux still reports old name (rename not yet visible in snapshot)
        let input = vec![sess("old-name"), sess("other")];
        let out = filter_pending_session_ops(&mut pending, input);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"old-name"), "old name should be remapped");
        assert!(names.contains(&"new-name"), "new name should appear");
        assert!(names.contains(&"other"), "other sessions pass through");
    }

    #[test]
    fn renamed_entry_cleared_when_new_name_confirmed() {
        let mut pending = HashMap::new();
        pending.insert(
            "old-name".to_string(),
            SessionOp::Renamed {
                new_name: "new-name".to_string(),
            },
        );
        // tmux now reports the new name (rename confirmed)
        let input = vec![sess("new-name"), sess("other")];
        let out = filter_pending_session_ops(&mut pending, input);
        // Entry cleared; new-name passes through unchanged
        assert!(!pending.contains_key("old-name"));
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"new-name"));
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
