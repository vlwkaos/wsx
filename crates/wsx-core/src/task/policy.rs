//! The one owner of what a task agent may do, so that no permission relies on the model: the
//! host derives a [`Policy`] from who the agent is and enforces it outside the model. What an
//! agent may do is its role's group of [`Capability`] values, cut down to the ones its
//! `[[agents]]` entry passed. Three consumers read it: the Seatbelt profile
//! (`wsx/src/task/boundary.rs`), the brokers' command check (`broker.rs`, `host.rs`) and the
//! approval rules of the outward commands (`WSX_TASK_TRUST`). What a role may reach in another
//! project and lend a worker is in the same table, and the instruction lines an agent reads
//! are built from it, so a prompt never promises what a broker refuses.
//! ref: docs/tasks.md "Chats", "Permissions"; docs/local-model.md

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;

/// Set by the broker on each command it runs for a chat: `trusted` or `untrusted`.
pub const TRUST_ENV: &str = "WSX_TASK_TRUST";
/// The two variables that name the lean runner's policy file and its hash (projp
/// `runner/README.md`); without both the runner denies every tool call.
pub const RUNNER_POLICY_ENV: &str = "PROJP_POLICY";
pub const RUNNER_POLICY_SHA_ENV: &str = "PROJP_POLICY_SHA256";
/// Set by the broker on each `wsx memory` command it runs: `deny` when the agent may not write a
/// rule. A missing value is the owner's own call, which may.
pub const RULES_ENV: &str = "WSX_MEMORY_RULES";
/// Programs the strict profile refuses to start: privilege, UI scripting and the service lead.
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
    /// `post`, `digest`, `say`: a message to the owner's chat or another lead.
    Notify,
    /// Reply text to the owner (the lead's turn, the stand-in).
    Answer,
    /// `after`, `hold`: order and park tasks.
    Backlog,
    /// Write the lead's notes (compaction, memo folding).
    Note,
    /// Write the handoff notes of a rotation.
    Handoff,
    /// `new`.
    CreateTask,
    /// `add`, `stop`, `move`: act on a task.
    Direct,
    /// `close` without `--discard`.
    Close,
    /// `close --discard`.
    Discard,
    /// `land` onto the default branch, `commit` in the main checkout.
    Land,
    /// `deploy`, `release`, `push`, `sync`.
    Deploy,
    /// `routine add|edit|delete|enable|disable|run|cancel|fire`, `checkin`.
    Routine,
    /// The owner's commands, which the CLI judges itself for a chat: `approve`, `deny`,
    /// `hand` (also `--back`), `dismiss`, and the hidden host commands.
    Approve,
    /// A worker's `result` and `check-run`.
    Result,
    /// `wsx memory search|status|list`: every role has it, whatever `may` lists.
    Recall,
    /// `wsx memory add|update|supersede|reindex` in the agent's own project. Never an
    /// untrusted agent's: its memory is read only.
    Remember,
    /// Write a `rule` record, which needs the owner's word (`owner:<id>`).
    Decide,
    /// `run`: ask the owner to run one command outside the boundary (docs/tasks.md
    /// "Approved runs").
    Ask,
}

/// Every capability, in bit order.
pub const ALL: [Capability; 19] = [
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
    Capability::Land,
    Capability::Deploy,
    Capability::Routine,
    Capability::Approve,
    Capability::Result,
    Capability::Recall,
    Capability::Remember,
    Capability::Decide,
    Capability::Ask,
];

/// What the strict profile takes from an untrusted agent: it reads memory and writes none.
const MEMORY_WRITES: [Capability; 2] = [Capability::Remember, Capability::Decide];

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

    pub fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn minus(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// A built-in role: its name and its group. There is no owner-defined group in config.
pub struct RoleInfo {
    pub name: &'static str,
    pub caps: &'static [Capability],
    /// The `wsx task` commands the role runs in another project's store (`-p <other>`), with its
    /// numbers, checks and land target. `ledger` (a read) and `say --from-chat` (to that
    /// project's main chat) are open to a role with any reach; `--discard` never is.
    pub reach: &'static [&'static str],
}

/// What the lead, and so every chat, runs in another project.
const MANAGER_REACH: [&str; 8] = [
    "new", "show", "list", "add", "stop", "hold", "land", "close",
];

/// The one role enum: a launch site's role, and the job a stand-in fills (`filled_by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Chat,
    Organizer,
    Worker,
    /// The review job of a ready task: reads its copy and decides land or changes.
    /// ref: docs/tasks.md "Review"
    Reviewer,
}

use Capability::*;

