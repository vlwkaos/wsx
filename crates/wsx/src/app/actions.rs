use super::*;

impl App {
    // ── Actions ───────────────────────────────────────────────────────────────

    pub(super) fn action_select(&mut self, terminal: &mut Tui) -> Result<()> {
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
    pub(super) fn unmute_on_interaction(&mut self, pane_id: runtime::PaneId) {
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

    pub(super) fn attach_session(
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

    pub(super) fn attach_pane(
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

    pub(super) fn terminal_target_label(
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

    pub(super) fn enter_terminal(
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
    pub(super) fn restart_exited_pane(&mut self, pane_id: runtime::PaneId) -> Result<()> {
        let revision = ops::restart_session(pane_id)?;
        // ^ A refresh read before the restart still says exited and would end the new stream.
        if let Some((_, applied)) = self.applied_revision.as_mut() {
            *applied = (*applied).max(revision);
        }
        Ok(())
    }

    pub(super) fn pane_mut(
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

    pub(super) fn leave_terminal_mode(&mut self, pane_id: runtime::PaneId) {
        self.unmute_on_interaction(pane_id);
        self.clear_active_terminal_selection();
        self.terminal_stream = None;
        self.pending_terminal_entry = None;
        self.pending_terminal_resume = None;
        self.terminal_escape_chord.reset();
        self.mode = Mode::Workspace;
    }

    pub(super) fn clear_active_terminal_selection(&mut self) {
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

    pub(super) fn terminal_cursor(&self) -> Option<runtime::Cursor> {
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

    pub(super) fn terminal_pane_size(&self, terminal: &Tui) -> (u16, u16) {
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

    pub(super) fn resize_terminal_pane(&mut self, _pane_id: runtime::PaneId, terminal: &Tui) {
        let (rows, cols) = self.terminal_pane_size(terminal);
        self.send_terminal_stream(runtime::TerminalClientMessage::Resize { rows, cols });
    }

    pub(super) fn resize_pending_terminal_entry(&mut self, terminal: &Tui) {
        let (rows, cols) = self.terminal_pane_size(terminal);
        let Some(pending) = self.pending_terminal_entry.as_mut() else {
            return;
        };
        pending.rows = rows;
        pending.cols = cols;
        self.send_terminal_stream(runtime::TerminalClientMessage::Resize { rows, cols });
    }

    pub(super) fn action_hard_quit(&mut self) {
        self.mode = Mode::Confirm {
            message: "Hard quit? Stops wsxd and all live processes. Saved sessions restart on next launch.".into(),
            pending: PendingAction::ShutdownDaemon,
        };
    }

    pub(super) fn action_add_project(&mut self) -> Result<()> {
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

    pub(super) fn action_add_worktree(&mut self) -> Result<()> {
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

    pub(super) fn action_add_session(&mut self) -> Result<()> {
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

    pub(super) fn action_split_pane(&mut self, axis: runtime::SplitAxis) -> Result<()> {
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
        let project = &self.workspace.projects[pi];
        let command = wsx_core::ops::with_account(&self.config, &project.path, Vec::new())?;
        let session = &project.worktrees[wi].sessions[si];
        let rows = self.terminal_area.height.max(1);
        let cols = self.terminal_area.width.max(1);
        match self.runtime_client.call(&runtime::Request::PaneSplit {
            session_id: session.session_id,
            target,
            axis,
            label: format!("terminal-{}", session.panes.len() + 1),
            command,
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

    pub(super) fn action_tasks(&mut self) {
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

    pub(super) fn action_add_routine(&mut self) -> Result<()> {
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

    pub(super) fn action_delete(&mut self) -> Result<()> {
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

    pub(super) fn action_edit_global_config(&mut self, _terminal: &mut Tui) {
        self.mode = Mode::GlobalSettings {
            form: GlobalSettingsForm::new(self.config.clone()),
        };
    }

    pub(super) fn dispatch_global_settings(
        &mut self,
        action: Action,
        terminal: &mut Tui,
    ) -> Result<()> {
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

    pub(super) fn dispatch_integration_manager(&mut self, action: Action) -> Result<()> {
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

    pub(super) fn spawn_install_integrations(
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

    pub(super) fn save_global_settings(&mut self) -> Result<()> {
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

    pub(super) fn reload_plugins(&mut self) {
        if matches!(
            self.runtime_client.call(&runtime::Request::PluginReload),
            Ok(runtime::Response::Plugins(_))
        ) {
            self.spawn_runtime_session_refresh();
        }
    }

    pub(super) fn edit_raw_global_config(&mut self, terminal: &mut Tui) {
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

    pub(super) fn action_edit(&mut self) -> Result<()> {
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

    pub(super) fn session_candidates(
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

    pub(super) fn active_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session).app_state() == AppSessionState::Active
        })
    }

    pub(super) fn idle_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session.agent.is_some()
                && session_state::derive(session).app_state() == AppSessionState::Idle
        })
    }

    pub(super) fn candidate_target(&self, candidates: &[usize], dir: isize) -> Option<usize> {
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

    pub(super) fn idle_target(&self, dir: isize) -> Option<usize> {
        self.candidate_target(&self.idle_candidates(), dir)
    }

    pub(super) fn action_move_idle(&mut self, dir: isize) {
        let Some(target) = self.idle_target(dir) else {
            self.set_status("No idle sessions");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    pub(super) fn action_send_ctrl_c(&mut self) -> Result<()> {
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

    pub(super) fn active_target(&self, dir: isize) -> Option<usize> {
        self.candidate_target(&self.active_candidates(), dir)
    }

    pub(super) fn action_move_active(&mut self, dir: isize) {
        let Some(target) = self.active_target(dir) else {
            self.set_status("No active sessions");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    pub(super) fn attention_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session).app_state() == AppSessionState::NeedsAttention
        })
    }

    pub(super) fn blocked_attention_candidates(&self) -> Vec<usize> {
        self.session_candidates(|session| {
            session_state::derive(session) == session_state::SessionHeuristic::Blocked
        })
    }

    pub(super) fn attention_target(&self, dir: isize) -> Option<usize> {
        if self.config.attention_priority == AttentionPriority::BlockedFirst {
            let blocked = self.blocked_attention_candidates();
            if !blocked.is_empty() {
                return self.candidate_target(&blocked, dir);
            }
        }
        self.candidate_target(&self.attention_candidates(), dir)
    }

    pub(super) fn action_next_attention(&mut self, dir: isize) {
        let Some(target) = self.attention_target(dir) else {
            self.set_status("No sessions need attention");
            return;
        };
        self.tree_selected = target;
        self.update_scroll();
    }

    pub(super) fn action_switch_idle(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.idle_target(dir) else {
            self.set_status("No idle sessions");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    pub(super) fn action_switch_active(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.active_target(dir) else {
            self.set_status("No active sessions");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    pub(super) fn action_switch_attention(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
        let Some(target) = self.attention_target(dir) else {
            self.set_status("No sessions need attention");
            return Ok(());
        };
        self.switch_to_flat_session(target, terminal)
    }

    pub(super) fn action_switch_group(&mut self, dir: isize, terminal: &mut Tui) -> Result<()> {
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
    pub(super) fn context_session_target(&self, dir: isize) -> Option<(usize, usize, usize)> {
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

    pub(super) fn action_switch_context_session(
        &mut self,
        dir: isize,
        terminal: &mut Tui,
    ) -> Result<()> {
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

    pub(super) fn reveal_session(
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

    pub(super) fn switch_to_flat_session(
        &mut self,
        target: usize,
        terminal: &mut Tui,
    ) -> Result<()> {
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

    pub(super) fn switch_terminal_session(
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

    pub(super) fn action_dismiss_attention(&mut self) {
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

    pub(super) fn action_set_alias(&mut self) -> Result<()> {
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
}
