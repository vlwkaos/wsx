use super::*;

impl App {
    // ── Project-group navigation ─────────────────────────────────────────────

    pub(super) fn persist_active_group(&mut self) {
        if !self.persist_group_selection {
            return;
        }
        let selection = wsx_core::cache::GroupSelection {
            selected: self.active_group.clone(),
            profile: self.active_profile.clone(),
        };
        if let Err(error) = wsx_core::cache::save_group_selection(&selection) {
            self.set_warning(format!("Could not save group selection: {error}"));
        }
    }

    pub(super) fn set_active_group(&mut self, key: GroupKey) {
        let groups = self.config.ordered_group_keys();
        self.group_header_scroll = groups
            .iter()
            .position(|candidate| candidate == &key)
            .unwrap_or(0);
        if self.active_group == key {
            return;
        }
        self.active_group = key;
        self.persist_active_group();
        self.recompute_visible();
        self.update_scroll();
        self.mark_dirty();
    }

    /// `P`: the next profile in config order. ref: README "Profiles"
    pub(super) fn action_profile_next(&mut self) {
        let names = self.config.profile_names();
        let Some(current) = &self.active_profile else {
            self.set_status("No profiles: add [[profiles]] to the config");
            return;
        };
        let index = names.iter().position(|name| name == current).unwrap_or(0);
        let next = names[(index + 1) % names.len()].clone();
        if &next == current {
            return;
        }
        self.active_profile = Some(next);
        self.persist_active_group();
        self.recompute_visible();
        self.update_scroll();
        self.mark_dirty();
    }

    pub(super) fn toggle_active_group(&mut self, key: GroupKey) {
        self.set_active_group(key);
    }

    pub(super) fn action_group_next(&mut self) {
        let groups = self.config.ordered_group_keys();
        let current = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        let next = (current + 1) % groups.len();
        self.set_active_group(groups[next].clone());
    }

    pub(super) fn action_group_prev(&mut self) {
        let groups = self.config.ordered_group_keys();
        let current = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        let previous = current.checked_sub(1).unwrap_or(groups.len() - 1);
        self.set_active_group(groups[previous].clone());
    }

    pub(super) fn action_group_manager(&mut self) {
        let groups = self.config.ordered_group_keys();
        let selected = groups
            .iter()
            .position(|key| key == &self.active_group)
            .unwrap_or(0);
        self.mode = Mode::GroupManager {
            selected,
            scroll: 0,
            purpose: GroupManagerPurpose::Switch,
        };
    }

    pub(super) fn action_assign_group(&mut self) {
        let Selection::Project(project_idx) = self.current_selection() else {
            return;
        };
        if self.config.groups.is_empty() {
            self.set_status("Create a group first with T");
            return;
        }
        self.mode = Mode::GroupManager {
            selected: 0,
            scroll: 0,
            purpose: GroupManagerPurpose::Assign { project_idx },
        };
    }

    pub(super) fn dispatch_group_manager(
        &mut self,
        selected: usize,
        scroll: usize,
        purpose: GroupManagerPurpose,
        action: Action,
    ) -> Result<()> {
        let group_count = match purpose {
            GroupManagerPurpose::Switch => self.config.groups.len() + 1,
            GroupManagerPurpose::Assign { .. } => self.config.groups.len(),
        };
        let layout = crate::ui::workspace_nav::SidebarLayout::new(self.tree_area);
        let visible_height = layout.list.height.max(1) as usize;
        let manager_mode = |selected: usize, scroll: usize| Mode::GroupManager {
            selected,
            scroll: crate::ui::workspace_tree::compute_scroll(selected, visible_height, scroll),
            purpose,
        };
        let toggle = |app: &mut Self, index: usize| -> Result<()> {
            match purpose {
                GroupManagerPurpose::Switch => {
                    if let Some(key) = app.config.ordered_group_keys().get(index).cloned() {
                        app.toggle_active_group(key);
                    }
                }
                GroupManagerPurpose::Assign { project_idx } => {
                    let Some(project) = app.workspace.projects.get(project_idx) else {
                        app.set_status("Project is no longer available");
                        return Ok(());
                    };
                    let Some(group) = app.config.groups.get(index).cloned() else {
                        return Ok(());
                    };
                    let path = project.path.clone();
                    if app.config.project_groups(&path).contains(&group) {
                        app.config.remove_project_from_group(&path, &group);
                    } else {
                        app.config.add_project_to_group(&path, &group);
                    }
                    app.config.save()?;
                    app.recompute_visible();
                }
            }
            Ok(())
        };
        match action {
            Action::InputEscape | Action::Quit | Action::GroupManager => {
                self.mode = Mode::Workspace
            }
            Action::InputChar('j') | Action::NavigateDown if group_count > 0 => {
                self.mode = manager_mode((selected + 1) % group_count, scroll)
            }
            Action::InputChar('k') | Action::NavigateUp if group_count > 0 => {
                self.mode = manager_mode(selected.checked_sub(1).unwrap_or(group_count - 1), scroll)
            }
            Action::MouseClick { col, row } => {
                if let Some(index) = layout.item_at(Position::new(col, row), scroll, group_count) {
                    toggle(self, index)?;
                    self.mode = manager_mode(index, scroll);
                }
            }
            Action::Select | Action::InputChar(' ') => {
                toggle(self, selected)?;
                self.mode = manager_mode(selected, scroll);
            }
            Action::InputChar('a') if purpose == GroupManagerPurpose::Switch => {
                self.mode = Mode::Input {
                    context: InputContext::AddGroup,
                    state: InputState::new("group name: "),
                };
            }
            Action::InputChar('r') if purpose == GroupManagerPurpose::Switch => {
                if selected < 1 {
                    self.set_status("Virtual groups cannot be renamed");
                } else {
                    let group_idx = selected - 1;
                    if let Some(name) = self.config.groups.get(group_idx) {
                        self.mode = Mode::Input {
                            context: InputContext::RenameGroup { group_idx },
                            state: InputState::with_value("new name: ", name.clone()),
                        };
                    }
                }
            }
            Action::InputChar('d') if purpose == GroupManagerPurpose::Switch => {
                if selected < 1 {
                    self.set_status("Virtual groups cannot be deleted");
                } else {
                    let group_idx = selected - 1;
                    if let Some(name) = self.config.groups.get(group_idx) {
                        let count = self
                            .config
                            .projects
                            .iter()
                            .filter(|project| project.groups.contains(name))
                            .count();
                        self.mode = Mode::Confirm {
                            message: format!("Delete group '{name}'? Removes {count} memberships"),
                            pending: PendingAction::DeleteGroup { group_idx },
                        };
                    }
                }
            }
            Action::InputChar('J') if purpose == GroupManagerPurpose::Switch && selected >= 1 => {
                let group_idx = selected - 1;
                if group_idx + 1 < self.config.groups.len() {
                    self.config.groups.swap(group_idx, group_idx + 1);
                    self.config.save()?;
                    self.mode = manager_mode(selected + 1, scroll);
                }
            }
            Action::InputChar('K') if purpose == GroupManagerPurpose::Switch && selected > 1 => {
                let group_idx = selected - 1;
                self.config.groups.swap(group_idx - 1, group_idx);
                self.config.save()?;
                self.mode = manager_mode(selected - 1, scroll);
            }
            _ => {}
        }
        Ok(())
    }
}
