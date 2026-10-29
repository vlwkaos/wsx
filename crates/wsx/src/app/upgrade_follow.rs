use super::*;

impl App {
    pub(super) fn apply_integration_scan(
        &mut self,
        result: Result<Vec<wsx_core::integration::IntegrationMetadata>>,
    ) {
        match result {
            Ok(metadata) => {
                if let Mode::IntegrationManager { manager, .. } = &mut self.mode {
                    manager.replace_metadata(metadata.clone());
                }
                self.integration_metadata = metadata;
                self.pending_integration_updates = self
                    .integration_metadata
                    .iter()
                    .filter(|metadata| {
                        metadata.available
                            && metadata.compatible
                            && metadata.install_status
                                == wsx_core::integration::InstallStatus::Outdated
                    })
                    .map(|metadata| metadata.target)
                    .collect();
                if let Some(target) = self.pending_integration_demand.take() {
                    self.prompt_for_integration_if_needed(target);
                }
                self.prompt_for_pending_integration_updates();
                self.needs_redraw = true;
            }
            Err(error) => self.set_warning(format!("Agent integration scan failed: {error}")),
        }
    }

    // ^ Startup and demand prompts share this modal-safe boundary and dismissal policy.
    pub(super) fn prompt_for_integration_if_needed(
        &mut self,
        target: wsx_core::integration::IntegrationTarget,
    ) {
        use wsx_core::integration::InstallStatus;
        if !matches!(self.mode, Mode::Workspace) {
            self.pending_integration_demand = Some(target);
            return;
        }
        if self.integration_metadata.is_empty() {
            self.pending_integration_demand = Some(target);
            return;
        }
        self.pending_integration_demand = None;
        let Some(metadata) = self.integration_metadata.iter().find(|metadata| {
            metadata.target == target
                && metadata.available
                && metadata.compatible
                && metadata.install_status != InstallStatus::Current
        }) else {
            return;
        };
        let remember_dismissal = metadata.install_status == InstallStatus::Missing;
        if remember_dismissal && self.dismissed_integration_prompts.contains(&target) {
            return;
        }
        self.pending_integration_updates
            .retain(|pending| *pending != target);
        self.mode = Mode::Confirm {
            message: if remember_dismissal {
                format!(
                    "Install the {} status integration? Declining disables this prompt until you install it in Global Settings.",
                    target.label()
                )
            } else {
                format!("Update the {} status integration?", target.label())
            },
            pending: PendingAction::InstallIntegrations {
                targets: vec![target],
                remember_dismissal,
            },
        };
        self.needs_redraw = true;
    }

    pub(super) fn prompt_for_pending_integration_updates(&mut self) {
        if !matches!(self.mode, Mode::Workspace) || self.pending_integration_updates.is_empty() {
            return;
        }
        let targets = std::mem::take(&mut self.pending_integration_updates);
        let labels = targets
            .iter()
            .map(|target| target.label())
            .collect::<Vec<_>>()
            .join(", ");
        self.mode = Mode::Confirm {
            message: format!("Update outdated agent integrations: {labels}?"),
            pending: PendingAction::InstallIntegrations {
                targets,
                remember_dismissal: false,
            },
        };
        self.needs_redraw = true;
    }

    pub fn upgrade_gate(&self) -> Option<&UpgradeGate> {
        self.upgrade.as_ref()
    }

