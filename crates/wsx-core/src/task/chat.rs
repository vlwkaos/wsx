//! The manager chat as rows: the log classified once, for the web's bubbles and the TUI's
//! chat, which only render them. ref: docs/tasks.md "Manager", docs/web-client-tasks.md
//! "Project screen"

use super::checks::CheckItem;
use super::manager::{
    quote_of, says_nothing, split_quote, ApprovalLine, Author, Level, ManagerEntry, ManagerEvent,
    Party, Short,
};
use super::store::{self, contains_bytes, ManagerDir, FIRST_READ_BYTES};
use crate::routine::RunRecord;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Most entries the chat shows at once.
pub const CHAT_ENTRIES: usize = 200;
/// A window around a message shows this many chat entries before it.
const AROUND_BEFORE: usize = 40;
/// A question older than this asks no more (docs/tasks.md "Manager").
pub const ASKS_WITHIN_MS: u64 = 7 * 24 * 3600 * 1000;

/// The path a `files/<path>` link address names, `%XX` decoded (`link_path` in task/manager.rs
/// writes each byte outside a safe set so). The chats, the web's media grant and the TUI read
/// the address through it.
pub fn file_href(href: &str) -> Option<String> {
    decoded(href.strip_prefix("files/")?)
}

/// The task and the path a `task/<n>/<path>` link address names: a file of open task `n`'s
/// working copy, `%XX` decoded as `file_href`. The `task/` prefix is not `files/`, so the form
/// cannot be read as a path of the main checkout. ref: docs/web-client.md "Files a message shows"
pub fn task_file_href(href: &str) -> Option<(u32, String)> {
    let (number, path) = href.strip_prefix("task/")?.split_once('/')?;
    let number = number.parse().ok().filter(|_| !number.starts_with('0'))?;
    Some((number, decoded(path)?))
}

fn decoded(path: &str) -> Option<String> {
    if path.is_empty() || path.contains(char::is_whitespace) {
        return None;
    }
    let (mut bytes, mut at) = (Vec::new(), path.bytes());
    while let Some(byte) = at.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex = [at.next()?, at.next()?];
        bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}

/// What `read` finds of every link or image address in a message, in order, once each.
fn linked<T: PartialEq>(text: &str, read: impl Fn(&str) -> Option<T>) -> Vec<T> {
    let mut found: Vec<T> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("](") {
        rest = &rest[open + 2..];
        let Some(end) = rest.find(')') else { break };
        if let Some(item) = read(&rest[..end]) {
            if !found.contains(&item) {
                found.push(item);
            }
        }
        rest = &rest[end + 1..];
    }
    found
}

/// Every `files/<path>` a message links or shows as an image (`[x](files/p)`, `![x](files/p)`),
/// decoded, in order, once each. ref: docs/web-client-chat.md "Markdown"
pub fn linked_files(text: &str) -> Vec<String> {
    linked(text, file_href)
}

/// The same for `task/<n>/<path>`: (task, path), in order, once each.
pub fn linked_task_files(text: &str) -> Vec<(u32, String)> {
    linked(text, task_file_href)
}

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

