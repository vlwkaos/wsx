//! The project manager's conversation and state: `tasks/<project-id>/manager/`.
//! ref: docs/tasks.md "Manager"
//!
//! The manager host is the only writer of these files; `wsx task chat` reads the log.

use super::status::{owner_waits_after, STOPPED_BY_REQUEST};
use super::store::TaskDir;
use super::{LogEvent, TaskRecord, TaskState, TurnUsage, WaitOn};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How urgent a post of `wsx task post` is, besides the default info (no level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Bad,
}

impl Level {
    /// The word the chat shows.
    pub fn word(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Bad => "bad",
        }
    }
}

/// Who wrote a message the manager receives: `owner`, `events`, `manager:<project>` or, for
/// an answer to a message this manager sent, `manager-reply:<project>`; inside a project,
/// `role:<name>` and `role-reply:<name>` (the manager is the role `manager`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Author {
    Owner,
    /// Written by wsx from task state changes and approval decisions.
    Events,
    /// The manager of another registered project (docs/tasks.md "Managers talk").
    Manager {
        project: String,
        reply: bool,
    },
    /// wsx asks for a recap after a quiet period (docs/tasks.md "Recap").
    Recap,
    /// A role of the same project, or its manager as `manager` (docs/tasks.md "Roles").
    Role {
        name: String,
        reply: bool,
    },
}

impl From<Author> for String {
    fn from(author: Author) -> Self {
        match author {
            Author::Owner => "owner".into(),
            Author::Events => "events".into(),
            Author::Recap => "recap".into(),
            Author::Manager {
                project,
                reply: false,
            } => format!("manager:{project}"),
            Author::Manager {
                project,
                reply: true,
            } => format!("manager-reply:{project}"),
            Author::Role { name, reply: false } => format!("role:{name}"),
            Author::Role { name, reply: true } => format!("role-reply:{name}"),
        }
    }
}

impl TryFrom<String> for Author {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let manager = |project: &str, reply| match project.is_empty() {
            true => Err(format!("{text} names no project")),
            false => Ok(Author::Manager {
                project: project.to_string(),
                reply,
            }),
        };
        match text.as_str() {
            "owner" => Ok(Self::Owner),
            "events" => Ok(Self::Events),
            "recap" => Ok(Self::Recap),
            _ => match (
                text.strip_prefix("manager:"),
                text.strip_prefix("manager-reply:"),
                text.strip_prefix("role:"),
                text.strip_prefix("role-reply:"),
            ) {
                (Some(project), ..) => manager(project, false),
                (_, Some(project), ..) => manager(project, true),
                (.., Some(name), _) | (.., Some(name)) if !name.is_empty() => Ok(Author::Role {
                    name: name.to_string(),
                    reply: text.starts_with("role-reply:"),
                }),
                _ => Err(format!("unknown author {text}")),
            },
        }
    }
}

/// One line of `manager/log.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManagerEvent {
    Message {
        id: u64,
        from: Author,
        text: String,
        /// An events message's lines as the chat shows them, one per event, written where each
        /// event is made; none for an owner's or a peer's message (docs/tasks.md "Manager").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        short: Vec<Short>,
        /// The id of the message its leading `> <line>` quotes (`resolve_quote`): a tap on the
        /// quote opens the chat there. None for a quote written before ids, or by hand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<u64>,
    },
    /// These messages went to the agent in one turn.
    Delivered { ids: Vec<u64> },
    Reply {
        id: u64,
        to: Vec<u64>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        /// A short reply to a turn of only events: the manager's own work, which the chat
        /// shows as a muted line, not a message (`quiet_reply`, docs/tasks.md "Manager").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        quiet: bool,
        /// The reply asks the owner (`asks_owner`); such a reply is never quiet. The chat tags it
        /// until it is answered (`chat::open_asks`, docs/tasks.md "Manager").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        asks: bool,
        /// A `minimal` agent wrote this while the manager's agent could not: the agent's name.
        /// The manager's agent still answers `to` itself and is told of this reply
        /// (docs/tasks.md "Agents").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provisional: Option<String>,
        /// A post of `wsx task post --level warn|bad`: the chat shows the level and the web
        /// pushes it at once. Info is none. ref: docs/tasks.md "Routines"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<Level>,
        /// wsx posted this with no agent turn (`wsx task post`, the digest, the session news):
        /// the chat sets it off in the secondary color (docs/tasks.md "Routines").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        post: bool,
        /// The id of the message its leading quote points at, as on `Message`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quote: Option<u64>,
    },
    /// This manager's message to another project's manager (`say --from-manager`).
    Sent {
        to: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply: bool,
    },
    /// A command or other step of the agent, in short form.
    Agent { text: String },
    /// A ping turn kept the prompt cache warm while a task works: no reply, not in the chat.
    /// ref: docs/tasks.md "Manager"
    Ping {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
    },
    /// The context passed `rotateTokens`, or the launch changed (`reason`): the next turn
    /// starts a fresh conversation.
    Rotated {
        tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The usage limit paused the manager (`until`: the time it resumes, "00:10") or lifted.
    /// Written by wsx; the chat shows it as a status line. ref: docs/web-client-tasks.md
    Limit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<String>,
    },
    /// The agent process ended outside a reply.
    Stopped { reason: String },
    /// The owner dismissed the question of reply `id` (`wsx task dismiss`): it asks no more.
    /// No turn reads it. ref: docs/tasks.md "Manager"
    Answered { id: u64 },
}

