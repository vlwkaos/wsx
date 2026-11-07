//! The manager chat as rows: the log classified once, for the web's bubbles and the TUI's
//! chat, which only render them. ref: docs/tasks.md "Manager", docs/web-client-tasks.md
//! "Project screen"

use super::manager::{
    quote_of, says_nothing, split_quote, ApprovalLine, Author, Level, ManagerEntry, ManagerEvent,
    Short,
};
use super::store::ManagerDir;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::HashSet;

/// The same line again this soon shows once.
const REPEAT_MS: u64 = 60_000;
/// Most entries the chat shows at once.
pub const CHAT_ENTRIES: usize = 200;
/// A chat reads this much of the log's end first, more only when its window needs it.
const FIRST_READ_BYTES: u64 = 256 * 1024;
/// A window around a message shows this many chat entries before it.
const AROUND_BEFORE: usize = 40;
/// A question older than this asks no more (docs/tasks.md "Manager").
pub const ASKS_WITHIN_MS: u64 = 7 * 24 * 3600 * 1000;

/// Which part of the log a chat shows (docs/tasks.md "Log window").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window<'a> {
    /// The newest `CHAT_ENTRIES`.
    Tail,
    /// The newest `CHAT_ENTRIES` after message `id`; all of them when `id` is unknown.
    After(u64),
    /// From the oldest entry whose row shows these words (case-insensitive) to the end, so a
    /// find in the chat reaches every match; the tail when none does.
    Find(&'a str),
    /// `CHAT_ENTRIES` from `AROUND_BEFORE` chat entries before message `id`, at any age the log
    /// keeps (every rolled file is read); the tail when `id` is not in the log.
    Around(u64),
}

impl<'a> Window<'a> {
    /// The window of a chat request: a find when it has words, else around `around`, else after
    /// `after`, else the tail. The web, the TUI and `wsx task chat` ask with these.
    pub fn new(find: Option<&'a str>, after: Option<u64>, around: Option<u64>) -> Self {
        match (find.filter(|words| !words.trim().is_empty()), around, after) {
            (Some(words), ..) => Self::Find(words),
            (None, Some(around), _) => Self::Around(around),
            (None, None, Some(after)) => Self::After(after),
            (None, None, None) => Self::Tail,
        }
    }
}

/// The entries a chat needs from the log's end: its window and the manager's last step
/// (`activity`). The open questions are `asking`'s.
pub fn read_chat(dir: &ManagerDir, window: &Window) -> Result<Vec<ManagerEntry>> {
    if let Window::Around(id) = window {
        // ^ Ids grow: an entry below `id` and none at it means the message is not in the log.
        // Else enough entries before it, or the oldest file's start, end the read.
        return dir.read_around(FIRST_READ_BYTES, |log| {
            match log.iter().position(|entry| entry.id() == Some(*id)) {
                Some(at) => log[..at].iter().filter(|e| in_chat(e)).count() >= AROUND_BEFORE,
                None => log
                    .iter()
                    .any(|entry| entry.id().is_some_and(|at| at < *id)),
            }
        });
    }
    dir.read_back(FIRST_READ_BYTES, |log| {
        let shown = match window {
            Window::Tail => log.iter().filter(|entry| in_chat(entry)).count() > CHAT_ENTRIES,
            // ^ Ids grow: an entry at or before `id` means every entry after it is read.
            Window::After(id) => log
                .iter()
                .any(|entry| entry.id().is_some_and(|at| at <= *id)),
            Window::Find(_) | Window::Around(_) => false,
        };
        shown
            && log.iter().any(|entry| {
                matches!(
                    entry.event,
                    ManagerEvent::Reply { .. } | ManagerEvent::Agent { .. }
                )
            })
    })
}

/// Whether `log` reaches back past the questions that can still ask at `now`.
fn reaches_asks(log: &[ManagerEntry], now: u64) -> bool {
    log.first()
        .is_some_and(|entry| now.saturating_sub(entry.unix_ms) >= ASKS_WITHIN_MS)
}

/// `Delivered`, `Agent`, `Ping` and `Answered` entries are not conversation: they use no room.
fn in_chat(entry: &ManagerEntry) -> bool {
    !matches!(
        entry.event,
        ManagerEvent::Delivered { .. }
            | ManagerEvent::Agent { .. }
            | ManagerEvent::Ping { .. }
            | ManagerEvent::Answered { .. }
    )
}

/// The entries of `window` in `log` (`read_chat`), and how many older ones were left out.
/// `names`: what the rows show, which a find matches.
pub fn chat_window<'a>(
    log: &'a [ManagerEntry],
    window: &Window,
    names: &Names,
) -> (Vec<&'a ManagerEntry>, usize) {
    if let Window::Around(id) = window {
        if let Some(at) = log.iter().position(|entry| entry.id() == Some(*id)) {
            let before = log[..at].iter().filter(|e| in_chat(e)).count();
            let skipped = before.saturating_sub(AROUND_BEFORE);
            let entries = log.iter().filter(|e| in_chat(e));
            return (entries.skip(skipped).take(CHAT_ENTRIES).collect(), skipped);
        }
    }
    let (start, limit) = match window {
        Window::Tail => (0, CHAT_ENTRIES),
        Window::After(after) => (
            log.iter()
                .position(|entry| entry.id() == Some(*after))
                .map_or(0, |index| index + 1),
            CHAT_ENTRIES,
        ),
        Window::Find(words) => match find_from(log, words, names) {
            Some(start) => (start, usize::MAX),
            None => (0, CHAT_ENTRIES),
        },
        Window::Around(_) => (0, CHAT_ENTRIES),
    };
    let mut entries: Vec<&ManagerEntry> = log[start..].iter().filter(|e| in_chat(e)).collect();
    let skipped = entries.len().saturating_sub(limit);
    entries.drain(..skipped);
    (entries, skipped)
}

