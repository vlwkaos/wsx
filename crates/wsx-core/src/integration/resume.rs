//! Direct argv plans for native coding-agent conversation restoration.
//!
//! References enter through wsxd's validated agent-report boundary. Planning
//! never invokes a shell and never infers identity from processes or terminal presentation.

use super::IntegrationTarget;
use crate::runtime::{AgentSessionRef, AgentSessionRefKind};

// ^ [[Session Model]] Native conversation resume recreates a provider process;
// it never restores the old PTY, process, terminal buffer, or lease.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

pub fn plan(provider: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    let validated = match session_ref.kind {
        AgentSessionRefKind::Id => AgentSessionRef::id(session_ref.value.clone()),
        AgentSessionRefKind::Path => AgentSessionRef::path(session_ref.value.clone()),
    }?;
    let value = validated.value;
    let target = provider.parse::<IntegrationTarget>().ok()?;
    let argv = match (target, session_ref.kind) {
        (IntegrationTarget::Pi, AgentSessionRefKind::Id | AgentSessionRefKind::Path) => {
            vec!["pi".into(), "--session".into(), value]
        }
        (IntegrationTarget::Omp, AgentSessionRefKind::Id | AgentSessionRefKind::Path) => {
            vec!["omp".into(), format!("--resume={value}")]
        }
        (IntegrationTarget::Claude, AgentSessionRefKind::Id) => {
            vec!["claude".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Codex, AgentSessionRefKind::Id) => {
            vec!["codex".into(), "resume".into(), value]
        }
        (IntegrationTarget::Copilot, AgentSessionRefKind::Id) => {
            vec!["copilot".into(), format!("--resume={value}")]
        }
        (IntegrationTarget::Devin, AgentSessionRefKind::Id) => {
            vec!["devin".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Droid, AgentSessionRefKind::Id) => {
            vec!["droid".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Kimi, AgentSessionRefKind::Id) => {
            vec!["kimi".into(), "--session".into(), value]
        }
        (IntegrationTarget::Opencode, AgentSessionRefKind::Id) => {
            vec!["opencode".into(), "--session".into(), value]
        }
        (IntegrationTarget::Kilo, AgentSessionRefKind::Id) => {
            vec!["kilo".into(), "--session".into(), value]
        }
        (IntegrationTarget::Hermes, AgentSessionRefKind::Id) => {
            vec!["hermes".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Qodercli, AgentSessionRefKind::Id) => {
            vec!["qodercli".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Qwen, AgentSessionRefKind::Id) => {
            vec!["qwen".into(), "--resume".into(), value]
        }
        (IntegrationTarget::Cursor, AgentSessionRefKind::Id) => vec![
            if cfg!(windows) {
                "cursor-agent.cmd"
            } else {
                "cursor-agent"
            }
            .into(),
            "--resume".into(),
            value,
        ],
        (IntegrationTarget::Mastracode, AgentSessionRefKind::Id) => {
            vec!["mastracode".into(), "--thread".into(), value]
        }
        (IntegrationTarget::AntigravityCli, AgentSessionRefKind::Id) => {
            vec!["agy".into(), "--conversation".into(), value]
        }
        (IntegrationTarget::Grok, AgentSessionRefKind::Id) => {
            vec!["grok".into(), "--resume".into(), value]
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        argv,
        dedupe_key: format!(
            "{}\0{:?}\0{}",
            target.cli_value(),
            session_ref.kind,
            session_ref.value
        ),
    })
}
