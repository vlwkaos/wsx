//! The TUI's manager chat of one project, or a role's (docs/tasks.md "Roles"): the
//! conversation, the open To check items and the waiting approvals, read off the UI thread when a file of the task store changes, and the
//! owner's message and decisions, also run off it. ref: docs/tasks.md "TUI"
use crate::{
    action::Action,
    ui::{input::InputState, text_form::dispatch_text_edit},
};
use ratatui::text::Line;
use std::{
    cell::{Cell, RefCell},
    io::Read as _,
    ops::Range,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};
use wsx_core::config::project::load_task_config;
use wsx_core::task::{
    call_socket,
    chat::{self, ChatRow, Names, Page, RowKind, Window},
    checks::{self, CheckItem, CheckState},
    manager::{activity, pending_approvals, ManagerRequest, ManagerResponse},
    owner,
    store::{self, ManagerDir, Stamp},
    view,
};

const CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// The host answers its status at once; a slow answer counts as not running.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// What a link of the chat opens (docs/tasks.md "Session news", "Routines"): a session, as `S`
/// enters one, or a file of the main checkout in the chat's file view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session(u64),
    File(String),
    /// A quote at the head of a message (`split_quote`): it goes to the quoted message, by its
    /// id when the log stored it (`ChatRow::quoted_id`), else the newest one above with that
    /// first line.
    Quote {
        line: String,
        id: Option<u64>,
    },
}

/// A piece of a message: plain words, or a link `[label](session/<id>)` / `[label](files/<path>)`.
/// An image `![alt](files/<path>)` is a link whose label is its address: the TUI shows no
/// picture. Any other link stays plain words as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    Link { label: &'a str, target: Target },
}

fn target_of(href: &str) -> Option<Target> {
    if let Some(id) = href.strip_prefix("session/") {
        return id.parse().ok().map(Target::Session);
    }
    chat::file_href(href).map(Target::File)
}

/// `text` as plain words and links, in order.
pub fn parts(text: &str) -> Vec<Part<'_>> {
    let (mut out, mut plain, mut from) = (Vec::new(), 0, 0);
    while let Some(open) = text[from..].find('[').map(|at| from + at) {
        from = open + 1;
        let Some(close) = text[from..].find(']').map(|at| from + at) else {
            break;
        };
        let Some(href) = text[close + 1..].strip_prefix('(') else {
            continue;
        };
        let Some(end) = href.find(')') else {
            continue;
        };
        let image = open > 0 && text.as_bytes()[open - 1] == b'!';
        let label = if image {
            &href[..end]
        } else {
            &text[from..close]
        };
        let target = target_of(&href[..end]);
        match target {
            Some(target) if !label.is_empty() && !text[from..close].contains(['[', '\n']) => {
                let lead = open - usize::from(image);
                if plain < lead {
                    out.push(Part::Text(&text[plain..lead]));
                }
                out.push(Part::Link { label, target });
                plain = close + 2 + end + 1;
                from = plain;
            }
            _ => {}
        }
    }
    if plain < text.len() {
        out.push(Part::Text(&text[plain..]));
    }
    out
}

/// The links of the conversation, oldest first: the message's time, the link's place in it and
/// what it opens. The time and place name a link while rows come and go.
pub fn links(rows: &[ChatRow]) -> Vec<(u64, usize, Target)> {
    let mut found = Vec::new();
    // ^ A queued message shows as one waiting line: it has no links.
    for row in rows
        .iter()
        .filter(|row| row.kind == RowKind::Message && !row.queued)
    {
        // ^ The quote is link 0 of its message; `chat_lines` counts the same way.
        let quoted = row.quoted.clone().map(|line| Target::Quote {
            line,
            id: row.quoted_id,
        });
        let targets = parts(&row.text).into_iter().filter_map(|part| match part {
            Part::Link { target, .. } => Some(target),
            Part::Text(_) => None,
        });
        found.extend(
            quoted
                .into_iter()
                .chain(targets)
                .enumerate()
                .map(|(nth, target)| (row.unix_ms, nth, target)),
        );
    }
    found
}

/// A file shown in place of the chat, from its top.
pub struct FileView {
    pub name: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    /// ^ The furthest scroll the last frame could show.
    pub max_scroll: Cell<usize>,
}

/// The most a file view reads.
const FILE_BYTES: u64 = 256 * 1024;

