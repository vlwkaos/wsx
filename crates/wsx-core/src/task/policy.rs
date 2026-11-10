//! The one owner of what a task agent may do, so that no permission relies on the model: the
//! host derives a [`Policy`] from who the agent is and enforces it outside the model. What an
//! agent may do is its role's group of [`Capability`] values, cut down to the ones its
//! `[[agents]]` entry passed. Three consumers read it: the Seatbelt profile
//! (`wsx/src/task/boundary.rs`), the brokers' command check (`broker.rs`, `host.rs`) and the
//! approval rules of the outward commands (`WSX_TASK_TRUST`).
//! ref: docs/tasks.md "Roles", "Permissions"; docs/design/local-model-fallback.md 3.2, 5

use super::Actor;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;

/// Set by the broker on each command it runs for a manager: `trusted` or `untrusted`.
pub const TRUST_ENV: &str = "WSX_TASK_TRUST";
/// Programs the strict profile refuses to start: privilege, UI scripting and the service manager.
pub const DENIED_EXEC: [&str; 5] = [
    "/usr/bin/sudo",
    "/usr/bin/su",
    "/usr/bin/osascript",
    "/bin/launchctl",
    "/usr/bin/open",
];

/// One thing an agent may do. A `wsx task` subcommand needs one (`TaskCmd::needs`); the
/// jobs of the fallback design (answer, backlog, note, handoff) are what a model that passed
/// the tier test may be given. In `[[agents]]`: `may = ["read", "answer"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `list`, `show`, `ledger`, `chat`, `search`, `check`, `manager`, `routine list|show|logs`.
    Read,
    /// `post`, `digest`, `say`: a message to the owner's chat or another manager.
    Notify,
    /// Reply text to the owner (the manager's turn, the stand-in).
    Answer,
    /// `after`, `hold`: order and park tasks.
    Backlog,
    /// Write the manager's notes (compaction, memo folding).
    Note,
    /// Write the handoff notes of a rotation.
    Handoff,
    /// `new`.
    CreateTask,
    /// `add`, `steer`, `stop`, `preview`: act on a task. A named role: only on ones it created.
    Direct,
    /// `close` without `--discard`.
    Close,
    /// `close --discard`.
    Discard,
    /// `land` of a child into its parent task.
    LandChild,
    /// `land` onto the default branch.
    Land,
    /// `deploy`, `release`, `push`, `trial`.
    Deploy,
    /// `routine add|edit|delete|enable|disable|run|cancel|fire`, `checkin`.
    Routine,
    /// `role new`, `role close`.
    RoleAdmin,
    /// The owner's commands, which the CLI judges itself for a manager: `approve`, `deny`,
    /// `hand`, `handback`, `dismiss`, and the hidden host commands.
    Approve,
    /// A worker's `result` and `check-run`.
    Result,
}

/// Every capability, in bit order.
pub const ALL: [Capability; 17] = [
    Capability::Read,
    Capability::Notify,
    Capability::Answer,
    Capability::Backlog,
    Capability::Note,
    Capability::Handoff,
    Capability::CreateTask,
    Capability::Direct,
    Capability::Close,
    Capability::Discard,
    Capability::LandChild,
    Capability::Land,
    Capability::Deploy,
    Capability::Routine,
    Capability::RoleAdmin,
    Capability::Approve,
    Capability::Result,
];

impl Capability {
    fn bit(self) -> u32 {
        1 << ALL.iter().position(|cap| *cap == self).unwrap_or(31)
    }

    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

/// A set of capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps(u32);

impl Caps {
    pub const ALL: Self = Self((1 << ALL.len()) - 1);

    pub fn of(list: &[Capability]) -> Self {
        Self(list.iter().fold(0, |bits, cap| bits | cap.bit()))
    }

