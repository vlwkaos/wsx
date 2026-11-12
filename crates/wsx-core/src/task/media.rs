//! The media archive of a project: what was captured or posted, newest first, for the web's
//! Media sheet and the TUI's list, which only render it. ref: docs/tasks.md "Media"
//!
//! Three sources: the To check captures (`checks`), the pictures and videos that messages of the
//! manager, its chats and its peers link (`files/<path>`), and the files an owner message
//! attached (the web's media store, which this crate cannot read: an upload's item names the
//! store's file and the web says whether the store still has it).

use super::chat::linked_files;
use super::chats;
use super::checks::{self, CheckState};
use super::manager::{Author, ManagerEntry, ManagerEvent};
use super::store::{contains_bytes, ManagerDir, TaskDir, FIRST_READ_BYTES};
use anyhow::Result;
use serde::Serialize;
use std::path::{Component, Path};

/// The line that starts the list of attached files in an owner message (the web writes it).
pub const ATTACHMENT_HEADER: &str = "[attached files]";
/// Most items in one page.
pub const PAGE_ITEMS: usize = 24;
const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];
const VIDEO_EXTENSIONS: [&str; 2] = ["mp4", "webm"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Image,
    Video,
}

impl Kind {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            _ => None,
        }
    }

    /// The kind a file name's extension shows; none for any other type.
    pub fn of(name: &str) -> Option<Self> {
        let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
        if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Image)
        } else if VIDEO_EXTENSIONS.contains(&extension.as_str()) {
            Some(Self::Video)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A To check item's capture; `check` names the item.
    /// `path` is its name in the task folder (`captures/r1-1.mp4`).
    Capture,
    /// A file of the main checkout that a message links; `path` is relative to it.
    File,
    /// A file an owner message attached; `path` is the media store's `<sha256>.<ext>`.
    Upload,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    /// `c<n>` for a capture, `m<message>.<n>` for the n-th file of a message.
    pub id: String,
    pub source: Source,
    pub kind: Kind,
    /// When the capture was collected or the message written.
    pub unix_ms: u64,
    pub task: Option<u32>,
    /// Who posted it: `owner`, `manager`, a chat or a peer project; `checks` for a capture.
    pub who: String,
    /// The check's text, the file's path or the upload's name.
    pub text: String,
    pub verdict: Option<CheckState>,
    pub check: Option<String>,
    pub path: Option<String>,
    /// A capture has a poster image.
    pub poster: bool,
    /// The file is gone: a capture the budget removed, or a file no longer in the checkout. An
    /// upload's is decided by the web.
    pub gone: bool,
}

impl Item {
    /// `<unix_ms>:<id>`, where the next page starts after this item.
    pub fn cursor(&self) -> String {
        format!("{}:{}", self.unix_ms, self.id)
    }
}

