//! Threads: one subject inside a chat, with its tasks and its open questions. The chat's own
//! messages are the trunk. A thread's state is a fold over three events of the chat's log
//! (`ThreadOpen`, `ThreadClosed`, `ThreadMerged`); a message, a reply and a task carry the slug.
//! A row of the chat log carries the bare slug; a task, a memory record and a search record carry
//! the key `<chat>/<slug>`, since chat names are unique on the machine and those live in the
//! project. ref: docs/tasks.md "Threads", docs/design/chat-system.md
use super::chat::{asking, OpenAsk};
use super::chats::{approvals_of, checks_of, Viewer};
use super::checks::{self, CheckState};
use super::manager::{pending_approvals, ApprovalLine, ManagerEntry, ManagerEvent};
use super::store::{self, contains_bytes, ManagerDir, TaskDir, FIRST_READ_BYTES};
use super::view::{checked_title, clip, project_tasks, readable_title};
use super::{call_task_host, HostRequest};
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
    let task = checks::read(project_id)
        .ok()?
        .into_iter()
        .find(|found| found.id == item)?
        .task;
    of_task(project_id, task)
}

/// The key of the thread task `task` belongs to, none for a task of the trunk.
pub fn of_task(project_id: &str, task: u32) -> Option<String> {
    TaskDir::open(project_id, task)
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

/// The file whose presence says the open work of the chat was offered for placing once.
const PLACED_FILE: &str = "threads-placed";

/// The open work of a chat that is in no thread: what the one placing turn and `thread unplaced`
/// list. An approval and a To check item follow their task.
#[derive(Debug, Clone, Default)]
pub struct Unplaced {
    /// Open root tasks of the chat: number and title.
    pub tasks: Vec<(u32, String)>,
    /// Open questions: reply id and the line a quote of it carries.
    pub asks: Vec<(u64, String)>,
    /// Pending approvals: id, task (none: it has no task and stays in the trunk) and label.
    pub approvals: Vec<(String, Option<u32>, String)>,
    /// Open To check items: id, task and check.
    pub checks: Vec<(String, u32, String)>,
}

impl Unplaced {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
            && self.asks.is_empty()
            && self.approvals.is_empty()
            && self.checks.is_empty()
    }

    /// The most entries of one list in `text`; the rest is `wsx task thread unplaced`.
    const SHOWN: usize = 30;

    /// The one instruction of the placing turn, the same for every adapter: the ids of the open
    /// work, and the two commands that place it.
    pub fn text(&self) -> String {
        let mut lines = vec![
            "Threads are new in this chat: a thread is one subject with its tasks and its open \
             questions. Place the open work below once."
                .to_string(),
        ];
        let mut list = |label: &str, items: Vec<String>| {
            if items.is_empty() {
                return;
            }
            let more = items.len().saturating_sub(Self::SHOWN);
            let mut line = format!("{label}: {}", items[..items.len() - more].join("; "));
            if more > 0 {
                line.push_str(&format!("; and {more} more (`wsx task thread unplaced`)"));
            }
            lines.push(line);
        };
        list(
            "Open tasks",
            self.tasks
                .iter()
                .map(|(task, title)| format!("#{task} {title}"))
                .collect(),
        );
        list(
            "Open questions of the owner (reply id)",
            self.asks
                .iter()
                .map(|(id, quote)| format!("{id} {quote}"))
                .collect(),
        );
        list(
            "Approvals waiting",
            self.approvals
                .iter()
                .map(|(id, task, label)| match task {
                    Some(task) => format!("{id} {label} (moves with #{task})"),
                    None => format!("{id} {label} (no task: stays in the trunk)"),
                })
                .collect(),
        );
        list(
            "To check",
            self.checks
                .iter()
                .map(|(id, task, check)| format!("{id} {check} (moves with #{task})"))
                .collect(),
        );
        lines.push(
            "For each current subject run `wsx task thread open \"<subject>\"` (a few words; \
             one thread may hold several tasks and questions), then `wsx task thread move <slug> \
             <task>...` and `--ask <reply id>...`. An approval or To check item moves with its \
             task, which may be landed. Leave work that is closed or landed, with nothing open, \
             in the trunk. Then reply with exactly `-`."
                .to_string(),
        );
        lines.join("\n")
    }
}

