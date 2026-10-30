//! The TUI's manager chat of one project, or a role's (docs/tasks.md "Roles"): the
//! conversation, the open To check items and the waiting approvals, read off the UI thread when a file of the task store changes, and the
//! owner's message and decisions, also run off it. ref: docs/tasks.md "TUI"
use crate::{
    action::Action,
    ui::{input::InputState, text_form::dispatch_text_edit},
};
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::task::{
    call_socket,
    checks::{self, CheckItem, CheckState},
    manager::{
        activity, chat_window, pending_approvals, says_nothing, Author, ManagerEvent,
        ManagerRequest, ManagerResponse,
    },
    owner,
    store::{self, ManagerDir, Stamp},
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
    },
    Line(String),
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

/// Where keys go: the chat, the message line, a fail's note, or a y/N question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Chat,
    Compose,
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
}

/// A check's answer: the host's status, and the store read again when a stamp changed.
struct Read {
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
    pub error: Option<String>,
    /// What the last message or decision answered; a failure stays until the next one.
    pub notice: Option<(String, bool)>,
    /// Lines up from the newest; 0 follows new lines.
    pub scroll: u16,
    /// ^ The furthest scroll the last frame could show, so `k` stops at the oldest line.
    pub max_scroll: Cell<u16>,
    pub focus: Focus,
    pub composer: InputState,
    pub note: InputState,
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
            error: None,
            notice: None,
            scroll: 0,
            max_scroll: Cell::new(0),
            focus: Focus::Chat,
            composer: InputState::new("> "),
            note: InputState::new(""),
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
                let view = read(&store, dir.as_ref()).map_err(|error| format!("{error:#}"));
                (now, view)
            });
            let _ = tx.send(Read { status, view });
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
                self.activity = view.activity;
                self.stamp = Some(stamp);
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
            move |_, _, store| match checks::close(&store, std::slice::from_ref(&id), state) {
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
fn read(store: &str, dir: Option<&ManagerDir>) -> anyhow::Result<View> {
    let log = match dir {
        Some(dir) => dir.read_log()?,
        None => Vec::new(),
    };
    let (entries, _) = chat_window(&log, None);
    let mut rows = Vec::new();
    for entry in entries {
        let said = |who, text: &str| Row::Said {
            who,
            unix_ms: entry.unix_ms,
            text: text.trim().to_string(),
            asks: false,
        };
        match &entry.event {
            ManagerEvent::Message { from, text, .. } => match from {
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
                Author::Events => rows.extend(
                    text.lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| Row::Line(event_line(line))),
                ),
            },
            // ^ A quiet `-` or empty reply says nothing changed: it shows nothing, as on the web.
            ManagerEvent::Reply {
                text, quiet: true, ..
            } if says_nothing(text) => {}
            ManagerEvent::Reply {
                text, quiet: true, ..
            } => rows.push(Row::Line(text.trim().to_string())),
            ManagerEvent::Reply { text, asks, .. } => rows.push(Row::Said {
                who: Who::Manager,
                unix_ms: entry.unix_ms,
                text: text.trim().to_string(),
                asks: *asks,
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
                "Manager paused: session limit, resumes at {until}"
            ))),
            ManagerEvent::Limit { until: None } => rows.push(Row::Line("Manager resumed".into())),
            ManagerEvent::Delivered { .. } | ManagerEvent::Agent { .. } => {}
        }
    }
    // ^ To check items and approvals are the manager's (docs/tasks.md "Roles").
    if dir.is_some_and(|dir| dir.role.is_some()) {
        return Ok(View {
            rows,
            items: Vec::new(),
            activity: activity(&log).map(str::to_string),
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
    })
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
        match (action, selected) {
            (Action::InputEscape, _) => return false,
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
}
