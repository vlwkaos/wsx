use super::*;

impl App {
    pub fn current_selection(&self) -> Selection {
        self.workspace
            .get_selection(self.tree_selected, self.flat())
    }

    pub(super) fn selected_worktree_indices(&self) -> Option<(usize, usize)> {
        match self.current_selection() {
            Selection::Worktree(pi, wi)
            | Selection::Session(pi, wi, _)
            | Selection::Pane(pi, wi, _, _) => Some((pi, wi)),
            _ => None,
        }
    }

    // ^ docs/ui-state-ownership.md "Selected row": a refresh keeps the cursor on its row by
    // identity, not by row number. Task sessions and worktrees come and go above it, and a
    // project above it collapses on a timer.
    pub(super) fn cursor_identity(&mut self) -> Option<wsx_core::cache::CursorIdentity> {
        self.ensure_flat();
        wsx_core::cache::resolve_cursor_identity(&self.workspace, self.flat(), self.tree_selected)
    }

    /// After `rebuild_flat`: the row `cursor` named, else the same row number, clamped.
    pub(super) fn restore_cursor(&mut self, cursor: Option<wsx_core::cache::CursorIdentity>) {
        if let Some(index) = cursor.and_then(|identity| {
            wsx_core::cache::find_cursor_index(&self.workspace, self.flat(), &identity)
        }) {
            self.tree_selected = index;
        }
        self.clamp_selected();
    }

    pub(super) fn clamp_selected(&mut self) {
        let len = self.flat().len();
        if len == 0 {
            self.tree_selected = 0;
        } else {
            self.tree_selected = self.tree_selected.min(len - 1);
        }
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub(super) fn touch_project(&mut self, project_idx: usize) {
        let Some(path) = self
            .workspace
            .projects
            .get(project_idx)
            .map(|project| project.path.clone())
        else {
            return;
        };
        self.stale_collapsed_projects.remove(&path);
        let now_unix_ms = wsx_core::task::now_unix_ms();
        self.project_touched_unix_ms
            .insert(path.clone(), now_unix_ms);
        if let AutoCollapsePolicy::Adaptive { base_hours } = self.config.auto_collapse {
            self.adaptive_collapse
                .entry(path.clone())
                .and_modify(|state| {
                    state.observe(base_hours, now_unix_ms);
                })
                .or_insert_with(|| AdaptiveCollapseState::new(base_hours, now_unix_ms));
        }
        let change = self.cache_changes.projects.entry(path.clone()).or_default();
        change.observed_touch_unix_ms = now_unix_ms;
        change.touched_unix_ms = Some(now_unix_ms);
        change.stale = Some(false);
        change.adaptive = Some(self.adaptive_collapse.get(&path).copied().map(Into::into));
        self.mark_dirty();
    }

    pub(super) fn set_worktree_expanded(&mut self, pi: usize, wi: usize, expanded: bool) {
        if let Some(worktree) = self.workspace.worktree_mut(pi, wi) {
            worktree.expanded = expanded;
            self.cache_changes
                .worktree_expanded
                .insert(worktree.path.clone(), expanded);
            self.mark_dirty();
        }
    }

    pub(super) fn set_routines_expanded(&mut self, pi: usize, expanded: bool) {
        let project = &mut self.workspace.projects[pi];
        project.routines_expanded = expanded;
        self.cache_changes
            .projects
            .entry(project.path.clone())
            .or_default()
            .routines_expanded = Some(expanded);
        self.touch_project(pi);
    }

    pub(super) fn manually_expand_project(&mut self, project_idx: usize) {
        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
            project.expanded = true;
            self.cache_changes
                .projects
                .entry(project.path.clone())
                .or_default()
                .expanded = Some(true);
            self.touch_project(project_idx);
        }
    }

    pub(super) fn manually_collapse_project(&mut self, project_idx: usize) {
        if let Some(project) = self.workspace.projects.get_mut(project_idx) {
            project.expanded = false;
            self.cache_changes
                .projects
                .entry(project.path.clone())
                .or_default()
                .expanded = Some(false);
            self.touch_project(project_idx);
        }
    }

    pub(super) fn nav_up(&mut self) {
        if self.tree_selected > 0 {
            self.tree_selected -= 1;
            self.update_scroll();
        }
    }

    pub(super) fn nav_down(&mut self) {
        let max = self.flat().len().saturating_sub(1);
        if self.tree_selected < max {
            self.tree_selected += 1;
            self.update_scroll();
        }
    }