/// The lines of `relative`, a file of `main` (never under `.git`, none through a link out of it),
/// at most `FILE_BYTES` of it.
fn read_file(main: &Path, relative: &str) -> Result<Vec<String>, String> {
    let path = Path::new(relative);
    let normal = path
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"));
    if !normal {
        return Err("not a file of the project".into());
    }
    let root = main.canonicalize().map_err(|error| error.to_string())?;
    let real = root
        .join(path)
        .canonicalize()
        .map_err(|_| "no such file".to_string())?;
    if !real.starts_with(&root) || !real.is_file() {
        return Err("not a file of the project".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&real)
        .and_then(|file| file.take(FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| error.to_string())?;
    if bytes.contains(&0) {
        return Err("a binary file".into());
    }
    let cut = bytes.len() as u64 > FILE_BYTES;
    bytes.truncate(FILE_BYTES as usize);
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes)
        .lines()
        .map(|line| line.replace('\t', "    "))
        .collect();
    if cut {
        lines.push("… cut at 256 KiB".into());
    }
    Ok(lines)
}

/// What the owner decides: an open To check item, a waiting approval or an open question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Check(CheckItem),
    Approval {
        id: String,
        label: String,
    },
    /// A reply that asks the owner (`chat::open_asks`); `key` is `ask-<id>`.
    Ask {
        id: u64,
        key: String,
        quote: String,
    },
}

impl Item {
    fn id(&self) -> &str {
        match self {
            Self::Check(item) => &item.id,
            Self::Approval { id, .. } => id,
            Self::Ask { key, .. } => key,
        }
    }
}

/// Where keys go: the chat, the message line, the find line, a fail's note, or a y/N question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Chat,
    Compose,
    Find,
    /// A file of a link, in place of the chat.
    File,
    Fail(String),
    Confirm {
        id: String,
        label: String,
        approve: bool,
    },
}

struct View {
    rows: Vec<ChatRow>,
    /// The other side's shown name: the manager's (`manager_shown_name`), or the role's.
    name: String,
    items: Vec<Item>,
    activity: Option<String>,
    /// The manager's agent in use, as `view::agent_label` writes it; none for a role.
    agent: Option<String>,
}

/// A check's answer: the host's status, and the store read again when a stamp changed.
struct Read {
    /// The find the view was read for (`ManagerChat::searched`).
    searched: String,
    /// The message the view was read around (`ManagerChat::around`).
    around: Option<u64>,
    status: Option<ManagerResponse>,
    view: Option<(Stamp, Result<View, String>)>,
}

pub struct ManagerChat {
    pub project: String,
    /// The role this chat talks to; none is the manager.
    pub role: Option<String>,
    main: PathBuf,
    store: String,
    pub rows: Vec<ChatRow>,
    /// The other side's shown name, read with the rows.
    name: String,
    pub items: Vec<Item>,
    pub selected: usize,
    pub loaded: bool,
    pub running: bool,
    pub busy: bool,
    pub activity: Option<String>,
    pub agent: Option<String>,
    pub error: Option<String>,
    /// What the last message or decision answered; a failure stays until the next one.
    pub notice: Option<(String, bool)>,
    /// Lines up from the newest; 0 follows new lines.
    pub scroll: usize,
    /// ^ The furthest scroll the last frame could show, so `k` stops at the oldest line.
    pub max_scroll: Cell<usize>,
    pub focus: Focus,
    /// The selected link, as `links` names it; Enter opens it.
    pub link: Option<(u64, usize)>,
    /// A session the owner opened by its link: the app enters it and clears this.
    pub session: Option<u64>,
    pub file: Option<FileView>,
    pub composer: InputState,
    pub note: InputState,
    /// What `/` finds in the conversation, case-insensitive; empty when no find is on.
    pub find: InputState,
    /// The current match, counted from the oldest; a new query starts at the newest.
    pub found: usize,
    /// The chat's width and height in the last frame: a key finds matches as they are drawn.
    pub area: Cell<(usize, u16)>,
    /// ^ The find the chat reads for: from the oldest message that shows it, so a find reaches
    /// every match (`Window::Find`, docs/tasks.md "TUI"); empty is the newest part.
    searched: String,
    /// The message a quote opened the chat at: the rows are the part of the log around it
    /// (`Window::Around`) until Esc reads the newest part again.
    pub around: Option<u64>,
    /// The row to scroll to once the read for `around` is in.
    jump_due: bool,
    /// The current match is still to be scrolled to: the log was not read for it yet.
    reveal_due: bool,
    /// The quoted message the chat went to: its heading is reversed.
    marked: Option<u64>,
    /// ^ The rows wrapped for the last width and link, until the rows change; the draw, a key
    /// and the find's count all read it, so a frame wraps nothing new.
    wrapped: RefCell<Option<Wrapped>>,
    /// The question a message answers (`r` on an open question): its line and id, sent as the
    /// quote (`> message <id>`).
    reply: Option<(String, u64)>,
    /// What runs now ("Sending", "Failing c3"), until its answer comes.
    pub acting: Option<String>,
    stamp: Option<Stamp>,
    pending: Option<mpsc::Receiver<Read>>,
    answer: Option<mpsc::Receiver<Answer>>,
    last_check: Instant,
}

