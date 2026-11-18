//! Threads: one subject inside a chat, with its tasks and its open questions. The chat's own
//! messages are the trunk. A thread's state is a fold over three events of the chat's log
//! (`ThreadOpen`, `ThreadClosed`, `ThreadMerged`); a message, a reply and a task carry the slug.
//! A row of the chat log carries the bare slug; a task, a memory record and a search record carry
//! the key `<chat>/<slug>`, since chat names are unique on the machine and those live in the
//! project. ref: docs/tasks.md "Threads", docs/design/chat-system.md
use super::chat::{asking, OpenAsk};
use super::manager::{ManagerEntry, ManagerEvent};
use super::store::{contains_bytes, ManagerDir, FIRST_READ_BYTES};
use super::view::checked_title;
use anyhow::{bail, Result};
use std::collections::BTreeMap;

/// The longest slug: the title's words, cut at a word.
const MAX_SLUG_CHARS: usize = 40;

/// What the log says about one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub slug: String,
    pub title: String,
    pub opened_ms: u64,
    pub open: bool,
    /// Set when the thread was merged: its rows and tasks read as this thread's.
    pub merged_into: Option<String>,
}

/// The threads of one chat, from its log.
#[derive(Debug, Clone, Default)]
pub struct Threads(BTreeMap<String, Entry>);

impl Threads {
    /// The fold of the thread events of `log`, in order. Opening a closed thread again reopens it.
    pub fn fold(log: &[ManagerEntry]) -> Self {
        let mut threads = BTreeMap::<String, Entry>::new();
        for entry in log {
            match &entry.event {
                ManagerEvent::ThreadOpen { thread, title, .. } => {
                    threads
                        .entry(thread.clone())
                        .and_modify(|found| found.open = true)
                        .or_insert_with(|| Entry {
                            slug: thread.clone(),
                            title: title.clone(),
                            opened_ms: entry.unix_ms,
                            open: true,
                            merged_into: None,
                        });
                }
                ManagerEvent::ThreadClosed { thread, .. } => {
                    if let Some(found) = threads.get_mut(thread) {
                        found.open = false;
                    }
                }
                ManagerEvent::ThreadMerged { thread, into, .. } => {
                    if let Some(found) = threads.get_mut(thread) {
                        found.open = false;
                        found.merged_into = Some(into.clone());
                    }
                }
                _ => {}
            }
        }
        Self(threads)
    }

    /// The thread events of the chat's whole log, which rolls at 8 MiB: a thread opened in an
    /// older file stays known. Only the lines of those events are parsed.
    pub fn read(dir: &ManagerDir) -> Result<Self> {
        let log = dir.read_all_where(
            FIRST_READ_BYTES,
            |line| contains_bytes(&line[..line.len().min(64)], br#""kind":"thread_"#),
            |_| false,
        )?;
        Ok(Self::fold(&log))
    }

    pub fn get(&self, slug: &str) -> Option<&Entry> {
        self.0.get(slug)
    }

    /// The thread a slug reads as: a merged thread reads as the one it went into.
    pub fn resolve<'a>(&'a self, slug: &'a str) -> &'a str {
        let mut at = slug;
        for _ in 0..self.0.len() {
            match self
                .0
                .get(at)
                .and_then(|found| found.merged_into.as_deref())
            {
                Some(into) => at = into,
                None => break,
            }
        }
        at
    }

    /// The open threads, oldest first.
    pub fn open_list(&self) -> Vec<&Entry> {
        let mut open: Vec<&Entry> = self.0.values().filter(|found| found.open).collect();
        open.sort_by_key(|found| found.opened_ms);
        open
    }

    /// Every thread: the open ones first, oldest first, then the others.
    pub fn listed(&self) -> Vec<&Entry> {
        let mut all: Vec<&Entry> = self.0.values().collect();
        all.sort_by_key(|found| (!found.open, found.opened_ms));
        all
    }

    fn open_count(&self) -> u32 {
        self.0.values().filter(|found| found.open).count() as u32
    }

    /// The entry of `slug` after merges, or the reason there is none.
    fn known(&self, slug: &str) -> Result<&Entry> {
        let slug = slug.trim();
        match self.0.get(self.resolve(slug)) {
            Some(found) => Ok(found),
            None => bail!("there is no thread {slug}; open threads: {}", self.names()),
        }
    }

    /// The open threads as the refusals name them.
    fn names(&self) -> String {
        let open = self.open_list();
        match open.is_empty() {
            true => "none".into(),
            false => open
                .iter()
                .map(|found| format!("{} \"{}\"", found.slug, found.title))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

/// `<chat>/<slug>`: how a task, a record and a search record name a thread.
pub fn key(chat: &str, slug: &str) -> String {
    format!("{chat}/{slug}")
}

/// The chat and slug of a key.
pub fn split_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('/')
        .filter(|(chat, slug)| !chat.is_empty() && !slug.is_empty())
}

/// The slug of a key that names a thread of `chat`; none for a thread of another chat.
pub fn slug_in<'a>(chat: &str, key: &'a str) -> Option<&'a str> {
    split_key(key)
        .filter(|(of, _)| *of == chat)
        .map(|(_, slug)| slug)
}

/// A slug from a title: its words in lowercase, joined by `-`, at most `MAX_SLUG_CHARS`, and
/// not one of `taken`.
fn slug_of(title: &str, taken: &Threads) -> String {
    let mut slug = String::new();
    for word in title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let joined = match slug.is_empty() {
            true => word.to_string(),
            false => format!("{slug}-{word}"),
        };
        if joined.chars().count() > MAX_SLUG_CHARS {
            break;
        }
        slug = joined;
    }
    if slug.is_empty() {
        slug = "thread".into();
    }
    let base = slug.clone();
    let mut n = 1;
    while taken.0.contains_key(&slug) {
        n += 1;
        slug = format!("{base}-{n}");
    }
    slug
}

