use super::*;

impl App {
    // ── Input confirm ─────────────────────────────────────────────────────────

    pub(super) fn confirm_input(&mut self, _terminal: &mut Tui) -> Result<()> {
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

    pub(super) fn confirm_action(&mut self) -> Result<()> {
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

    pub(super) fn restore_failed_routine_delete(
        &mut self,
        project_idx: usize,
        name: &str,
        error: String,
    ) {
        self.mode = Mode::Workspace;
        self.select_routine(project_idx, Some(name));
        self.set_status(format!("Routine '{name}' not deleted: {error}"));
    }
}
