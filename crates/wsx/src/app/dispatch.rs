use super::*;

impl App {
    // ── Action dispatch ───────────────────────────────────────────────────────

    pub(super) fn dispatch(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
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
            if let Some(chat) = &mut list.chat {
                if !chat.key(&action) {
                    list.chat = None;
                }
                self.needs_redraw = true;
                return Ok(());
            }
            match (action, list.detail.is_some()) {
                (Action::EnterMove, false) => list.open_chat(None),
                (Action::NavigateDown, false) => list.select(1),
                (Action::NavigateUp, false) => list.select(-1),
                (Action::Select, false) => list.open_selected(),
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

    pub(super) fn dispatch_terminal(
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

    pub(super) fn send_terminal_keys(
        &mut self,
        keys: impl IntoIterator<Item = crossterm::event::KeyEvent>,
    ) {
        for key in keys {
            if let Some(key) = runtime_key_event(key) {
                self.send_terminal_stream(runtime::TerminalClientMessage::Key(key));
            }
        }
    }

    pub(super) fn send_terminal_stream(&mut self, message: runtime::TerminalClientMessage) {
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
    pub(super) fn send_terminal_bytes_once(
        &self,
        pane_id: runtime::PaneId,
        bytes: Vec<u8>,
    ) -> Result<()> {
        self.terminal_input_tx
            .send((pane_id, bytes))
            .map_err(|_| anyhow::anyhow!("terminal input worker stopped"))
    }

    pub(super) fn is_workspace_click(&self, mouse: crossterm::event::MouseEvent) -> bool {
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

    pub(super) fn handle_terminal_group_header_scroll(
        &mut self,
        mouse: crossterm::event::MouseEvent,
    ) -> bool {
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

    pub(super) fn send_terminal_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let Some(mouse) = runtime_mouse_event(mouse, self.terminal_area) else {
            return;
        };
        self.send_terminal_stream(runtime::TerminalClientMessage::Mouse(mouse));
    }

    pub(super) fn dispatch_normal(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
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
            Action::ProfileNext => self.action_profile_next(),
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

    pub(super) fn handle_mouse_click(
        &mut self,
        col: u16,
        row: u16,
        terminal: &mut Tui,
        compact_tree: bool,
    ) -> Result<()> {
        let pos = Position { x: col, y: row };
        if matches!(self.mode, Mode::Workspace) && self.group_header_area.contains(pos) {
            let strip = crate::ui::workspace_nav::fit_group_strip(
                &crate::ui::workspace_nav::header_title(self.active_profile.as_deref()),
                &self.config.ordered_group_keys(),
                &self.active_group,
                usize::from(self.group_header_area.width),
                self.group_header_scroll,
            );
            match strip.target_at(usize::from(col - self.group_header_area.x)) {
                Some(crate::ui::workspace_nav::GroupStripTarget::Title) => {
                    if self.active_profile.is_some() {
                        self.action_profile_next()
                    }
                }
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

    pub(super) fn handle_workspace_mouse_scroll(&mut self, col: u16, row: u16, delta: i8) {
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

    pub(super) fn scroll_group_header(&mut self, delta: i8) {
        let last = self.config.ordered_group_keys().len().saturating_sub(1);
        self.group_header_scroll = if delta < 0 {
            self.group_header_scroll.saturating_sub(1)
        } else {
            self.group_header_scroll.saturating_add(1).min(last)
        };
        self.needs_redraw = true;
    }

    pub(super) fn dispatch_input(&mut self, action: Action, terminal: &mut Tui) -> Result<()> {
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

    pub(super) fn dispatch_confirm(&mut self, action: Action, _terminal: &mut Tui) -> Result<()> {
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

    pub(super) fn dispatch_search(&mut self, action: Action, _terminal: &mut Tui) -> Result<()> {
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

    pub(super) fn dispatch_routine_preset_picker(&mut self, action: Action) -> Result<()> {
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

    pub(super) fn dispatch_routine_editor(&mut self, action: Action) -> Result<()> {
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

    pub(super) fn save_routine_form(&mut self) -> Result<()> {
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

    pub(super) fn select_routine(&mut self, project_idx: usize, name: Option<&str>) {
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

    pub(super) fn search_matches(&self, query: &str) -> Vec<usize> {
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

    pub(super) fn search_apply(&mut self) {
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
    pub(super) fn search_advance(&mut self) {
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
}