    // The handoff blocks its caller until the successor answers, so it runs off the UI thread.
    pub(super) fn spawn_upgrade_attempt(&mut self) {
        let tx = self.runtime_tx.clone();
        let client = self.runtime_client.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::UpgradeAttempt(
                runtime::ensure_available(&client).map_err(Into::into),
            ));
        });
    }

    pub(super) fn apply_upgrade_attempt(&mut self, result: Result<runtime::Availability>) {
        // The user may have opened the old daemon while this attempt ran.
        let Some(gate) = self.upgrade.as_mut() else {
            return;
        };
        self.needs_redraw = true;
        match result {
            Ok(runtime::Availability::ReplacementDeferred {
                live_runtimes,
                blockers,
                ..
            }) => {
                gate.live_runtimes = live_runtimes;
                gate.step = UpgradeStep::Waiting(blockers);
                gate.retry_at = Some(Instant::now() + UPGRADE_RETRY);
            }
            Ok(availability) => {
                // A handoff that waited is finished by the daemon, not by this request.
                let availability = match availability {
                    runtime::Availability::Current => runtime::Availability::DaemonReplaced {
                        previous_version: gate.daemon_version.clone(),
                    },
                    other => other,
                };
                self.upgrade = None;
                self.announce_runtime_availability(&availability);
                self.spawn_runtime_refresh();
            }
            Err(error) => {
                let daemon_version = gate.daemon_version.clone();
                self.upgrade = None;
                self.set_warning(format!(
                    "Upgrade to wsxd {} did not finish; this window runs on wsxd {daemon_version}\n{error}",
                    runtime::WSX_VERSION
                ));
                // Do not request the handoff again on the way in: read the running daemon.
                self.spawn_runtime_refresh_with(|_| Ok(runtime::Availability::Current));
            }
        }
    }

    pub(super) fn retry_upgrade(&mut self) {
        let Some(gate) = self.upgrade.as_mut() else {
            return;
        };
        if gate
            .retry_at
            .is_some_and(|retry_at| retry_at <= Instant::now())
        {
            gate.retry_at = None;
            self.spawn_upgrade_attempt();
        }
    }

    // Only a waiting upgrade offers this; a running handoff cannot be left.
    pub(super) fn open_on_running_daemon(&mut self) {
        if !matches!(
            self.upgrade.as_ref().map(|gate| &gate.step),
            Some(UpgradeStep::Waiting(_))
        ) {
            return;
        }
        self.upgrade = None;
        self.needs_redraw = true;
        // The normal start reports the waiting upgrade as a notice.
        self.spawn_runtime_refresh();
    }

    /// Show where this process came from and return to the terminal that was open.
    pub fn resume_after_follow(&mut self, from_version: &str, pane: Option<runtime::PaneId>) {
        self.follow_resume_pane = pane;
        self.set_notice(
            NoticeLevel::Success,
            format!(
                "wsx {from_version} became wsx {} with the upgraded wsxd\nSessions kept running",
                runtime::WSX_VERSION
            ),
        );
    }

    pub(super) fn resume_followed_terminal(&mut self, terminal: &mut Tui) -> Result<()> {
        // The first authoritative snapshot restores the cursor from the cache.
        if self.startup_availability_pending {
            return Ok(());
        }
        let Some(pane_id) = self.follow_resume_pane.take() else {
            return Ok(());
        };
        self.ensure_flat();
        let target = self
            .cached_flat
            .iter()
            .enumerate()
            .find_map(|(index, entry)| match *entry {
                FlatEntry::Session {
                    project_idx,
                    worktree_idx,
                    session_idx,
                } => self
                    .workspace
                    .session(project_idx, worktree_idx, session_idx)
                    .filter(|session| session.pane_id == pane_id)
                    .map(|_| (index, project_idx, worktree_idx, session_idx)),
                _ => None,
            });
        let Some((index, project_idx, worktree_idx, session_idx)) = target else {
            self.set_warning("The terminal that was open is no longer listed");
            return Ok(());
        };
        self.tree_selected = index;
        self.update_scroll();
        self.switch_terminal_session(project_idx, worktree_idx, session_idx, terminal)
    }

    // ^ docs/daemon-live-upgrade.md: checked when the daemon connection changes and on
    // the slow tick. The file check is local; the daemon question runs off this thread.
    pub(super) fn spawn_follow_check(&mut self) {
        if self.follow_check_pending || self.follow_target.is_some() {
            return;
        }
        let Some((path, identity)) = self.installed.as_ref().and_then(|wsx| wsx.replaced()) else {
            return;
        };
        self.follow_check_pending = true;
        let client = self.runtime_client.clone();
        let tx = self.runtime_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(RuntimeResult::FollowCheck(crate::follow::target(
                &client, path, identity,
            )));
        });
    }

    // Unsent input, a pending operation or a half-typed prefix would be lost by a new process.
    pub(super) fn follow_is_safe_now(&self) -> bool {
        matches!(self.mode, Mode::Workspace | Mode::Terminal { .. })
            && self.pending_terminal_entry.is_none()
            && !self.terminal_escape_chord.is_pending()
            && self.jobs.is_empty()
            && self.pending_deletions.is_empty()
            && self.pending_session_kills.is_empty()
            && self.pending_session_orders.is_empty()
    }

    pub(super) fn follow_installed_wsx(&mut self, terminal: &mut Tui) -> Result<()> {
        if self.follow_target.is_none() || !self.follow_is_safe_now() {
            return Ok(());
        }
        // The new process restores the cursor from the cache.
        let saved = self.persist_state(true);
        let (Some(target), Some(installed)) = (self.follow_target.take(), self.installed.as_ref())
        else {
            return Ok(());
        };
        let pane = match self.mode {
            Mode::Terminal { pane_id } => Some(pane_id),
            _ => None,
        };
        let mobile = self.force_mobile;
        // Shell modes first: a failed exec then returns to this TUI.
        let exec = saved.and_then(|()| {
            tui::with_raw_mode_disabled(terminal, || -> Result<()> {
                Err(installed.exec(mobile, pane).into())
            })
        });
        let failure = match exec {
            Err(error) if error.is::<tui::TerminalModeError>() => return Err(error),
            Err(error) => error,
            Ok(()) => return Ok(()),
        };
        let daemon_version = target.daemon_version.clone();
        if let Some(installed) = self.installed.as_mut() {
            installed.refuse(target);
        }
        self.force_terminal_redraw = true;
        self.set_warning(format!(
            "Could not switch to the installed wsx for wsxd {daemon_version}; this window keeps wsx {}\n{failure}",
            runtime::WSX_VERSION
        ));
        Ok(())
    }
}