/// Open a thread in the chat and return its slug.
pub fn open(dir: &ManagerDir, title: &str) -> Result<String> {
    let title = checked_title(title)?;
    let threads = Threads::read(dir)?;
    let thread = slug_of(&title, &threads);
    dir.append(ManagerEvent::ThreadOpen {
        thread: thread.clone(),
        title,
        open: threads.open_count() + 1,
    })?;
    Ok(thread)
}

/// Close a thread by hand, with `why`. Refused while a question of the thread waits for the owner.
pub fn close(dir: &ManagerDir, slug: &str, why: &str, now: u64) -> Result<String> {
    let threads = Threads::read(dir)?;
    let found = threads.known(slug)?;
    let thread = found.slug.clone();
    if !found.open {
        return Ok(thread);
    }
    let asking: Vec<u64> = asking(dir, now)?
        .iter()
        .filter(|ask| asks_in(&threads, ask, &thread))
        .map(|ask| ask.id)
        .collect();
    if !asking.is_empty() {
        bail!(
            "thread {thread} has {} open question{} of the owner (reply {}); it stays open until they are answered",
            asking.len(),
            if asking.len() == 1 { "" } else { "s" },
            asking.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
        );
    }
    dir.append(ManagerEvent::ThreadClosed {
        thread: thread.clone(),
        why: why.trim().to_string(),
        open: threads.open_count() - 1,
    })?;
    Ok(thread)
}

/// The thread a message joins: `slug` after merges. A closed thread opens again.
pub fn join(dir: &ManagerDir, slug: &str) -> Result<String> {
    let threads = Threads::read(dir)?;
    let found = threads.known(slug)?;
    if !found.open {
        dir.append(ManagerEvent::ThreadOpen {
            thread: found.slug.clone(),
            title: found.title.clone(),
            open: threads.open_count() + 1,
        })?;
    }
    Ok(found.slug.clone())
}

/// Merge thread `from` into the open thread `into`: nothing is rewritten, readers map one to the other.
pub fn merge(dir: &ManagerDir, from: &str, into: &str) -> Result<()> {
    let threads = Threads::read(dir)?;
    let (from, into) = (threads.known(from)?, threads.known(into)?);
    if from.slug == into.slug {
        bail!("{} is one thread: nothing to merge", from.slug);
    }
    if !into.open {
        bail!(
            "thread {} is closed; merge into an open thread: {}",
            into.slug,
            threads.names()
        );
    }
    dir.append(ManagerEvent::ThreadMerged {
        thread: from.slug.clone(),
        into: into.slug.clone(),
        open: threads.open_count() - u32::from(from.open),
    })
}

