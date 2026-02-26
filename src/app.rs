// App state machine and event loop.
// ref: ratatui app patterns — https://ratatui.rs/concepts/application-patterns/

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use ratatui::layout::Rect;

use crate::{
    action::Action,
    config::global::GlobalConfig,
    event::poll_event,
    git::{info as git_info, worktree as git_worktree},
    model::workspace::{FlatEntry, Selection, WorkspaceState, flatten_tree},
    ops,
    tmux::{capture, monitor, session},
    tui::{self, Tui},
    ui::{self, input::InputState},
};

// ── Timer ─────────────────────────────────────────────────────────────────────

struct Timer {
    last: Instant,
    interval: Duration,
}

impl Timer {
    fn new(interval_ms: u64) -> Self {
        Self { last: Instant::now(), interval: Duration::from_millis(interval_ms) }
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
const CAPTURE_INTERVAL_MS: u64 = 500;
const RESCAN_INTERVAL_MS: u64 = 2000;
const ACTIVITY_INTERVAL_MS: u64 = 1000;
pub use ops::IDLE_SECS;

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
    Help,
    Search {
        query: String,
        match_idx: usize,
    },
}

pub enum InputContext {
    AddProject,
    AddWorktree { project_idx: usize },
    AddSession { project_idx: usize, worktree_idx: usize },
    AddSessionCmd { project_idx: usize, worktree_idx: usize, session_name: String },
    OpenRun { project_idx: usize, worktree_idx: usize },
    SetAlias { project_idx: usize, worktree_idx: usize },
    RenameSession { project_idx: usize, worktree_idx: usize, session_idx: usize },
}

impl InputContext {
    pub fn title(&self) -> &'static str {
        match self {
            InputContext::AddProject => "Add Project",
            InputContext::AddWorktree { .. } => "Add Worktree",
            InputContext::AddSession { .. } => "New Session — name",
            InputContext::AddSessionCmd { .. } => "New Session — command",
            InputContext::OpenRun { .. } => "Open (ephemeral run)",
            InputContext::SetAlias { .. } => "Set Alias",
            InputContext::RenameSession { .. } => "Rename Session",
        }
    }
}

pub enum PendingAction {
    DeleteProject { project_idx: usize },
    DeleteWorktree { project_idx: usize, worktree_idx: usize },
    DeleteSession { project_idx: usize, worktree_idx: usize, session_idx: usize },
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
    pub status_message: Option<String>,
    status_message_expires: Option<Instant>,
    pub loading: bool,
    needs_redraw: bool,
    capture_timer: Timer,
    rescan_timer: Timer,
    activity_timer: Timer,
    cached_flat: Vec<FlatEntry>,
    flat_dirty: bool,
}

impl App {
    pub fn new() -> Result<Self> {
        let config = GlobalConfig::load()?;
        let mut workspace = ops::load_workspace(&config);
        let tree_selected = crate::cache::apply_cache(&mut workspace);
        let cached_flat = flatten_tree(&workspace);

        Ok(Self {
            workspace,
            tree_selected,
            tree_scroll: 0,
            tree_visible_height: 20,
            tree_area: Rect::default(),
            preview_area: Rect::default(),
            mode: Mode::Normal,
            config,
            status_message: None,
            status_message_expires: None,
            loading: false,
            needs_redraw: true,
            capture_timer: Timer::new(CAPTURE_INTERVAL_MS),
            rescan_timer: Timer::new(RESCAN_INTERVAL_MS),
            activity_timer: Timer::new(ACTIVITY_INTERVAL_MS),
            cached_flat,
            flat_dirty: false,
        })
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.status_message_expires = Some(Instant::now() + Duration::from_secs(4));
    }

    fn ensure_flat(&mut self) {
        if self.flat_dirty {
            self.cached_flat = flatten_tree(&self.workspace);
            self.flat_dirty = false;
        }
    }

    fn rebuild_flat(&mut self) {
        self.flat_dirty = true;
        self.ensure_flat();
    }