/// The id of the events message that told of each routine run's end, for the chat to open at:
/// the first Events message at or after the run's end that has the run's result line
/// (`RunRecord::result_line`) as a line of its `short`. None for a run still going, one that
/// printed nothing (it makes no event), or one older than the log kept. No agent reads this.
/// ref: docs/tasks.md "Routines"
pub fn run_messages(dir: &ManagerDir, runs: &[&RunRecord]) -> Vec<Option<u64>> {
    let ends: Vec<u64> = runs
        .iter()
        .filter_map(|run| run.finished_epoch)
        .map(|end| end.max(0) as u64 * 1000)
        .collect();
    let Some(oldest) = ends.iter().min().copied() else {
        return vec![None; runs.len()];
    };
    let log = dir
        .read_back(FIRST_READ_BYTES, |log| {
            log.first().is_some_and(|entry| entry.unix_ms <= oldest)
        })
        .unwrap_or_default();
    runs.iter()
        .map(|run| {
            let end = run.finished_epoch?.max(0) as u64 * 1000;
            let line = Short::plain(&run.result_line()).text;
            log.iter().find_map(|entry| match &entry.event {
                ManagerEvent::Message {
                    id,
                    from: Author::Events,
                    short,
                    ..
                } if entry.unix_ms >= end && short.iter().any(|each| each.text == line) => {
                    Some(*id)
                }
                _ => None,
            })
        })
        .collect()
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

/// The owner message a manager's reply to the turn `to` quotes, or None. `log` holds the entries
/// up to now, the reply not yet. The one place that chooses which message a reply relates to and
/// whether the quote helps (docs/tasks.md "A quote"). A quote means "this answers that message",
/// so only a message the manager answers can be quoted, and a turn with no owner message (events,
/// another chat's words) has none. A quote helps when the reply will not sit directly under what
/// it answers: the newest owner message of the turn, quoted when the turn held more than one or
/// any chat item was logged after it.
pub fn quoted_message(log: &[ManagerEntry], to: &[u64]) -> Option<u64> {
    let owners: Vec<usize> = (0..log.len())
        .filter(|&at| {
            log[at].id().is_some_and(|id| to.contains(&id))
                && matches!(
                    &log[at].event,
                    ManagerEvent::Message {
                        from: Author::Owner,
                        ..
                    }
                )
        })
        .collect();
    let &newest = owners.last()?;
    let apart = owners.len() > 1 || log[newest + 1..].iter().any(in_chat);
    log[newest].id().filter(|_| apart)
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
            let entries = entries.skip(skipped).take(CHAT_ENTRIES).collect();
            return (divider_first(log, entries), skipped);
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
    (divider_first(log, entries), skipped)
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
            && chat_rows(&[entry], &[], &[], &[], names).iter().any(|row| {
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
/// A reply whose turn made an approval request with a card (`asked_in_turn`) is no question: the
/// card is the one item, and the decision settles it (docs/tasks.md "Approvals").
pub fn open_asks(
    log: &[ManagerEntry],
    approvals: &[ApprovalLine],
    chat: &str,
    now: u64,
) -> Vec<OpenAsk> {
    let mut open: Vec<OpenAsk> = Vec::new();
    // ^ The turn began when its messages were delivered; its reply ends it.
    let mut turn_from: Option<u64> = None;
    for entry in log {
        match &entry.event {
            ManagerEvent::Delivered { .. } => turn_from = Some(entry.unix_ms),
            ManagerEvent::Reply { id, text, asks, .. } => {
                let from = turn_from.take();
                if *asks && !asked_in_turn(approvals, chat, from, entry.unix_ms) {
                    open.push(OpenAsk {
                        id: *id,
                        unix_ms: entry.unix_ms,
                        quote: quote_of(text),
                    });
                }
            }
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

/// Whether chat `chat`'s agent made a request with a Yes/No card (`said` none) between `from`, the
/// delivery of its turn, and `to`, the turn's reply. A request is stamped with the chat of the
/// agent that ran the command (`Actor`) and the time it ran; one turn runs at a time in a chat,
/// so a request in its bounds is the turn's own. No delivery in the log: no link.
fn asked_in_turn(approvals: &[ApprovalLine], chat: &str, from: Option<u64>, to: u64) -> bool {
    from.is_some_and(|from| {
        approvals.iter().any(|line| match line {
            ApprovalLine::Request {
                said: None,
                chat: Some(by),
                unix_ms,
                ..
            } => by == chat && (from..=to).contains(unix_ms),
            _ => false,
        })
    })
}

/// The open questions of `dir`'s chat: the chats mark them, the Home row and the Workspace
/// row show them in the accent, the TUI's items and the web's reservoir list them. Only the
/// lines `open_asks` reads are parsed: on the wsx log, 7 days are 1 MB of it.
pub fn asking(dir: &ManagerDir, now: u64) -> Result<Vec<OpenAsk>> {
    // ^ Where serde writes them: `unix_ms`, `kind` and a message's `from` first, a reply's
    // `asks` after its text, near the end (`ManagerEvent`). A quote in a text is escaped, so a
    // text never holds these quoted words. The marks leave out the colon: a line written with
    // a space after it (a hand-written fixture) passes too; what passes needlessly is parsed.
    let keep = |line: &[u8]| {
        let head = &line[..line.len().min(128)];
        let tail = &line[line.len().saturating_sub(256)..];
        contains_bytes(head, b"\"answered\"")
            || contains_bytes(head, b"\"delivered\"")
            || (contains_bytes(head, b"\"message\"") && contains_bytes(head, b"\"owner\""))
            || (contains_bytes(head, b"\"reply\"")
                && contains_bytes(tail, b"\"asks\"")
                && contains_bytes(tail, b"true"))
    };
    Ok(open_asks(
        &dir.read_back_where(FIRST_READ_BYTES, keep, |log| reaches_asks(log, now))?,
        &store::read_approvals(&dir.project_id).unwrap_or_default(),
        &dir.chat,
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
    /// manager, or a message of another manager or a chat.
    Message,
    /// A muted line of wsx: an event in its short form, or the usage limit.
    Line,
    /// A quiet reply: the manager's own work, a muted line under its name.
    Quiet,
    /// A new conversation, or the manager stopped.
    Note,
    /// What the manager and another chat said to each other, folded into one line that opens to
    /// the words: `Talked to <name>`.
    Talk,
}

/// Who wrote a row: `owner`, `manager` (the chat's other side) or `peer:<key>` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(into = "String")]
pub enum Speaker {
    Owner,
    Manager,
    /// Another project's manager, or a chat of this project.
    Peer(Party),
}

impl From<Speaker> for String {
    fn from(speaker: Speaker) -> Self {
        match speaker {
            Speaker::Owner => "owner".into(),
            Speaker::Manager => "manager".into(),
            Speaker::Peer(party) => format!("peer:{}", party.key()),
        }
    }
}

/// The names the rows show.
pub struct Names<'a> {
    /// The chat's other side: the manager's shown name (`manager_shown_name`), or the chat's.
    pub own: &'a str,
    /// The project's manager, as another chat names it (`manager_name_in`).
    pub manager: &'a str,
}

/// How a chat names the project's first chat: its shown name, and in another chat (`chat`) the
/// word `manager` after it, so the room has three speakers (the owner, the manager, the chat)
/// and the bare project name is none. ref: docs/tasks.md "Chats"
pub fn manager_name_in(chat: Option<&str>, shown: &str) -> String {
    match chat {
        Some(_) => format!("{shown} manager"),
        None => shown.to_string(),
    }
}

/// What a muted row says as marks: one task, its new state and its checks. Built from the facts
/// of the events the row is or answers (`Short`), never read from the manager's prose, so the
/// web and the TUI draw the same marks (a `#n` link, a status chip, a check count).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Token {
    pub task: u32,
    /// The task's state in the words of `task::status` (`landed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// `status::tone` of `state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<&'static str>,
    /// How many checks passed, of how many.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<[u32; 2]>,
}

impl Token {
    fn of(short: &Short) -> Option<Self> {
        Some(Self {
            task: short.task?,
            tone: short.state.as_deref().and_then(super::status::tone),
            state: short.state.clone(),
            checks: short.checks,
        })
    }
}

/// One message of a `Talk` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Said {
    /// Who wrote it: the other chat's name, or the chat's own manager.
    pub name: String,
    /// The other chat's key (`Party::key`) when it wrote it; none for the chat's own manager, so
    /// a client reads a bare `#n` as the right project's task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub text: String,
}

/// The most marks a muted reply shows.
const TOKENS: usize = 4;

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
    /// A quiet reply's first sentence (`view::first_sentence`) when it says more: the chats show
    /// it and a More that opens the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_sentence: Option<String>,
    /// The line a quote of this message carries (`quote_of`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// The id of the message the leading quote points at: a tap goes to it at any age.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_id: Option<u64>,
    /// The To check item the leading quote is the check of (`ManagerEvent::Message::check`): a tap
    /// goes to its bubble, else to the To check list at it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_check: Option<String>,
    /// An event line's leading task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<u32>,
    /// A `Talk` row's messages, oldest first; its `text` is empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub said: Vec<Said>,
    /// The marks of an event line that changes a task's state, and of a muted reply to such
    /// events: the newest per task, at most `TOKENS`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<Token>,
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
    /// An owner message the held manager has not taken into a turn (`waiting_ids`): the chats
    /// show it as waiting, after the other rows.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
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
            first_sentence: None,
            quote: None,
            quoted_id: None,
            quoted_check: None,
            task: None,
            said: Vec::new(),
            tokens: Vec::new(),
            asks: false,
            post: false,
            level: None,
            approvals: Vec::new(),
            provisional: None,
            danger: false,
            queued: false,
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
            quoted_check: entry.check_id().map(str::to_string),
            ..Self::new(RowKind::Message, key, entry, body.trim().to_string())
        }
    }
}

/// A chat read for a viewer: the page and the approvals that go with it.
pub struct Reading {
    pub page: Page,
    /// The approvals this chat shows: its own requests (`chats::owner`) and their decisions.
    pub approvals: Vec<ApprovalLine>,
    /// The open To check items of the tasks this chat owns.
    pub items: Vec<CheckItem>,
}

/// The one read of a chat for the web, the TUI and `wsx task chat`: the names the viewer sees
/// (the manager's shown name and the chat's own), the approvals of
/// the chat that owns them, and the page at `window`. No folder yet is an empty chat. `edit` is
/// `page`'s.
pub fn read_for(
    dir: Option<&ManagerDir>,
    (project, store_id): (&str, &str),
    settings: &crate::model::workspace::TaskConfig,
    window: &Window,
    edit: impl FnMut(&mut ManagerEntry),
) -> Result<Reading> {
    let shown = settings.manager_shown_name(project);
    let chat = dir.and_then(ManagerDir::named);
    let manager = manager_name_in(chat, shown);
    let own = dir.map_or_else(
        || shown.to_string(),
        |dir| super::chats::shown_name(dir, shown),
    );
    let names = Names {
        own: &own,
        manager: &manager,
    };
    let viewer = super::chats::Viewer::new(store_id, dir.map(|dir| dir.chat.as_str()));
    let approvals = super::chats::approvals_of(super::store::read_approvals(store_id)?, &viewer);
    let items = super::chats::checks_of(store_id, super::checks::open_items(store_id)?, &viewer);
    let page = match dir {
        Some(dir) => page(
            dir,
            window,
            &names,
            &super::manager::pending_approvals(&approvals),
            super::now_unix_ms(),
            edit,
        )?,
        None => Page::default(),
    };
    Ok(Reading {
        page,
        approvals,
        items,
    })
}

/// A chat window read once: what the web, the TUI and `wsx task chat` all need.
#[derive(Default)]
pub struct Page {
    /// The log as `read_chat` returned it; `activity` reads its end.
    pub log: Vec<ManagerEntry>,
    /// The entries of the window, after the caller's `edit`.
    pub entries: Vec<ManagerEntry>,
    /// How many older entries the window left out.
    pub skipped: usize,
    pub asks: Vec<OpenAsk>,
    pub rows: Vec<ChatRow>,
}

/// Read `dir`'s chat at `window` and classify it: the log, the window's entries, the open
/// questions and the rows. `edit` changes each entry of the window before its row is made (the
/// web turns an owner message's file paths into attachments). `approvals`: see `chat_rows`.
pub fn page(
    dir: &ManagerDir,
    window: &Window,
    names: &Names,
    approvals: &[&ApprovalLine],
    now: u64,
    mut edit: impl FnMut(&mut ManagerEntry),
) -> Result<Page> {
    let log = read_chat(dir, window)?;
    let asks = asking(dir, now)?;
    let (entries, skipped) = chat_window(&log, window, names);
    let mut entries: Vec<ManagerEntry> = entries.into_iter().cloned().collect();
    entries.iter_mut().for_each(&mut edit);
    let rows = chat_rows(
        &entries.iter().collect::<Vec<_>>(),
        approvals,
        &asks,
        &waiting_ids(&log),
        names,
    );
    Ok(Page {
        log,
        entries,
        skipped,
        asks,
        rows,
    })
}

/// The owner messages in `log` the manager has not taken into a turn while it is held (a usage
/// limit, an agent error or a failed start: the newest `limit` or `stopped` entry that no
/// resume or reply followed). Else none: a message waits only moments for a free manager.
/// ref: docs/tasks.md "Manager"
pub fn waiting_ids(log: &[ManagerEntry]) -> Vec<u64> {
    let held = log.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Limit { until } => Some(until.is_some()),
        ManagerEvent::Stopped { .. } => Some(true),
        // ^ A stand-in's reply and a post are written during a hold: they end none.
        ManagerEvent::Reply {
            provisional: None,
            post: false,
            ..
        } => Some(false),
        _ => None,
    });
    if held != Some(true) {
        return Vec::new();
    }
    let taken: HashSet<u64> = log
        .iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Delivered { ids } => Some(ids),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    log.iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                id,
                from: Author::Owner,
                ..
            } if !taken.contains(id) => Some(*id),
            _ => None,
        })
        .collect()
}

