//! The TUI's manager chat of one project, or a role's (docs/tasks.md "Roles"): the
//! conversation, the open To check items and the waiting approvals, read off the UI thread when a file of the task store changes, and the
//! owner's message and decisions, also run off it. ref: docs/tasks.md "TUI"
use crate::{
    action::Action,
    ui::{input::InputState, text_form::dispatch_text_edit},
};
use std::{
    cell::Cell,
    collections::HashSet,
    io::Read as _,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::config::project::load_task_config;
use wsx_core::task::{
    call_socket,
    checks::{self, CheckItem, CheckState},
    manager::{
        activity, chat_window, pending_approvals, quote_of, says_nothing, split_quote, Author,
        ManagerEvent, ManagerRequest, ManagerResponse, CHAT_ENTRIES,
    },
    owner,
    store::{self, ManagerDir, Stamp},
    view,
};

const CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// The host answers its status at once; a slow answer counts as not running.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Owner,
    Manager,
    /// Another project's manager (docs/tasks.md "Managers talk").
    Peer(String),
}

/// One entry of the conversation: a message with its author and time, or one muted line
/// (an event, a quiet reply, a new conversation, a stop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Said {
        who: Who,
        unix_ms: u64,
        text: String,
        /// A manager reply that asks the owner, before the owner wrote again.
        asks: bool,
        /// A post of wsx, not an agent turn (`is_post`): its bar is the secondary color.
        routine: bool,
    },
    Line(String),
}

/// What a link of the chat opens (docs/tasks.md "Session news", "Routines"): a session, as `S`
/// enters one, or a file of the main checkout in the chat's file view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Session(u64),
    File(String),
    /// A quote at the head of a message (`split_quote`): it goes to the quoted message, the
    /// newest one above with that first line.
    Quote(String),
}

/// A piece of a message: plain words, or a link `[label](session/<id>)` / `[label](files/<path>)`.
/// Any other link stays plain words as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    Link { label: &'a str, target: Target },
}

fn target_of(href: &str) -> Option<Target> {
    if let Some(id) = href.strip_prefix("session/") {
        return id.parse().ok().map(Target::Session);
    }
    let path = href
        .strip_prefix("files/")
        .filter(|path| !path.is_empty())?;
    // ^ `link_path` in task/manager.rs writes each byte outside a safe set as `%XX`.
    let (mut bytes, mut at) = (Vec::new(), path.bytes());
    while let Some(byte) = at.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex = [at.next()?, at.next()?];
        bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
    }
    String::from_utf8(bytes).ok().map(Target::File)
}

