// Single source of truth for session state.
// `SessionInfo` carries raw inputs only (bell, foreground, pane_capture,
// last_activity, muted); UI and CLI consumers read the 3-state projection
// via `derive(...).app_state()` — never re-implement the logic.

use wsx_core::model::workspace::{ForegroundKind, SessionInfo};
use wsx_core::ops::IDLE_SECS;
use wsx_core::tmux::capture::{self, CaptureHint};

/// User-facing session state — the 3-state projection rendered as a tree icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSessionState {
    Idle,           // gray ○ — at rest
    Active,         // green ◉ — process is actively doing work, or running infra
    NeedsAttention, // yellow ● — bell, agent done, shell prompt, or interactive prompt waiting
}

/// Internal fine-grained classification. `derive` produces this; `app_state`
/// projects it to the 3 user-facing states. Carries the working/done
/// distinction agents need but the user-facing icon collapses away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHeuristic {
    Muted,
    Bell,
    WaitingForConfirm,
    WaitingForInput,
    AgentWorking, // Agent foreground + recent output
    AgentDone,    // Agent foreground + no recent output (waiting for you to look)
    ServerRunning, // Runtime or InteractiveApp — green while the process exists
    ShellPrompt,  // Shell + recent output (just returned to prompt)
    ShellIdle,    // Shell + no recent output (at rest)
    PassiveViewer,
    Unknown,
}

impl SessionHeuristic {
    pub fn app_state(self) -> AppSessionState {
        match self {
            SessionHeuristic::Bell
            | SessionHeuristic::WaitingForConfirm
            | SessionHeuristic::WaitingForInput
            | SessionHeuristic::AgentDone
            | SessionHeuristic::ShellPrompt => AppSessionState::NeedsAttention,
            SessionHeuristic::AgentWorking | SessionHeuristic::ServerRunning => {
                AppSessionState::Active
            }
            SessionHeuristic::Muted
            | SessionHeuristic::ShellIdle
            | SessionHeuristic::PassiveViewer
            | SessionHeuristic::Unknown => AppSessionState::Idle,
        }
    }
}

fn is_recently_active(session: &SessionInfo) -> bool {
    session
        .last_activity
        .map(|t| t.elapsed().as_secs() < IDLE_SECS)
        .unwrap_or(false)
}