/// The chat's lines at one width with one link selected, the line of that link, and the
/// matches of the last query.
type Found = Arc<Vec<(usize, Range<usize>)>>;

struct Wrapped {
    width: usize,
    link: Option<(u64, usize)>,
    marked: Option<u64>,
    lines: Arc<Vec<Line<'static>>>,
    selected: Option<usize>,
    found: Option<(String, Found)>,
}

/// An action's words, whether it failed, and a sent message to give back to the composer.
type Answer = (String, bool, Option<String>);

impl ManagerChat {
    pub fn new(project: String, main: PathBuf, role: Option<String>) -> Self {
        let store = store::project_id(&project, &main);
        // ^ The shown name before the first read, which reads it again.
        let name = role.clone().unwrap_or_else(|| {
            load_task_config(&main)
                .unwrap_or_default()
                .manager_shown_name(&project)
                .to_string()
        });
        let mut chat = Self {
            name,
            project,
            role,
            main,
            store,
            rows: Vec::new(),
            items: Vec::new(),
            selected: 0,
            loaded: false,
            running: false,
            busy: false,
            activity: None,
            agent: None,
            error: None,
            notice: None,
            scroll: 0,
            max_scroll: Cell::new(0),
            focus: Focus::Chat,
            link: None,
            session: None,
            file: None,
            composer: InputState::new("> "),
            note: InputState::new(""),
            find: InputState::new("/"),
            found: 0,
            area: Cell::new((0, 0)),
            searched: String::new(),
            around: None,
            jump_due: false,
            reply: None,
            reveal_due: false,
            marked: None,
            wrapped: RefCell::new(None),
            acting: None,
            stamp: None,
            pending: None,
            answer: None,
            last_check: Instant::now(),
        };
        chat.check();
        chat
    }

    fn check(&mut self) {
        let (tx, rx) = mpsc::sync_channel(1);
        let (store, last, role) = (self.store.clone(), self.stamp.clone(), self.role.clone());
        let (searched, around) = (self.searched.clone(), self.around);
        let (main, project) = (self.main.clone(), self.project.clone());
        self.pending = Some(rx);
        self.last_check = Instant::now();
        std::thread::spawn(move || {
            let dir = ManagerDir::existing_as(&store, role.as_deref())
                .ok()
                .flatten();
            let status = dir.as_ref().and_then(|dir| {
                call_socket::<_, ManagerResponse>(
                    &dir.socket_path(),
                    &ManagerRequest::Status,
                    STATUS_TIMEOUT,
                )
                .ok()
            });
            let now = store::stamp(&store);
            let view = (last.as_ref() != Some(&now)).then(|| {
                let view = read(&project, &store, &main, dir.as_ref(), (&searched, around))
                    .map_err(|error| format!("{error:#}"));
                (now, view)
            });
            let _ = tx.send(Read {
                searched,
                around,
                status,
                view,
            });
        });
    }