    pub(super) fn nav_left(&mut self) {
        let entry = self.flat().get(self.tree_selected).cloned();
        match entry {
            Some(FlatEntry::Project { idx }) => {
                if self.workspace.projects[idx].expanded {
                    self.manually_collapse_project(idx);
                    self.rebuild_flat();
                    self.clamp_selected();
                }
            }
            Some(FlatEntry::Worktree { project_idx: pi, worktree_idx: wi }) => {
                if self.workspace.projects[pi].worktrees[wi].expanded {
                    self.set_worktree_expanded(pi, wi, false);
                    self.touch_project(pi);
                    self.rebuild_flat();
                    self.clamp_selected();
                } else {
                    // Jump to parent project
                    if let Some(pos) = self.flat().iter().position(|e| matches!(e, FlatEntry::Project { idx } if *idx == pi)) {
                        self.tree_selected = pos;
                        self.update_scroll();
                    }
                }
            }
            Some(FlatEntry::Session { project_idx: pi, worktree_idx: wi, .. }) => {
                if let Some(pos) = self.flat().iter().position(|e| {
                    matches!(e, FlatEntry::Worktree { project_idx: p, worktree_idx: w } if *p == pi && *w == wi)
                }) {
                    self.tree_selected = pos;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::Pane {
                project_idx: pi,
                worktree_idx: wi,
                session_idx: si,
                ..
            }) => {
                if let Some(pos) = self.flat().iter().position(|entry| {
                    matches!(entry, FlatEntry::Session { project_idx, worktree_idx, session_idx } if *project_idx == pi && *worktree_idx == wi && *session_idx == si)
                }) {
                    self.tree_selected = pos;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::RoutinesHeader { project_idx: pi }) => {
                if self.workspace.projects[pi].routines_expanded {
                    self.set_routines_expanded(pi, false);
                    self.rebuild_flat();
                    self.clamp_selected();
                } else if let Some(pos) = self.flat().iter().position(|entry| matches!(entry, FlatEntry::Project { idx } if *idx == pi)) {
                    self.tree_selected = pos;
                }
            }
            Some(FlatEntry::Routine { project_idx: pi, .. }) => {
                if let Some(pos) = self.flat().iter().position(|entry| matches!(entry, FlatEntry::RoutinesHeader { project_idx } if *project_idx == pi)) {
                    self.tree_selected = pos;
                }
            }
            None => {}
        }
    }

    pub(super) fn nav_right(&mut self) {
        let entry = self.flat().get(self.tree_selected).cloned();
        match entry {
            Some(FlatEntry::Project { idx: pi }) => {
                if !self.workspace.projects[pi].expanded {
                    self.manually_expand_project(pi);
                    self.mark_dirty();
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].worktrees.is_empty() {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::Worktree {
                project_idx: pi,
                worktree_idx: wi,
            }) => {
                if !self.workspace.projects[pi].worktrees[wi].expanded {
                    self.set_worktree_expanded(pi, wi, true);
                    self.touch_project(pi);
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].worktrees[wi]
                    .sessions
                    .is_empty()
                {
                    self.tree_selected += 1;
                    self.update_scroll();
                }
            }
            Some(FlatEntry::RoutinesHeader { project_idx: pi }) => {
                if !self.workspace.projects[pi].routines_expanded {
                    self.set_routines_expanded(pi, true);
                    self.rebuild_flat();
                } else if !self.workspace.projects[pi].routines.is_empty() {
                    self.tree_selected += 1;
                }
            }
            _ => {}
        }
    }

    pub(super) fn jump_project(&mut self, dir: isize) {
        let flat = self.flat();
        let current = self.tree_selected;
        let target = if dir > 0 {
            flat.iter()
                .enumerate()
                .find(|(i, e)| *i > current && matches!(e, FlatEntry::Project { .. }))
                .map(|(i, _)| i)
        } else {
            flat.iter()
                .enumerate()
                .rev()
                .find(|(i, e)| *i < current && matches!(e, FlatEntry::Project { .. }))
                .map(|(i, _)| i)
        };
        if let Some(pos) = target {
            self.tree_selected = pos;
            self.update_scroll();
        }
    }

    pub(super) fn update_scroll(&mut self) {
        // tree_visible_height is set each frame from actual terminal size; fall back to 20
        let visible = self.tree_visible_height.max(1);
        self.tree_scroll_manual = false;
        self.tree_scroll = crate::ui::workspace_tree::compute_scroll(
            self.tree_selected,
            visible,
            self.tree_scroll,
        );
        // ^ Captured terminal PUA glyphs can have widths that differ from ratatui's model.
        // Clear only when entering or leaving that content; ordinary previews diff cleanly.
        self.force_preview_redraw |= self.shows_preview()
            && (self.last_rendered_preview_was_session
                || matches!(self.current_selection(), Selection::Session(..)));
    }
}
