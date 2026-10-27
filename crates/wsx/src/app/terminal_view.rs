use super::*;

impl App {
    pub fn terminal_escape_label(&self) -> &str {
        &self.terminal_escape_chord.label
    }

    pub fn terminal_literal_escape_label(&self) -> String {
        self.terminal_escape_chord.literal_label()
    }

    pub fn terminal_session_navigation_label(&self) -> &'static str {
        self.terminal_escape_chord.session_navigation_label()
    }

    pub fn terminal_workspace_hint(&self) -> String {
        format!("({})workspace", self.terminal_escape_chord.label)
    }

    pub fn terminal_prefix_pending(&self) -> bool {
        self.terminal_escape_chord.is_pending()
    }

    pub fn terminal_command_hints(&self) -> Vec<String> {
        let Some(workspace) = self.terminal_escape_chord.suffix_label() else {
            return vec![self.terminal_workspace_hint().to_ascii_lowercase()];
        };
        let prefix = self
            .terminal_escape_chord
            .prefix_label()
            .to_ascii_lowercase();
        let hint = |key: &str, action: &str| format!("({key}){action}");
        let mut hints = vec![hint(&prefix, "commands")];
        if self.terminal_prefix_pending() {
            hints.push(hint("esc", "cancel"));
        }
        hints.push(hint(&workspace.to_ascii_lowercase(), "workspace"));
        // A hint names a key only while the key has a target.
        if self.context_session_target(1).is_some() {
            hints.push(hint(self.terminal_session_navigation_label(), "session"));
        }
        if adjacent_group(&self.config.ordered_group_keys(), &self.active_group, 1).is_some() {
            hints.push(hint("{/}", "group"));
        }
        hints.extend([
            crate::ui::IDLE_ITERATION_HINT.to_string(),
            crate::ui::ACTIVE_ITERATION_HINT.to_string(),
            crate::ui::ATTENTION_ITERATION_HINT.to_string(),
        ]);
        if !self.is_mobile {
            hints.push(hint("b", "sidebar"));
        }
        hints.push(hint("q", "quit"));
        if self.terminal_prefix_pending() {
            hints.push(hint(&prefix, "send"));
        }
        hints
    }

    pub fn terminal_quit_label(&self) -> Option<String> {
        self.terminal_escape_chord.quit_label()
    }

    pub fn terminal_sidebar_hint(&self) -> Option<String> {
        (!self.is_mobile)
            .then(|| self.terminal_escape_chord.sidebar_label())
            .flatten()
            .map(|label| format!("({label})sidebar"))
    }

    pub(super) fn terminal_sidebar_baseline(&self) -> TerminalSidebar {
        self.terminal_sidebar_override
            .unwrap_or(self.config.terminal_sidebar)
    }

    pub(crate) fn effective_terminal_sidebar(&self) -> TerminalSidebar {
        let baseline = self.terminal_sidebar_baseline();
        if !self.is_mobile
            && self.config.terminal_prefix_shows_sidebar
            && self.terminal_prefix_pending()
            && baseline == TerminalSidebar::Compact
        {
            TerminalSidebar::Expanded
        } else {
            baseline
        }
    }

    pub(super) fn sync_terminal_sidebar_geometry(
        &mut self,
        previous: Option<TerminalSidebar>,
        terminal: &Tui,
    ) {
        let current =
            matches!(self.mode, Mode::Terminal { .. }).then(|| self.effective_terminal_sidebar());
        let (Some(previous), Some(current)) = (previous, current) else {
            return;
        };
        if previous == current {
            return;
        }
        let pane_id = match &self.mode {
            Mode::Terminal { pane_id } => *pane_id,
            _ => return,
        };
        if self.pending_terminal_entry.is_some() {
            self.resize_pending_terminal_entry(terminal);
        } else {
            self.resize_terminal_pane(pane_id, terminal);
        }
        self.force_terminal_redraw = true;
        self.needs_redraw = true;
    }
}
