use super::*;

impl App {
    pub fn flush_cache(&mut self) -> Result<()> {
        self.persist_state(true)
    }

    // ^ docs/ui-state-ownership.md: observations cannot overwrite another client's intent.
    pub(super) fn persist_state(&mut self, sync: bool) -> Result<()> {
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
        let committed = match self.cache_changes.save(&self.cache_path, sync) {
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

    pub(super) fn mark_dirty(&mut self) {
        self.cache_dirty = true;
    }

    pub fn refresh_all(&mut self) -> Result<()> {
        // ^ Keep one ordered refresh stream so older snapshots cannot clear mutation tombstones.
        self.spawn_runtime_refresh();
        Ok(())
    }

    pub(super) fn write_cache_if_dirty(&mut self) {
        if self.cache_dirty {
            let _ = self.persist_state(false);
        }
    }

    pub(super) fn filter_pending_deletions(
        &mut self,
        worktrees: Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)>,
    ) -> Vec<(PathBuf, Vec<git_worktree::WorktreeEntry>)> {
        filter_pending_deletions(&mut self.pending_deletions, worktrees)
    }

    pub(super) fn spawn_runtime_refresh(&mut self) {
        self.spawn_runtime_refresh_with(runtime::ensure_available);
    }

    pub(super) fn spawn_runtime_refresh_with(
        &mut self,
        availability: fn(&runtime::Client) -> std::io::Result<runtime::Availability>,
    ) {
        if self.runtime_refresh_pending {
            self.runtime_full_refresh_stale = true;
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let config = self.config.clone();
        let client = self.runtime_client.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FullRefresh(collect_runtime_refresh(
                &client,
                &config,
                availability,
            )));
        });
    }

    pub(super) fn spawn_background_runtime_refresh(&mut self) {
        if self.runtime_refresh_pending {
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let config = self.config.clone();
        let client = self.runtime_client.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FullRefresh(collect_runtime_refresh(
                &client,
                &config,
                runtime::ensure_background_available,
            )));
        });
    }

    pub(super) fn spawn_runtime_session_refresh(&mut self) {
        if self.runtime_refresh_pending {
            self.runtime_refresh_stale = true;
            return;
        }
        self.runtime_refresh_pending = true;
        let tx = self.runtime_tx.clone();
        let client = self.runtime_client.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::SessionRefresh(ops::runtime_snapshot(
                &client,
            )));
        });
    }

    pub(super) fn apply_runtime_refresh(&mut self, result: RuntimeRefresh) {
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

    pub(super) fn start_runtime_monitor(&mut self) {
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

    pub(super) fn announce_startup_availability(&mut self, availability: &runtime::Availability) {
        if !std::mem::take(&mut self.startup_availability_pending) {
            return;
        }
        self.announce_runtime_availability(availability);
    }

    pub(super) fn announce_runtime_availability(&mut self, availability: &runtime::Availability) {
        if let Some((level, message)) = runtime_availability_notice(availability) {
            self.set_notice(level, message);
        }
    }

    pub(super) fn apply_runtime_session_refresh(&mut self, result: Result<runtime::Snapshot>) {
        self.runtime_refresh_pending = false;
        match result {
            Ok(snapshot) => self.apply_projected_snapshot(snapshot, None),
            Err(error) => {
                self.apply_runtime_event(runtime::EventSignal::Disconnected(error.to_string()))
            }
        }
        self.spawn_queued_runtime_refresh();
    }

    pub(super) fn spawn_queued_runtime_refresh(&mut self) {
        if std::mem::take(&mut self.runtime_full_refresh_stale) {
            self.runtime_refresh_stale = false;
            self.spawn_runtime_refresh();
        } else if std::mem::take(&mut self.runtime_refresh_stale) {
            self.spawn_runtime_session_refresh();
        }
    }

    pub(super) fn apply_runtime_snapshot(
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

    pub(super) fn apply_projected_snapshot(
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

    pub(super) fn reconcile_pending_session_orders(&mut self) {
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

    pub(super) fn selected_terminal_identity(
        &self,
    ) -> Option<(runtime::PaneId, runtime::TerminalId)> {
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

    pub(super) fn spawn_plugin_view(&mut self) {
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

    pub(super) fn apply_plugin_view(
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

    pub(super) fn spawn_runtime_capture(&mut self) {
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

    pub(super) fn apply_runtime_frame(&mut self, frame: Result<(u64, runtime::TerminalFrame)>) {
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
    pub(super) fn tick_git_fetch(&mut self) {
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
}
