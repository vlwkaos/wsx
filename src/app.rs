// App state machine and event loop.
// ref: ratatui app patterns — https://ratatui.rs/concepts/application-patterns/

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    action::Action,
    config::global::GlobalConfig,
    event::poll_event,
    git::{info as git_info, worktree as git_worktree},
    model::workspace::{FlatEntry, Selection, SessionInfo, WorkspaceState, WorktreeInfo, flatten_tree},
    ops,
    tmux::{capture, monitor, session},
    tui::{self, Tui},
    ui::{self, input::InputState},
};

const TICK_MS: u64 = 100;
const CAPTURE_INTERVAL_MS: u64 = 500;
const RESCAN_INTERVAL_MS: u64 = 2000;

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
    Help,
}

pub enum InputContext {
    AddProject,
    AddWorktree { project_idx: usize },
    AddSession { project_idx: usize, worktree_idx: usize },
    OpenRun { project_idx: usize, worktree_idx: usize },
    SetAlias { project_idx: usize, worktree_idx: usize },
    RenameSession { project_idx: usize, worktree_idx: usize, session_idx: usize },
}

impl InputContext {
    pub fn title(&self) -> &'static str {
        match self {
            InputContext::AddProject => "Add Project",
            InputContext::AddWorktree { .. } => "Add Worktree",
            InputContext::AddSession { .. } => "New Session",
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
    pub mode: Mode,
    pub config: GlobalConfig,
    pub status_message: Option<String>,
    pub loading: bool,
    last_capture: Instant,
    last_rescan: Instant,
}

impl App {
    pub fn new() -> Result<Self> {
        let config = GlobalConfig::load()?;
        let workspace = ops::load_workspace(&config);

        Ok(Self {
            workspace,
            tree_selected: 0,
            tree_scroll: 0,
            mode: Mode::Normal,
            config,
            status_message: None,
            loading: false,
            last_capture: Instant::now(),
            last_rescan: Instant::now(),
        })
    }