/// One event as the chat shows it, written where the event is made: `#3 done`,
/// `Approved land #5`. `task`: its leading task, which the web shows as a chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Short {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<u32>,
    /// Whether the event needs the manager's judgment now: false for a landing, a close, a wait
    /// that ends by itself, a stop someone asked for and an automatic deploy that worked. Those
    /// go along with the manager's next turn and cost none. ref: docs/tasks.md "Manager"
    #[serde(default = "wakes_default", skip_serializing_if = "is_wakes")]
    pub wakes: bool,
}

fn wakes_default() -> bool {
    true
}

fn is_wakes(wakes: &bool) -> bool {
    *wakes
}

impl Short {
    /// A line's first clause, before `; `: the short form of an event with no words of its own
    /// (a deploy, a routine, a session) and of a line logged before `short`.
    pub fn plain(line: &str) -> Self {
        Self {
            text: line.split("; ").next().unwrap_or(line).trim().to_string(),
            task: None,
            wakes: true,
        }
    }

    /// The event starts no turn of its own.
    pub fn quiet(mut self) -> Self {
        self.wakes = false;
        self
    }

    /// Whether a message of events starts a turn: one of its lines does, or `short` says
    /// nothing (a message logged before `short`).
    pub fn any_wakes(short: &[Self]) -> bool {
        short.is_empty() || short.iter().any(|short| short.wakes)
    }