/// The one table of roles. Every chat of docs/tasks.md "Chats" runs as the lead; the organizer
/// is the stand-in's job.
pub const ROLES: [(Role, RoleInfo); 4] = [
    (
        Role::Chat,
        RoleInfo {
            name: "manager",
            caps: &ALL,
            reach: &MANAGER_REACH,
        },
    ),
    (
        Role::Organizer,
        RoleInfo {
            name: "organizer",
            caps: &[Read, Answer, Backlog, Note, Recall],
            reach: &[],
        },
    ),
    (
        Role::Worker,
        RoleInfo {
            name: "worker",
            caps: &[Result, Recall, Remember, Ask],
            reach: &[],
        },
    ),
    (
        Role::Reviewer,
        RoleInfo {
            name: "reviewer",
            caps: &[Read, Recall],
            reach: &[],
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

    /// Whether the role runs `sub` (a `wsx task` subcommand, not `--discard`) in another project.
    pub fn reaches(self, sub: &str) -> bool {
        self.info().reach.contains(&sub)
    }

    /// The instruction line of the role's reach into other projects.
    pub fn reach_line(self) -> String {
        match self.info().reach {
            [] => "Stay in your own project.".into(),
            reach => format!(
                "`-p <project>` runs {} in another project, not its deploys, releases or pushes. An owner's '@<chat>' means: consult that chat with `wsx chat say --to <chat> --from-chat`.",
                reach.join(", ")
            ),
        }
    }

    /// Why a command the role may not run in project `other` is refused.
    pub fn reach_refusal(self, other: &str) -> String {
        match self.info().reach {
            [] => format!(
                "a {} acts only in its own project (for {other} it may read `wsx chat ledger -p {other}`)",
                self.info().name
            ),
            reach => format!(
                "a chat acts in another project only with {}, never --discard or an approved action; for {other} it may also read `wsx chat ledger -p {other}` and `wsx chat say -p {other} --from-chat`",
                reach.join(", ")
            ),
        }
    }
}

/// The instruction sentence of `task new --skills`: offered only where the project lends the
/// owner's skills (`tasks.skills: owner`), the same switch `lend_refusal` reads.
pub fn lend_line(owner_skills: bool) -> &'static str {
    match owner_skills {
        true => "`new --skills` lends a worker the ones its task needs. ",
        false => "",
    }
}

/// Why `task new --skills` is refused, or none when the project lends skills.
pub fn lend_refusal(owner_skills: bool) -> Option<&'static str> {
    (!owner_skills).then_some("--skills: the project lends no skills (tasks.skills is not owner)")
}

impl Role {
    /// Whether an `[[agents]]` entry whose `may` is `may` fills this role for another agent: only
    /// the organizer (the stand-in) is ever filled so, by an entry that lists `answer`. A task or
    /// chat keeps its own agent for life (docs/design/orchestration.md section 8).
    /// ref: docs/tasks.md "Agents"
    pub fn filled_by(self, may: Option<&[Capability]>) -> bool {
        self == Self::Organizer && may.is_some_and(|may| may.contains(&Answer))
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
    /// role's. (Rust's `for` is a keyword: this is the design's `Policy::for`.) Reading memory
    /// needs no `may`, and an untrusted agent writes none. ref: docs/memory.md "Agents"
    pub fn of(trust: Trust, role: Role, may: Option<&[Capability]>) -> Self {
        let passed = role.caps().and(may.map_or(Caps::ALL, Caps::of));
        // ^ `recall` reads and `ask` only asks the owner, who decides: an agent's `may` list,
        // which predates both, never takes them away.
        let granted = passed.or(Caps::of(&[Capability::Recall, Capability::Ask]).and(role.caps()));
        Self {
            trust,
            role,
            granted: match trust {
                Trust::Trusted => granted,
                Trust::Untrusted => granted.minus(Caps::of(&MEMORY_WRITES)),
            },
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
            return format!("{what} is not for the {} role", self.role.info().name);
        }
        if self.strict() && needs.iter().any(|cap| MEMORY_WRITES.contains(cap)) {
            return format!("{what} is refused: an agent on a model wsx does not trust reads memory and writes none");
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

/// The lean runner's policy file for one Pi launch: what its tools may read and write and
/// which paths are secret. The host builds it from the same paths as the Seatbelt profile
/// (`Boundary::runner_file`), so the runner is a second layer over one rule, not a rule of
/// its own. ref: docs/tasks.md "Pi adapter"
#[derive(Debug, Serialize)]
pub struct RunnerFile {
    pub tools: Vec<&'static str>,
    /// Absolute folder prefixes; each contains itself and all below it.
    pub read: Vec<String>,
    pub write: Vec<String>,
    /// `path.matchesGlob` patterns.
    pub secrets: Vec<String>,
}

impl RunnerFile {
    /// The file's bytes and their lowercase hex SHA-256.
    pub fn render(&self) -> (Vec<u8>, String) {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        let hash = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (bytes, hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ^ Owner-asked check: the instruction lines and the refusals read the one `reach` list,
    // so a prompt never promises what a broker refuses.
    #[test]
    fn prompt_and_checks_read_one_table() {
        for (role, info) in &ROLES {
            let (line, refusal) = (role.reach_line(), role.reach_refusal("other"));
            for sub in info.reach {
                assert!(role.reaches(sub), "{} {sub}", info.name);
                assert!(
                    line.contains(sub) && refusal.contains(sub),
                    "{} {sub}",
                    info.name
                );
            }
            // ^ A command outside the table is neither run nor promised.
            assert!(
                !role.reaches("deploy")
                    && !line.contains("deploy,")
                    && !refusal.contains("deploy,")
            );
            assert_eq!(
                line.contains("--from-chat"),
                !info.reach.is_empty(),
                "{}",
                info.name
            );
        }
        assert!(lend_line(true).contains("--skills") && lend_refusal(true).is_none());
        assert!(lend_line(false).is_empty() && lend_refusal(false).is_some());
    }
}