    /// Take a finished read or action and start the next check when due. True when the
    /// view changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.answer {
            if let Ok((words, failed, unsent)) = rx.try_recv() {
                self.answer = None;
                self.acting = None;
                self.notice = Some((words, failed));
                // ^ A message that did not go keeps its words for another try.
                if let Some(text) = unsent.filter(|_| self.composer.value().is_empty()) {
                    self.composer = InputState::with_value("> ", text);
                }
                // ^ Read again at once: the action wrote the store.
                self.stamp = None;
                self.last_check = Instant::now()
                    .checked_sub(CHECK_INTERVAL)
                    .unwrap_or(self.last_check);
                changed = true;
            }
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(read) => {
                    self.pending = None;
                    changed |= self.apply(read);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
            }
        } else if self.last_check.elapsed() >= CHECK_INTERVAL {
            self.check();
        }
        changed
    }

    fn apply(&mut self, read: Read) -> bool {
        let (running, busy) = (
            read.status.is_some(),
            read.status.as_ref().is_some_and(|status| status.busy),
        );
        let mut changed = (running, busy) != (self.running, self.busy);
        (self.running, self.busy) = (running, busy);
        match read.view {
            None => {}
            Some((stamp, Ok(view))) => {
                let selected = self
                    .items
                    .get(self.selected)
                    .map(|item| item.id().to_string());
                self.items = view.items;
                // ^ The selection follows its item, not its position.
                self.selected = selected
                    .and_then(|id| self.items.iter().position(|item| item.id() == id))
                    .unwrap_or(self.selected)
                    .min(self.items.len().saturating_sub(1));
                if view.rows != self.rows {
                    self.rows = view.rows;
                    self.wrapped.take();
                }
                self.name = view.name;
                if self.reveal_due {
                    self.reveal();
                }
                if self.jump_due && read.around == self.around {
                    self.arrive();
                }
                self.activity = view.activity;
                self.agent = view.agent;
                // ^ A read for another find: read again.
                self.stamp =
                    (read.searched == self.searched && read.around == self.around).then_some(stamp);
                self.loaded = true;
                self.error = None;
                changed = true;
            }
            Some((_, Err(error))) => {
                self.error = Some(error);
                changed = true;
            }
        }
        changed
    }

    /// The name the chat shows for the other side: the role's, or the manager's shown name.
    pub fn who(&self) -> &str {
        &self.name
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }

    pub fn select(&mut self, delta: isize) {
        let count = self.items.len();
        if count > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        self.scroll = self
            .scroll
            .saturating_add_signed(delta as isize)
            .min(self.max_scroll.get());
    }

    /// Run `act` off the UI thread unless an action runs; its answer comes through `poll`.
    fn run(
        &mut self,
        doing: String,
        act: impl FnOnce(String, PathBuf, String) -> Answer + Send + 'static,
    ) {
        if self.acting.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let (project, main, store) = (self.project.clone(), self.main.clone(), self.store.clone());
        self.acting = Some(doing);
        self.notice = None;
        self.answer = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(act(project, main, store));
        });
    }

    /// Send the composer's text as `wsx task say` does, starting the manager when needed.
    pub fn send(&mut self) {
        let mut text = self.composer.value().trim().to_string();
        if text.is_empty() || self.acting.is_some() {
            return;
        }
        // ^ The owner's quote format (`split_quote`): the web's reply writes the same.
        if let Some((_, id)) = self.reply.take() {
            text = format!("> message {id}\n\n{text}");
        }
        self.newest();
        self.composer = InputState::new("> ");
        self.scroll = 0;
        let role = self.role.clone();
        self.run("Sending".into(), move |project, main, _| {
            let said = std::env::current_exe()
                .map_err(anyhow::Error::from)
                .and_then(|exe| {
                    owner::say(
                        &project,
                        &main,
                        &exe,
                        role.as_deref(),
                        crate::task::launch_manager(&main),
                        text.clone(),
                        None,
                        Vec::new(),
                    )
                });
            match said {
                Ok(response) if response.ok => ("Sent".into(), false, None),
                Ok(response) => (
                    format!("The manager refused it: {}", response.message),
                    true,
                    Some(text),
                ),
                Err(error) => (format!("Not sent: {error:#}"), true, Some(text)),
            }
        });
    }

    /// Pass or skip the selected To check item.
    pub fn close_check(&mut self, state: CheckState) {
        let Some(Item::Check(item)) = self.selected_item() else {
            return;
        };
        let id = item.id.clone();
        let verb = checks::state_name(state);
        self.run(
            format!("Marking {id} {verb}"),
            move |_, _, store| match checks::close(&store, std::slice::from_ref(&id), state, None) {
                Ok(_) => (format!("{id} {verb}"), false, None),
                Err(error) => (format!("{id} not {verb}: {error:#}"), true, None),
            },
        );
    }

    /// Dismiss the open question of reply `id`, as `wsx task dismiss` does.
    fn dismiss(&mut self, id: u64) {
        self.run(format!("Dismissing {id}"), move |_, _, store| {
            let done = ManagerDir::existing(&store).and_then(|dir| {
                let dir = dir.ok_or_else(|| anyhow::anyhow!("no manager"))?;
                chat::dismiss(&dir, id, wsx_core::task::now_unix_ms())
            });
            match done {
                Ok(()) => ("Dismissed".into(), false, None),
                Err(error) => (format!("Not dismissed: {error:#}"), true, None),
            }
        });
    }

    /// Fail item `id` with the note: it starts the task that fixes it, as `check fail` does.
    pub fn fail_check(&mut self, id: String) {
        let note = self.note.value().trim().to_string();
        if note.is_empty() {
            return;
        }
        self.focus = Focus::Chat;
        self.run(
            format!("Failing {id}"),
            move |project, _, store| match crate::task::fail_check(&project, &store, &id, &note) {
                Ok(item) => (
                    format!(
                        "{id} failed; #{} fixes it",
                        item.fix_task.unwrap_or_default()
                    ),
                    false,
                    None,
                ),
                Err(error) => (format!("{id} not failed: {error:#}"), true, None),
            },
        );
    }

    /// Approve or deny request `id` as `wsx task approve` and `deny` do.
    pub fn decide(&mut self, id: String, approve: bool) {
        self.focus = Focus::Chat;
        let doing = if approve { "Approving" } else { "Denying" };
        self.run(format!("{doing} {id}"), move |project, main, _| {
            if !approve {
                return match owner::deny(&project, &main, &id, None) {
                    Ok(()) => (format!("{id} denied"), false, None),
                    Err(error) => (format!("{id} not denied: {error:#}"), true, None),
                };
            }
            let approved = std::env::current_exe()
                .map_err(anyhow::Error::from)
                .and_then(|exe| owner::approve(&project, &main, &id, &exe));
            match approved {
                Ok(done) => (format!("{id} approved; {}", done.outcome), !done.ok, None),
                Err(error) => (format!("{id} not approved: {error:#}"), true, None),
            }
        });
    }
}