#[derive(Debug, Default)]
pub struct Filter {
    pub task: Option<u32>,
    pub kind: Option<Kind>,
    /// A previous page's `next`: only older items.
    pub before: Option<String>,
    pub limit: usize,
    /// Also the owner's uploads: only a reader of the web's media store can say whether one is
    /// still there.
    pub with_uploads: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Page {
    pub items: Vec<Item>,
    /// The cursor of the next page; none at the end.
    pub next: Option<String>,
}

/// One page of the project's media, newest first. `checkout` is the main checkout, for the
/// files that messages link.
pub fn list(project_id: &str, filter: &Filter, checkout: Option<&Path>) -> Result<Page> {
    let limit = if filter.limit == 0 {
        PAGE_ITEMS
    } else {
        filter.limit
    };
    let before = filter.before.as_deref().and_then(parse_cursor);
    let older = |item: &Item| {
        before
            .as_ref()
            .is_none_or(|at| key(item) < (at.0, at.1.as_str()))
    };
    let mut items = capture_items(project_id, filter)?;
    // ^ A message has no task: a task filter leaves the logs unread.
    if filter.task.is_none() {
        let first = ManagerDir::existing(project_id)?;
        for dir in first.into_iter().chain(chats::all(project_id)) {
            let items_of = |entries: &[ManagerEntry]| -> Vec<Item> {
                entries
                    .iter()
                    .flat_map(|entry| entry_items(entry, dir.name(), checkout, filter.with_uploads))
                    .filter(|item| filter.kind.is_none_or(|kind| kind == item.kind))
                    .collect()
            };
            // ^ Newest first: a log read back until `limit` + 1 items older than the cursor are
            // in has every item that can be in the page or show that another page follows.
            let entries = dir.read_all_where(
                FIRST_READ_BYTES,
                |line| {
                    contains_bytes(line, b"files/")
                        || contains_bytes(line, ATTACHMENT_HEADER.as_bytes())
                },
                |entries| items_of(entries).iter().filter(|item| older(item)).count() > limit,
            )?;
            items.extend(items_of(&entries));
        }
    }
    items.retain(&older);
    items.sort_by(|a, b| key(b).cmp(&key(a)));
    let next = (items.len() > limit).then(|| items[limit - 1].cursor());
    items.truncate(limit);
    Ok(Page { items, next })
}

fn key(item: &Item) -> (u64, &str) {
    (item.unix_ms, item.id.as_str())
}

fn parse_cursor(cursor: &str) -> Option<(u64, String)> {
    let (ms, id) = cursor.split_once(':')?;
    Some((ms.parse().ok()?, id.to_string()))
}

fn capture_items(project_id: &str, filter: &Filter) -> Result<Vec<Item>> {
    let mut found = Vec::new();
    for item in checks::read(project_id)? {
        let Some(capture) = item.capture.as_deref() else {
            continue;
        };
        let Some(kind) = Kind::of(capture) else {
            continue;
        };
        if filter.task.is_some_and(|task| task != item.task)
            || filter.kind.is_some_and(|wanted| wanted != kind)
        {
            continue;
        }
        let gone = TaskDir::open(project_id, item.task)
            .map_or(true, |dir| checks::capture_removed(&dir, &item));
        found.push(Item {
            id: item.id.clone(),
            source: Source::Capture,
            kind,
            unix_ms: item.added_unix_ms,
            task: Some(item.task),
            who: "checks".into(),
            text: item.check.clone(),
            verdict: Some(item.state),
            check: Some(item.id.clone()),
            path: Some(capture.to_string()),
            poster: item.poster.is_some(),
            gone,
        });
    }
    Ok(found)
}

/// The files one log entry posts: its links, and the uploads of an owner message.
fn entry_items(
    entry: &ManagerEntry,
    dir_name: &str,
    checkout: Option<&Path>,
    with_uploads: bool,
) -> Vec<Item> {
    let (id, who, text, uploads) = match &entry.event {
        ManagerEvent::Reply { id, text, .. } => (*id, dir_name.to_string(), text, false),
        ManagerEvent::Message { id, from, text, .. } => match from {
            Author::Owner => (*id, "owner".to_string(), text, with_uploads),
            Author::Peer { from, .. } => (*id, from.name().to_string(), text, false),
            Author::Events | Author::Recap => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let item = |n: usize, source, kind, text: &str, path: &str, gone| Item {
        id: format!("m{id}.{n}"),
        source,
        kind,
        unix_ms: entry.unix_ms,
        task: None,
        who: who.clone(),
        text: text.to_string(),
        verdict: None,
        check: None,
        path: Some(path.to_string()),
        poster: false,
        gone,
    };
    let mut found = Vec::new();
    for path in linked_files(text) {
        // ^ A path off the checkout (`../`) is no file of the project: left out, not "removed".
        let Some(kind) = Kind::of(&path).filter(|_| plain_path(&path)) else {
            continue;
        };
        let gone = checkout.is_some_and(|root| !in_checkout(root, &path));
        found.push(item(found.len(), Source::File, kind, &path, &path, gone));
    }
    if uploads {
        for (file, name) in attached_files(text) {
            let Some(kind) = Kind::of(&file) else {
                continue;
            };
            found.push(item(found.len(), Source::Upload, kind, &name, &file, false));
        }
    }
    found
}

fn plain_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
}

/// Whether `path` names a file inside `root` (no symlink out: only the answer is read, never
/// the file).
fn in_checkout(root: &Path, path: &str) -> bool {
    root.join(path)
        .canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(file, root)| file.starts_with(root) && file.is_file())
}

/// The media store's `<sha256>.<ext>` and the name of each attachment line of an owner message
/// (`path (name)`, or `copy (name: summary) original: path (open it only for ...)` with a
/// compact copy; the web's `Media::prompt_line`).
pub fn attached_files(text: &str) -> Vec<(String, String)> {
    let Some((_, list)) = text.rsplit_once(&format!("\n\n{ATTACHMENT_HEADER}\n")) else {
        return Vec::new();
    };
    list.lines().filter_map(attachment).collect()
}

fn attachment(line: &str) -> Option<(String, String)> {
    let original = line.split_once(") original: ");
    let path = original.map_or(line, |(_, rest)| rest);
    let path = path.split_once(" (").map_or(path, |(path, _)| path);
    let file = path.rsplit('/').next()?;
    let (hash, extension) = file.split_once('.')?;
    let valid = hash.len() == 64
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && !extension.is_empty()
        && extension.bytes().all(|b| b.is_ascii_alphanumeric());
    if !valid {
        return None;
    }
    let first = original.map_or(line, |(first, _)| first);
    let name = first
        .split_once(" (")
        .map(|(_, rest)| match original {
            Some(_) => rest.split(": ").next().unwrap_or(rest),
            None => rest.strip_suffix(')').unwrap_or(rest),
        })
        .unwrap_or(file);
    Some((file.to_string(), name.to_string()))
}
