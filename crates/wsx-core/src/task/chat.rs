//! The manager chat as rows: the log classified once, for the web's bubbles and the TUI's
//! chat, which only render them. ref: docs/tasks.md "Manager", docs/web-client-tasks.md
//! "Project screen"

use super::manager::{
    quote_of, says_nothing, split_quote, ApprovalLine, Author, Level, ManagerEntry, ManagerEvent,
    Short,
};
use serde::Serialize;
use std::collections::HashSet;

/// The same line again this soon shows once.
const REPEAT_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    /// A message with its author: the owner's, a reply of the manager, what it sent another
    /// manager, or a message of another manager or a role.
    Message,
    /// A muted line of wsx: an event in its short form, or the usage limit.
    Line,
    /// A quiet reply: the manager's own work, a muted line under its name.
    Quiet,
    /// A new conversation, or the manager stopped.
    Note,
}

/// Who wrote a row: `owner`, `manager` (the chat's other side) or `peer:<key>` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub enum Speaker {
    Owner,
    Manager,
    /// Another project's manager by its project, or a role as `role:<name>`.
    Peer(String),
}

impl From<Speaker> for String {
    fn from(speaker: Speaker) -> Self {
        match speaker {
            Speaker::Owner => "owner".into(),
            Speaker::Manager => "manager".into(),
            Speaker::Peer(key) => format!("peer:{key}"),
        }
    }
}

/// The names the rows show.
pub struct Names<'a> {
    /// The chat's other side: the manager's shown name (`manager_shown_name`), or the role's.
    pub own: &'a str,
    /// The project's manager by its shown name, as a role's chat names it.
    pub manager: &'a str,
    /// Another project's manager by its shown name.
    pub peer: &'a dyn Fn(&str) -> String,
}

/// One row of the chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatRow {
    pub kind: RowKind,
    /// Stable while rows come and go: `<entry kind>-<id>`, `-<n>` for an event line.
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<Speaker>,
    /// The author's shown name: "You", the manager's, a peer's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// "Recap", "Reply", "To <name>", "Reply to <name>".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The message's leading quote (`split_quote`); `text` is the words after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted: Option<String>,
    pub text: String,
    /// The line a quote of this message carries (`quote_of`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// An event line's leading task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<u32>,
    /// A reply that asks the owner, with no owner message after it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub asks: bool,
    /// A reply wsx posted with no agent turn.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub post: bool,
    /// A post's level (`wsx task post --level warn|bad`): the chats show its word and color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// The pending approvals this reply carries: each goes to the first reply at or after it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<String>,
    /// A stand-in agent wrote this reply: its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisional: Option<String>,
    /// The manager stopped.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub danger: bool,
}

impl ChatRow {
    fn new(kind: RowKind, key: String, entry: &ManagerEntry, text: String) -> Self {
        Self {
            kind,
            key,
            id: entry.id(),
            unix_ms: entry.unix_ms,
            author: None,
            name: None,
            tag: None,
            quoted: None,
            text,
            quote: None,
            task: None,
            asks: false,
            post: false,
            level: None,
            approvals: Vec::new(),
            provisional: None,
            danger: false,
        }
    }

    /// A message of `author`, its leading quote apart.
    fn said(key: String, entry: &ManagerEntry, author: Speaker, name: String, text: &str) -> Self {
        let (quoted, body) = split_quote(text);
        let quote = Some(quote_of(text)).filter(|quote| !quote.is_empty());
        Self {
            author: Some(author),
            name: Some(name),
            quoted: quoted.map(str::to_string),
            quote,
            ..Self::new(RowKind::Message, key, entry, body.trim().to_string())
        }
    }
}