/// The open work of `dir`'s chat that is in no thread. Closed and landed work with nothing
/// open is not listed.
pub fn unplaced(dir: &ManagerDir, now: u64) -> Result<Unplaced> {
    let viewer = Viewer::new(&dir.project_id, Some(&dir.chat));
    let tasks = project_tasks(&dir.project_id)?;
    let in_thread = |task: u32| {
        tasks.iter().any(|(_, record)| {
            record
                .as_ref()
                .is_ok_and(|r| r.id == task && r.thread.is_some())
        })
    };
    let mut found = Unplaced::default();
    for (task_dir, record) in &tasks {
        let Ok(record) = record else { continue };
        if record.state.is_open()
            && record.parent.is_none()
            && record.thread.is_none()
            && viewer.owns(record.created_by.chat())
        {
            let title = readable_title(task_dir, record).unwrap_or_default();
            found.tasks.push((record.id, clip(&title, 70)));
        }
    }
    for ask in asking(dir, now)?
        .into_iter()
        .filter(|ask| ask.threads.is_empty())
    {
        found.asks.push((ask.id, clip(&ask.quote, 70)));
    }
    let lines = approvals_of(store::read_approvals(&dir.project_id)?, &viewer);
    for line in pending_approvals(&lines) {
        if let ApprovalLine::Request { id, task, .. } = line {
            if !task.is_some_and(in_thread) {
                found
                    .approvals
                    .push((id.clone(), *task, super::owner::approval_label(line)));
            }
        }
    }
    for item in checks_of(&dir.project_id, checks::read(&dir.project_id)?, &viewer) {
        if item.state == CheckState::Open && !in_thread(item.task) {
            found
                .checks
                .push((item.id, item.task, clip(&item.check, 70)));
        }
    }
    Ok(found)
}

/// Place work in thread `slug`: tasks (with their open children in no thread) and open questions
/// (reply ids). Everything is checked before anything is written. A task of another chat, a
/// task that is not there and a reply that is no open question are refused.
pub fn place(
    dir: &ManagerDir,
    slug: &str,
    task_ids: &[u32],
    asks: &[u64],
    now: u64,
) -> Result<String> {
    let viewer = Viewer::new(&dir.project_id, Some(&dir.chat));
    let open = asking(dir, now)?;
    if let Some(id) = asks
        .iter()
        .find(|id| !open.iter().any(|ask| ask.id == **id))
    {
        bail!("reply {id} is not an open question");
    }
    let all = project_tasks(&dir.project_id)?;
    let mut moving: Vec<u32> = Vec::new();
    for id in task_ids {
        let owned = all.iter().any(|(_, record)| {
            record
                .as_ref()
                .is_ok_and(|r| r.id == *id && viewer.owns(r.created_by.chat()))
        });
        if !owned {
            bail!("task #{id} is not a task of chat {}", dir.chat);
        }
        moving.push(*id);
    }
    // ^ A child joins the thread of its parent (`new --into`): the open ones in no thread move too.
    while let Some(child) = all.iter().find_map(|(_, record)| {
        let record = record.as_ref().ok()?;
        (record.state.is_open()
            && record.thread.is_none()
            && !moving.contains(&record.id)
            && record.parent.is_some_and(|parent| moving.contains(&parent)))
        .then_some(record.id)
    }) {
        moving.push(child);
    }
    let thread = join(dir, slug)?;
    let key = key(&dir.chat, &thread);
    for id in &moving {
        let task_dir = TaskDir::open(&dir.project_id, *id)?;
        match call_task_host(
            &task_dir,
            &HostRequest::Thread {
                thread: key.clone(),
            },
            std::time::Duration::from_secs(15),
        ) {
            Ok(response) if response.ok => {}
            Ok(response) => bail!("task #{id} refused: {}", response.message),
            // ^ A task at rest has no host to ask; its record has no other writer.
            Err(error) => {
                let mut record = task_dir.read_record()?;
                if record.state.is_open() {
                    return Err(error);
                }
                record.thread = Some(key.clone());
                task_dir.write_record(&record)?;
            }
        }
    }
    if !asks.is_empty() {
        dir.append(ManagerEvent::ThreadMoved {
            ids: asks.to_vec(),
            thread: thread.clone(),
        })?;
    }
    Ok(thread)
}

/// Whether the chat's open work was not yet offered for placing.
pub fn placing_due(dir: &ManagerDir) -> bool {
    !dir.path.join(PLACED_FILE).exists()
}

/// Record that the open work was offered, so it is once.
pub fn placing_done(dir: &ManagerDir) -> Result<()> {
    std::fs::write(dir.path.join(PLACED_FILE), b"1\n")?;
    Ok(())
}