/// The chat window's entries (`chat_window`) as rows. `approvals`: the pending approvals
/// (`pending_approvals`); a quiet reply that carries one is a message. `asks`: the open
/// questions (`open_asks`), whose rows ask. `waiting`: the owner messages not taken into a
/// turn (`waiting_ids`), whose rows come last, queued.
pub fn chat_rows(
    entries: &[&ManagerEntry],
    approvals: &[&ApprovalLine],
    asks: &[OpenAsk],
    waiting: &[u64],
    names: &Names,
) -> Vec<ChatRow> {
    let entries = without_ask_lines(entries);
    let entries = entries.as_slice();
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
    // ^ A posted event shows once, as the reply that says it to the owner (`post`); the log keeps
    // the event for the manager (docs/tasks.md "Manager").
    let posted: HashSet<u64> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Reply { to, post: true, .. } => Some(to.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    // ^ What each message is, so a reply knows what it answers (`bubble`).
    let said: HashMap<u64, &ManagerEvent> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message { id, .. } => Some((*id, &entry.event)),
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
    let mut rows: Vec<ChatRow> = Vec::new();
    let mut queued: Vec<ChatRow> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if held_kind(entry) {
            // ^ A run of holds is folded where it starts (`hold_rows`).
            if index == 0 || !held_kind(entries[index - 1]) {
                let run = entries[index..].iter().take_while(|entry| held_kind(entry));
                hold_rows(&mut rows, &run.copied().collect::<Vec<_>>(), names);
            }
            continue;
        }
        match &entry.event {
            ManagerEvent::Message {
                from: Author::Events,
                id,
                ..
            } if posted.contains(id) => {}
            ManagerEvent::Message {
                from: Author::Events,
                text,
                short,
                ..
            } => event_rows(&mut rows, entry, text, short),
            ManagerEvent::Message { from, text, .. } => {
                let row = message_row(entry, from, text);
                match row {
                    Some(row) if row.id.is_some_and(|id| waiting.contains(&id)) => {
                        queued.push(ChatRow {
                            queued: true,
                            ..row
                        })
                    }
                    Some(row) => push_row(&mut rows, row),
                    None => {}
                }
            }
            ManagerEvent::Reply { .. } => rows.extend(reply_row(
                entry,
                std::mem::take(&mut carried[index]),
                &recaps,
                asks,
                names,
                &said,
            )),
            _ => {
                if let Some(row) = note_row(entry, names) {
                    push_row(&mut rows, row);
                }
            }
        }
    }
    rows.extend(queued);
    rows
}

