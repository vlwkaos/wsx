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

    pub(super) fn do_create_session(
        &mut self,
        pi: usize,
        wi: usize,
        session_label: String,
        command: Option<String>,
    ) -> Result<Option<runtime::SessionId>> {
        let Some((proj_name, wt_path, wt_slug)) = self.workspace.projects.get(pi).and_then(|p| {
            let wt = p.worktrees.get(wi)?;
            Some((p.name.clone(), wt.path.clone(), wt.session_slug(&p.name)))
        }) else {
            self.set_status("Worktree is no longer available");
            return Ok(None);
        };
        let explicit_name = if session_label.is_empty() {
            None
        } else {
            Some(session_label)
        };
        let (session_id, display_name) = ops::create_session(
            &self.config,
            &proj_name,
            &wt_slug,
            &wt_path,
            explicit_name,
            command,
        )?;
        self.set_status(format!("Session '{}' created", display_name));
        // Expand before the authoritative Runtime refresh reveals the new pane.
        self.set_worktree_expanded(pi, wi, true);
        self.spawn_runtime_refresh();
        Ok(Some(session_id))
    }

    pub(super) fn do_delete_project(&mut self, pi: usize) -> Result<()> {
        let Some((name, path)) = self
            .workspace
            .projects
            .get(pi)
            .map(|p| (p.name.clone(), p.path.clone()))
        else {
            self.set_status("Project is no longer available");
            return Ok(());
        };
        self.workspace.projects.remove(pi);
        self.rebuild_flat();
        ops::unregister_project(&path, &mut self.config);
        self.config.save()?;
        self.clamp_selected();
        self.set_status(format!("Unregistered: {}", name));
        Ok(())
    }

    pub(super) fn do_delete_session(&mut self, pi: usize, wi: usize, si: usize) -> Result<()> {
        let Some(session) = self.workspace.session(pi, wi, si) else {
            self.set_status("Session is no longer available");
            return Ok(());
        };
        let session_id = session.session_id;
        let display_name = session.display_name.clone();
        self.spawn_bg(format!("kill {display_name}"), move || {
            ops::kill_session(session_id)?;
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
        ops::rename_session(session_id, &new_name)?;
        if let Some(session) = self.workspace.session_mut(pi, wi, si) {
            session.display_name = new_name.clone();
        }
        self.mark_dirty();
        self.set_status(format!("Session renamed to '{}'", new_name));
        Ok(())
    }
}
