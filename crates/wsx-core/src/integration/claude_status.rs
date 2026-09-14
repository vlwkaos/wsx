use crate::runtime::AgentState;
use std::time::Duration;

pub const WORKING_EVENT_LEAD: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvidence {
    Idle,
    Working,
    Blocked,
}

pub fn classify(title: &str, text: &str) -> Option<TerminalEvidence> {
    if title_is_working(title) {
        return Some(TerminalEvidence::Working);
    }

    let recent = text
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    let lower = recent.join("\n").to_ascii_lowercase();
    let visible_choice = lower.contains("enter to confirm")
        || lower.contains("enter to select")
        || lower.contains("arrow keys to navigate")
        || lower.contains("arrows to navigate")
        || lower.contains("↑/↓ to navigate")
        || lower.contains("↑↓ to navigate");
    let visible_permission = lower.contains("do you want to proceed?")
        && (lower.contains("1. yes")
            || lower.contains("2. yes")
            || lower.contains("2. no")
            || lower.contains("3. no"));
    if (lower.contains("esc to cancel") && (visible_choice || visible_permission))
        || (lower.contains("run a dynamic workflow?") && lower.contains("esc to cancel"))
    {
        return Some(TerminalEvidence::Blocked);
    }
    if title_is_idle(title) {
        return Some(TerminalEvidence::Idle);
    }

    recent
        .iter()
        .rev()
        .take(3)
        .any(|line| line.trim_start().starts_with('❯'))
        .then_some(TerminalEvidence::Idle)
}

pub fn reconcile(
    event_state: AgentState,
    event_age: Duration,
    terminal: Option<TerminalEvidence>,
) -> AgentState {
    match event_state {
        AgentState::Done | AgentState::Error | AgentState::Blocked => event_state,
        AgentState::Working => match terminal {
            Some(TerminalEvidence::Blocked) => AgentState::Blocked,
            Some(TerminalEvidence::Idle) if event_age >= WORKING_EVENT_LEAD => AgentState::Idle,
            _ => AgentState::Working,
        },
        AgentState::Idle | AgentState::Unknown => match terminal {
            Some(TerminalEvidence::Working) => AgentState::Working,
            Some(TerminalEvidence::Blocked) => AgentState::Blocked,
            Some(TerminalEvidence::Idle) => AgentState::Idle,
            None => event_state,
        },
    }
}

fn title_is_working(title: &str) -> bool {
    let Some(first) = title.trim_start().chars().next() else {
        return false;
    };
    matches!(first, '\u{2800}'..='\u{28ff}' | '◐' | '◓' | '◑' | '◒')
}

fn title_is_idle(title: &str) -> bool {
    title.trim_start().starts_with('✳')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_accepts_only_visible_claude_chrome() {
        for title in ["⠋ task", "◐ task"] {
            assert_eq!(classify(title, ""), Some(TerminalEvidence::Working));
        }
        assert_eq!(classify("✳ task", ""), Some(TerminalEvidence::Idle));
        assert_eq!(
            classify(
                "",
                "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel"
            ),
            Some(TerminalEvidence::Blocked)
        );
        assert_eq!(
            classify("", "────────────────\n❯ "),
            Some(TerminalEvidence::Idle)
        );
        assert_eq!(
            classify(
                "✳ task",
                "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel"
            ),
            Some(TerminalEvidence::Blocked)
        );
        assert_eq!(
            classify(
                "project",
                "❯ old prompt\nassistant output\nrunning tool\nstatus"
            ),
            None
        );
        assert_eq!(
            classify("project", "previous output said esc to cancel yesterday"),
            None
        );
    }

    #[test]
    fn reconciliation_preserves_outcomes_and_corrects_stale_working() {
        let settled = WORKING_EVENT_LEAD + Duration::from_millis(1);
        assert_eq!(
            reconcile(
                AgentState::Working,
                Duration::ZERO,
                Some(TerminalEvidence::Idle)
            ),
            AgentState::Working
        );
        assert_eq!(
            reconcile(AgentState::Working, settled, Some(TerminalEvidence::Idle)),
            AgentState::Idle
        );
        assert_eq!(
            reconcile(
                AgentState::Working,
                settled,
                Some(TerminalEvidence::Blocked)
            ),
            AgentState::Blocked
        );
        assert_eq!(
            reconcile(AgentState::Idle, settled, Some(TerminalEvidence::Working)),
            AgentState::Working
        );
        for state in [AgentState::Done, AgentState::Error, AgentState::Blocked] {
            assert_eq!(
                reconcile(state, settled, Some(TerminalEvidence::Idle)),
                state
            );
        }
        assert_eq!(
            reconcile(AgentState::Working, settled, None),
            AgentState::Working
        );
    }
}