/// Classify a session. Precedence: muted > bell > capture hint > foreground.
/// For Agent and Shell, recent activity splits the heuristic further
/// (working vs done; prompt vs idle).
pub fn derive(session: &SessionInfo) -> SessionHeuristic {
    if session.muted {
        return SessionHeuristic::Muted;
    }
    if session.has_activity {
        return SessionHeuristic::Bell;
    }
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
    let recent = is_recently_active(session);
    match session.foreground {
        ForegroundKind::Agent => {
            if recent {
                SessionHeuristic::AgentWorking
            } else {
                SessionHeuristic::AgentDone
            }
        }
        ForegroundKind::Runtime | ForegroundKind::InteractiveApp => SessionHeuristic::ServerRunning,
        ForegroundKind::Shell => {
            if recent {
                SessionHeuristic::ShellPrompt
            } else {
                SessionHeuristic::ShellIdle
            }
        }
        ForegroundKind::PassiveViewer => SessionHeuristic::PassiveViewer,
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
    use wsx_core::model::workspace::{ForegroundKind, SessionInfo};
    use std::time::Instant;

    fn sess(
        foreground: ForegroundKind,
        has_activity: bool,
        recent: bool,
        capture: Option<&str>,
        muted: bool,
    ) -> SessionInfo {
        SessionInfo {
            name: "s".into(),
            display_name: "s".into(),
            has_activity,
            pane_capture: capture.map(|c| c.to_string()),
            last_activity: if recent { Some(Instant::now()) } else { None },
            foreground,
            is_running_wsx: false,
            muted,
        }
    }

    // ── A. per-branch derive ─────────────────────────────────────────────────

    #[test]
    fn given_muted_session_when_derived_then_muted() {
        let s = sess(ForegroundKind::Shell, false, false, None, true);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }

    #[test]
    fn given_bell_session_when_derived_then_bell() {
        let s = sess(ForegroundKind::Shell, true, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::Bell);
    }

    #[test]
    fn given_confirm_capture_when_derived_then_waiting_for_confirm() {
        let s = sess(ForegroundKind::Unknown, false, false, Some("Continue? [y/n]"), false);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }

    #[test]
    fn given_input_wait_capture_when_derived_then_waiting_for_input() {
        let s = sess(ForegroundKind::Unknown, false, false, Some("waiting for user"), false);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForInput);
    }

    #[test]
    fn given_non_prompt_capture_when_derived_then_capture_does_not_match() {
        let s = sess(ForegroundKind::Unknown, false, false, Some("just some output"), false);
        assert_eq!(derive(&s), SessionHeuristic::Unknown);
    }

    #[test]
    fn given_empty_capture_string_when_derived_then_falls_through_to_foreground() {
        // Some("") is a capture that matches no hint; must fall through.
        let s = sess(ForegroundKind::Shell, false, true, Some(""), false);
        assert_eq!(derive(&s), SessionHeuristic::ShellPrompt);
    }

    #[test]
    fn given_agent_foreground_and_recent_when_derived_then_agent_working() {
        let s = sess(ForegroundKind::Agent, false, true, None, false);
        assert_eq!(derive(&s), SessionHeuristic::AgentWorking);
    }

    #[test]
    fn given_agent_foreground_and_not_recent_when_derived_then_agent_done() {
        let s = sess(ForegroundKind::Agent, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::AgentDone);
    }

    #[test]
    fn given_runtime_foreground_when_derived_then_server_running() {
        let s = sess(ForegroundKind::Runtime, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::ServerRunning);
    }

    #[test]
    fn given_interactive_app_foreground_when_derived_then_server_running() {
        let s = sess(ForegroundKind::InteractiveApp, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::ServerRunning);
    }

    #[test]
    fn given_shell_foreground_and_recent_when_derived_then_shell_prompt() {
        let s = sess(ForegroundKind::Shell, false, true, None, false);
        assert_eq!(derive(&s), SessionHeuristic::ShellPrompt);
    }

    #[test]
    fn given_shell_foreground_and_not_recent_when_derived_then_shell_idle() {
        let s = sess(ForegroundKind::Shell, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::ShellIdle);
    }

    #[test]
    fn given_passive_viewer_foreground_when_derived_then_passive_viewer() {
        let s = sess(ForegroundKind::PassiveViewer, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::PassiveViewer);
    }

    #[test]
    fn given_unknown_foreground_when_derived_then_unknown() {
        let s = sess(ForegroundKind::Unknown, false, false, None, false);
        assert_eq!(derive(&s), SessionHeuristic::Unknown);
    }

    // ── B. precedence interactions ───────────────────────────────────────────

    #[test]
    fn given_muted_and_bell_and_agent_when_derived_then_muted_wins() {
        let s = sess(ForegroundKind::Agent, true, true, None, true);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }

    #[test]
    fn given_muted_and_bell_and_capture_when_derived_then_muted_wins() {
        let s = sess(ForegroundKind::Agent, true, true, Some("Continue? [y/n]"), true);
        assert_eq!(derive(&s), SessionHeuristic::Muted);
    }

    #[test]
    fn given_bell_and_confirm_capture_and_agent_when_derived_then_bell_wins() {
        let s = sess(ForegroundKind::Agent, true, false, Some("Continue? [y/n]"), false);
        assert_eq!(derive(&s), SessionHeuristic::Bell);
    }

    #[test]
    fn given_confirm_capture_and_agent_recent_when_derived_then_capture_wins_over_agent() {
        let s = sess(ForegroundKind::Agent, false, true, Some("Continue? [y/n]"), false);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }

    #[test]
    fn given_confirm_capture_and_shell_recent_when_derived_then_capture_wins_over_shell() {
        let s = sess(ForegroundKind::Shell, false, true, Some("Continue? [y/n]"), false);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }

    #[test]
    fn given_input_wait_capture_and_runtime_when_derived_then_capture_wins_over_runtime() {
        let s = sess(ForegroundKind::Runtime, false, false, Some("waiting for user"), false);
        assert_eq!(derive(&s), SessionHeuristic::WaitingForInput);
    }

    #[test]
    fn given_confirm_and_input_phrases_when_derived_then_confirm_wins() {
        let s = sess(
            ForegroundKind::Unknown,
            false,
            false,
            Some("waiting for user\nContinue? [y/n]"),
            false,
        );
        assert_eq!(derive(&s), SessionHeuristic::WaitingForConfirm);
    }

    #[test]
    fn given_non_prompt_capture_and_agent_recent_when_derived_then_agent_working() {
        let s = sess(ForegroundKind::Agent, false, true, Some("just some output"), false);
        assert_eq!(derive(&s), SessionHeuristic::AgentWorking);
    }

    // ── C. activity gating ───────────────────────────────────────────────────

    #[test]
    fn given_runtime_recent_vs_none_when_derived_then_no_gating() {
        let recent = sess(ForegroundKind::Runtime, false, true, None, false);
        let none = sess(ForegroundKind::Runtime, false, false, None, false);
        assert_eq!(derive(&recent), SessionHeuristic::ServerRunning);
        assert_eq!(derive(&none), SessionHeuristic::ServerRunning);
    }

    #[test]
    fn given_interactive_app_recent_vs_none_when_derived_then_no_gating() {
        let recent = sess(ForegroundKind::InteractiveApp, false, true, None, false);
        let none = sess(ForegroundKind::InteractiveApp, false, false, None, false);
        assert_eq!(derive(&recent), SessionHeuristic::ServerRunning);
        assert_eq!(derive(&none), SessionHeuristic::ServerRunning);
    }

    // ── D. app_state projection ─────────────────────────────────────────────

    #[test]
    fn given_heuristic_muted_when_projected_then_idle() {
        assert_eq!(SessionHeuristic::Muted.app_state(), AppSessionState::Idle);
    }

    #[test]
    fn given_heuristic_bell_when_projected_then_needs_attention() {
        assert_eq!(SessionHeuristic::Bell.app_state(), AppSessionState::NeedsAttention);
    }

    #[test]
    fn given_heuristic_waiting_for_confirm_when_projected_then_needs_attention() {
        assert_eq!(
            SessionHeuristic::WaitingForConfirm.app_state(),
            AppSessionState::NeedsAttention
        );
    }

    #[test]
    fn given_heuristic_waiting_for_input_when_projected_then_needs_attention() {
        assert_eq!(
            SessionHeuristic::WaitingForInput.app_state(),
            AppSessionState::NeedsAttention
        );
    }

    #[test]
    fn given_heuristic_agent_working_when_projected_then_active() {
        assert_eq!(SessionHeuristic::AgentWorking.app_state(), AppSessionState::Active);
    }

    #[test]
    fn given_heuristic_agent_done_when_projected_then_needs_attention() {
        assert_eq!(
            SessionHeuristic::AgentDone.app_state(),
            AppSessionState::NeedsAttention
        );
    }

    #[test]
    fn given_heuristic_server_running_when_projected_then_active() {
        assert_eq!(SessionHeuristic::ServerRunning.app_state(), AppSessionState::Active);
    }

    #[test]
    fn given_heuristic_shell_prompt_when_projected_then_needs_attention() {
        assert_eq!(
            SessionHeuristic::ShellPrompt.app_state(),
            AppSessionState::NeedsAttention
        );
    }

    #[test]
    fn given_heuristic_shell_idle_when_projected_then_idle() {
        assert_eq!(SessionHeuristic::ShellIdle.app_state(), AppSessionState::Idle);
    }

    #[test]
    fn given_heuristic_passive_viewer_when_projected_then_idle() {
        assert_eq!(SessionHeuristic::PassiveViewer.app_state(), AppSessionState::Idle);
    }

    #[test]
    fn given_heuristic_unknown_when_projected_then_idle() {
        assert_eq!(SessionHeuristic::Unknown.app_state(), AppSessionState::Idle);
    }

    // ── E. status_label ─────────────────────────────────────────────────────

    #[test]
    fn given_idle_state_when_labelled_then_idle_string() {
        let s = sess(ForegroundKind::PassiveViewer, false, false, None, false);
        assert_eq!(status_label(&s), "idle");
    }

    #[test]
    fn given_muted_state_when_labelled_then_idle_string() {
        let s = sess(ForegroundKind::Agent, true, true, None, true);
        assert_eq!(status_label(&s), "idle");
    }

    #[test]
    fn given_active_state_when_labelled_then_active_string() {
        let s = sess(ForegroundKind::Runtime, false, false, None, false);
        assert_eq!(status_label(&s), "active");
    }

    #[test]
    fn given_needs_attention_state_when_labelled_then_attention_string() {
        let s = sess(ForegroundKind::Shell, true, false, None, false);
        assert_eq!(status_label(&s), "attention");
    }

    // ── F. realistic-workspace distribution (the v0.15.9-catcher) ───────────

    #[test]
    fn given_realistic_workspace_when_classified_then_each_session_state_matches_spec() {
        // Regression guard for the v0.15.9 "everything green" release. The
        // earlier draft asserted aggregate counts (4/4/2), but those counts
        // are reachable by several wrong mappings. Pin each session's
        // expected outcome individually, then keep the aggregate as a
        // belt-and-suspenders consistency check.
        let cases: Vec<(SessionInfo, SessionHeuristic, AppSessionState)> = vec![
            (
                sess(ForegroundKind::Agent, false, true, None, false),
                SessionHeuristic::AgentWorking,
                AppSessionState::Active,
            ),
            (
                sess(ForegroundKind::Agent, false, false, None, false),
                SessionHeuristic::AgentDone,
                AppSessionState::NeedsAttention,
            ),
            (
                sess(ForegroundKind::Runtime, false, false, None, false),
                SessionHeuristic::ServerRunning,
                AppSessionState::Active,
            ),
            (
                sess(ForegroundKind::Runtime, false, true, None, false),
                SessionHeuristic::ServerRunning,
                AppSessionState::Active,
            ),
            (
                sess(ForegroundKind::InteractiveApp, false, false, None, false),
                SessionHeuristic::ServerRunning,
                AppSessionState::Active,
            ),
            (
                sess(ForegroundKind::Shell, false, true, None, false),
                SessionHeuristic::ShellPrompt,
                AppSessionState::NeedsAttention,
            ),
            (
                sess(ForegroundKind::Shell, false, false, None, false),
                SessionHeuristic::ShellIdle,
                AppSessionState::Idle,
            ),
            (
                sess(ForegroundKind::Shell, true, false, None, false),
                SessionHeuristic::Bell,
                AppSessionState::NeedsAttention,
            ),
            (
                sess(ForegroundKind::Agent, false, false, Some("Continue? [y/n]"), false),
                SessionHeuristic::WaitingForConfirm,
                AppSessionState::NeedsAttention,
            ),
            (
                sess(ForegroundKind::Agent, false, true, None, true),
                SessionHeuristic::Muted,
                AppSessionState::Idle,
            ),
        ];
        for (i, (s, expected_heur, expected_state)) in cases.iter().enumerate() {
            let actual_heur = derive(s);
            assert_eq!(
                actual_heur, *expected_heur,
                "session #{i}: heuristic mismatch (got {:?}, want {:?})",
                actual_heur, expected_heur
            );
            assert_eq!(
                actual_heur.app_state(),
                *expected_state,
                "session #{i}: app_state mismatch",
            );
        }
        let (mut active, mut attention, mut idle) = (0, 0, 0);
        for (_, _, st) in &cases {
            match st {
                AppSessionState::Active => active += 1,
                AppSessionState::NeedsAttention => attention += 1,
                AppSessionState::Idle => idle += 1,
            }
        }
        assert_eq!((active, attention, idle), (4, 4, 2));
    }

    #[test]
    fn given_workspace_with_no_recent_activity_then_no_agent_or_shell_is_active() {
        let sessions = vec![
            sess(ForegroundKind::Agent, false, false, None, false),
            sess(ForegroundKind::Shell, false, false, None, false),
            sess(ForegroundKind::Runtime, false, false, None, false),
            sess(ForegroundKind::InteractiveApp, false, false, None, false),
            sess(ForegroundKind::PassiveViewer, false, false, None, false),
            sess(ForegroundKind::Unknown, false, false, None, false),
        ];
        for s in &sessions {
            let state = derive(s).app_state();
            if matches!(s.foreground, ForegroundKind::Agent | ForegroundKind::Shell) {
                assert_ne!(
                    state,
                    AppSessionState::Active,
                    "Agent/Shell without recent activity must not be Active (foreground={:?})",
                    s.foreground
                );
            }
        }
    }

    #[test]
    fn given_workspace_with_no_recent_activity_when_classified_then_runtime_and_app_remain_active() {
        let runtime = sess(ForegroundKind::Runtime, false, false, None, false);
        let app = sess(ForegroundKind::InteractiveApp, false, false, None, false);
        assert_eq!(derive(&runtime).app_state(), AppSessionState::Active);
        assert_eq!(derive(&app).app_state(), AppSessionState::Active);
    }

    #[test]
    fn given_quiet_workspace_when_classified_then_attention_count_equals_quiet_agents() {
        let sessions = vec![
            sess(ForegroundKind::Agent, false, false, None, false),
            sess(ForegroundKind::Agent, false, false, None, false),
            sess(ForegroundKind::Shell, false, false, None, false),
            sess(ForegroundKind::Runtime, false, false, None, false),
            sess(ForegroundKind::InteractiveApp, false, false, None, false),
            sess(ForegroundKind::PassiveViewer, false, false, None, false),
            sess(ForegroundKind::Unknown, false, false, None, false),
        ];
        let agent_count = sessions
            .iter()
            .filter(|s| matches!(s.foreground, ForegroundKind::Agent))
            .count();
        let attention_count = sessions
            .iter()
            .filter(|s| derive(s).app_state() == AppSessionState::NeedsAttention)
            .count();
        assert_eq!(attention_count, agent_count);
    }
}