    /// A task's event: `#<task> <words>`.
    pub fn task(task: u32, words: &str) -> Self {
        Self {
            text: format!("#{task} {words}"),
            task: Some(task),
            wakes: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerEntry {
    pub unix_ms: u64,
    #[serde(flatten)]
    pub event: ManagerEvent,
}

/// The longest quiet reply, as long as the web's first sentence (`firstSentence` in lib.js).
const QUIET_REPLY_CHARS: usize = 200;

/// Whether a reply to a turn of only events is quiet: one line of at most 200 characters. A
/// longer reply (a table, a list, paragraphs) is a message even after events.
pub fn quiet_reply(text: &str) -> bool {
    let text = text.trim();
    !text.contains('\n') && text.chars().count() <= QUIET_REPLY_CHARS
}

/// Whether a reply says nothing to the owner: `-` (nothing changed) or no text. It is quiet, and
/// no chat shows it (docs/tasks.md "Manager").
pub fn says_nothing(text: &str) -> bool {
    matches!(text.trim(), "" | "-")
}

/// Whether a task's new state needs the manager's judgment now. The others (a landing, a close,
/// a wait on `limit` or `disk`, a stop the owner or the manager asked for, an idle worker whose
/// task waits for another to land, a held parent that is ready again because a child landed on
/// it with its checks passed) go along with its next turn. ref: docs/tasks.md "Manager"
pub fn state_wakes(dir: &TaskDir, record: &TaskRecord) -> bool {
    match record.state {
        TaskState::Landed | TaskState::Closed => false,
        // ^ Ready means the checks passed; failed ones are `Failed` and wake. The newest of a
        // child's land, a handback and a worker result before the current revision made it.
        TaskState::Ready if record.hold.is_some() => !revision_from_child(dir, record.revision),
        TaskState::Waiting => {
            !owner_waits_after(dir, record)
                && !record
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| match waiting.on {
                        WaitOn::Limit | WaitOn::Disk => true,
                        WaitOn::Owner => waiting.reason == STOPPED_BY_REQUEST,
                        _ => false,
                    })
        }
        _ => true,
    }
}

/// Whether revision `revision` was made by a child's land: the newest `child_landed`, `handback`
/// or worker `result` before its `revision` entry is the land. A log tail without it says no.
fn revision_from_child(dir: &TaskDir, revision: u32) -> bool {
    const TAIL_BYTES: u64 = 512 * 1024;
    let Ok(entries) = dir.read_log_tail(TAIL_BYTES) else {
        return false;
    };
    entries
        .iter()
        .rev()
        .skip_while(|entry| !matches!(&entry.event, LogEvent::Revision { revision: r, .. } if *r == revision))
        .find_map(|entry| match &entry.event {
            LogEvent::ChildLanded { .. } => Some(true),
            LogEvent::Handback { .. } | LogEvent::Result { .. } => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

/// Whether a reply asks the owner: its last line ends with a question mark. A reply that ends with
/// a code block ends with its fence, and one that ends with a table with a row, which does not
/// count; trailing emphasis or a closing bracket does not hide the mark.
pub fn asks_owner(text: &str) -> bool {
    let last = text.trim_end().lines().last().unwrap_or_default().trim();
    !last.starts_with('|')
        && last
            .trim_end_matches(['*', '_', '`', ')', '"', '\''])
            .ends_with(['?', '？'])
}

/// The longest quoted line, its ellipsis included.
const QUOTE_CHARS: usize = 120;
/// A reply that opens with this and a message id quotes that message.
pub const QUOTE_REF: &str = "> message ";

/// The id a reply quotes with `> message <id>` on its first line.
pub fn quoted_id(text: &str) -> Option<u64> {
    let line = text.trim_start().strip_prefix(QUOTE_REF)?;
    line.lines().next()?.trim().parse().ok()
}

/// A message split into its leading owner-format quote (`> <line>` and an empty line) and the
/// words after it. The web's composer writes this format (`withQuote` in web/reply.js).
pub fn split_quote(text: &str) -> (Option<&str>, &str) {
    let Some((line, rest)) = text.strip_prefix("> ").and_then(|t| t.split_once('\n')) else {
        return (None, text);
    };
    (Some(line), rest.strip_prefix('\n').unwrap_or(rest))
}

/// A message's first line without its own quote, cut to 120 characters: the line a quote of it
/// carries, and the only cut of it (the web reads `quote` of a chat row).
pub fn quote_of(text: &str) -> String {
    let line = split_quote(text)
        .1
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    super::view::clip(line, QUOTE_CHARS)
}

/// A message that opens with `> message <id>` (a manager's reply, or the owner's Reply) quotes
/// that message in the owner's format (`> <its first line>` and an empty line) and keeps the id,
/// so the chat needs no second format and a tap finds the message at any age. Only a message the
/// chat shows as a bubble can be quoted: the owner's, another manager's, a role's or a reply that
/// says something. A bad or unknown id drops the line: the message still stands, and the note
/// says why. Returns the text, the quoted id and the note.
pub fn resolve_quote(text: &str, log: &[ManagerEntry]) -> (String, Option<u64>, Option<String>) {
    let text = text.trim_start();
    let Some(line) = text.strip_prefix(QUOTE_REF) else {
        return (text.to_string(), None, None);
    };
    let (line, rest) = line.split_once('\n').unwrap_or((line, ""));
    let rest = rest.trim_start_matches('\n');
    let id = quoted_id(text);
    let said = id.and_then(|id| {
        log.iter().find_map(|entry| match &entry.event {
            ManagerEvent::Message {
                id: found,
                from: Author::Owner | Author::Manager { .. } | Author::Role { .. },
                text,
                ..
            } if *found == id => Some(text),
            ManagerEvent::Reply {
                id: found,
                text,
                quiet: false,
                ..
            } if *found == id && !says_nothing(text) => Some(text),
            _ => None,
        })
    });
    match said
        .map(|said| quote_of(said))
        .filter(|quote| !quote.is_empty())
    {
        Some(quote) => (format!("> {quote}\n\n{rest}"), id, None),
        None => (
            rest.to_string(),
            None,
            Some(format!("no message {} to quote: line dropped", line.trim())),
        ),
    }
}

impl ManagerEntry {
    /// An entry as read from the log: a reply stored as quiet before the length rule keeps its
    /// flag only when it is short, and one that `says_nothing` is quiet.
    pub(super) fn read(line: &[u8]) -> Option<Self> {
        let mut entry: Self = serde_json::from_slice(line).ok()?;
        if let ManagerEvent::Reply { text, quiet, .. } = &mut entry.event {
            *quiet = (*quiet && quiet_reply(text)) || says_nothing(text);
        }
        Some(entry)
    }

    /// The id of the message this message or reply quotes (`ManagerEvent::Message::quote`).
    pub fn quote_id(&self) -> Option<u64> {
        match &self.event {
            ManagerEvent::Message { quote, .. } | ManagerEvent::Reply { quote, .. } => *quote,
            _ => None,
        }
    }

    /// The id of a message or reply, which `chat --after` counts from.
    pub fn id(&self) -> Option<u64> {
        match &self.event {
            ManagerEvent::Message { id, .. } | ManagerEvent::Reply { id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// What the manager does now: its last step after its last reply. Shown while it is busy.
pub fn activity(log: &[ManagerEntry]) -> Option<&str> {
    log.iter()
        .rev()
        .take_while(|entry| !matches!(entry.event, ManagerEvent::Reply { .. }))
        .find_map(|entry| match &entry.event {
            ManagerEvent::Agent { text } => Some(text.as_str()),
            _ => None,
        })
}

/// A task as the manager last reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    pub state: TaskState,
    pub revision: u32,
}

impl Seen {
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            state: record.state,
            revision: record.revision,
        }
    }
}

/// `manager/state.json`: what the manager host must remember across a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagerState {
    /// The conversation the next turn resumes; none after a rotation.
    pub agent_session: Option<String>,
    /// A fallback agent in use; none: `tasks.manager.adapter` and `model`. The turn after the
    /// own agent's limit ends starts a fresh conversation on the own agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ManagerAgent>,
    pub next_id: u64,
    /// Task states already turned into events. Absent until the host's first start.
    pub seen: Option<BTreeMap<u32, Seen>>,
    /// Approval decisions already turned into events.
    pub decisions_seen: u64,
    /// The `unix_ms` of the newest deploy record already turned into events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploys_seen_ms: Option<u64>,
    /// The `finished_epoch` of the newest routine run already turned into events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routines_seen: Option<i64>,
    /// The attention state (`kind:since`) of each project agent session already posted, by
    /// session id. Absent until the host's first start. ref: docs/tasks.md "Session news"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions_seen: Option<BTreeMap<u64, String>>,
    /// The stall of each open task already turned into an event, by its marker: the same
    /// stall is reported once. ref: docs/tasks.md "Manager" (Stalls)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub stalls: BTreeMap<u32, String>,
    /// The digest of the instructions the conversation in `agent_session` began with.
    /// Claude keeps a conversation's first system prompt also on resume, so other
    /// instructions start a new conversation; settings, tools and model apply at every start.
    pub instructions: Option<String>,
    /// The last main-conversation context size and when the agent last answered: an idle large
    /// conversation rotates, a warm small one takes changed instructions as a message.
    /// ref: docs/tasks.md "Manager"
    pub context_tokens: u64,
    pub last_reply_ms: u64,
    /// A rotation happened: the next fresh conversation opens with the last exchanges.
    pub handover: bool,
    /// The tasks as the last turn's ledger summary showed them; none in a fresh conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_seen: Option<BTreeMap<u32, Seen>>,
    /// The conversation's last `session_cost_micro_usd`, which a resumed process counts on from.
    pub session_cost_micro_usd: u64,
    /// The last task the host started with `tasks.workflow.idle`, and when it started it:
    /// the next one waits for session notes newer than that. ref: docs/tasks.md "Workflow"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_task: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// When the last recap was asked for: the next one needs a change after it, and names
    /// the token cost since. ref: docs/tasks.md "Recap"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recap_ms: Option<u64>,
    /// The agent's usage limit: the messages of the turn it ended, answered once it resumes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitHold>,
    /// The newest owner message a `minimal` agent has answered.
    #[serde(skip_serializing_if = "is_zero")]
    pub provisional_to: u64,
    /// The id of the manager's newest reply that says something: the turn text names it, since
    /// a reply quotes a message by id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reply: Option<u64>,
    /// A provisional reply the manager's agent has not been told of yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub provisional: bool,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A manager turn the usage limit ended. ref: docs/tasks.md "Manager"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitHold {
    pub until_ms: u64,
    /// Limits in a row, for the backoff when no reset time is known.
    pub attempt: u32,
    pub ids: Vec<u64>,
    /// Every held message came from events, so the reply after the reset may be quiet. A hold
    /// an older host wrote has no such mark: it counts as not events-only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub events: bool,
    /// The limit moved the manager to a fallback agent: the held turn goes to the new
    /// conversation at once, with its messages in full. ref: docs/tasks.md "Agents"
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switched: bool,
}

/// The fallback agent the manager runs on while its own is at a long usage limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerAgent {
    pub name: String,
    pub adapter: String,
    pub model: String,
}

/// A stable digest of an agent's instructions, to tell whether a conversation began with them.
pub fn instructions_digest(instructions: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(instructions.as_bytes())[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the manager asked the owner to approve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    Land,
    Deploy,
    Release,
    /// `close --discard`
    Discard,
    /// `role new`: the manager asks once for a role (docs/tasks.md "Roles").
    Role,
    /// `push`: the default branch to origin, run by the host (docs/tasks.md "Push").
    Push,
}

/// One line of `approvals.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalLine {
    Request {
        id: String,
        action: ApprovalAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        /// A role request: the role's name and the brief it opens with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
        /// A forced push: origin's tip the request saw, which the push leases (empty: origin
        /// had no such branch).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<String>,
        /// A push: the landed tasks whose commits it brings to origin, and how many commits
        /// it brings (docs/tasks.md "Push").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tasks: Vec<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commits: Option<u32>,
        unix_ms: u64,
    },
    Decision {
        id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// What running the approved action said, or why it was not run.
        outcome: String,
        /// The owner message of the manager's turn that approved it: the owner asked for
        /// the action in the chat, so no button was needed. ref: docs/tasks.md "Approvals"
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_said: Option<u64>,
        unix_ms: u64,
    },
}

impl ApprovalLine {
    pub fn id(&self) -> &str {
        match self {
            Self::Request { id, .. } | Self::Decision { id, .. } => id,
        }
    }
}

/// Requests with no decision yet, oldest first. A decision settles the requests of its id
/// before it: an id reused by an older wsx (which miscounted lines it could not read) is a
/// new request.
pub fn pending_approvals(lines: &[ApprovalLine]) -> Vec<&ApprovalLine> {
    lines
        .iter()
        .enumerate()
        .filter(|(at, line)| {
            matches!(line, ApprovalLine::Request { .. })
                && !lines[at + 1..].iter().any(|later| {
                    matches!(later, ApprovalLine::Decision { .. }) && later.id() == line.id()
                })
        })
        .map(|(_, line)| line)
        .collect()
}

/// The request a decision at `at` settles: the last one of its id before it.
pub fn decided_request(lines: &[ApprovalLine], at: usize) -> Option<&ApprovalLine> {
    let id = lines.get(at)?.id();
    lines[..at]
        .iter()
        .rev()
        .find(|line| matches!(line, ApprovalLine::Request { .. }) && line.id() == id)
}

/// Whether the owner's message `id` is in the manager's running turn: the last delivered
/// turn has no reply yet and holds that message from the owner. ref: docs/tasks.md "Approvals"
pub fn owner_said_in_turn(log: &[ManagerEntry], id: u64) -> Result<(), String> {
    let turn = log.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Reply { .. } => Some(None),
        ManagerEvent::Delivered { ids } => Some(Some(ids)),
        _ => None,
    });
    let Some(Some(ids)) = turn else {
        return Err("no manager turn runs".into());
    };
    if !ids.contains(&id) {
        return Err(format!("message {id} is not in the running turn"));
    }
    let from = log.iter().find_map(|entry| match &entry.event {
        ManagerEvent::Message { id: said, from, .. } if *said == id => Some(from),
        _ => None,
    });
    match from {
        Some(Author::Owner) => Ok(()),
        Some(_) => Err(format!("message {id} is not from the owner")),
        None => Err(format!("there is no message {id}")),
    }
}

/// One request on the manager host's control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ManagerRequest {
    Status,
    Say {
        text: String,
        /// Another project's manager; none is the owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Author>,
        /// The chat's short form of an events message (`ManagerEvent::Message::short`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        short: Vec<Short>,
    },
    /// wsx writes to the owner with no manager turn: `event` as a delivered events message and
    /// `text` as the reply to it, with no usage. Refused while a turn runs: a reply after that
    /// turn's delivery would read as its end. ref: docs/tasks.md "Routines"
    Post {
        event: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<Level>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub busy: bool,
    pub message: String,
}