/// What a host before the tag wrote after a turn that asked the owner: an events message
/// "asked the owner: Approve push (a3)" and a post reply to it, "Asked you: ...". An approval shows
/// once, as the tag of the manager's reply (`chat_rows`), so such a pair has no row; the old logs keep it.
const OLD_ASK_LINE: &str = "asked the owner: ";

fn without_ask_lines<'a>(entries: &[&'a ManagerEntry]) -> Vec<&'a ManagerEntry> {
    let old: HashSet<u64> = entries
        .iter()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                from: Author::Events,
                id,
                text,
                ..
            } if text.starts_with(OLD_ASK_LINE) => Some(*id),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .copied()
        .filter(|entry| match &entry.event {
            ManagerEvent::Message { id, .. } => !old.contains(id),
            ManagerEvent::Reply { to, post: true, .. } => {
                to.is_empty() || !to.iter().all(|id| old.contains(id))
            }
            _ => true,
        })
        .collect()
}

/// The window `shown` of `log` in the order its rows show: a new conversation's divider goes
/// directly above the first message it answers, one logged before the rotation and delivered in
/// the first `Delivered` after it (the host rotates as it takes the waiting messages for a turn).
/// The log keeps its order. A rotation at idle, one with a stop or pause between it and that
/// message, or one whose message is outside the window keeps its place, so holds fold as before.
/// ref: docs/tasks.md "Manager" (New conversation)
fn divider_first<'a>(
    log: &'a [ManagerEntry],
    mut shown: Vec<&'a ManagerEntry>,
) -> Vec<&'a ManagerEntry> {
    let place = |list: &[&ManagerEntry], entry: &ManagerEntry| {
        list.iter().position(|item| std::ptr::eq(*item, entry))
    };
    for (at, rotated) in log.iter().enumerate() {
        if !matches!(rotated.event, ManagerEvent::Rotated { .. }) {
            continue;
        }
        let Some(from) = place(&shown, rotated) else {
            continue;
        };
        let Some(ids) = log[at + 1..].iter().find_map(|entry| match &entry.event {
            ManagerEvent::Delivered { ids } => Some(ids),
            _ => None,
        }) else {
            continue;
        };
        let first = log[..at].iter().position(
            |entry| matches!(&entry.event, ManagerEvent::Message { id, .. } if ids.contains(id)),
        );
        let Some(first) = first.filter(|&first| !log[first..at].iter().any(held_kind)) else {
            continue;
        };
        if place(&shown, &log[first]).is_some() {
            let moved = shown.remove(from);
            let to = place(&shown, &log[first]).unwrap_or(from);
            shown.insert(to, moved);
        }
    }
    shown
}