/// Whether the open question reads as one of thread `slug`.
fn asks_in(threads: &Threads, ask: &OpenAsk, slug: &str) -> bool {
    ask.threads
        .iter()
        .any(|thread| threads.resolve(thread) == slug)
}

/// The threads an entry of the chat log carries: a message's one, a reply's several.
pub fn row_threads(entry: &ManagerEntry) -> Vec<&str> {
    match &entry.event {
        ManagerEvent::Message { thread, .. } => thread.as_deref().into_iter().collect(),
        ManagerEvent::Reply { threads, .. } => threads.iter().map(String::as_str).collect(),
        _ => Vec::new(),
    }
}

/// The thread of an owner's message (`to` is none) or a reply, by the rules in this order: the
/// thread of the row it quotes, the thread of the window it was written in, the thread of the
/// task its To check item belongs to; none is the trunk. `log` holds the rows up to now.
/// No model reads the message.
pub fn tag(
    log: &[ManagerEntry],
    quote: Option<u64>,
    window: Option<&str>,
    item: Option<&str>,
) -> Option<String> {
    let quoted = quote.and_then(|id| {
        log.iter()
            .find(|entry| entry.id() == Some(id))
            .and_then(|entry| row_threads(entry).first().map(|slug| slug.to_string()))
    });
    quoted
        .or_else(|| window.map(str::to_string))
        .or_else(|| item.map(str::to_string))
}

/// The threads a reply to messages `to` carries: those of the messages, each once.
pub fn reply_threads(log: &[ManagerEntry], to: &[u64]) -> Vec<String> {
    let mut threads: Vec<String> = Vec::new();
    for entry in log
        .iter()
        .filter(|entry| entry.id().is_some_and(|id| to.contains(&id)))
    {
        for slug in row_threads(entry) {
            if !threads.iter().any(|seen| seen == slug) {
                threads.push(slug.to_string());
            }
        }
    }
    threads
}

/// The key of the thread the task of To check item `item` belongs to.
pub fn of_item(project_id: &str, item: &str) -> Option<String> {
    let task = super::checks::read(project_id)
        .ok()?
        .into_iter()
        .find(|found| found.id == item)?
        .task;
    of_task(project_id, task)
}

/// The key of the thread task `task` belongs to, none for a task of the trunk.
pub fn of_task(project_id: &str, task: u32) -> Option<String> {
    super::store::TaskDir::open(project_id, task)
        .ok()?
        .read_record()
        .ok()?
        .thread
}

/// A search's `--thread`: one thread of one chat. A key of a thread merged into it matches too.
pub struct Filter {
    chat: String,
    slug: String,
    threads: Threads,
}

impl Filter {
    /// `spec` is `<chat>/<slug>`, or a bare slug that exactly one chat of the repo has.
    pub fn new(project_id: &str, spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let dirs = match split_key(spec) {
            Some((chat, _)) => ManagerDir::existing(chat)?.into_iter().collect(),
            None => super::chats::all(project_id),
        };
        let slug = split_key(spec).map_or(spec, |(_, slug)| slug);
        let mut found = Vec::new();
        for dir in dirs {
            let threads = Threads::read(&dir)?;
            if threads.get(slug).is_some() {
                found.push((dir.chat.clone(), threads));
            }
        }
        match found.len() {
            1 => {
                let (chat, threads) = found.remove(0);
                Ok(Self {
                    slug: threads.resolve(slug).to_string(),
                    chat,
                    threads,
                })
            }
            0 => bail!("there is no thread {spec}; `wsx task thread list` names a chat's threads"),
            _ => bail!(
                "thread {spec} is in {} chats; name one: {}",
                found.len(),
                found
                    .iter()
                    .map(|(chat, _)| key(chat, slug))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Whether a doc or record that carries `keys` belongs to the thread.
    pub fn matches<S: AsRef<str>>(&self, keys: &[S]) -> bool {
        keys.iter().any(|found| {
            slug_in(&self.chat, found.as_ref())
                .is_some_and(|slug| self.threads.resolve(slug) == self.slug)
        })
    }
}