/// The chat window's entries (`chat_window`) as rows. `approvals`: the pending approvals
/// (`pending_approvals`); a quiet reply that carries one is a message.
pub fn chat_rows(
    entries: &[&ManagerEntry],
    approvals: &[&ApprovalLine],
    names: &Names,
) -> Vec<ChatRow> {
    // ^ wsx's recap request is not shown; the answer to it is tagged (docs/tasks.md "Recap").
    let recaps: HashSet<u64> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                id,
                from: Author::Recap,
                ..
            } => Some(*id),
            _ => None,
        })
        .collect();
    // ^ A reply asks until the owner writes after it (docs/tasks.md "Manager").
    let answered = entries.iter().rposition(|entry| {
        matches!(
            entry.event,
            ManagerEvent::Message {
                from: Author::Owner,
                ..
            }
        )
    });
    let mut carried: Vec<Vec<String>> = vec![Vec::new(); entries.len()];
    for line in approvals {
        let ApprovalLine::Request {
            id, unix_ms: at, ..
        } = line
        else {
            continue;
        };
        let reply = entries.iter().position(|entry| {
            matches!(entry.event, ManagerEvent::Reply { .. }) && entry.unix_ms >= *at
        });
        if let Some(index) = reply {
            carried[index].push(id.to_string());
        }
    }
    let peer_name = |key: &str| match key.strip_prefix("role:") {
        Some("manager") => names.manager.to_string(),
        Some(role) => role.to_string(),
        None => (names.peer)(key),
    };
    let mut rows: Vec<ChatRow> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let key = |kind: &str| match entry.id() {
            Some(id) => format!("{kind}-{id}"),
            None => format!("{kind}-{}", entry.unix_ms),
        };
        match &entry.event {
            ManagerEvent::Message {
                from: Author::Owner,
                text,
                ..
            } => rows.push(ChatRow::said(
                key("message"),
                entry,
                Speaker::Owner,
                "You".into(),
                text,
            )),
            ManagerEvent::Message {
                from: Author::Recap,
                ..
            } => {}
            ManagerEvent::Message {
                from: Author::Manager { project, reply },
                text,
                ..
            } => rows.push(ChatRow {
                tag: reply.then(|| "Reply".into()),
                ..ChatRow::said(
                    key("message"),
                    entry,
                    Speaker::Peer(project.clone()),
                    peer_name(project),
                    text,
                )
            }),
            ManagerEvent::Message {
                from: Author::Role { name, reply },
                text,
                ..
            } => {
                let peer = format!("role:{name}");
                rows.push(ChatRow {
                    tag: reply.then(|| "Reply".into()),
                    ..ChatRow::said(
                        key("message"),
                        entry,
                        Speaker::Peer(peer.clone()),
                        peer_name(&peer),
                        text,
                    )
                })
            }
            ManagerEvent::Message {
                from: Author::Events,
                text,
                short,
                ..
            } => {
                // ^ A line logged before `short` shows its first clause.
                let lines: Vec<Short> = match short.is_empty() {
                    false => short.clone(),
                    true => text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(Short::plain)
                        .collect(),
                };
                for (nth, line) in lines.into_iter().enumerate() {
                    let repeat = rows.last().is_some_and(|last| {
                        last.kind == RowKind::Line
                            && last.text == line.text
                            && entry.unix_ms.saturating_sub(last.unix_ms) < REPEAT_MS
                    });
                    if !repeat {
                        rows.push(ChatRow {
                            task: line.task,
                            ..ChatRow::new(
                                RowKind::Line,
                                format!("{}-{nth}", key("message")),
                                entry,
                                line.text,
                            )
                        });
                    }
                }
            }
            ManagerEvent::Reply {
                to,
                text,
                quiet,
                asks,
                provisional,
                post,
                level,
                ..
            } => {
                let approvals = std::mem::take(&mut carried[index]);
                // ^ A quiet reply is a muted line under the manager's name unless it carries an
                // approval; `-` or nothing says nothing changed and shows nothing.
                if *quiet && approvals.is_empty() {
                    if !says_nothing(text) {
                        rows.push(ChatRow {
                            author: Some(Speaker::Manager),
                            name: Some(names.own.to_string()),
                            ..ChatRow::new(
                                RowKind::Quiet,
                                key("reply"),
                                entry,
                                text.trim().to_string(),
                            )
                        });
                    }
                    continue;
                }
                rows.push(ChatRow {
                    tag: to
                        .iter()
                        .any(|id| recaps.contains(id))
                        .then(|| "Recap".into()),
                    asks: *asks && answered.is_none_or(|owner| index > owner),
                    post: *post,
                    level: *level,
                    approvals,
                    provisional: provisional.clone(),
                    ..ChatRow::said(
                        key("reply"),
                        entry,
                        Speaker::Manager,
                        names.own.to_string(),
                        text,
                    )
                })
            }
            // ^ An empty `sent` (older hosts logged one without --json) shows nothing.
            ManagerEvent::Sent { text, .. } if text.trim().is_empty() => {}
            ManagerEvent::Sent { to, text, reply } => rows.push(ChatRow {
                tag: Some(format!(
                    "{} {}",
                    if *reply { "Reply to" } else { "To" },
                    peer_name(to)
                )),
                ..ChatRow::said(
                    key("sent"),
                    entry,
                    Speaker::Manager,
                    names.own.to_string(),
                    text,
                )
            }),
            ManagerEvent::Limit { until } => rows.push(ChatRow::new(
                RowKind::Line,
                key("limit"),
                entry,
                match until {
                    Some(until) => format!("{} paused: usage limit, resumes at {until}", names.own),
                    None => format!("{} resumed", names.own),
                },
            )),
            ManagerEvent::Rotated { tokens, reason } => rows.push(ChatRow::new(
                RowKind::Note,
                key("rotated"),
                entry,
                match reason {
                    Some(reason) => {
                        format!("New conversation: {reason}; the ledger carries the state.")
                    }
                    None => format!(
                        "New conversation after {} tokens; the ledger carries the state.",
                        grouped(*tokens)
                    ),
                },
            )),
            ManagerEvent::Stopped { reason } => rows.push(ChatRow {
                danger: true,
                ..ChatRow::new(
                    RowKind::Note,
                    key("stopped"),
                    entry,
                    format!("{} stopped: {reason}", names.own),
                )
            }),
            ManagerEvent::Delivered { .. }
            | ManagerEvent::Agent { .. }
            | ManagerEvent::Ping { .. } => {}
        }
    }
    rows
}

/// `n` with a comma between each group of three digits: `152,000`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}