/// The oldest entry of `log` whose row shows `words`, case-insensitive: its author's name, its
/// tag, its quote or its text, as `chat_rows` makes them.
fn find_from(log: &[ManagerEntry], words: &str, names: &Names) -> Option<usize> {
    let words = words.to_lowercase();
    if words.is_empty() {
        return None;
    }
    let shows = |text: &Option<String>| {
        text.as_ref()
            .is_some_and(|text| text.to_lowercase().contains(&words))
    };
    log.iter().position(|entry| {
        in_chat(entry)
            && chat_rows(&[entry], &[], &[], names).iter().any(|row| {
                row.text.to_lowercase().contains(&words)
                    || shows(&row.name)
                    || shows(&row.tag)
                    || shows(&row.quoted)
            })
    })
}

/// A reply that asks the owner and is not answered yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenAsk {
    pub id: u64,
    pub unix_ms: u64,
    /// The line a quote of it carries (`quote_of`).
    pub quote: String,
}

/// The replies that ask the owner (`asks`) and are not answered, oldest first. An owner
/// message that quotes one answers it; one with no quote answers the newest open one before
/// it; one that quotes anything else answers none. A dismiss (`Answered`) answers its reply.
/// A question older than `ASKS_WITHIN_MS` before `now` asks no more (docs/tasks.md "Manager").
pub fn open_asks(log: &[ManagerEntry], now: u64) -> Vec<OpenAsk> {
    let mut open: Vec<OpenAsk> = Vec::new();
    for entry in log {
        match &entry.event {
            ManagerEvent::Reply {
                id,
                text,
                asks: true,
                ..
            } => open.push(OpenAsk {
                id: *id,
                unix_ms: entry.unix_ms,
                quote: quote_of(text),
            }),
            ManagerEvent::Message {
                from: Author::Owner,
                text,
                quote,
                ..
            } => {
                // ^ A quote with its id answers that question; an old one, by its line.
                let answered = match (quote, split_quote(text).0) {
                    (Some(id), _) => open.iter().rposition(|ask| ask.id == *id),
                    (None, Some(quoted)) => open.iter().rposition(|ask| ask.quote == quoted.trim()),
                    (None, None) => open.len().checked_sub(1),
                };
                if let Some(at) = answered {
                    open.remove(at);
                }
            }
            ManagerEvent::Answered { id } => open.retain(|ask| ask.id != *id),
            _ => {}
        }
    }
    open.retain(|ask| now.saturating_sub(ask.unix_ms) < ASKS_WITHIN_MS);
    open
}

/// The open questions of `dir`'s chat: the chats mark them, the Home row and the Workspace
/// row show them in the accent, the TUI's items and the web's reservoir list them. Only the
/// lines `open_asks` reads are parsed: on the wsx log, 7 days are 1 MB of it.
pub fn asking(dir: &ManagerDir, now: u64) -> Result<Vec<OpenAsk>> {
    // ^ Where serde writes them: `unix_ms`, `kind` and a message's `from` first, a reply's
    // `asks` after its text, near the end (`ManagerEvent`). A quote in a text is escaped, so a
    // text never holds these quoted words. The marks leave out the colon: a line written with
    // a space after it (a hand-written fixture) passes too; what passes needlessly is parsed.
    let has = |part: &[u8], mark: &[u8]| part.windows(mark.len()).any(|at| at == mark);
    let keep = |line: &[u8]| {
        let head = &line[..line.len().min(128)];
        let tail = &line[line.len().saturating_sub(256)..];
        has(head, b"\"answered\"")
            || (has(head, b"\"message\"") && has(head, b"\"owner\""))
            || (has(head, b"\"reply\"") && has(tail, b"\"asks\"") && has(tail, b"true"))
    };
    Ok(open_asks(
        &dir.read_back_where(FIRST_READ_BYTES, keep, |log| reaches_asks(log, now))?,
        now,
    ))
}

/// The owner dismisses the question of reply `id`: it asks no more, and no turn reads it.
/// Refused for a reply that is not an open question.
pub fn dismiss(dir: &ManagerDir, id: u64, now: u64) -> Result<()> {
    if !asking(dir, now)?.iter().any(|ask| ask.id == id) {
        bail!("reply {id} is not an open question");
    }
    dir.append(ManagerEvent::Answered { id })
}

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
    /// The id of the message the leading quote points at: a tap goes to it at any age.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_id: Option<u64>,
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
            quoted_id: None,
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
            quoted_id: entry.quote_id(),
            ..Self::new(RowKind::Message, key, entry, body.trim().to_string())
        }
    }
}

/// The chat window's entries (`chat_window`) as rows. `approvals`: the pending approvals
/// (`pending_approvals`); a quiet reply that carries one is a message. `asks`: the open
/// questions (`open_asks`), whose rows ask.
pub fn chat_rows(
    entries: &[&ManagerEntry],
    approvals: &[&ApprovalLine],
    asks: &[OpenAsk],
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
                id,
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
                    asks: asks.iter().any(|ask| ask.id == *id),
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
            | ManagerEvent::Ping { .. }
            | ManagerEvent::Answered { .. } => {}
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
