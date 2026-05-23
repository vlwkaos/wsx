// Single source of truth for session state.
// SessionInfo carries only raw inputs (bell, foreground, pane_capture, muted);
// every UI/CLI consumer derives state through `derive` here — nowhere else.

use crate::model::workspace::{ForegroundKind, SessionInfo};
use crate::tmux::capture::{self, CaptureHint};

/// User-facing session state — the 3-state projection rendered as a tree icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSessionState {
    Idle,           // gray ○ — empty shell, nothing running
    Active,         // green ◉ — a process is running
    NeedsAttention, // yellow ● — bell, or an interactive prompt is waiting
}

/// Internal fine-grained classification. `derive` produces this; `app_state`
/// projects it down to the 3 user-facing states. Deliberately richer than
/// `AppSessionState` so future signals can read the heuristic without
/// re-deriving — it is not surfaced to the user directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHeuristic {
    Muted,
    Bell,
    WaitingForConfirm,
    WaitingForInput,
    RunningAgent,
    RunningRuntime,
    RunningApp,
    PassiveViewer,
    Shell,
    Unknown,
}

impl SessionHeuristic {
    pub fn app_state(self) -> AppSessionState {
        match self {
            SessionHeuristic::Bell
            | SessionHeuristic::WaitingForConfirm
            | SessionHeuristic::WaitingForInput => AppSessionState::NeedsAttention,
            SessionHeuristic::RunningAgent
            | SessionHeuristic::RunningRuntime
            | SessionHeuristic::RunningApp => AppSessionState::Active,
            SessionHeuristic::Muted
            | SessionHeuristic::PassiveViewer
            | SessionHeuristic::Shell
            | SessionHeuristic::Unknown => AppSessionState::Idle,
        }
    }
}

/// Classify a session. Precedence: muted > bell > capture hint > foreground.
pub fn derive(session: &SessionInfo) -> SessionHeuristic {
    if session.muted {
        return SessionHeuristic::Muted;
    }
    if session.has_activity {
        return SessionHeuristic::Bell;
    }
    // Capture hints escalate regardless of foreground kind — an interactive
    // prompt warrants attention whether the process is an agent or a runtime.
    if let Some(hint) = session
        .pane_capture
        .as_deref()
        .and_then(capture::detect_capture_hint)
    {
        return match hint {
            CaptureHint::WaitingForConfirm => SessionHeuristic::WaitingForConfirm,
            CaptureHint::WaitingForInput => SessionHeuristic::WaitingForInput,
        };
    }
    match session.foreground {
        ForegroundKind::Agent => SessionHeuristic::RunningAgent,
        ForegroundKind::Runtime => SessionHeuristic::RunningRuntime,
        ForegroundKind::InteractiveApp => SessionHeuristic::RunningApp,
        ForegroundKind::PassiveViewer => SessionHeuristic::PassiveViewer,
        ForegroundKind::Shell => SessionHeuristic::Shell,
        ForegroundKind::Unknown => SessionHeuristic::Unknown,
    }
}