    pub fn has(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    pub fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// A built-in role: its line and its group. There is no owner-defined group in config.
pub struct RoleInfo {
    pub name: &'static str,
    pub brief: &'static str,
    pub caps: &'static [Capability],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Manager,
    Area,
    Organizer,
    Scribe,
    Worker,
}

use Capability::*;

/// The one table of roles. A named role of docs/tasks.md "Roles" is an `Area`; the others are
/// the jobs the fallback design gives a small model. The rules it holds replace the refusals
/// that `not_for_roles` once spelled out one by one.
pub const ROLES: [(Role, RoleInfo); 5] = [
    (
        Role::Manager,
        RoleInfo {
            name: "manager",
            brief: "runs the project: every capability",
            caps: &ALL,
        },
    ),
    (
        Role::Area,
        RoleInfo {
            name: "area",
            brief: "a named role for one area: its own tasks and their children, no approvals, deploys, routines or roles",
            caps: &[
                Read, Notify, Answer, Backlog, Note, Handoff, CreateTask, Direct, Close, LandChild,
            ],
        },
    ),
    (
        Role::Organizer,
        RoleInfo {
            name: "organizer",
            brief: "reads, answers the owner, keeps the backlog and the notes",
            caps: &[Read, Answer, Backlog, Note],
        },
    ),
    (
        Role::Scribe,
        RoleInfo {
            name: "scribe",
            brief: "writes notes and handoffs",
            caps: &[Note, Handoff],
        },
    ),
    (
        Role::Worker,
        RoleInfo {
            name: "worker",
            brief: "builds in its own copy and reports its result and checks",
            caps: &[Result],
        },
    ),
];

impl Role {
    pub fn info(self) -> &'static RoleInfo {
        &ROLES
            .iter()
            .find(|(role, _)| *role == self)
            .expect("every role is in the table")
            .1
    }

    pub fn caps(self) -> Caps {
        Caps::of(self.info().caps)
    }

    /// The role the agent of an actor runs as: the manager host's as `Manager`, a named
    /// role's as `Area`; the owner (no actor) has none.
    pub fn of_actor(actor: &Actor) -> Option<Self> {
        match actor {
            Actor::Owner => None,
            Actor::Manager => Some(Self::Manager),
            Actor::Role(_) => Some(Self::Area),
        }
    }
}

/// Whether wsx can rely on the agent's model to behave as Anthropic's own would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    Untrusted,
}

/// Trusted only for the Claude adapter on Anthropic's own endpoint. The model name is not
/// consulted: Anthropic's endpoint serves only Claude models, so a model that is not Claude's
/// reaches the CLI only through `ANTHROPIC_BASE_URL`. Pi runs any model: untrusted.
pub fn trust(adapter: &str, base_url: Option<&OsStr>) -> Trust {
    match (adapter, base_url.filter(|url| !url.is_empty())) {
        ("claude", None) => Trust::Trusted,
        _ => Trust::Untrusted,
    }
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    /// The mark the broker put on this command. Missing is `Trusted`, as the actor mark is
    /// the host's, not a boundary (a command the agent runs without the broker cannot write
    /// the state, which its profile denies); any other value is `Untrusted`.
    pub fn from_env() -> Self {
        match std::env::var(TRUST_ENV).as_deref() {
            Err(_) | Ok("trusted") => Self::Trusted,
            Ok(_) => Self::Untrusted,
        }
    }
}

/// What the agent that runs now may do: its role's group cut down to what its `[[agents]]`
/// entry passed. `Copy`, so a host shares it with its broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub trust: Trust,
    pub role: Role,
    granted: Caps,
}

impl Policy {
    /// `may`: the capabilities the agent passed; `None` is today's behavior, all of the
    /// role's. (Rust's `for` is a keyword: this is the design's `Policy::for`.)
    pub fn of(trust: Trust, role: Role, may: Option<&[Capability]>) -> Self {
        Self {
            trust,
            role,
            granted: role.caps().and(may.map_or(Caps::ALL, Caps::of)),
        }
    }

    /// The strict profile applies.
    pub fn strict(&self) -> bool {
        self.trust == Trust::Untrusted
    }

    pub fn allows(&self, cap: Capability) -> bool {
        self.granted.has(cap)
    }

    /// Whether any of `needs` is granted; a command with no need is for nobody.
    pub fn allows_any(&self, needs: &[Capability]) -> bool {
        needs.iter().any(|cap| self.allows(*cap))
    }

    /// Why `what` is refused: the role's group lacks it, or the agent did not pass it.
    pub fn refusal(&self, what: &str, needs: &[Capability]) -> String {
        if !needs.iter().any(|cap| self.role.caps().has(*cap)) {
            return role_refusal(self.role, what);
        }
        let passed: Vec<String> = ALL
            .iter()
            .filter(|cap| self.allows(**cap))
            .map(|cap| cap.name())
            .collect();
        format!(
            "{what} is beyond what this agent may do (`may` of its [[agents]] entry); it passed: {}",
            passed.join(", ")
        )
    }
}

/// `what` is not in the role's group.
pub fn role_refusal(role: Role, what: &str) -> String {
    match role {
        Role::Area => {
            format!("{what} is not for a role; ask the manager (`wsx task say --from-role`)")
        }
        other => format!("{what} is not for the {} role", other.info().name),
    }
}
