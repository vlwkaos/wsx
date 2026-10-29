use super::*;

impl App {
    // ── Move project ──────────────────────────────────────────────────────────

    pub(super) fn action_enter_move(&mut self) {
        match self.current_selection() {
            Selection::Project(pi) => {
                self.mode = Mode::Move { project_idx: pi };
            }
            Selection::Worktree(pi, wi) => {
                self.mode = Mode::MoveWorktree {
                    project_idx: pi,
                    worktree_idx: wi,
                };
            }
            Selection::Session(pi, wi, si) => {
                self.mode = Mode::MoveSession {
                    project_idx: pi,
                    worktree_idx: wi,
                    session_idx: si,
                };
            }
            _ => self.set_status("Select a project, worktree, or session to move"),
        }
    }

    pub(super) fn move_project(&mut self, pi: usize, delta: isize) {
        let Some(new_pi) =
            adjacent_visible_project_index(&self.workspace, &self.visible_projects, pi, delta)
        else {
            return;
        };
        self.workspace.projects.swap(pi, new_pi);
        self.mode = Mode::Move {
            project_idx: new_pi,
        };
        self.rebuild_flat();
        if let Some(pos) = self
            .flat()
            .iter()
            .position(|e| matches!(e, FlatEntry::Project { idx } if *idx == new_pi))
        {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    pub(super) fn move_project_down(&mut self, pi: usize) {
        self.move_project(pi, 1);
    }

    pub(super) fn move_project_up(&mut self, pi: usize) {
        self.move_project(pi, -1);
    }

    pub(super) fn move_worktree(&mut self, pi: usize, wi: usize, delta: isize) {
        let new_wi = wi as isize + delta;
        if new_wi < 0 {
            return;
        }
        let new_wi = new_wi as usize;
        let Some(project) = self.workspace.projects.get_mut(pi) else {
            return;
        };
        if wi >= project.worktrees.len() || new_wi >= project.worktrees.len() {
            return;
        }
        project.worktrees.swap(wi, new_wi);
        self.sync_config_worktree_order(pi);
        self.mode = Mode::MoveWorktree {
            project_idx: pi,
            worktree_idx: new_wi,
        };
        self.rebuild_flat();
        if let Some(pos) = self.flat().iter().position(|entry| {
            matches!(entry, FlatEntry::Worktree { project_idx, worktree_idx }
                if *project_idx == pi && *worktree_idx == new_wi)
        }) {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    pub(super) fn move_session(
        &mut self,
        pi: usize,
        wi: usize,
        si: usize,
        delta: isize,
    ) -> Result<()> {
        let new_si = si as isize + delta;
        if new_si < 0 {
            return Ok(());
        }
        let new_si = new_si as usize;
        let Some(worktree) = self.workspace.worktree_mut(pi, wi) else {
            return Ok(());
        };
        let sessions = &mut worktree.sessions;
        if new_si >= sessions.len() || si >= sessions.len() {
            return Ok(());
        }
        let session_id = sessions[si].session_id;
        let target_session_id = sessions[new_si].session_id;
        let placement = if delta < 0 {
            runtime::SessionPlacement::Before
        } else {
            runtime::SessionPlacement::After
        };
        let revision = ops::reorder_session(
            &self.runtime_client,
            session_id,
            target_session_id,
            placement,
            sessions[si].revision,
        )?;
        sessions.swap(si, new_si);
        sessions[new_si].revision = revision;
        let session_ids = sessions.iter().map(|session| session.session_id).collect();
        let worktree_path = self.workspace.projects[pi].worktrees[wi].path.clone();
        self.pending_session_orders.insert(
            worktree_path,
            PendingSessionOrder {
                moved_session_id: session_id,
                revision,
                session_ids,
            },
        );
        self.mode = Mode::MoveSession {
            project_idx: pi,
            worktree_idx: wi,
            session_idx: new_si,
        };
        self.rebuild_flat();
        if let Some(pos) = self.flat().iter().position(|e| {
            matches!(e, FlatEntry::Session { project_idx: p, worktree_idx: w, session_idx: s }
                if *p == pi && *w == wi && *s == new_si)
        }) {
            self.tree_selected = pos;
            self.update_scroll();
        }
        Ok(())
    }

    pub(super) fn sync_config_worktree_order(&mut self, project_idx: usize) {
        let Some(project) = self.workspace.projects.get(project_idx) else {
            return;
        };
        self.config.set_worktree_order(
            &project.path,
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.path.clone())
                .collect(),
        );
    }

    pub(super) fn sync_config_project_order(&mut self) {
        let ordered: Vec<_> = self
            .workspace
            .projects
            .iter()
            .filter_map(|wp| {
                self.config
                    .projects
                    .iter()
                    .find(|c| c.path == wp.path)
                    .cloned()
            })
            .collect();
        self.config.projects = ordered;
    }
}
