//! The manager chat as rows: the log classified once, for the web's bubbles and the TUI's
//! chat, which only render them. ref: docs/tasks.md "Manager", docs/web-client-tasks.md
//! "Project screen"

use super::checks::CheckItem;
use super::manager::{
    quote_of, says_nothing, split_quote, ApprovalLine, Author, Level, ManagerEntry, ManagerEvent,
    Party, Short,
};
use super::store::{contains_bytes, ManagerDir, FIRST_READ_BYTES};
use crate::routine::RunRecord;
use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::HashSet;

/// The same line again this soon shows once.
const REPEAT_MS: u64 = 60_000;
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
    let path = href
        .strip_prefix("files/")
        .filter(|path| !path.is_empty() && !path.contains(char::is_whitespace))?;
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

/// Every `files/<path>` a message links or shows as an image (`[x](files/p)`, `![x](files/p)`),
/// decoded, in order, once each. ref: docs/web-client-chat.md "Markdown"
pub fn linked_files(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("](") {
        rest = &rest[open + 2..];
        let Some(end) = rest.find(')') else { break };
        if let Some(path) = file_href(&rest[..end]) {
            if !found.contains(&path) {
                found.push(path);
            }
        }
        rest = &rest[end + 1..];
    }
    found
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

/// A follow-up quotes an earlier reply only when it is among this many chat entries from the end.
const FOLLOW_UP_WITHIN: usize = 50;

/// Whether `log` (read back from the end) holds the chat entries a follow-up looks through.
pub fn reaches_follow_up(log: &[ManagerEntry]) -> bool {
    log.iter().filter(|entry| in_chat(entry)).count() > FOLLOW_UP_WITHIN
}

/// The message a manager's reply to the turn `to` quotes, or None. `log` holds the entries up to
/// now, the reply not yet. The one place that chooses which message a reply relates to and whether
/// the quote helps (docs/tasks.md "A quote"); a quote helps when the reply will not sit directly
/// under what it answers:
/// 1. A turn with owner messages: the newest is the one answered, quoted when the turn held more
///    than one or any chat item was logged after it.
/// 2. A turn of only events: the manager's newest earlier reply that names a task the events
///    report (a promise), when it is among the last `FOLLOW_UP_WITHIN` chat entries and other
///    chat items lie between it and this turn's messages.
pub fn quoted_message(log: &[ManagerEntry], to: &[u64]) -> Option<u64> {
    let mine = |entry: &ManagerEntry| entry.id().is_some_and(|id| to.contains(&id));
    let owners: Vec<usize> = (0..log.len())
        .filter(|&at| {
            mine(&log[at])
                && matches!(
                    &log[at].event,
                    ManagerEvent::Message {
                        from: Author::Owner,
                        ..
                    }
                )
        })
        .collect();
    if let Some(&newest) = owners.last() {
        let apart = owners.len() > 1 || log[newest + 1..].iter().any(in_chat);
        return log[newest].id().filter(|_| apart);
    }
    let tasks: Vec<u64> = log
        .iter()
        .filter(|entry| mine(entry))
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                from: Author::Events,
                text,
                ..
            } => Some(text),
            _ => None,
        })
        .flat_map(|text| text.lines().filter_map(leading_task))
        .collect();
    let shown: Vec<usize> = (0..log.len()).filter(|&at| in_chat(&log[at])).collect();
    let in_view = &shown[shown.len().saturating_sub(FOLLOW_UP_WITHIN)..];
    let promise = in_view.iter().rev().copied().find(|&at| {
        matches!(&log[at].event, ManagerEvent::Reply { text, quiet: false, post: false, .. }
            if !says_nothing(text) && tasks.iter().any(|&task| names_task(text, task)))
    })?;
    let between = log[promise + 1..]
        .iter()
        .any(|entry| in_chat(entry) && !mine(entry));
    log[promise].id().filter(|_| between)
}