/// The key of an entry's row: its kind and id, else its time.
fn row_key(entry: &ManagerEntry, kind: &str) -> String {
    match entry.id() {
        Some(id) => format!("{kind}-{id}"),
        None => format!("{kind}-{}", entry.unix_ms),
    }
}

/// A peer's name: a chat's.
fn peer_name(party: &Party) -> String {
    party.name().to_string()
}

/// The row of a message from the owner, a manager or a chat. Events have `event_rows`; a recap
/// request shows nothing.
fn message_row(entry: &ManagerEntry, from: &Author, text: &str) -> Option<ChatRow> {
    let key = row_key(entry, "message");
    let peer = match from {
        Author::Owner => {
            return Some(ChatRow::said(
                key,
                entry,
                Speaker::Owner,
                "You".into(),
                text,
            ))
        }
        Author::Peer { from, .. } => from,
        Author::Events | Author::Recap => return None,
    };
    talk(key, entry, peer, Some(peer), "", text)
}

/// The row of what another chat and the manager said to each other: the muted line `Talked to
/// <name>` that opens to the words (`push_row` joins a run with one chat). Empty words (`-`) show
/// nothing. ref: docs/design/transcript.md
fn talk(
    key: String,
    entry: &ManagerEntry,
    with: &Party,
    from: Option<&Party>,
    own: &str,
    text: &str,
) -> Option<ChatRow> {
    let text = text.trim();
    if says_nothing(text) {
        return None;
    }
    let name = peer_name(with);
    Some(ChatRow {
        author: Some(Speaker::Peer(with.clone())),
        tag: Some(format!("Talked to {name}")),
        said: vec![Said {
            name: from.map_or_else(|| own.to_string(), peer_name),
            peer: from.map(Party::key),
            text: text.to_string(),
        }],
        name: Some(name),
        ..ChatRow::new(RowKind::Talk, key, entry, String::new())
    })
}

