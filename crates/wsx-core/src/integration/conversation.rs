//! Provider launch plans for wsxd-owned structured conversations.
//!
//! Plans contain direct argv only. The daemon owns process supervision, strict
//! protocol parsing, lifecycle, and persistence.

use super::IntegrationTarget;
use crate::runtime::{AgentSessionRef, AgentSessionRefKind, ConversationCapabilities};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationLaunchPlan {
    pub argv: Vec<String>,
    pub capabilities: ConversationCapabilities,
}

pub fn plan(
    provider: &str,
    session_ref: Option<&AgentSessionRef>,
) -> Option<ConversationLaunchPlan> {
    let target = provider.parse::<IntegrationTarget>().ok()?;
    match target {
        IntegrationTarget::Pi => pi_plan(session_ref),
        _ => None,
    }
}

fn pi_plan(session_ref: Option<&AgentSessionRef>) -> Option<ConversationLaunchPlan> {
    let mut argv = vec!["pi".into(), "--mode".into(), "rpc".into()];
    if let Some(session_ref) = session_ref {
        let validated = match session_ref.kind {
            AgentSessionRefKind::Id => AgentSessionRef::id(session_ref.value.clone()),
            AgentSessionRefKind::Path => AgentSessionRef::path(session_ref.value.clone()),
        }?;
        argv.extend(["--session".into(), validated.value]);
    }
    Some(ConversationLaunchPlan {
        argv,
        capabilities: ConversationCapabilities {
            prompt: true,
            steer: true,
            follow_up: true,
            abort: true,
            model_selection: false,
            commands: true,
            interactions: true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_new_conversation_uses_rpc_without_disabling_persistence() {
        let plan = plan("pi", None).unwrap();
        assert_eq!(plan.argv, ["pi", "--mode", "rpc"]);
        assert!(plan.capabilities.prompt);
        assert!(plan.capabilities.interactions);
        assert!(!plan.capabilities.model_selection);
    }

    #[test]
    fn pi_resume_accepts_valid_id_and_absolute_path() {
        for session_ref in [
            AgentSessionRef::id("session-id").unwrap(),
            AgentSessionRef::path("/sessions/session.jsonl").unwrap(),
        ] {
            let plan = plan("pi", Some(&session_ref)).unwrap();
            assert_eq!(&plan.argv[..3], ["pi", "--mode", "rpc"]);
            assert_eq!(&plan.argv[3..], ["--session", session_ref.value.as_str()]);
        }
    }

    #[test]
    fn unsupported_provider_has_no_structured_plan() {
        assert_eq!(plan("claude", None), None);
        assert_eq!(plan("unknown", None), None);
    }
}