/// The task a line of an events message reports: the number after its leading `#`.
fn leading_task(line: &str) -> Option<u64> {
    let digits: String = line
        .strip_prefix('#')?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Whether `text` names task `task` as `#<task>`, not as the start of a longer number.
fn names_task(text: &str, task: u64) -> bool {
    let tag = format!("#{task}");
    text.match_indices(&tag)
        .any(|(at, _)| !text[at + tag.len()..].starts_with(|c: char| c.is_ascii_digit()))
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
    let keep = |line: &[u8]| {
        let head = &line[..line.len().min(128)];
        let tail = &line[line.len().saturating_sub(256)..];
        contains_bytes(head, b"\"answered\"")
            || (contains_bytes(head, b"\"message\"") && contains_bytes(head, b"\"owner\""))
            || (contains_bytes(head, b"\"reply\"")
                && contains_bytes(tail, b"\"asks\"")
                && contains_bytes(tail, b"true"))
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
    /// manager, or a message of another manager or a chat.
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
    /// Another project's manager by its shown name.
    pub peer: &'a dyn Fn(&str) -> String,
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
    /// The To check item the leading quote is the check of (`ManagerEvent::Message::check`): a tap
    /// goes to its bubble, else to the To check list at it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_check: Option<String>,
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
            quote: None,
            quoted_id: None,
            quoted_check: None,
            task: None,
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
/// (the manager's shown name, the chat's own, another project's by `peer`), the approvals of
/// the chat that owns them, and the page at `window`. No folder yet is an empty chat. `edit` is
/// `page`'s.
pub fn read_for(
    dir: Option<&ManagerDir>,
    (project, store_id): (&str, &str),
    settings: &crate::model::workspace::TaskConfig,
    window: &Window,
    peer: &dyn Fn(&str) -> String,
    edit: impl FnMut(&mut ManagerEntry),
) -> Result<Reading> {
    let shown = settings.manager_shown_name(project);
    let chat = dir.and_then(|dir| dir.chat.as_deref());
    let manager = manager_name_in(chat, shown);
    let names = Names {
        own: chat.unwrap_or(shown),
        manager: &manager,
        peer,
    };
    let open = super::chats::open_names(store_id);
    let approvals =
        super::chats::approvals_of(super::store::read_approvals(store_id)?, chat, &open);
    let items =
        super::chats::checks_of(store_id, super::checks::open_items(store_id)?, chat, &open);
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
                text,
                short,
                ..
            } => event_rows(&mut rows, entry, text, short),
            ManagerEvent::Message { from, text, .. } => {
                let row = message_row(entry, from, text, names);
                match row {
                    Some(row) if row.id.is_some_and(|id| waiting.contains(&id)) => {
                        queued.push(ChatRow {
                            queued: true,
                            ..row
                        })
                    }
                    row => rows.extend(row),
                }
            }
            ManagerEvent::Reply { .. } => rows.extend(reply_row(
                entry,
                std::mem::take(&mut carried[index]),
                &recaps,
                asks,
                names,
            )),
            _ => rows.extend(note_row(entry, names)),
        }
    }
    rows.extend(queued);
    rows
}

/// The key of an entry's row: its kind and id, else its time.
fn row_key(entry: &ManagerEntry, kind: &str) -> String {
    match entry.id() {
        Some(id) => format!("{kind}-{id}"),
        None => format!("{kind}-{}", entry.unix_ms),
    }
}

/// A peer's name: a chat of this project, or another project's manager.
fn peer_name(party: &Party, names: &Names) -> String {
    match party {
        Party::Chat(chat) if chat == "manager" => names.manager.to_string(),
        Party::Chat(chat) => chat.clone(),
        Party::Project(project) => (names.peer)(project),
    }
}

/// The row of a message from the owner, a manager or a chat. Events have `event_rows`; a recap
/// request shows nothing.
fn message_row(entry: &ManagerEntry, from: &Author, text: &str, names: &Names) -> Option<ChatRow> {
    let key = row_key(entry, "message");
    let (peer, reply) = match from {
        Author::Owner => {
            return Some(ChatRow::said(
                key,
                entry,
                Speaker::Owner,
                "You".into(),
                text,
            ))
        }
        Author::Peer { from, reply } => (from, reply),
        Author::Events | Author::Recap => return None,
    };
    Some(ChatRow {
        tag: reply.then(|| "Reply".into()),
        ..ChatRow::said(
            key,
            entry,
            Speaker::Peer(peer.clone()),
            peer_name(peer, names),
            text,
        )
    })
}

/// The lines of an events message, one row each unless the line repeats the one before it.
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
                    format!("{}-{nth}", row_key(entry, "message")),
                    entry,
                    line.text,
                )
            });
        }
    }
}

/// The row of a manager reply. `approvals`: the ids of the pending approvals it carries.
fn reply_row(
    entry: &ManagerEntry,
    approvals: Vec<String>,
    recaps: &HashSet<u64>,
    asks: &[OpenAsk],
    names: &Names,
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
    // ^ A quiet reply is a muted line under the manager's name unless it carries an
    // approval; `-` or nothing says nothing changed and shows nothing.
    if *quiet && approvals.is_empty() {
        return (!says_nothing(text)).then(|| ChatRow {
            author: Some(Speaker::Manager),
            name: Some(names.own.to_string()),
            ..ChatRow::new(
                RowKind::Quiet,
                row_key(entry, "reply"),
                entry,
                text.trim().to_string(),
            )
        });
    }
    Some(ChatRow {
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
        ManagerEvent::Sent { to, text, reply } => Some(ChatRow {
            tag: Some(format!(
                "{} {}",
                if *reply { "Reply to" } else { "To" },
                peer_name(&Party::from_key(to), names)
            )),
            ..ChatRow::said(
                row_key(entry, "sent"),
                entry,
                Speaker::Manager,
                names.own.to_string(),
                text,
            )
        }),
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