/// `row` after the rows shown, a `Talk` joined to the `Talk` with the same chat before it: one
/// line for a run of messages, which keeps its first key and takes the last time.
fn push_row(rows: &mut Vec<ChatRow>, row: ChatRow) {
    match rows.last_mut() {
        Some(last)
            if row.kind == RowKind::Talk
                && last.kind == RowKind::Talk
                && last.author == row.author =>
        {
            last.said.extend(row.said);
            last.unix_ms = row.unix_ms;
        }
        _ => rows.push(row),
    }
}

/// The lines of an events message, one row each unless the line repeats: a task's line is a repeat
/// when it has the text of that task's latest line since the last message bubble (any time later);
/// a line with no task, when it has the text of the row before it.
fn event_rows(rows: &mut Vec<ChatRow>, entry: &ManagerEntry, text: &str, short: &[Short]) {
    // ^ A line logged before `short` shows its first clause.
    let lines: Vec<Short> = match short.is_empty() {
        false => short.to_vec(),
        true => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(Short::plain)
            .collect(),
    };
    for (nth, line) in lines.into_iter().enumerate() {
        let latest = match line.task {
            Some(task) => rows
                .iter()
                .rev()
                .take_while(|row| row.kind != RowKind::Message)
                .find(|row| row.kind == RowKind::Line && row.task == Some(task)),
            None => rows.last().filter(|row| row.kind == RowKind::Line),
        };
        let repeat = latest.is_some_and(|row| row.text == line.text);
        if !repeat {
            rows.push(ChatRow {
                task: line.task,
                tokens: line.state.iter().filter_map(|_| Token::of(&line)).collect(),
                ..ChatRow::new(
                    RowKind::Line,
                    format!("{}-{nth}", row_key(entry, "message")),
                    entry,
                    line.text,
                )
            });
        }
    }
}