    fn flat(&self) -> &[FlatEntry] {
        debug_assert!(!self.flat_dirty, "flat() called with dirty cache");
        &self.cached_flat
    }

    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        loop {
            if self.needs_redraw {
                self.ensure_flat();
                tui::draw_sync(terminal, |frame| ui::render(frame, self))?;
                self.needs_redraw = false;
            }

            let in_input = matches!(self.mode, Mode::Input { .. } | Mode::Search { .. });
            if let Some(action) = poll_event(Duration::from_millis(TICK_MS), in_input)? {
                if action == Action::Quit && matches!(self.mode, Mode::Normal) {
                    crate::cache::save_cache(&self.workspace, self.tree_selected);
                    break;
                }
                self.needs_redraw = true;
                if let Err(e) = self.dispatch(action, terminal) {
                    self.set_status(format!("Error: {}", e));
                }
            } else {
                self.tick()?;
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if let Some(expires) = self.status_message_expires {
            if Instant::now() >= expires {
                self.status_message = None;
                self.status_message_expires = None;
                self.needs_redraw = true;
            }
        }

        if self.rescan_timer.ready() {
            if let Err(e) = self.refresh_all() {
                self.set_status(format!("Refresh error: {}", e));
            }
            self.activity_timer.last = Instant::now(); // rescan subsumes activity check
            self.needs_redraw = true;
        } else if self.activity_timer.ready() {
            if self.refresh_activity() {
                self.needs_redraw = true;
            }
        }

        if self.capture_timer.ready() {
            self.refresh_captures();
        }

        Ok(())
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        let sessions_with_paths = session::list_sessions_with_paths();
        let activity = monitor::session_activity();
        ops::refresh_workspace(&mut self.workspace, &self.config, &sessions_with_paths, &activity);
        self.rebuild_flat();
        self.clamp_selected();
        crate::cache::save_cache(&self.workspace, self.tree_selected);
        Ok(())
    }

    fn refresh_activity(&mut self) -> bool {
        let activity = monitor::session_activity();
        ops::update_activity(&mut self.workspace, &activity)
    }

    fn refresh_captures(&mut self) {
        let sel = self.current_selection();

        // Load git info when a worktree or session is selected
        let (pi, wi) = match sel {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
            _ => return,
        };

        let git_fetch = self.workspace.worktree(pi, wi)
            .filter(|w| w.git_info.is_none())
            .map(|w| w.path.clone());

        if let Some(path) = git_fetch {
            let default_branch = self.workspace.projects.get(pi)
                .map(|p| p.default_branch.clone())
                .unwrap_or_else(|| "main".to_string());

            if let Some(gi) = git_info::get_git_info(&path, &default_branch) {
                if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
                    wt.git_info = Some(gi);
                    self.needs_redraw = true;
                }
            }
        }

        // Capture pane for selected session
        if let Selection::Session(pi, wi, si) = sel {
            let sess_name = self.workspace.session(pi, wi, si).map(|s| s.name.clone());

            if let Some(name) = sess_name {
                if session::session_exists(&name) {
                    if let Some(raw) = capture::capture_pane(&name) {
                        let trimmed = capture::trim_capture(&raw);
                        if let Some(s) = self.workspace.session_mut(pi, wi, si) {
                            if s.pane_capture.as_deref() != Some(&trimmed) {
                                s.pane_capture = Some(trimmed);
                                self.needs_redraw = true;
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn current_selection(&self) -> Selection {
        self.workspace.get_selection(self.tree_selected, self.flat())
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
            Some(FlatEntry::Worktree { project_idx: pi, worktree_idx: wi }) => {
                if !self.workspace.projects[pi].worktrees[wi].expanded {
                    self.workspace.projects[pi].worktrees[wi].expanded = true;
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].worktrees[wi].sessions.is_empty() {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            _ => {}
        }
    }

    fn jump_project(&mut self, dir: isize) {
        let flat = self.flat();
        let current = self.tree_selected;
        let target = if dir > 0 {
            flat.iter().enumerate()
                .find(|(i, e)| *i > current && matches!(e, FlatEntry::Project { .. }))
                .map(|(i, _)| i)
        } else {
            flat.iter().enumerate()
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
            self.tree_selected, visible, self.tree_scroll,
        );
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
                let path = self.workspace.projects.get(pi).map(|p| p.path.join(".gtrignore"));
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
                Action::Select | Action::InputEscape | Action::Quit | Action::EnterMove => {
                    self.sync_config_project_order();
                    self.config.save()?;
                    self.mode = Mode::Normal;
                }
                _ => {}
            }
            return Ok(());
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
            Mode::Config { .. } | Mode::Move { .. } => unreachable!(),
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
            Action::OpenRun => self.action_open_run()?,
            Action::Delete => self.action_delete()?,
            Action::Clean => self.action_clean()?,
            Action::Edit => self.action_edit()?,
            Action::SetAlias => self.action_set_alias()?,
            Action::Refresh => self.refresh_all()?,
            Action::Help => { self.mode = Mode::Help; }
            Action::NextAttention => self.action_next_attention(1),
            Action::PrevAttention => self.action_next_attention(-1),
            Action::DismissAttention => self.action_dismiss_attention(),
            Action::EnterMove => self.action_enter_move(),
            Action::JumpProjectDown => self.jump_project(1),
            Action::JumpProjectUp => self.jump_project(-1),
            Action::SearchStart => {
                self.mode = Mode::Search { query: String::new(), match_idx: 0 };
            }
            Action::MouseClick { col, row } => self.handle_mouse_click(col, row, terminal)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_click(&mut self, col: u16, row: u16, terminal: &mut Tui) -> Result<()> {
        let ta = self.tree_area;
        let pa = self.preview_area;
        if col >= ta.x && col < ta.x + ta.width && row >= ta.y && row < ta.y + ta.height {
            // Content starts after top border (y+1), ends before bottom border (y+height-1)
            let content_top = ta.y + 1;
            let content_bottom = ta.y + ta.height.saturating_sub(1);
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
        } else if col >= pa.x && col < pa.x + pa.width && row >= pa.y && row < pa.y + pa.height {
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
            Action::InputTab | Action::NavigateDown => {
                if let Mode::Input { context: InputContext::AddProject, state } = &mut self.mode {
                    state.select_next();
                }
            }
            Action::NavigateUp => {
                if let Mode::Input { context: InputContext::AddProject, state } = &mut self.mode {
                    state.select_prev();
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn dispatch_confirm(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        match action {
            Action::ConfirmYes | Action::Select => self.confirm_action(terminal)?,
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
                if let Mode::Search { ref mut query, ref mut match_idx } = self.mode {
                    query.push(c);
                    *match_idx = 0;
                }
                self.search_apply();
            }
            Action::InputBackspace => {
                if let Mode::Search { ref mut query, ref mut match_idx } = self.mode {
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

    fn search_text(&self, entry: &FlatEntry) -> String {
        match entry {
            FlatEntry::Project { idx } =>
                self.workspace.projects[*idx].name.to_lowercase(),
            FlatEntry::Worktree { project_idx: pi, worktree_idx: wi } => {
                let wt = &self.workspace.projects[*pi].worktrees[*wi];
                let mut s = wt.branch.to_lowercase();
                if let Some(a) = &wt.alias { s.push(' '); s.push_str(&a.to_lowercase()); }
                s.push(' '); s.push_str(&wt.name.to_lowercase());
                s
            }
            FlatEntry::Session { project_idx: pi, worktree_idx: wi, session_idx: si } =>
                self.workspace.projects[*pi].worktrees[*wi].sessions[*si]
                    .display_name.to_lowercase(),
        }
    }

    fn search_matches(&self, query: &str) -> Vec<usize> {
        if query.is_empty() { return vec![]; }
        let q = query.to_lowercase();
        self.flat().iter().enumerate()
            .filter(|(_, e)| self.search_text(e).contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move cursor to first match; exit search when narrowed to one result.
    fn search_apply(&mut self) {
        let query = match &self.mode {
            Mode::Search { query, .. } => query.clone(),
            _ => return,
        };
        let matches = self.search_matches(&query);
        if matches.is_empty() { return; }
        self.tree_selected = matches[0];
        self.update_scroll();
        if matches.len() == 1 {
            self.mode = Mode::Normal;
        }
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
        if let Mode::Search { ref mut match_idx, .. } = self.mode {
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
                self.workspace.projects[pi].worktrees[wi].expanded = !self.workspace.projects[pi].worktrees[wi].expanded;
                self.rebuild_flat();
                self.clamp_selected();
            }
            Selection::None => {}
        }
        Ok(())
    }

    fn attach_to_session(&self, name: &str, terminal: &mut Tui) -> Result<()> {
        session::apply_session_defaults(name);
        match session::attach_session_cmd(name) {
            session::AttachCommand::SwitchClient(n) => session::switch_client(&n)?,
            session::AttachCommand::Attach(n) => {
                tui::with_raw_mode_disabled(terminal, || session::attach_foreground(&n))?;
            }
        }
        Ok(())
    }

    fn attach_session(&mut self, pi: usize, wi: usize, si: usize, terminal: &mut Tui) -> Result<()> {
        let name = self.workspace.session(pi, wi, si).map(|s| s.name.clone());

        let Some(name) = name else {
            self.set_status("Session not found");
            return Ok(());
        };

        self.attach_to_session(&name, terminal)
    }

    fn action_add_project(&mut self) -> Result<()> {
        self.mode = Mode::Input {
            context: InputContext::AddProject,
            state: InputState::new_path("path: ", "~/".to_string()),
        };
        Ok(())
    }

    fn action_add_worktree(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi) | Selection::Worktree(pi, _) | Selection::Session(pi, _, _) => pi,
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
            context: InputContext::AddSession { project_idx: pi, worktree_idx: wi },
            state: InputState::new("name (optional): "),
        };
        Ok(())
    }

    fn action_open_run(&mut self) -> Result<()> {
        let (pi, wi) = match self.current_selection() {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
            _ => {
                self.set_status("Select a worktree first");
                return Ok(());
            }
        };
        self.mode = Mode::Input {
            context: InputContext::OpenRun { project_idx: pi, worktree_idx: wi },
            state: InputState::new("run: "),
        };
        Ok(())
    }

    fn action_delete(&mut self) -> Result<()> {
        match self.current_selection() {
            Selection::Session(pi, wi, si) => {
                let display_name = self.workspace.projects[pi].worktrees[wi].sessions[si].display_name.clone();
                self.mode = Mode::Confirm {
                    message: format!("Kill session '{}'?", display_name),
                    pending: PendingAction::DeleteSession { project_idx: pi, worktree_idx: wi, session_idx: si },
                };
            }
            Selection::Worktree(pi, wi) => {
                let wt = &self.workspace.projects[pi].worktrees[wi];
                if wt.is_main {
                    self.set_status("Cannot delete main worktree");
                    return Ok(());
                }
                let merged = git_worktree::is_branch_merged(
                    &self.workspace.projects[pi].path,
                    &wt.branch,
                    &self.workspace.projects[pi].default_branch,
                );
                let msg = if merged {
                    format!("Delete worktree '{}'?", wt.name)
                } else {
                    format!("Delete UNMERGED worktree '{}'? Changes will be lost!", wt.name)
                };
                self.mode = Mode::Confirm {
                    message: msg,
                    pending: PendingAction::DeleteWorktree { project_idx: pi, worktree_idx: wi },
                };
            }
            Selection::Project(pi) => {
                let name = self.workspace.projects[pi].name.clone();
                self.mode = Mode::Confirm {
                    message: format!("Unregister project '{}'? (files not deleted)", name),
                    pending: PendingAction::DeleteProject { project_idx: pi },
                };
            }
            Selection::None => {}
        }
        Ok(())
    }

    fn action_clean(&mut self) -> Result<()> {
        match self.current_selection() {
            Selection::Worktree(pi, wi) => {
                let (repo, wt_path, branch, default_branch, is_main, session_names) = {
                    let p = &self.workspace.projects[pi];
                    let wt = &p.worktrees[wi];
                    let names: Vec<String> = wt.sessions.iter().map(|s| s.name.clone()).collect();
                    (p.path.clone(), wt.path.clone(), wt.branch.clone(), p.default_branch.clone(), wt.is_main, names)
                };
                if is_main {
                    self.set_status("Cannot clean main worktree");
                    return Ok(());
                }
                if !git_worktree::is_branch_merged(&repo, &branch, &default_branch) {
                    self.set_status(format!("'{}' not merged into {}", branch, default_branch));
                    return Ok(());
                }
                ops::delete_worktree(&repo, &wt_path, &branch, &session_names)?;
                self.workspace.projects[pi].worktrees.remove(wi);
                self.rebuild_flat();
                self.clamp_selected();
                self.set_status(format!("Cleaned: {}", branch));
            }
            Selection::Project(pi) | Selection::Session(pi, _, _) => {
                let (path, branch) = {
                    let p = &self.workspace.projects[pi];
                    (p.path.clone(), p.default_branch.clone())
                };
                let removed = git_worktree::clean_merged(&path, &branch)?;
                self.set_status(if removed.is_empty() {
                    "No merged worktrees to clean".into()
                } else {
                    format!("Cleaned: {}", removed.join(", "))
                });
                self.refresh_all()?;
            }
            Selection::None => {
                let snapshots: Vec<_> = self.workspace.projects
                    .iter()
                    .map(|p| (p.path.clone(), p.default_branch.clone()))
                    .collect();
                let mut total = 0usize;
                for (path, branch) in snapshots {
                    if let Ok(r) = git_worktree::clean_merged(&path, &branch) {
                        total += r.len();
                    }
                }
                self.set_status(format!("Cleaned {} merged worktrees", total));
                self.refresh_all()?;
            }
        }
        Ok(())
    }

    fn action_edit(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi) | Selection::Worktree(pi, _) | Selection::Session(pi, _, _) => pi,
            Selection::None => {
                self.set_status("Select a project or worktree");
                return Ok(());
            }
        };
        self.mode = Mode::Config { project_idx: pi };
        Ok(())
    }

    fn attention_candidates(&self) -> Vec<usize> {
        self.flat().iter().enumerate()
            .filter_map(|(i, entry)| {
                let FlatEntry::Session { project_idx: pi, worktree_idx: wi, session_idx: si } = entry else {
                    return None;
                };
                let sess = self.workspace.session(*pi, *wi, *si)?;
                let currently_active = sess.last_activity
                    .map(|t| t.elapsed().as_secs() < IDLE_SECS)
                    .unwrap_or(false);
                let needs_attention = !sess.muted && !currently_active
                    && sess.has_running_app && !sess.running_app_suppressed;
                if needs_attention { Some(i) } else { None }
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
            candidates.iter()
                .find(|&&i| i > self.tree_selected)
                .or_else(|| candidates.first())
                .copied()
                .unwrap()
        } else {
            candidates.iter().rev()
                .find(|&&i| i < self.tree_selected)
                .or_else(|| candidates.last())
                .copied()
                .unwrap()
        };

        // ensure parent project + worktree are expanded so the session is visible
        if let Some(FlatEntry::Session { project_idx: pi, worktree_idx: wi, .. }) = self.flat().get(next).cloned() {
            self.workspace.projects[pi].expanded = true;
            self.workspace.projects[pi].worktrees[wi].expanded = true;
            self.rebuild_flat();
        }

        self.tree_selected = next;
        self.update_scroll();
    }

    fn action_dismiss_attention(&mut self) {
        if let Selection::Session(pi, wi, si) = self.current_selection() {
            if let Some(sess) = self.workspace.session_mut(pi, wi, si) {
                if sess.has_running_app && !sess.running_app_suppressed {
                    sess.running_app_suppressed = true;
                    self.set_status("Dismissed");
                    return;
                }
                // Idle session — toggle mute
                sess.muted = !sess.muted;
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
                    .alias.clone().unwrap_or_default();
                self.mode = Mode::Input {
                    context: InputContext::SetAlias { project_idx: pi, worktree_idx: wi },
                    state: InputState::with_value("alias: ", current),
                };
            }
            Selection::Session(pi, wi, si) => {
                let current = self.workspace.projects[pi].worktrees[wi].sessions[si].display_name.clone();
                self.mode = Mode::Input {
                    context: InputContext::RenameSession { project_idx: pi, worktree_idx: wi, session_idx: si },
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

    fn confirm_input(&mut self, terminal: &mut Tui) -> Result<()> {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        if let Mode::Input { context, state } = mode {
            let value = state.value().trim().to_string();
            match context {
                InputContext::AddProject => self.do_register_project(ops::expand_path(&value))?,
                InputContext::AddWorktree { project_idx } => {
                    if !value.is_empty() { self.do_create_worktree(project_idx, value)?; }
                }
                InputContext::AddSession { project_idx, worktree_idx } => {
                    // Step 1: got name, now ask for command
                    self.mode = Mode::Input {
                        context: InputContext::AddSessionCmd { project_idx, worktree_idx, session_name: value },
                        state: InputState::new("command (optional): "),
                    };
                    return Ok(());
                }
                InputContext::AddSessionCmd { project_idx, worktree_idx, session_name } => {
                    let cmd = if value.is_empty() { None } else { Some(value) };
                    self.do_create_session(project_idx, worktree_idx, session_name, cmd)?;
                }
                InputContext::OpenRun { project_idx, worktree_idx } => {
                    if !value.is_empty() { self.do_open_run(project_idx, worktree_idx, value, terminal)?; }
                }
                InputContext::SetAlias { project_idx, worktree_idx } => {
                    self.do_apply_alias(project_idx, worktree_idx, value)?;
                }
                InputContext::RenameSession { project_idx, worktree_idx, session_idx } => {
                    if !value.is_empty() { self.do_rename_session(project_idx, worktree_idx, session_idx, value)?; }
                }
            }
        }
        Ok(())
    }

    fn confirm_action(&mut self, terminal: &mut Tui) -> Result<()> {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        if let Mode::Confirm { pending, .. } = mode {
            self.loading = true;
            tui::draw_sync(terminal, |frame| ui::render(frame, self))?;
            let result = match pending {
                PendingAction::DeleteProject { project_idx } => self.do_delete_project(project_idx),
                PendingAction::DeleteWorktree { project_idx, worktree_idx } => {
                    self.do_delete_worktree(project_idx, worktree_idx)
                }
                PendingAction::DeleteSession { project_idx, worktree_idx, session_idx } => {
                    self.do_delete_session(project_idx, worktree_idx, session_idx)
                }
            };
            self.loading = false;
            result?;
        }
        Ok(())
    }

    // ── Dispatch to ops ───────────────────────────────────────────────────────

    fn do_register_project(&mut self, path: PathBuf) -> Result<()> {
        let project = ops::register_project(path, &mut self.config)?;
        self.workspace.projects.push(project);
        self.rebuild_flat();
        self.config.save()?;
        self.set_status("Project registered");
        Ok(())
    }

    fn do_create_worktree(&mut self, pi: usize, branch: String) -> Result<()> {
        let (repo_path, default_branch, proj_config) = {
            let p = &self.workspace.projects[pi];
            (p.path.clone(), p.default_branch.clone(), p.config.clone().unwrap_or_default())
        };
        let (_wt_path, warning) = ops::create_worktree(&repo_path, &default_branch, &proj_config, &branch)?;
        if let Some(w) = warning {
            self.set_status(w);
        }
        self.refresh_all()?;
        self.set_status(format!("Created worktree: {}", branch));
        Ok(())
    }

    fn do_create_session(&mut self, pi: usize, wi: usize, session_name: String, command: Option<String>) -> Result<()> {
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug())
        };
        let explicit_name = if session_name.is_empty() { None } else { Some(session_name) };
        let (_tmux_name, display_name) = ops::create_session(&proj_name, &wt_slug, &wt_path, explicit_name, command)?;
        self.set_status(format!("Session '{}' created", display_name));
        self.refresh_all()?;
        // Auto-expand the worktree so the new session is visible
        if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
            wt.expanded = true;
        }
        Ok(())
    }

    fn do_open_run(&mut self, pi: usize, wi: usize, command: String, terminal: &mut Tui) -> Result<()> {
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug())
        };
        let name = ops::create_ephemeral_session(&proj_name, &wt_slug, &wt_path, &command)?;
        self.attach_to_session(&name, terminal)
    }

    fn do_delete_worktree(&mut self, pi: usize, wi: usize) -> Result<()> {
        let (repo, path, branch, session_names) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            let names: Vec<String> = wt.sessions.iter().map(|s| s.name.clone()).collect();
            (p.path.clone(), wt.path.clone(), wt.branch.clone(), names)
        };
        ops::delete_worktree(&repo, &path, &branch, &session_names)?;
        self.workspace.projects[pi].worktrees.remove(wi);
        self.rebuild_flat();
        self.clamp_selected();
        self.set_status(format!("Deleted: {}", branch));
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
        ops::delete_session(&tmux_name)?;
        self.workspace.projects[pi].worktrees[wi].sessions.remove(si);
        self.rebuild_flat();
        self.clamp_selected();
        self.set_status(format!("Killed session: {}", display_name));
        Ok(())
    }

    fn do_apply_alias(&mut self, pi: usize, wi: usize, alias: String) -> Result<()> {
        let branch = self.workspace.projects[pi].worktrees[wi].branch.clone();
        let proj_path = self.workspace.projects[pi].path.clone();

        ops::set_alias(&mut self.config, &proj_path, &branch, &alias);
        self.config.save()?;

        let wt = &mut self.workspace.projects[pi].worktrees[wi];
        wt.alias = if alias.is_empty() { None } else { Some(alias.clone()) };

        self.set_status(if alias.is_empty() {
            format!("Alias cleared for '{}'", branch)
        } else {
            format!("Alias '{}' set for '{}'", alias, branch)
        });
        Ok(())
    }

    fn do_rename_session(&mut self, pi: usize, wi: usize, si: usize, new_name: String) -> Result<()> {
        let old_tmux_name = self.workspace.projects[pi].worktrees[wi].sessions[si].name.clone();
        let proj_name = self.workspace.projects[pi].name.clone();
        let wt_slug = self.workspace.projects[pi].worktrees[wi].session_slug();
        let new_tmux_name = format!("{}-{}-{}", proj_name, wt_slug, new_name);
        ops::rename_session(&old_tmux_name, &new_tmux_name)?;
        let sess = &mut self.workspace.projects[pi].worktrees[wi].sessions[si];
        sess.name = new_tmux_name;
        sess.display_name = new_name.clone();
        self.set_status(format!("Session renamed to '{}'", new_name));
        Ok(())
    }

    // ── Move project ──────────────────────────────────────────────────────────

    fn action_enter_move(&mut self) {
        if let Selection::Project(pi) = self.current_selection() {
            self.mode = Mode::Move { project_idx: pi };
            self.set_status("MOVE: j/k to reorder  Enter/Esc to confirm");
        } else {
            self.set_status("Select a project to move");
        }
    }

    fn move_project(&mut self, pi: usize, delta: isize) {
        let new_pi = (pi as isize + delta) as usize;
        let len = self.workspace.projects.len();
        if new_pi >= len { return; }
        self.workspace.projects.swap(pi, new_pi);
        self.mode = Mode::Move { project_idx: new_pi };
        self.rebuild_flat();
        if let Some(pos) = self.flat().iter().position(|e| matches!(e, FlatEntry::Project { idx } if *idx == new_pi)) {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    fn move_project_down(&mut self, pi: usize) {
        if pi + 1 < self.workspace.projects.len() { self.move_project(pi, 1); }
    }

    fn move_project_up(&mut self, pi: usize) {
        if pi > 0 { self.move_project(pi, -1); }
    }

    fn sync_config_project_order(&mut self) {
        let ordered: Vec<_> = self.workspace.projects.iter()
            .filter_map(|wp| self.config.projects.iter().find(|c| c.path == wp.path).cloned())
            .collect();
        self.config.projects = ordered;
    }
}