pub fn status_label(session: &SessionInfo) -> &'static str {
    match derive(session).app_state() {
        AppSessionState::Idle => "idle",
        AppSessionState::Active => "active",
        AppSessionState::NeedsAttention => "attention",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::workspace::{ForegroundKind, SessionInfo};

    fn session(
        muted: bool,
        has_activity: bool,
        pane_capture: Option<&str>,
        foreground: ForegroundKind,
    ) -> SessionInfo {
        session_full(muted, has_activity, pane_capture, foreground, false)
    }

    // Extended helper: exposes is_running_wsx so the wsx-observing-itself case is reachable.
    fn session_full(
        muted: bool,
        has_activity: bool,
        pane_capture: Option<&str>,
        foreground: ForegroundKind,
        is_running_wsx: bool,
    ) -> SessionInfo {
        SessionInfo {
            name: "proj-wt-sess".to_string(),
            display_name: "sess".to_string(),
            has_activity,
            pane_capture: pane_capture.map(|s| s.to_string()),
            last_activity: Some(std::time::Instant::now()),
            foreground,
            is_running_wsx,
            muted,
        }
    }

    // ── derive: single factor → heuristic ────────────────────────────────

    #[test]
    fn given_muted_session_when_derive_then_muted() {
        let s = session(true, false, None, ForegroundKind::Unknown);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }
    #[test]
    fn given_bell_set_when_derive_then_bell() {
        let s = session(false, true, None, ForegroundKind::Unknown);
        assert_eq!(derive(&s), SessionHeuristic::Bell);
    }
    #[test]
    fn given_confirm_prompt_capture_when_derive_then_waiting_for_confirm() {
        let s = session(false, false, Some("build done\nContinue? [y/n]"), ForegroundKind::Unknown);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }
    #[test]
    fn given_input_wait_capture_when_derive_then_waiting_for_input() {
        let s = session(false, false, Some("agent paused\nwaiting for user"), ForegroundKind::Unknown);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForInput);
    }
    #[test]
    fn given_agent_foreground_when_derive_then_running_agent() {
        let s = session(false, false, None, ForegroundKind::Agent);
        assert_eq!(derive(&s), SessionHeuristic::RunningAgent);
    }
    #[test]
    fn given_runtime_foreground_when_derive_then_running_runtime() {
        let s = session(false, false, None, ForegroundKind::Runtime);
        assert_eq!(derive(&s), SessionHeuristic::RunningRuntime);
    }
    #[test]
    fn given_interactive_app_foreground_when_derive_then_running_app() {
        let s = session(false, false, None, ForegroundKind::InteractiveApp);
        assert_eq!(derive(&s), SessionHeuristic::RunningApp);
    }
    #[test]
    fn given_passive_viewer_foreground_when_derive_then_passive_viewer() {
        let s = session(false, false, None, ForegroundKind::PassiveViewer);
        assert_eq!(derive(&s), SessionHeuristic::PassiveViewer);
    }
    #[test]
    fn given_shell_foreground_when_derive_then_shell() {
        let s = session(false, false, None, ForegroundKind::Shell);
        assert_eq!(derive(&s), SessionHeuristic::Shell);
    }
    #[test]
    fn given_unknown_foreground_when_derive_then_unknown() {
        let s = session(false, false, None, ForegroundKind::Unknown);
        assert_eq!(derive(&s), SessionHeuristic::Unknown);
    }

    // ── derive: precedence — muted > bell > capture hint > foreground ────

    #[test]
    fn given_muted_and_bell_and_running_when_derive_then_muted_wins() {
        let s = session(true, true, None, ForegroundKind::Agent);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }
    #[test]
    fn given_bell_and_running_foreground_when_derive_then_bell_wins() {
        let s = session(false, true, None, ForegroundKind::Agent);
        assert_eq!(derive(&s), SessionHeuristic::Bell);
    }
    #[test]
    fn given_muted_and_confirm_capture_when_derive_then_muted_wins() {
        let s = session(true, false, Some("Continue? [y/n]"), ForegroundKind::Agent);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }
    #[test]
    fn given_bell_and_confirm_capture_when_derive_then_bell_wins() {
        let s = session(false, true, Some("Continue? [y/n]"), ForegroundKind::Agent);
        assert_eq!(derive(&s), SessionHeuristic::Bell);
    }
    #[test]
    fn given_confirm_capture_and_shell_foreground_when_derive_then_capture_escalates() {
        let s = session(false, false, Some("Continue? [y/n]"), ForegroundKind::Shell);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }
    #[test]
    fn given_input_wait_capture_and_runtime_foreground_when_derive_then_capture_escalates() {
        let s = session(false, false, Some("waiting for user"), ForegroundKind::Runtime);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForInput);
    }

    // ── derive: capture fall-through — no prompt marker → use foreground ─

    #[test]
    fn given_capture_with_no_prompt_when_derive_then_falls_through_to_foreground() {
        let s = session(
            false,
            false,
            Some("just some ordinary log output\nnothing to see here"),
            ForegroundKind::Agent,
        );
        assert_eq!(derive(&s), SessionHeuristic::RunningAgent);
    }
    #[test]
    fn given_empty_string_capture_when_derive_then_falls_through_to_foreground() {
        let s = session(false, false, Some(""), ForegroundKind::Shell);
        assert_eq!(derive(&s), SessionHeuristic::Shell);
    }
    #[test]
    fn given_capture_with_both_confirm_and_input_markers_when_derive_then_confirm_wins() {
        let s = session(
            false,
            false,
            Some("waiting for user\nContinue? [y/n]"),
            ForegroundKind::Unknown,
        );
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }
    #[test]
    fn given_running_wsx_session_when_derive_then_still_derives_from_foreground() {
        let s = session_full(false, false, None, ForegroundKind::Agent, true);
        assert_eq!(derive(&s), SessionHeuristic::RunningAgent);
    }

    // ── app_state(): 10 → 3 projection ───────────────────────────────────

    #[test]
    fn app_state_bell_is_needs_attention() {
        assert_eq!(SessionHeuristic::Bell.app_state(), AppSessionState::NeedsAttention);
    }
    #[test]
    fn app_state_waiting_for_confirm_is_needs_attention() {
        assert_eq!(
            SessionHeuristic::WaitingForConfirm.app_state(),
            AppSessionState::NeedsAttention
        );
    }
    #[test]
    fn app_state_waiting_for_input_is_needs_attention() {
        assert_eq!(
            SessionHeuristic::WaitingForInput.app_state(),
            AppSessionState::NeedsAttention
        );
    }
    #[test]
    fn app_state_running_agent_is_active() {
        assert_eq!(SessionHeuristic::RunningAgent.app_state(), AppSessionState::Active);
    }
    #[test]
    fn app_state_running_runtime_is_active() {
        assert_eq!(SessionHeuristic::RunningRuntime.app_state(), AppSessionState::Active);
    }
    #[test]
    fn app_state_running_app_is_active() {
        assert_eq!(SessionHeuristic::RunningApp.app_state(), AppSessionState::Active);
    }
    #[test]
    fn app_state_muted_is_idle() {
        assert_eq!(SessionHeuristic::Muted.app_state(), AppSessionState::Idle);
    }
    #[test]
    fn app_state_passive_viewer_is_idle() {
        assert_eq!(SessionHeuristic::PassiveViewer.app_state(), AppSessionState::Idle);
    }
    #[test]
    fn app_state_shell_is_idle() {
        assert_eq!(SessionHeuristic::Shell.app_state(), AppSessionState::Idle);
    }
    #[test]
    fn app_state_unknown_is_idle() {
        assert_eq!(SessionHeuristic::Unknown.app_state(), AppSessionState::Idle);
    }

    // ── status_label() ───────────────────────────────────────────────────

    #[test]
    fn given_idle_session_when_status_label_then_idle() {
        let s = session(false, false, None, ForegroundKind::Shell);
        assert_eq!(status_label(&s), "idle");
    }
    #[test]
    fn given_running_session_when_status_label_then_active() {
        let s = session(false, false, None, ForegroundKind::Agent);
        assert_eq!(status_label(&s), "active");
    }
    #[test]
    fn given_bell_session_when_status_label_then_attention() {
        let s = session(false, true, None, ForegroundKind::Unknown);
        assert_eq!(status_label(&s), "attention");
    }
}