/// The row of a manager reply. `approvals`: the ids of the pending approvals it carries. `said`:
/// the messages of the window, which tell what the reply answers.
///
/// The one rule of what reaches the owner as a bubble: a reply is a bubble when it answers an
/// owner message or a recap (or none the window knows), asks, carries an approval or is a post.
/// Any other reply, one to events or to another chat, is a muted line (`Quiet`) with the marks of
/// the events it answers; its words open under it. A reply that says nothing (`-`) has no row.
/// ref: docs/design/transcript.md
fn reply_row(
    entry: &ManagerEntry,
    approvals: Vec<String>,
    recaps: &HashSet<u64>,
    asks: &[OpenAsk],
    names: &Names,
    said: &HashMap<u64, &ManagerEvent>,
) -> Option<ChatRow> {
    let ManagerEvent::Reply {
        to,
        text,
        quiet,
        id,
        provisional,
        post,
        level,
        ..
    } = &entry.event
    else {
        return None;
    };
    let asking = asks.iter().any(|ask| ask.id == *id);
    // ^ A reply to events keeps the rule of `quiet`: a long one, or one for the owner, is a
    // message. A reply only to another chat's words is its own work.
    let to_peers = !to.is_empty()
        && to.iter().all(|id| {
            matches!(
                said.get(id),
                Some(ManagerEvent::Message {
                    from: Author::Peer { .. },
                    ..
                })
            )
        });
    // ^ A quiet reply stays a muted line unless it carries an approval; a posted line is quiet.
    let keeps = !approvals.is_empty() || (!*quiet && (asking || *post || level.is_some()));
    if says_nothing(text) && !keeps {
        return None;
    }
    if !keeps && (*quiet || to_peers) {
        let text = text.trim();
        let first = super::view::first_sentence(text);
        let mut tokens: Vec<Token> = Vec::new();
        for token in to.iter().filter_map(|id| match said.get(id) {
            Some(ManagerEvent::Message { short, .. }) => Some(short),
            _ => None,
        }) {
            for token in token.iter().filter_map(Token::of) {
                tokens.retain(|kept| kept.task != token.task);
                tokens.push(token);
            }
        }
        tokens.drain(..tokens.len().saturating_sub(TOKENS));
        return Some(ChatRow {
            author: Some(Speaker::Manager),
            name: Some(names.own.to_string()),
            first_sentence: (first != text).then_some(first),
            tokens,
            ..ChatRow::new(
                RowKind::Quiet,
                row_key(entry, "reply"),
                entry,
                text.to_string(),
            )
        });
    }
    Some(ChatRow {
        tag: to
            .iter()
            .any(|id| recaps.contains(id))
            .then(|| "Recap".into()),
        asks: asking,
        post: *post,
        level: *level,
        approvals,
        provisional: provisional.clone(),
        ..ChatRow::said(
            row_key(entry, "reply"),
            entry,
            Speaker::Manager,
            names.own.to_string(),
            text,
        )
    })
}