    pub fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        loop {
            terminal.draw(|frame| ui::render(frame, self))?;

            let in_input = matches!(self.mode, Mode::Input { .. });
            if let Some(action) = poll_event(Duration::from_millis(TICK_MS), in_input)? {
                if action == Action::Quit && matches!(self.mode, Mode::Normal) {
                    break;
                }
                if let Err(e) = self.dispatch(action, terminal) {
                    self.status_message = Some(format!("Error: {}", e));
                }
            } else {
                self.tick()?;
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();

        if now.duration_since(self.last_rescan) >= Duration::from_millis(RESCAN_INTERVAL_MS) {
            if let Err(e) = self.refresh_all() {
                self.status_message = Some(format!("Refresh error: {}", e));
            }
            self.last_rescan = now;
        }

        if now.duration_since(self.last_capture) >= Duration::from_millis(CAPTURE_INTERVAL_MS) {
            self.refresh_captures();
            self.last_capture = now;
        }

        Ok(())
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        let sessions_with_paths = session::list_sessions_with_paths();
        let activity = monitor::session_activity();

        let aliases_by_path: Vec<(PathBuf, std::collections::HashMap<String, String>)> =
            self.config.projects.iter()
                .map(|e| (e.path.clone(), e.aliases.clone()))
                .collect();

        for i in 0..self.workspace.projects.len() {
            let path = self.workspace.projects[i].path.clone();
            let aliases = aliases_by_path.iter()
                .find(|(p, _)| p == &path)
                .map(|(_, a)| a.clone())
                .unwrap_or_default();

            if let Ok(entries) = git_worktree::list_worktrees(&path) {
                let mut new_worktrees = Vec::new();
                for entry in entries {
                    let alias = aliases.get(&entry.branch).cloned();
                    let wt_path = entry.path.clone();

                    let sessions: Vec<SessionInfo> = sessions_with_paths.iter()
                        .filter(|(_, sp)| sp == &wt_path)
                        .map(|(name, _)| SessionInfo {
                            name: name.clone(),
                            is_wsx_owned: name.starts_with("wsx_"),
                            has_activity: activity.get(name).copied().unwrap_or(false),
                            pane_capture: None,
                        })
                        .collect();

                    // Preserve git_info and expanded state from existing worktree
                    let (git_info, expanded) = self.workspace.projects[i].worktrees.iter()
                        .find(|w| w.path == entry.path)
                        .map(|w| (w.git_info.clone(), w.expanded))
                        .unwrap_or((None, false));

                    new_worktrees.push(WorktreeInfo {
                        name: entry.name,
                        branch: entry.branch,
                        path: entry.path,
                        is_main: entry.is_main,
                        alias,
                        sessions,
                        expanded,
                        git_info,
                    });
                }
                self.workspace.projects[i].worktrees = new_worktrees;
            }
        }

        // Clamp selection after refresh (worktree counts may change)
        self.clamp_selected();
        Ok(())
    }

    fn refresh_captures(&mut self) {
        let sel = self.current_selection();

        // Load git info when a worktree or session is selected
        let (pi, wi) = match sel {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
            _ => return,
        };

        let needs_git = self.workspace.projects.get(pi)
            .and_then(|p| p.worktrees.get(wi))
            .map(|w| w.git_info.is_none())
            .unwrap_or(false);

        if needs_git {
            let wt_path = self.workspace.projects.get(pi)
                .and_then(|p| p.worktrees.get(wi))
                .map(|w| w.path.clone());
            let default_branch = self.workspace.projects.get(pi)
                .map(|p| p.default_branch.clone())
                .unwrap_or_else(|| "main".to_string());

            if let Some(path) = wt_path {
                if let Some(gi) = git_info::get_git_info(&path, &default_branch) {
                    if let Some(wt) = self.workspace.projects.get_mut(pi)
                        .and_then(|p| p.worktrees.get_mut(wi)) {
                        wt.git_info = Some(gi);
                    }
                }
            }
        }

        // Capture pane for selected session
        if let Selection::Session(pi, wi, si) = sel {
            let sess_name = self.workspace.projects.get(pi)
                .and_then(|p| p.worktrees.get(wi))
                .and_then(|w| w.sessions.get(si))
                .map(|s| s.name.clone());

            if let Some(name) = sess_name {
                if session::session_exists(&name) {
                    if let Some(raw) = capture::capture_pane(&name) {
                        let trimmed = capture::trim_capture(&raw);
                        if let Some(s) = self.workspace.projects.get_mut(pi)
                            .and_then(|p| p.worktrees.get_mut(wi))
                            .and_then(|w| w.sessions.get_mut(si)) {
                            s.pane_capture = Some(trimmed);
                        }
                    }
                }
            }
        }
    }

    pub fn current_selection(&self) -> Selection {
        self.workspace.get_selection(self.tree_selected)
    }

    fn flat_len(&self) -> usize {
        flatten_tree(&self.workspace).len()
    }

    fn clamp_selected(&mut self) {
        let len = self.flat_len();
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
        let max = self.flat_len().saturating_sub(1);
        if self.tree_selected < max {
            self.tree_selected += 1;
            self.update_scroll();
        }
    }

    fn nav_left(&mut self) {
        let flat = flatten_tree(&self.workspace);
        match flat.get(self.tree_selected) {
            Some(FlatEntry::Project { idx }) => {
                self.workspace.projects[*idx].expanded = false;
                self.clamp_selected();
            }
            Some(FlatEntry::Worktree { project_idx, worktree_idx }) => {
                let pi = *project_idx;
                let wi = *worktree_idx;
                if self.workspace.projects[pi].worktrees[wi].expanded {
                    self.workspace.projects[pi].worktrees[wi].expanded = false;
                    self.clamp_selected();
                } else {
                    // Jump to parent project
                    if let Some(pos) = flat.iter().position(|e| matches!(e, FlatEntry::Project { idx } if *idx == pi)) {
                        self.tree_selected = pos;
                        self.update_scroll();
                    }
                }
            }
            Some(FlatEntry::Session { project_idx, worktree_idx, .. }) => {
                let pi = *project_idx;
                let wi = *worktree_idx;
                if let Some(pos) = flat.iter().position(|e| {
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
        let flat = flatten_tree(&self.workspace);
        match flat.get(self.tree_selected) {
            Some(FlatEntry::Project { idx }) => {
                let pi = *idx;
                if !self.workspace.projects[pi].expanded {
                    self.workspace.projects[pi].expanded = true;
                } else if !self.workspace.projects[pi].worktrees.is_empty() {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::Worktree { project_idx, worktree_idx }) => {
                let (pi, wi) = (*project_idx, *worktree_idx);
                if !self.workspace.projects[pi].worktrees[wi].expanded {
                    self.workspace.projects[pi].worktrees[wi].expanded = true;
                } else if !self.workspace.projects[pi].worktrees[wi].sessions.is_empty() {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            _ => {}
        }
    }

    fn update_scroll(&mut self) {
        let visible = 20usize;
        self.tree_scroll = crate::ui::workspace_tree::compute_scroll(
            self.tree_selected, visible, self.tree_scroll,
        );
    }

    // ── Action dispatch ───────────────────────────────────────────────────────

    fn dispatch(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
        match &self.mode {
            Mode::Normal => self.dispatch_normal(action, terminal)?,
            Mode::Input { .. } => self.dispatch_input(action, terminal)?,
            Mode::Confirm { .. } => self.dispatch_confirm(action, terminal)?,
            Mode::Config { .. } | Mode::Help => {
                if matches!(action, Action::InputEscape | Action::Quit | Action::Help) {
                    self.mode = Mode::Normal;
                }
            }
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
            _ => {}
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
            Action::ConfirmNo | Action::InputEscape | Action::Quit => {
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Ok(())
    }

    // ── Actions ───────────────────────────────────────────────────────────────

    fn action_select(&mut self, terminal: &mut Tui) -> Result<()> {
        match self.current_selection() {
            Selection::Session(pi, wi, si) => {
                self.attach_session(pi, wi, si, terminal)?;
            }
            Selection::Project(pi) => {
                self.workspace.projects[pi].expanded = !self.workspace.projects[pi].expanded;
                self.clamp_selected();
            }
            Selection::Worktree(pi, wi) => {
                let wt = &mut self.workspace.projects[pi].worktrees[wi];
                wt.expanded = !wt.expanded;
                self.clamp_selected();
            }
            Selection::None => {}
        }
        Ok(())
    }

    fn attach_session(&mut self, pi: usize, wi: usize, si: usize, terminal: &mut Tui) -> Result<()> {
        let name = self.workspace.projects.get(pi)
            .and_then(|p| p.worktrees.get(wi))
            .and_then(|w| w.sessions.get(si))
            .map(|s| s.name.clone());

        let Some(name) = name else {
            self.status_message = Some("Session not found".into());
            return Ok(());
        };

        match session::attach_session_cmd(&name) {
            session::AttachCommand::SwitchClient(n) => {
                session::switch_client(&n)?;
            }
            session::AttachCommand::Attach(n) => {
                tui::with_raw_mode_disabled(terminal, || {
                    std::process::Command::new("tmux")
                        .args(["attach-session", "-t", &n])
                        .status()?;
                    Ok(())
                })?;
            }
        }
        Ok(())
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
                self.status_message = Some("Select a project first (press p to add one)".into());
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
                self.status_message = Some("Select a worktree first".into());
                return Ok(());
            }
        };
        self.mode = Mode::Input {
            context: InputContext::AddSession { project_idx: pi, worktree_idx: wi },
            state: InputState::new("command (optional): "),
        };
        Ok(())
    }

    fn action_open_run(&mut self) -> Result<()> {
        let (pi, wi) = match self.current_selection() {
            Selection::Worktree(pi, wi) | Selection::Session(pi, wi, _) => (pi, wi),
            _ => {
                self.status_message = Some("Select a worktree first".into());
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
                let name = self.workspace.projects[pi].worktrees[wi].sessions[si].name.clone();
                self.mode = Mode::Confirm {
                    message: format!("Kill session '{}'?", name),
                    pending: PendingAction::DeleteSession { project_idx: pi, worktree_idx: wi, session_idx: si },
                };
            }
            Selection::Worktree(pi, wi) => {
                let wt = &self.workspace.projects[pi].worktrees[wi];
                if wt.is_main {
                    self.status_message = Some("Cannot delete main worktree".into());
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
        let pi = match self.current_selection() {
            Selection::Project(pi) | Selection::Worktree(pi, _) | Selection::Session(pi, _, _) => Some(pi),
            Selection::None => None,
        };

        match pi {
            Some(pi) => {
                let (path, branch) = {
                    let p = &self.workspace.projects[pi];
                    (p.path.clone(), p.default_branch.clone())
                };
                let removed = git_worktree::clean_merged(&path, &branch)?;
                self.status_message = Some(if removed.is_empty() {
                    "No merged worktrees to clean".into()
                } else {
                    format!("Cleaned: {}", removed.join(", "))
                });
                self.refresh_all()?;
            }
            None => {
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
                self.status_message = Some(format!("Cleaned {} merged worktrees", total));
                self.refresh_all()?;
            }
        }
        Ok(())
    }

    fn action_edit(&mut self) -> Result<()> {
        let pi = match self.current_selection() {
            Selection::Project(pi) | Selection::Worktree(pi, _) | Selection::Session(pi, _, _) => pi,
            Selection::None => {
                self.status_message = Some("Select a project or worktree".into());
                return Ok(());
            }
        };
        self.mode = Mode::Config { project_idx: pi };
        Ok(())
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
                let current = self.workspace.projects[pi].worktrees[wi].sessions[si].name.clone();
                self.mode = Mode::Input {
                    context: InputContext::RenameSession { project_idx: pi, worktree_idx: wi, session_idx: si },
                    state: InputState::with_value("name: ", current),
                };
            }
            _ => {
                self.status_message = Some("Select a worktree or session".into());
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
                    self.do_create_session(project_idx, worktree_idx, if value.is_empty() { None } else { Some(value) })?;
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
            terminal.draw(|frame| ui::render(frame, self))?;
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
        self.config.save()?;
        self.status_message = Some("Project registered".into());
        Ok(())
    }

    fn do_create_worktree(&mut self, pi: usize, branch: String) -> Result<()> {
        let (repo_path, default_branch, proj_config) = {
            let p = &self.workspace.projects[pi];
            (p.path.clone(), p.default_branch.clone(), p.config.clone().unwrap_or_default())
        };
        let (_wt_path, warning) = ops::create_worktree(&repo_path, &default_branch, &proj_config, &branch)?;
        if let Some(w) = warning {
            self.status_message = Some(w);
        }
        self.refresh_all()?;
        self.status_message = Some(format!("Created worktree: {}", branch));
        Ok(())
    }

    fn do_create_session(&mut self, pi: usize, wi: usize, command: Option<String>) -> Result<()> {
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug())
        };
        let name = ops::create_session(&proj_name, &wt_slug, &wt_path, command)?;
        self.status_message = Some(format!("Session '{}' created", name));
        self.refresh_all()?;
        Ok(())
    }

    fn do_open_run(&mut self, pi: usize, wi: usize, command: String, terminal: &mut Tui) -> Result<()> {
        let (proj_name, wt_path, wt_slug) = {
            let p = &self.workspace.projects[pi];
            let wt = &p.worktrees[wi];
            (p.name.clone(), wt.path.clone(), wt.session_slug())
        };
        let name = ops::create_ephemeral_session(&proj_name, &wt_slug, &wt_path, &command)?;
        match session::attach_session_cmd(&name) {
            session::AttachCommand::SwitchClient(n) => session::switch_client(&n)?,
            session::AttachCommand::Attach(n) => {
                tui::with_raw_mode_disabled(terminal, || {
                    std::process::Command::new("tmux")
                        .args(["attach-session", "-t", &n])
                        .status()?;
                    Ok(())
                })?;
            }
        }
        Ok(())
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
        self.clamp_selected();
        self.status_message = Some(format!("Deleted: {}", branch));
        Ok(())
    }

    fn do_delete_project(&mut self, pi: usize) -> Result<()> {
        let (name, path) = {
            let p = &self.workspace.projects[pi];
            (p.name.clone(), p.path.clone())
        };
        self.workspace.projects.remove(pi);
        ops::unregister_project(&path, &mut self.config);
        self.config.save()?;
        self.clamp_selected();
        self.status_message = Some(format!("Unregistered: {}", name));
        Ok(())
    }

    fn do_delete_session(&mut self, pi: usize, wi: usize, si: usize) -> Result<()> {
        let name = self.workspace.projects[pi].worktrees[wi].sessions[si].name.clone();
        ops::delete_session(&name)?;
        self.workspace.projects[pi].worktrees[wi].sessions.remove(si);
        self.clamp_selected();
        self.status_message = Some(format!("Killed session: {}", name));
        Ok(())
    }

    fn do_apply_alias(&mut self, pi: usize, wi: usize, alias: String) -> Result<()> {
        let branch = self.workspace.projects[pi].worktrees[wi].branch.clone();
        let proj_path = self.workspace.projects[pi].path.clone();

        ops::set_alias(&mut self.config, &proj_path, &branch, &alias);
        self.config.save()?;

        let wt = &mut self.workspace.projects[pi].worktrees[wi];
        wt.alias = if alias.is_empty() { None } else { Some(alias.clone()) };

        self.status_message = Some(if alias.is_empty() {
            format!("Alias cleared for '{}'", branch)
        } else {
            format!("Alias '{}' set for '{}'", alias, branch)
        });
        Ok(())
    }

    fn do_rename_session(&mut self, pi: usize, wi: usize, si: usize, new_name: String) -> Result<()> {
        let old_name = self.workspace.projects[pi].worktrees[wi].sessions[si].name.clone();
        ops::rename_session(&old_name, &new_name)?;
        self.workspace.projects[pi].worktrees[wi].sessions[si].name = new_name.clone();
        self.status_message = Some(format!("Session renamed to '{}'", new_name));
        Ok(())
    }
}
