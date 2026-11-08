use super::*;

impl App {
    // ── Dispatch to ops ───────────────────────────────────────────────────────

    pub(super) fn do_register_project(&mut self, path: PathBuf) -> Result<()> {
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

    /// Creates a session on a worker. The reply expands the worktree and, for `open_shell`,
    /// waits for the refresh to list it and enters it (`pending_shell`).
    pub(super) fn do_create_session(
        &mut self,
        pi: usize,
        wi: usize,
        session_label: String,
        command: Option<String>,
        open_shell: bool,
    ) {
        let Some((proj_name, wt_path, wt_slug)) = self.workspace.projects.get(pi).and_then(|p| {
            let wt = p.worktrees.get(wi)?;
            Some((p.name.clone(), wt.path.clone(), wt.session_slug(&p.name)))
        }) else {
            self.set_status("Worktree is no longer available");
            return;
        };
        let explicit_name = if session_label.is_empty() {
            None
        } else {
            Some(session_label)
        };
        let client = self.runtime_client.clone();
        let config = self.config.clone();
        self.spawn_bg("create session", move || {
            let (session_id, display_name) = ops::create_session(
                &client,
                &config,
                &proj_name,
                &wt_slug,
                &wt_path,
                explicit_name,
                command,
            )?;
            Ok(BgOutcome::SessionCreated {
                worktree: wt_path,
                session_id,
                display_name,
                open_shell,
            })
        });
    }

    pub(super) fn apply_session_created(
        &mut self,
        worktree: &Path,
        session_id: runtime::SessionId,
        display_name: &str,
        open_shell: bool,
    ) {
        self.set_status(format!("Session '{display_name}' created"));
        self.expand_worktree_at(worktree);
        self.spawn_runtime_refresh();
        if open_shell {
            // ^ Entering needs the cursor on the session's row, which the refresh adds.
            self.pending_shell = Some((session_id, Instant::now()));
        }
    }

    /// Expands the worktree at `path` before the refresh reveals a new pane; the rows may
    /// have moved since the request.
    pub(super) fn expand_worktree_at(&mut self, path: &Path) {
        let found = self
            .workspace
            .projects
            .iter()
            .enumerate()
            .find_map(|(pi, project)| {
                project
                    .worktrees
                    .iter()
                    .position(|worktree| worktree.path == path)
                    .map(|wi| (pi, wi))
            });
        if let Some((pi, wi)) = found {
            self.set_worktree_expanded(pi, wi, true);
        }
    }

    pub(super) fn do_delete_project(&mut self, pi: usize, halt: bool) -> Result<()> {
        let Some((name, path)) = self
            .workspace
            .projects
            .get(pi)
            .map(|p| (p.name.clone(), p.path.clone()))
        else {
            self.set_status("Project is no longer available");
            return Ok(());
        };
        if !halt {
            return self.unregister_project_at(&path, &name, None);
        }
        // ^ The same close the web's Remove runs with its halt choice; hidden task sessions too.
        let client = self.runtime_client.clone();
        self.spawn_bg(format!("halt {name}"), move || {
            let halted = ops::halt_project_sessions(&client, &path)?;
            Ok(BgOutcome::ProjectHalted { path, name, halted })
        });
        Ok(())
    }

    /// Drops the project at `path` from the config and the rows. `halted` is how many
    /// sessions were closed first, when the owner chose to.
    pub(super) fn unregister_project_at(
        &mut self,
        path: &PathBuf,
        name: &str,
        halted: Option<usize>,
    ) -> Result<()> {
        let Some(pi) = self
            .workspace
            .projects
            .iter()
            .position(|project| &project.path == path)
        else {
            self.set_status("Project is no longer available");
            return Ok(());
        };
        self.workspace.projects.remove(pi);
        self.rebuild_flat();
        ops::unregister_project(path, &mut self.config);
        self.config.save()?;
        self.clamp_selected();
        self.set_status(match halted {
            Some(halted) => format!("Unregistered: {name}, halted {halted} sessions"),
            None => format!("Unregistered: {name}"),
        });
        Ok(())
    }

    pub(super) fn do_delete_session(&mut self, pi: usize, wi: usize, si: usize) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session is no longer available");
            return Ok(());
        };
        let session_id = session.session_id;
        let display_name = session.display_name.clone();
        let client = self.runtime_client.clone();
        self.spawn_bg(format!("kill {display_name}"), move || {
            ops::kill_session(&client, session_id)?;
            Ok(BgOutcome::SessionKilled {
                session_id,
                display_name,
            })
        });
        Ok(())
    }

    pub(super) fn do_apply_alias(&mut self, pi: usize, wi: usize, alias: String) -> Result<()> {
        let Some((branch, proj_path)) = self
            .workspace
            .worktree(pi, wi)
            .map(|wt| (wt.branch.clone(), self.workspace.projects[pi].path.clone()))
        else {
            self.set_status("Worktree is no longer available");
            return Ok(());
        };

        ops::set_alias(&mut self.config, &proj_path, &branch, &alias);
        self.config.save()?;

        if let Some(wt) = self.workspace.worktree_mut(pi, wi) {
            wt.alias = (!alias.is_empty()).then(|| alias.clone());
        }

        self.set_status(if alias.is_empty() {
            format!("Alias cleared for '{}'", branch)
        } else {
            format!("Alias '{}' set for '{}'", alias, branch)
        });
        Ok(())
    }

    pub(super) fn do_rename_session(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        new_name: String,
    ) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session is no longer available");
            return Ok(());
        };
        let session_id = session.session_id;
        let pane_id = session.pane_id;
        self.unmute_on_interaction(pane_id);
        let client = self.runtime_client.clone();
        self.spawn_bg("rename session", move || {
            ops::rename_session(&client, session_id, &new_name)?;
            Ok(BgOutcome::SessionRenamed {
                session_id,
                name: new_name,
            })
        });
        Ok(())
    }
}
