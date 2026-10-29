use super::*;

impl App {
    pub(super) fn apply_runtime_event(&mut self, signal: runtime::EventSignal) {
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

    pub(super) fn apply_routine_result(&mut self, result: RoutineRefreshResult) {
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

    pub(super) fn tick(&mut self) -> Result<()> {
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
            // ^ A moved cursor is saved here, not per key: without a snapshot apply nothing
            // else writes it before the slow timer (docs/ui-state-ownership.md "Selected row").
            if self.tree_selected != self.cache_cursor.0 {
                self.mark_dirty();
                self.write_cache_if_dirty();
            }
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

    pub(super) fn expire_notice(&mut self, now: Instant) {
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

    pub(super) fn spawn_routine_refresh(&mut self) {
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

    pub(super) fn spawn_git_local(&mut self, path: PathBuf, default_branch: String) {
        self.spawn_git_local_with_options(path, default_branch, false);
    }

    pub(super) fn spawn_git_local_with_options(
        &mut self,
        path: PathBuf,
        default_branch: String,
        force: bool,
    ) {
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

    pub(super) fn spawn_git_local_for_selected(&mut self) {
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
    pub(super) fn spawn_git_local_for_all(&mut self) {
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

    pub(super) fn apply_fetch_result(&mut self, path: PathBuf, outcome: FetchOutcome) {
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

    pub(super) fn apply_git_local_result(&mut self, path: PathBuf, info: Option<GitInfo>) {
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
}