/// The conversation as rows (`chat_rows`), the open items then the waiting approvals, and
/// the manager's last step. No manager folder yet is an empty chat.
fn read(
    project: &str,
    store: &str,
    main: &Path,
    dir: Option<&ManagerDir>,
    (searched, around): (&str, Option<u64>),
) -> anyhow::Result<View> {
    let window = Window::new(Some(searched), None, around);
    let settings = load_task_config(main).unwrap_or_default();
    let manager = settings.manager_shown_name(project).to_string();
    let role = dir.and_then(|dir| dir.role.clone());
    let name = role.clone().unwrap_or_else(|| manager.clone());
    // ^ Another registered project's manager by its set name (docs/tasks.md "Config"), each
    // project's config read once.
    let names = std::cell::RefCell::new(std::collections::HashMap::<String, String>::new());
    let peer = |other: &str| {
        names
            .borrow_mut()
            .entry(other.to_string())
            .or_insert_with(|| {
                crate::task::project_entry(Some(other))
                    .ok()
                    .and_then(|(_, main)| load_task_config(&main).ok())
                    .map_or_else(
                        || other.to_string(),
                        |config| config.manager_shown_name(other).to_string(),
                    )
            })
            .clone()
    };
    // ^ To check items and approvals are the manager's (docs/tasks.md "Roles").
    let approvals = match role {
        Some(_) => Vec::new(),
        None => store::read_approvals(store)?,
    };
    let names = Names {
        own: &name,
        manager: &manager,
        peer: &peer,
    };
    let Page {
        log, asks, rows, ..
    } = match dir {
        Some(dir) => chat::page(
            dir,
            &window,
            &names,
            &pending_approvals(&approvals),
            wsx_core::task::now_unix_ms(),
            |_| {},
        )?,
        None => Page::default(),
    };
    if role.is_some() {
        return Ok(View {
            rows,
            name,
            items: Vec::new(),
            activity: activity(&log).map(str::to_string),
            agent: None,
        });
    }
    let mut items: Vec<Item> = checks::open_items(store)?
        .into_iter()
        .map(Item::Check)
        .collect();
    items.extend(
        pending_approvals(&approvals)
            .into_iter()
            .map(|line| Item::Approval {
                id: line.id().to_string(),
                label: owner::approval_label(line),
            }),
    );
    items.extend(asks.into_iter().map(|ask| Item::Ask {
        id: ask.id,
        key: format!("ask-{}", ask.id),
        quote: ask.quote,
    }));
    Ok(View {
        rows,
        name,
        items,
        activity: activity(&log).map(str::to_string),
        // ^ A fallback's while one runs (docs/tasks.md "Agents").
        agent: dir
            .map(|dir| view::manager_agent(dir, &settings))
            .and_then(|agent| view::agent_label(&agent)),
    })
}

