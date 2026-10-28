use super::*;

impl App {
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
            self.open_pending_shell(terminal)?;
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
                // ^ The manager chat takes every character itself (task_chat.rs `key`).
                Mode::Tasks { list } if list.chat.is_some() => EventMode::Input,
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

    pub(super) fn drain_terminal_stream(&mut self) {
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

    pub(super) fn apply_terminal_update(
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

    pub(super) fn request_terminal_resync(&self, generation: u64) {
        if let Some(active) = self
            .terminal_stream
            .as_ref()
            .filter(|active| active.generation == generation)
        {
            active.stream.request_resync();
        }
    }

    pub(super) fn pending_terminal_entry_is_current(&self, pending: PendingTerminalEntry) -> bool {
        let mode_matches = matches!(self.mode, Mode::Workspace)
            || matches!(self.mode, Mode::Terminal { pane_id } if pane_id == pending.pane_id);
        mode_matches
            && self.selected_terminal_identity() == Some((pending.pane_id, pending.terminal_id))
    }

    pub(super) fn activate_pending_terminal_entry(&mut self, pending: PendingTerminalEntry) {
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

    pub(super) fn cancel_pending_terminal_entry_if_stale(&mut self) {
        let stale = self
            .pending_terminal_entry
            .is_some_and(|pending| !self.pending_terminal_entry_is_current(pending));
        if stale {
            self.pending_terminal_entry = None;
            self.terminal_stream = None;
        }
    }

    pub(super) fn drain_async_results(&mut self) {
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

    // An unannounced disconnect, or a lease that ran out while wsxd was busy, reattaches once
    // without a takeover; a repeat within the window is a failure. Only a takeover
    // (`lease_transferred`) moves the pane (docs/web-client-terminal.md "Reconnect").
    pub(super) fn stream_loss_reattaches(&mut self, error: &runtime::ApiError) -> bool {
        match error.code.as_str() {
            "daemon_handoff" => true,
            "stream_disconnected" | "lease_lost" => {
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

    pub(super) fn prepare_terminal_resume(&mut self) -> Option<u64> {
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

    pub(super) fn begin_resume_boundary(&mut self) {
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

    pub(super) fn apply_resume_refresh(
        &mut self,
        generation: u64,
        result: Result<runtime::Snapshot>,
    ) {
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

    pub(super) fn reconnect_terminal_after_resume(&mut self, terminal: &Tui) {
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
}