/// `text` as plain words and links, in order.
pub fn parts(text: &str) -> Vec<Part<'_>> {
    let (mut out, mut plain, mut from) = (Vec::new(), 0, 0);
    while let Some(open) = text[from..].find('[').map(|at| from + at) {
        from = open + 1;
        let Some(close) = text[from..].find(']').map(|at| from + at) else {
            break;
        };
        let label = &text[from..close];
        let Some(href) = text[close + 1..].strip_prefix('(') else {
            continue;
        };
        let Some(end) = href.find(')') else {
            continue;
        };
        let target = target_of(&href[..end]);
        match target {
            Some(target) if !label.is_empty() && !label.contains(['[', '\n']) => {
                if plain < open {
                    out.push(Part::Text(&text[plain..open]));
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
pub fn links(rows: &[Row]) -> Vec<(u64, usize, Target)> {
    let mut found = Vec::new();
    for row in rows {
        if let Row::Said { unix_ms, text, .. } = row {
            // ^ The quote is link 0 of its message; `chat_lines` counts the same way.
            let (quote, body) = split_quote(text);
            let quoted = quote.map(|line| Target::Quote(line.to_string()));
            let targets = parts(body).into_iter().filter_map(|part| match part {
                Part::Link { target, .. } => Some(target),
                Part::Text(_) => None,
            });
            found.extend(
                quoted
                    .into_iter()
                    .chain(targets)
                    .enumerate()
                    .map(|(nth, target)| (*unix_ms, nth, target)),
            );
        }
    }
    found
}

/// A file shown in place of the chat, from its top.
pub struct FileView {
    pub name: String,
    pub lines: Vec<String>,
    pub scroll: u16,
    /// ^ The furthest scroll the last frame could show.
    pub max_scroll: Cell<u16>,
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

/// What the owner decides: an open To check item or a waiting approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Check(CheckItem),
    Approval { id: String, label: String },
}

impl Item {
    fn id(&self) -> &str {
        match self {
            Self::Check(item) => &item.id,
            Self::Approval { id, .. } => id,
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
    rows: Vec<Row>,
    items: Vec<Item>,
    activity: Option<String>,
    /// The manager's agent in use, as `view::agent_label` writes it; none for a role.
    agent: Option<String>,
}

/// A check's answer: the host's status, and the store read again when a stamp changed.
struct Read {
    /// Whether the view holds the whole log (`ManagerChat::whole`) or its newest part.
    whole: bool,
    status: Option<ManagerResponse>,
    view: Option<(Stamp, Result<View, String>)>,
}

pub struct ManagerChat {
    pub project: String,
    /// The role this chat talks to; none is the manager.
    pub role: Option<String>,
    main: PathBuf,
    store: String,
    pub rows: Vec<Row>,
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
    pub scroll: u16,
    /// ^ The furthest scroll the last frame could show, so `k` stops at the oldest line.
    pub max_scroll: Cell<u16>,
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
    /// ^ The whole log, not its newest `CHAT_ENTRIES`, once a find opened: a find reaches
    /// every message (docs/tasks.md "TUI").
    whole: bool,
    /// The current match is still to be scrolled to: the whole log was not read yet.
    reveal_due: bool,
    /// What runs now ("Sending", "Failing c3"), until its answer comes.
    pub acting: Option<String>,
    stamp: Option<Stamp>,
    pending: Option<mpsc::Receiver<Read>>,
    answer: Option<mpsc::Receiver<Answer>>,
    last_check: Instant,
}

/// An action's words, whether it failed, and a sent message to give back to the composer.
type Answer = (String, bool, Option<String>);

impl ManagerChat {
    pub fn new(project: String, main: PathBuf, role: Option<String>) -> Self {
        let store = store::project_id(&project, &main);
        let mut chat = Self {
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
            whole: false,
            reveal_due: false,
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
        let whole = self.whole;
        let main = self.main.clone();
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
                let view =
                    read(&store, &main, dir.as_ref(), whole).map_err(|error| format!("{error:#}"));
                (now, view)
            });
            let _ = tx.send(Read {
                whole,
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
                self.rows = view.rows;
                if self.reveal_due {
                    self.reveal();
                }
                self.activity = view.activity;
                self.agent = view.agent;
                // ^ A read started before the find opened has the newest part only: read again.
                self.stamp = (read.whole == self.whole).then_some(stamp);
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

    /// The name the chat shows for the other side: the role's, or `Manager`.
    pub fn who(&self) -> &str {
        self.role.as_deref().unwrap_or("Manager")
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
        self.scroll =
            (i32::from(self.scroll) + delta).clamp(0, i32::from(self.max_scroll.get())) as u16;
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
        let text = self.composer.value().trim().to_string();
        if text.is_empty() || self.acting.is_some() {
            return;
        }
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

/// The conversation as rows, the open items then the waiting approvals, and the manager's
/// last step. No manager folder yet is an empty chat.
fn read(store: &str, main: &Path, dir: Option<&ManagerDir>, whole: bool) -> anyhow::Result<View> {
    let log = match dir {
        Some(dir) => dir.read_log()?,
        None => Vec::new(),
    };
    let limit = if whole { usize::MAX } else { CHAT_ENTRIES };
    let (entries, _) = chat_window(&log, None, limit);
    let mut rows = Vec::new();
    // ^ The ids of the events messages: a reply to them with no usage is a post (`is_post`).
    let mut events = HashSet::new();
    for entry in entries {
        let said = |who, text: &str| Row::Said {
            who,
            unix_ms: entry.unix_ms,
            text: text.trim().to_string(),
            asks: false,
            routine: false,
        };
        match &entry.event {
            ManagerEvent::Message { id, from, text } => match from {
                // ^ A reply asks until the owner writes after it (docs/tasks.md "Manager").
                Author::Owner => {
                    for row in &mut rows {
                        if let Row::Said { asks, .. } = row {
                            *asks = false;
                        }
                    }
                    rows.push(said(Who::Owner, text))
                }
                // ^ wsx's recap request is not shown, only the manager's answer to it.
                Author::Recap => {}
                Author::Manager { project, .. } => {
                    rows.push(said(Who::Peer(project.clone()), text))
                }
                // ^ In a role's chat the manager is a peer too, by its own word.
                Author::Role { name, .. } => rows.push(said(
                    Who::Peer(match name.as_str() {
                        "manager" => "Manager".into(),
                        name => name.into(),
                    }),
                    text,
                )),
                Author::Events => {
                    events.insert(*id);
                    rows.extend(
                        text.lines()
                            .filter(|line| !line.trim().is_empty())
                            .map(|line| Row::Line(event_line(line))),
                    )
                }
            },
            // ^ A quiet `-` or empty reply says nothing changed: it shows nothing, as on the web.
            ManagerEvent::Reply {
                text, quiet: true, ..
            } if says_nothing(text) => {}
            ManagerEvent::Reply {
                text, quiet: true, ..
            } => rows.push(Row::Line(text.trim().to_string())),
            ManagerEvent::Reply {
                to,
                text,
                usage,
                asks,
                provisional,
                ..
            } => rows.push(Row::Said {
                who: Who::Manager,
                unix_ms: entry.unix_ms,
                text: match provisional {
                    Some(name) => format!("Stand-in {name}, not checked: {}", text.trim()),
                    None => text.trim().to_string(),
                },
                asks: *asks,
                routine: is_post(usage.is_none(), to, &events),
            }),
            // ^ An empty `sent` (older hosts logged one without --json) shows nothing.
            ManagerEvent::Sent { text, .. } if text.trim().is_empty() => {}
            ManagerEvent::Sent { to, text, .. } => {
                let to = to.strip_prefix("role:").unwrap_or(to);
                rows.push(said(Who::Manager, &format!("To {to}: {text}")))
            }
            ManagerEvent::Rotated { .. } => rows.push(Row::Line("New conversation".into())),
            ManagerEvent::Stopped { reason } => {
                rows.push(Row::Line(format!("Manager stopped: {reason}")))
            }
            ManagerEvent::Limit { until: Some(until) } => rows.push(Row::Line(format!(
                "Manager paused: usage limit, resumes at {until}"
            ))),
            ManagerEvent::Limit { until: None } => rows.push(Row::Line("Manager resumed".into())),
            ManagerEvent::Delivered { .. }
            | ManagerEvent::Agent { .. }
            | ManagerEvent::Ping { .. } => {}
        }
    }
    // ^ To check items and approvals are the manager's (docs/tasks.md "Roles").
    if dir.is_some_and(|dir| dir.role.is_some()) {
        return Ok(View {
            rows,
            items: Vec::new(),
            activity: activity(&log).map(str::to_string),
            agent: None,
        });
    }
    let mut items: Vec<Item> = checks::open_items(store)?
        .into_iter()
        .map(Item::Check)
        .collect();
    let approvals = store::read_approvals(store)?;
    items.extend(
        pending_approvals(&approvals)
            .into_iter()
            .map(|line| Item::Approval {
                id: line.id().to_string(),
                label: owner::approval_label(line),
            }),
    );
    Ok(View {
        rows,
        items,
        activity: activity(&log).map(str::to_string),
        // ^ A fallback's while one runs (docs/tasks.md "Agents").
        agent: dir.and_then(|dir| {
            let settings = load_task_config(main).unwrap_or_default();
            view::agent_label(&view::manager_agent(dir, &settings))
        }),
    })
}

/// A post of wsx, not an agent turn: a reply with no usage to events messages only, as the
/// digest, `wsx task post` and the session news are written (docs/tasks.md "Routines").
/// The web's `conversation` in bubbles.js reads the same rule.
fn is_post(no_usage: bool, to: &[u64], events: &HashSet<u64>) -> bool {
    no_usage && !to.is_empty() && to.iter().all(|id| events.contains(id))
}

/// An event line written by wsx in short form: its first clause, `#3 ready r2: <title>`.
fn event_line(line: &str) -> String {
    line.split("; ").next().unwrap_or(line).trim().to_string()
}

impl ManagerChat {
    /// One key of the chat (input-mode actions: every character comes as `InputChar`).
    /// False when the chat closes.
    pub fn key(&mut self, action: &Action) -> bool {
        match self.focus.clone() {
            Focus::Chat => return self.chat_key(action),
            Focus::Compose => match action {
                Action::Select => self.send(),
                Action::InputEscape => self.focus = Focus::Chat,
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
            (Action::InputChar('G'), _) => self.scroll = 0,
            (Action::InputTab, _) => self.select(1),
            (Action::InputBackTab, _) => self.select(-1),
            (Action::InputChar('i') | Action::Select, _) => self.focus = Focus::Compose,
            (Action::InputChar('p'), Some(Item::Check(_))) => self.close_check(CheckState::Passed),
            (Action::InputChar('s'), Some(Item::Check(_))) => self.close_check(CheckState::Skipped),
            (Action::InputChar('f'), Some(Item::Check(item))) => {
                self.note = InputState::new(format!("Fail {}, what is wrong: ", item.id));
                self.focus = Focus::Fail(item.id);
            }
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
        let (lines, line) =
            crate::ui::task_chat::chat_lines(&self.rows, self.who(), width, self.link);
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
            Some(Target::Quote(line)) => self.show_quoted(&line),
            None => self.link = None,
        }
    }

    /// Scroll to the newest message above the selected one whose first line is `line`.
    fn show_quoted(&mut self, line: &str) {
        let Some(from) = self.link.and_then(|(ms, _)| {
            self.rows
                .iter()
                .position(|row| matches!(row, Row::Said { unix_ms, .. } if *unix_ms == ms))
        }) else {
            return;
        };
        let found = self.rows[..from]
            .iter()
            .rposition(|row| matches!(row, Row::Said { text, .. } if quote_of(text) == line));
        let Some(at) = found else {
            self.notice = Some((
                "That message is no longer in the conversation shown here.".into(),
                true,
            ));
            return;
        };
        let (width, _) = self.area.get();
        let who = self.who().to_string();
        let lines = |rows: &[Row]| {
            crate::ui::task_chat::chat_lines(rows, &who, width, None)
                .0
                .len()
        };
        // ^ A message starts after the blank line that parts it from the one before.
        self.center(
            lines(&self.rows[..at]) + usize::from(at > 0),
            lines(&self.rows),
        );
    }

    fn close_file(&mut self) {
        self.file = None;
        self.focus = Focus::Chat;
    }

    fn scroll_file(&mut self, delta: i32) {
        if let Some(file) = &mut self.file {
            file.scroll = (i32::from(file.scroll).saturating_add(delta))
                .clamp(0, i32::from(file.max_scroll.get())) as u16;
        }
    }

    fn open_find(&mut self) {
        self.focus = Focus::Find;
        if !self.whole {
            self.whole = true;
            // ^ Read again at once, now the whole log.
            self.stamp = None;
            self.last_check = Instant::now()
                .checked_sub(CHECK_INTERVAL)
                .unwrap_or(self.last_check);
        }
    }

    fn close_find(&mut self) {
        self.find = InputState::new("/");
        self.reveal_due = false;
        self.focus = Focus::Chat;
    }

    /// The matches of the find as the last frame drew the chat: (line, byte range).
    pub fn matches(&self) -> Vec<(usize, std::ops::Range<usize>)> {
        let (width, _) = self.area.get();
        let lines = crate::ui::task_chat::chat_lines(&self.rows, self.who(), width, self.link).0;
        crate::ui::task_chat::find_in(&lines, self.find.value())
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
        let total =
            crate::ui::task_chat::chat_lines(&self.rows, self.who(), self.area.get().0, self.link)
                .0
                .len();
        self.center(matches[self.found].0, total);
    }

    /// Scroll so `line` of `total` sits mid-chat.
    fn center(&mut self, line: usize, total: usize) {
        let height = usize::from(self.area.get().1);
        // ^ `scroll` counts lines up from the newest: the line's distance from the end, less half the chat.
        let below = total.saturating_sub(line + 1);
        let scroll = below.saturating_sub(height / 2);
        self.scroll = scroll.min(total.saturating_sub(height)) as u16;
    }
}