impl ManagerChat {
    /// One key of the chat (input-mode actions: every character comes as `InputChar`).
    /// False when the chat closes.
    pub fn key(&mut self, action: &Action) -> bool {
        match self.focus.clone() {
            Focus::Chat => return self.chat_key(action),
            Focus::Compose => match action {
                Action::Select => self.send(),
                Action::InputEscape => {
                    // ^ Esc drops what the message would answer, not its words.
                    if self.reply.take().is_some() {
                        self.composer =
                            InputState::with_value("> ", self.composer.value().to_string());
                    }
                    self.focus = Focus::Chat
                }
                _ => {
                    dispatch_text_edit(&mut self.composer, action);
                }
            },
            Focus::Find => match action {
                Action::Select => self.focus = Focus::Chat,
                Action::InputEscape => self.close_find(),
                _ => {
                    if dispatch_text_edit(&mut self.find, action) {
                        // ^ Each edit starts again at the newest match.
                        self.found = usize::MAX;
                        self.search();
                        self.reveal();
                    }
                }
            },
            Focus::File => match action {
                Action::InputEscape => self.close_file(),
                Action::InputChar('j') | Action::NavigateDown => self.scroll_file(1),
                Action::InputChar('k') | Action::NavigateUp => self.scroll_file(-1),
                Action::InputChar('G') => self.scroll_file(i32::MAX),
                Action::InputChar('g') => self.scroll_file(i32::MIN),
                _ => {}
            },
            Focus::Fail(id) => match action {
                Action::Select => self.fail_check(id),
                Action::InputEscape => self.focus = Focus::Chat,
                _ => {
                    dispatch_text_edit(&mut self.note, action);
                }
            },
            Focus::Confirm { id, approve, .. } => match action {
                Action::InputChar('y') | Action::Select => self.decide(id, approve),
                Action::InputChar('n') | Action::InputEscape => self.focus = Focus::Chat,
                _ => {}
            },
        }
        true
    }

    fn chat_key(&mut self, action: &Action) -> bool {
        let selected = self.selected_item().cloned();
        let finding = !self.find.value().is_empty();
        match (action, selected) {
            // ^ Esc clears a find first; the next Esc leaves the chat.
            (Action::InputEscape, _) if finding => self.close_find(),
            (Action::InputEscape, _) if self.link.is_some() => self.link = None,
            (Action::InputEscape, _) if self.around.is_some() => self.newest(),
            (Action::InputEscape, _) => return false,
            (Action::InputChar('/'), _) => self.open_find(),
            (Action::InputChar('['), _) => self.step_link(-1),
            (Action::InputChar(']'), _) => self.step_link(1),
            (Action::Select, _) if self.link.is_some() => self.open_link(),
            // ^ While a find is on, n and N move between matches, before an approval's n.
            (Action::InputChar('n'), _) if finding => self.step(-1),
            (Action::InputChar('N'), _) if finding => self.step(1),
            (Action::InputChar('k') | Action::NavigateUp, _) => self.scroll_by(1),
            (Action::InputChar('j') | Action::NavigateDown, _) => self.scroll_by(-1),
            (Action::InputChar('G'), _) if self.around.is_some() => self.newest(),
            (Action::InputChar('G'), _) => self.scroll = 0,
            (Action::InputTab, _) => self.select(1),
            (Action::InputBackTab, _) => self.select(-1),
            // ^ Enter on an open question shows its reply, as on a quote with an id.
            (Action::Select, Some(Item::Ask { id, .. })) => self.show_quoted("", Some(id)),
            (Action::InputChar('i') | Action::Select, _) => self.focus = Focus::Compose,
            (Action::InputChar('p'), Some(Item::Check(_))) => self.close_check(CheckState::Passed),
            (Action::InputChar('s'), Some(Item::Check(_))) => self.close_check(CheckState::Skipped),
            (Action::InputChar('f'), Some(Item::Check(item))) => {
                self.note = InputState::new(format!("Fail {}, what is wrong: ", item.id));
                self.focus = Focus::Fail(item.id);
            }
            (Action::InputChar('r'), Some(Item::Ask { id, quote, .. })) => {
                self.composer = InputState::with_value(
                    format!("Reply to “{}”: ", view::clip(&quote, 40)),
                    self.composer.value().to_string(),
                );
                self.reply = Some((quote, id));
                self.focus = Focus::Compose;
            }
            (Action::InputChar('d'), Some(Item::Ask { id, .. })) => self.dismiss(id),
            (Action::InputChar(key @ ('y' | 'n')), Some(Item::Approval { id, label })) => {
                self.focus = Focus::Confirm {
                    id,
                    label,
                    approve: *key == 'y',
                }
            }
            _ => {}
        }
        true
    }