/// The row of what the manager itself did: a message it sent, a pause, a new conversation or a
/// stop. The other entries have none.
fn note_row(entry: &ManagerEntry, names: &Names) -> Option<ChatRow> {
    match &entry.event {
        // ^ An empty `sent` (older hosts logged one without --json) shows nothing.
        ManagerEvent::Sent { text, .. } if text.trim().is_empty() => None,
        ManagerEvent::Sent { to, text, .. } => talk(
            row_key(entry, "sent"),
            entry,
            &Party::from_key(to),
            None,
            names.own,
            text,
        ),
        ManagerEvent::Limit { until } => Some(ChatRow::new(
            RowKind::Line,
            row_key(entry, "limit"),
            entry,
            match until {
                Some(until) => format!("{} paused: usage limit, resumes at {until}", names.own),
                None => format!("{} resumed", names.own),
            },
        )),
        ManagerEvent::Rotated { tokens, reason } => Some(ChatRow::new(
            RowKind::Note,
            row_key(entry, "rotated"),
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
        ManagerEvent::Stopped { reason } => Some(ChatRow {
            danger: true,
            ..ChatRow::new(
                RowKind::Note,
                row_key(entry, "stopped"),
                entry,
                format!("{} stopped: {reason}", names.own),
            )
        }),
        // ^ A fork as one note in each chat; closing the new chat before it owns a task undoes
        // it (docs/tasks.md "Lead and forks").
        ManagerEvent::Forked { to, .. } => Some(ChatRow::new(
            RowKind::Note,
            row_key(entry, &format!("forked-{to}")),
            entry,
            format!("\u{2442} Forked to {to}"),
        )),
        ManagerEvent::ForkedFrom { from, .. } => Some(ChatRow::new(
            RowKind::Note,
            row_key(entry, &format!("forked-from-{from}")),
            entry,
            format!("\u{2442} Forked from {from}"),
        )),
        ManagerEvent::ForkUndone { to } => Some(ChatRow::new(
            RowKind::Note,
            row_key(entry, &format!("fork-undone-{to}")),
            entry,
            format!("\u{2442} Fork to {to} undone"),
        )),
        _ => None,
    }
}

/// Whether an entry is a hold: the manager stopped, paused or resumed, or began a new
/// conversation.
fn held_kind(entry: &ManagerEntry) -> bool {
    matches!(
        entry.event,
        ManagerEvent::Stopped { .. } | ManagerEvent::Limit { .. } | ManagerEvent::Rotated { .. }
    )
}

/// The rows of a run of hold entries with nothing shown between them. Those of one reason (a
/// stop's, or the reason of the new conversation it brought) fold into one row when they hold
/// twice or more: `wsx stopped 3 times: <reason>; next try 10:11`. A run of usage limits with
/// no stop folds the same way. Else each entry has its own row (`note_row`).
fn hold_rows(rows: &mut Vec<ChatRow>, run: &[&ManagerEntry], names: &Names) {
    let mut segment: Vec<&ManagerEntry> = Vec::new();
    let mut reason: Option<&str> = None;
    for entry in run {
        let own = match &entry.event {
            ManagerEvent::Stopped { reason } => Some(reason.as_str()),
            ManagerEvent::Rotated {
                reason: Some(reason),
                ..
            } => Some(reason.as_str()),
            _ => None,
        };
        if matches!(entry.event, ManagerEvent::Rotated { reason: None, .. })
            || own.is_some_and(|own| !segment.is_empty() && reason != Some(own))
        {
            fold_segment(rows, &segment, reason, names);
            segment.clear();
            reason = None;
        }
        reason = own.or(reason);
        segment.push(entry);
    }
    fold_segment(rows, &segment, reason, names);
}

/// One segment of `hold_rows`: `reason` is the stop's, none for a run of limits alone.
fn fold_segment(
    rows: &mut Vec<ChatRow>,
    segment: &[&ManagerEntry],
    reason: Option<&str>,
    names: &Names,
) {
    let count =
        |counts: fn(&ManagerEvent) -> bool| segment.iter().filter(|e| counts(&e.event)).count();
    let stops = count(|event| matches!(event, ManagerEvent::Stopped { .. }));
    let pauses = count(|event| matches!(event, ManagerEvent::Limit { until: Some(_) }));
    let (Some(first), Some(last)) = (segment.first(), segment.last()) else {
        return;
    };
    let times = if reason.is_some() { stops } else { pauses };
    if times < 2 {
        rows.extend(segment.iter().filter_map(|entry| note_row(entry, names)));
        return;
    }
    // ^ How the run ended: paused until a time, resumed, or neither (a stop with no pause).
    let end = segment.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Limit { until } => Some(until.as_deref()),
        ManagerEvent::Stopped { .. } => Some(None),
        _ => None,
    });
    let own = names.own;
    let (text, kind) = match (reason, end) {
        (Some(reason), Some(Some(until))) => (
            format!("{own} stopped {times} times: {reason}; next try {until}"),
            RowKind::Note,
        ),
        (Some(reason), _) => (
            format!("{own} stopped {times} times: {reason}"),
            RowKind::Note,
        ),
        (None, Some(Some(until))) => (
            format!("{own} paused {times} times: usage limit, resumes at {until}"),
            RowKind::Line,
        ),
        (None, _) => (
            format!("{own} paused {times} times: usage limit"),
            RowKind::Line,
        ),
    };
    // ^ The first entry keys the row, the last gives its time: a run that grows keeps its row.
    rows.push(ChatRow {
        danger: reason.is_some(),
        unix_ms: last.unix_ms,
        ..ChatRow::new(kind, row_key(first, "held"), first, text)
    });
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