    /// Select the next link toward `delta` (-1 older, 1 newer), from none the newest, and show it.
    fn step_link(&mut self, delta: isize) {
        let links = links(&self.rows);
        let at = self
            .link
            .and_then(|link| links.iter().position(|(ms, nth, _)| (*ms, *nth) == link));
        let next = match at {
            Some(at) => at
                .saturating_add_signed(delta)
                .min(links.len().saturating_sub(1)),
            None => links.len().saturating_sub(1),
        };
        let Some((ms, nth, _)) = links.get(next) else {
            return;
        };
        self.link = Some((*ms, *nth));
        let (width, _) = self.area.get();
        let (lines, line) = self.wrapped(width);
        if let Some(line) = line {
            self.center(line, lines.len());
        }
    }

    /// Open the selected link: a session is the app's to enter, a file shows in place of the chat.
    fn open_link(&mut self) {
        let target = links(&self.rows)
            .into_iter()
            .find(|(ms, nth, _)| Some((*ms, *nth)) == self.link)
            .map(|(.., target)| target);
        match target {
            Some(Target::Session(id)) => self.session = Some(id),
            Some(Target::File(path)) => match read_file(&self.main, &path) {
                Ok(lines) => {
                    self.file = Some(FileView {
                        name: path,
                        lines,
                        scroll: 0,
                        max_scroll: Cell::new(0),
                    });
                    self.focus = Focus::File;
                }
                Err(error) => self.notice = Some((format!("files/{path}: {error}"), true)),
            },
            Some(Target::Quote { line, id }) => self.show_quoted(&line, id),
            None => self.link = None,
        }
    }

    /// Open the chat at message `id`, marked, as a tap on a quote of it does (a routine's last
    /// run: docs/tasks.md "Routines").
    pub fn go_to(&mut self, id: u64) {
        self.show_quoted("", Some(id));
    }

    /// Show the quoted message: by `id` when the quote has one (centered and marked, reading the
    /// log around it when the rows lack it), else the newest message above the selected one
    /// whose first line is `line`.
    fn show_quoted(&mut self, line: &str, id: Option<u64>) {
        if let Some(id) = id {
            self.marked = Some(id);
            self.wrapped.take();
            if self.rows.iter().any(|row| row.id == Some(id)) {
                self.arrive();
            } else {
                // ^ The rows are a different part now: the selected link is not among them.
                self.around = Some(id);
                self.jump_due = true;
                self.link = None;
                self.read_again();
            }
            return;
        }
        let Some(from) = self.link.and_then(|(ms, _)| {
            self.rows
                .iter()
                .position(|row| row.kind == RowKind::Message && row.unix_ms == ms)
        }) else {
            return;
        };
        let found = self.rows[..from]
            .iter()
            .rposition(|row| row.quote.as_deref() == Some(line));
        let Some(at) = found else {
            self.notice = Some((
                "That message is no longer in the conversation shown here.".into(),
                true,
            ));
            return;
        };
        self.center_row(at);
    }

    /// Center the message of row `at`.
    fn center_row(&mut self, at: usize) {
        let (width, _) = self.area.get();
        let lines = |rows: &[ChatRow]| {
            crate::ui::task_chat::chat_lines(rows, width, None, None)
                .0
                .len()
        };
        // ^ A message starts after the blank line that parts it from the one before.
        self.center(
            lines(&self.rows[..at]) + usize::from(at > 0),
            lines(&self.rows),
        );
    }

    /// Scroll to the marked row, which the rows now hold, or say the log lost it.
    fn arrive(&mut self) {
        self.jump_due = false;
        let Some(at) = self.rows.iter().position(|row| row.id == self.marked) else {
            self.notice = Some((
                "That message is no longer in the conversation.".into(),
                true,
            ));
            self.newest();
            return;
        };
        self.center_row(at);
    }

    /// Back to the newest part of the conversation, from a read around a quoted message.
    fn newest(&mut self) {
        if self.around.take().is_some() {
            self.scroll = 0;
            self.read_again();
        }
        self.marked = None;
        self.jump_due = false;
        self.wrapped.take();
    }

    fn close_file(&mut self) {
        self.file = None;
        self.focus = Focus::Chat;
    }

    fn scroll_file(&mut self, delta: i32) {
        if let Some(file) = &mut self.file {
            file.scroll = file
                .scroll
                .saturating_add_signed(delta as isize)
                .min(file.max_scroll.get());
        }
    }

    fn open_find(&mut self) {
        self.focus = Focus::Find;
    }

    /// Read again at once for the find's words (`Window::Find`) when they changed.
    fn search(&mut self) {
        let words = self.find.value().trim().to_string();
        if words != self.searched {
            self.searched = words;
            self.read_again();
        }
    }

    fn close_find(&mut self) {
        self.find = InputState::new("/");
        self.reveal_due = false;
        self.focus = Focus::Chat;
        // ^ Back to the newest part: its read drops the old rows, and the scroll counted
        // lines of the find's read.
        if !self.searched.is_empty() {
            self.scroll = 0;
        }
        self.search();
    }

    /// Read the store at the next poll, whatever the stamp says.
    fn read_again(&mut self) {
        self.stamp = None;
        self.last_check = Instant::now()
            .checked_sub(CHECK_INTERVAL)
            .unwrap_or(self.last_check);
    }

    /// The matches of the find as the last frame drew the chat: (line, byte range).
    pub fn matches(&self) -> Found {
        let (lines, _) = self.wrapped(self.area.get().0);
        let mut cache = self.wrapped.borrow_mut();
        let cache = cache.as_mut().expect("`wrapped` filled the cache");
        let query = self.find.value();
        match &cache.found {
            Some((asked, found)) if asked == query => found.clone(),
            _ => {
                let found = Arc::new(crate::ui::task_chat::find_in(&lines, query));
                cache.found = Some((query.to_string(), found.clone()));
                found
            }
        }
    }

    /// The chat's lines at `width` and the line of the selected link: wrapped once per rows,
    /// width and link.
    pub fn wrapped(&self, width: usize) -> (Arc<Vec<Line<'static>>>, Option<usize>) {
        let mut cache = self.wrapped.borrow_mut();
        let fresh = cache.as_ref().is_some_and(|cache| {
            cache.width == width && cache.link == self.link && cache.marked == self.marked
        });
        if !fresh {
            let (lines, selected) =
                crate::ui::task_chat::chat_lines(&self.rows, width, self.link, self.marked);
            *cache = Some(Wrapped {
                width,
                link: self.link,
                marked: self.marked,
                lines: Arc::new(lines),
                selected,
                found: None,
            });
        }
        let cache = cache.as_ref().expect("filled above");
        (cache.lines.clone(), cache.selected)
    }

    /// Move `delta` matches, newer for +1, wrapping, and show the match.
    fn step(&mut self, delta: isize) {
        let count = self.matches().len();
        if count > 0 {
            let at = self.found.min(count - 1) as isize;
            self.found = (at + delta).rem_euclid(count as isize) as usize;
        }
        self.reveal();
    }

    /// Scroll so the current match sits mid-chat; `found` past the end means the newest.
    fn reveal(&mut self) {
        let matches = self.matches();
        let Some(last) = matches.len().checked_sub(1) else {
            self.reveal_due = !self.find.value().is_empty();
            return;
        };
        self.reveal_due = false;
        self.found = self.found.min(last);
        let total = self.wrapped(self.area.get().0).0.len();
        self.center(matches[self.found].0, total);
    }

    /// Scroll so `line` of `total` sits mid-chat.
    fn center(&mut self, line: usize, total: usize) {
        let height = usize::from(self.area.get().1);
        // ^ `scroll` counts lines up from the newest: the line's distance from the end, less half the chat.
        let below = total.saturating_sub(line + 1);
        let scroll = below.saturating_sub(height / 2);
        self.scroll = scroll.min(total.saturating_sub(height));
    }
}
