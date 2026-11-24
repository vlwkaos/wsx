//! The TUI's task list: the project's ledger, read off the UI thread and read
//! again when a file of its task store changes. ref: docs/tasks.md "TUI"
use crate::action::Action;
use serde_json::Value;
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::task::{
    ledger, now_unix_ms, owner,
    store::{self, Stamp},
    usage, view, TaskState, WaitOn,
};

/// The chat rail's width, and the pane width from which it shows beside the chat: the chat
/// alone keeps its 100 columns and more (docs/tasks.md "TUI").
pub const RAIL_WIDTH: u16 = 24;
pub const RAIL_MIN: u16 = 130;
/// The pane width from which a task shows beside the chat, the chat at 40% of the rest.
pub const BESIDE_MIN: u16 = 120;

/// Where the keys go while the chat is open (docs/tasks.md "TUI"): the chat (its timeline or
/// its message line, `task_chat::Focus`), the task beside it, or the chat rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Chat,
    Task,
    Rail,
}

/// What an item of the chat rail opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RailTarget {
    /// The project's manager (`open_chat(None)`); the main chat's row is this item.
    Manager,
    Chat(String),
    /// The For you list in place of the chat.
    ForYou,
}

/// An item of the chat rail: its name, the counts its marks show, what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailItem {
    pub title: String,
    pub counts: wsx_core::task::needs::ChatCounts,
    pub target: RailTarget,
}

/// How often the store's files are compared with the last read.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How often the spend lines are summed again; a ledger read between keeps the last ones. The
/// sum reads the logs of every project of the workspace.
const USAGE_INTERVAL: Duration = Duration::from_secs(30);

/// A check's answer: none when nothing is due, else the new stamps, the ledger read when a
/// stamp changed, and the spend lines (`usage`, `usage_all`) when they were due.
type Read = Option<(Stamp, Option<Result<Value, String>>, Option<[Value; 2]>)>;

/// The project's `overview.md` in a read-only scroll view, markdown as written (a mermaid
/// graph reads as the code block it is). ref: docs/tasks.md "TUI"
pub struct Overview {
    /// The file's text; none when the file is missing or unread.
    pub text: Result<Option<String>, String>,
    /// Lines down from the top.
    pub scroll: u16,
    /// ^ The furthest scroll and the height of the last frame, which `j` and a page use.
    pub max_scroll: Cell<u16>,
    pub height: Cell<u16>,
}

impl Overview {
    fn new(text: Result<Option<(String, u64)>, String>) -> Self {
        Self {
            text: text.map(|text| text.map(|(text, _)| text)),
            scroll: 0,
            max_scroll: Cell::new(0),
            height: Cell::new(0),
        }
    }

    /// One key. False when the view closes (`Esc` or `q`); the keys of the list or the
    /// chat under it do nothing meanwhile.
    pub fn key(&mut self, action: &Action) -> bool {
        let page = i32::from(self.height.get().saturating_sub(1).max(1));
        let delta = match action {
            Action::InputEscape | Action::Quit => return false,
            Action::NavigateDown | Action::InputChar('j') => 1,
            Action::NavigateUp | Action::InputChar('k') => -1,
            Action::PageDown => page,
            Action::PageUp => -page,
            _ => 0,
        };
        self.scroll =
            (i32::from(self.scroll) + delta).clamp(0, i32::from(self.max_scroll.get())) as u16;
        true
    }
}

/// One row: an open task in the ledger's chain order (under its band), an open chat, then a
/// recent landed or closed task. A chat row has no number; its title is the chat's shown name
/// (`chats::shown_name`) and `chat` its id, which the actions take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: u64,
    pub title: String,
    /// A chat's id; empty for a task.
    pub chat: String,
    pub state: String,
    /// The status label (wsx-core `task::status`), or a chat's goal; empty when nothing.
    pub status: String,
    /// A chat's pill counts, marked after its name; zero for a task.
    pub counts: wsx_core::task::needs::ChatCounts,
    pub section: Section,
    /// Where an open task stands in its chain (`flow.place`): the chain's head, and how far in.
    pub root: u64,
    pub depth: u8,
}

impl TaskRow {
    /// A row of the Chats section that is no chat and no task: a folder's heading, the
    /// `+N dismissed` count. Nothing on it opens or acts.
    fn note(title: String) -> Self {
        Self {
            id: 0,
            title,
            chat: String::new(),
            state: String::new(),
            status: String::new(),
            counts: Default::default(),
            section: Section::Chats,
            root: 0,
            depth: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// The one muted row "N wait for you" that opens the For you list; it has no heading.
    Entry,
    /// The For you list's rows (`rows_you`).
    You,
    Moving,
    Waiting,
    Chats,
    Recent,
}

impl Section {
    pub fn heading(self) -> &'static str {
        match self {
            Self::Entry => "",
            Self::You => "For you",
            Self::Moving => "Moving",
            Self::Waiting => "Waiting",
            Self::Chats => "Chats",
            Self::Recent => "Recent",
        }
    }
}

pub struct TaskList {
    pub project: String,
    main: PathBuf,
    store: String,
    /// Every project of the workspace (name, path), for the all-projects spend.
    projects: Vec<(String, PathBuf)>,
    pub ledger: Option<Value>,
    pub rows: Vec<TaskRow>,
    pub error: Option<String>,
    pub selected: usize,
    /// The task whose detail shows, by number, and its scroll. With the chat open it shows
    /// beside the chat (a `#N` link of it, `open_task`).
    pub detail: Option<u64>,
    /// Where the keys go while the chat is open.
    pub region: Region,
    /// The rail's items (`rail_list`) and the selected one.
    rail_list: Vec<RailItem>,
    pub rail: usize,
    /// The Tasks pane's width at the last frame: the chat view's layout follows it
    /// (`rail_shown`, `task_drawn`), and so do the keys that depend on what shows.
    pub width: Cell<u16>,
    /// Unsent message lines of the chats left (`stash_draft`), by chat; none is the manager.
    drafts: std::collections::HashMap<Option<String>, String>,
    pub scroll: u16,
    /// The day rows show tokens, not cost (`u`).
    pub tokens: bool,
    /// The agent the spend lines and the day rows count (`a`): an adapter or
    /// `adapter/model` of `usage_all.agents`; none for all.
    pub agent: Option<String>,
    /// The project's manager chat, opened over the list with `m`.
    pub chat: Option<Box<crate::task_chat::ChatScreen>>,
    /// The New chat form, opened with `n` over the list.
    pub new_chat: Option<crate::task_new_chat::NewChat>,
    /// The project's overview, opened with `o` over the list or the chat.
    pub overview: Option<Overview>,
    /// The For you list shows instead of the tasks (`Enter` on the "N wait for you" row).
    pub you: bool,
    /// The `?` popup over the list, the detail or the chat.
    pub help: Option<crate::ui::help::HelpView>,
    /// A land or a hand of the task shown, waiting for `y` (the action's name in `owner_actions`).
    pub confirm: Option<&'static str>,
    /// The chat row's close (`d`), waiting for `y` (docs/tasks.md "Chats").
    pub close_chat: Option<String>,
    /// What an owner action of the detail is doing now, and how the last one ended (a failure
    /// stays until the next key).
    pub acting: Option<String>,
    pub notice: Option<(String, bool)>,
    answer: Option<mpsc::Receiver<(String, bool)>>,
    stamp: Option<Stamp>,
    pending: Option<mpsc::Receiver<Read>>,
    last_check: Instant,
    /// When the spend lines were summed last (none: due now), and whether a ledger read since
    /// holds older ones.
    usage_at: Option<Instant>,
    usage_stale: bool,
}

impl TaskList {
    pub fn new(project: String, main: PathBuf, projects: Vec<(String, PathBuf)>) -> Self {
        let store = store::project_id(&project, &main);
        let mut list = Self {
            project,
            main,
            store,
            projects,
            ledger: None,
            rows: Vec::new(),
            error: None,
            selected: 0,
            detail: None,
            scroll: 0,
            tokens: false,
            agent: None,
            chat: None,
            new_chat: None,
            overview: None,
            you: false,
            help: None,
            confirm: None,
            region: Region::Chat,
            rail: 0,
            rail_list: rail_list(None),
            width: Cell::new(0),
            drafts: Default::default(),
            close_chat: None,
            acting: None,
            notice: None,
            answer: None,
            stamp: None,
            pending: None,
            last_check: Instant::now(),
            usage_at: None,
            usage_stale: false,
        };
        list.check();
        list
    }

    /// Start a check when none runs; the thread reads the ledger only when a stamp changed.
    fn check(&mut self) {
        let (tx, rx) = mpsc::sync_channel(1);
        let (project, main, store, last, projects) = (
            self.project.clone(),
            self.main.clone(),
            self.store.clone(),
            self.stamp.clone(),
            self.projects.clone(),
        );
        let filter = usage::Filter {
            agent: self.agent.clone(),
            ..usage::Filter::default()
        };
        // ^ The chat shows no spend lines: their sum waits until it closes.
        let usage_due = self.chat.is_none()
            && self
                .usage_at
                .is_none_or(|at| self.usage_stale && at.elapsed() >= USAGE_INTERVAL);
        self.pending = Some(rx);
        self.last_check = Instant::now();
        std::thread::spawn(move || {
            let now = store::stamp(&store);
            let changed = last.as_ref() != Some(&now);
            let ledger = changed
                .then(|| ledger::build(&project, &main).map_err(|error| format!("{error:#}")));
            // ^ Every project's spend summed: the slowest part of a read, so on its own timer.
            // It counts the chosen agent's turns. ref: docs/tasks.md "TUI"
            let usage = usage_due.then(|| {
                let stores: Vec<(String, String)> = projects
                    .iter()
                    .map(|(name, path)| (name.clone(), store::project_id(name, path)))
                    .collect();
                let now = now_unix_ms();
                [
                    usage::all(&[(project, store)], now, &filter),
                    usage::all(&stores, now, &filter),
                ]
            });
            let _ = tx.send((changed || usage.is_some()).then_some((now, ledger, usage)));
        });
    }

    /// `a`: count the next agent of the 7 days, after the last all of them again. The ledger is
    /// read again at once; a read of the last agent still running is dropped.
    pub fn cycle_agent(&mut self) {
        let agents: Vec<String> = self
            .ledger
            .as_ref()
            .and_then(|ledger| ledger["usage_all"]["agents"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|agent| agent.as_str().map(str::to_string))
            .collect();
        let next = match &self.agent {
            None => 0,
            Some(agent) => agents
                .iter()
                .position(|each| each == agent)
                .map_or(0, |at| at + 1),
        };
        self.agent = agents.get(next).cloned();
        self.stamp = None;
        self.usage_at = None;
        self.check();
    }

    /// Open the manager's chat, or another's (docs/tasks.md "Chats").
    pub fn open_chat(&mut self, chat: Option<String>) {
        self.stash_draft();
        let mut open =
            crate::task_chat::ChatScreen::new(self.project.clone(), self.main.clone(), chat);
        if let Some(draft) = self.drafts.remove(&self.chat_key(open.chat.as_deref())) {
            open.composer = crate::ui::input::InputState::with_value("> ", draft);
        }
        self.chat = Some(Box::new(open));
    }

    /// Keep the open chat's unsent message line for its next open, in this view.
    pub fn stash_draft(&mut self) {
        let Some(chat) = &self.chat else {
            return;
        };
        let draft = chat.composer.value().to_string();
        if !draft.is_empty() {
            let key = self.chat_key(chat.chat.as_deref());
            self.drafts.insert(key, draft);
        }
    }

    /// `n`: the New chat form, whose `default` agent is the project's manager's.
    pub fn open_new_chat(&mut self) {
        let default = wsx_core::config::project::load_task_config(&self.main)
            .ok()
            .and_then(|config| {
                wsx_core::task::view::agent_label(&wsx_core::task::view::default_agent(&config))
            });
        self.new_chat = Some(crate::task_new_chat::NewChat::new(default));
    }

    /// A key of the New chat form: it opens the chat made and closes on Esc.
    pub fn new_chat_key(&mut self, action: &Action) {
        let Some(form) = &mut self.new_chat else {
            return;
        };
        match form.key(action, &self.store) {
            crate::task_new_chat::Outcome::Open => {}
            crate::task_new_chat::Outcome::Cancel => self.new_chat = None,
            crate::task_new_chat::Outcome::Done(chat) => {
                self.new_chat = None;
                self.open_chat(Some(chat));
                self.stamp = None;
            }
        }
    }

    /// `o`: the overview as it is now; a missing file or a failed read shows as words.
    pub fn open_overview(&mut self) {
        self.overview = Some(Overview::new(
            store::read_overview(&self.store).map_err(|error| format!("{error:#}")),
        ));
    }

    /// The selected row's open chat, which `Enter` opens.
    pub fn selected_chat(&self) -> Option<&str> {
        self.rows
            .get(self.selected)
            .filter(|row| row.section == Section::Chats && row.state == "open")
            .map(|row| row.chat.as_str())
    }

    /// `x` on a chat row: its row leaves the list (`chats_home::dismiss`); the chat, its project
    /// and its tasks go on, and a new message brings the row back. The web's Dismiss.
    pub fn dismiss_chat(&mut self) {
        let Some(name) = self.selected_chat().map(str::to_string) else {
            return;
        };
        self.notice = Some(
            match wsx_core::task::chats_home::dismiss(&name, wsx_core::task::now_unix_ms()) {
                Ok(()) => (format!("Dismissed {name}"), false),
                Err(error) => (format!("{error:#}"), true),
            },
        );
        self.stamp = None;
        self.check();
    }

    /// A key on the list for a chat row's close: `d` asks, `y` or `Enter` closes it through
    /// `chats::close` (as `wsx chat close`), `n` or `Esc` drops it. False when the key is not
    /// for it.
    pub fn close_chat_key(&mut self, action: &Action) -> bool {
        // ^ How the last close ended shows until the next key.
        if self.detail.is_none() {
            self.notice = None;
        }
        let Some(name) = self.close_chat.take() else {
            if *action == Action::Delete {
                if let Some(chat) = self.selected_chat() {
                    self.close_chat = Some(chat.to_string());
                    return true;
                }
            }
            return false;
        };
        if matches!(action, Action::ConfirmYes | Action::Select) {
            // ^ The CLI's own path: it hands the chat back to the main chat (`owner::close_chat`).
            let argv = ["chat", "close", &name].map(String::from).to_vec();
            let closed = std::env::current_exe()
                .map_err(|error| error.to_string())
                .and_then(|wsx| owner::run_as_owner(&wsx, &self.project, argv));
            self.notice = Some(match closed {
                Ok(said) => (
                    said["message"]
                        .as_str()
                        .map_or_else(|| format!("Closed {name}"), str::to_string),
                    false,
                ),
                Err(said) => (said, true),
            });
            self.stamp = None;
            self.check();
        } else if !matches!(
            action,
            Action::NextAttention | Action::InputEscape | Action::Quit
        ) {
            self.close_chat = Some(name);
        }
        true
    }

    /// `Enter`: an open chat, else a task's detail.
    pub fn open_selected(&mut self) {
        match (self.selected_chat(), self.rows.get(self.selected)) {
            (Some(chat), _) => self.open_chat(Some(chat.to_string())),
            (None, Some(row)) if row.section == Section::Entry => self.set_you(true),
            // ^ A For you item opens its place: a task's detail, else the chat that holds it.
            (None, Some(row)) if row.section == Section::You && row.id == 0 => {
                let chat = row.chat.clone();
                self.open_chat(Some(chat));
            }
            (None, Some(row)) if row.section != Section::Chats => self.open_detail(),
            _ => {}
        }
    }

    /// Show the For you list (`on`) or the tasks again; the rows read the ledger again.
    pub fn set_you(&mut self, on: bool) {
        self.you = on;
        self.selected = 0;
        if let Some(ledger) = &self.ledger {
            self.rows = if on { rows_you(ledger) } else { rows(ledger) };
        }
    }

    /// Take a finished read and start the next check when due. True when the view changed.
    /// An open chat reads its own log too; the list keeps reading, for the rail's counts and the
    /// task beside the chat (a read that finds the store unchanged is a stamp compare).
    pub fn poll(&mut self) -> bool {
        let chatted = self.chat.as_mut().is_some_and(|chat| chat.poll());
        chatted | self.poll_list()
    }

    fn poll_list(&mut self) -> bool {
        // ^ A thread that died without an answer ends the action too, or no key could run another.
        let ended = match self.answer.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(answer)) => Some(answer),
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                Some(("The action ended without an answer".into(), true))
            }
            _ => None,
        };
        let changed = ended.is_some();
        if ended.is_some() {
            self.answer = None;
            self.acting = None;
            self.notice = ended;
        }
        if let Some(rx) = &self.pending {
            return changed
                | match rx.try_recv() {
                    Ok(read) => {
                        self.pending = None;
                        read.is_some_and(|(stamp, ledger, usage)| self.apply(stamp, ledger, usage))
                    }
                    Err(mpsc::TryRecvError::Empty) => false,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.pending = None;
                        false
                    }
                };
        }
        if self.last_check.elapsed() >= CHECK_INTERVAL {
            self.check();
        }
        changed
    }

    fn apply(
        &mut self,
        stamp: Stamp,
        ledger: Option<Result<Value, String>>,
        usage: Option<[Value; 2]>,
    ) -> bool {
        let Some(ledger) = ledger else {
            // ^ Only the spend lines were due.
            if let (Some(ledger), Some(sums)) = (&mut self.ledger, usage) {
                self.usage_at = Some(Instant::now());
                self.usage_stale = false;
                [ledger["usage"], ledger["usage_all"]] = sums;
                return true;
            }
            return false;
        };
        match ledger {
            Ok(mut ledger) => {
                // ^ A read with no new sums keeps the last ones.
                match (usage, &self.ledger) {
                    (Some(sums), _) => {
                        self.usage_at = Some(Instant::now());
                        self.usage_stale = false;
                        [ledger["usage"], ledger["usage_all"]] = sums;
                    }
                    (None, old) => {
                        self.usage_stale = true;
                        if let Some(old) = old {
                            ledger["usage"] = old["usage"].clone();
                            ledger["usage_all"] = old["usage_all"].clone();
                        }
                    }
                }
                let selected = self
                    .rows
                    .get(self.selected)
                    .map(|row| (row.id, row.chat.clone()));
                self.rows = if self.you {
                    rows_you(&ledger)
                } else {
                    rows(&ledger)
                };
                // ^ The selection and the detail follow their task, not its position.
                self.selected = selected
                    .and_then(|(id, chat)| {
                        self.rows
                            .iter()
                            .position(|row| row.id == id && (id != 0 || row.chat == chat))
                    })
                    .unwrap_or(self.selected)
                    .min(self.rows.len().saturating_sub(1));
                self.ledger = Some(ledger);
                self.rail_list = rail_list(self.ledger.as_ref());
                // ^ The ledger's open and recent tasks, not the rows: the For you rows list fewer.
                if let Some(id) = self.detail.filter(|_| self.detail_task().is_none()) {
                    self.task_gone(id);
                }
                self.error = None;
                self.stamp = Some(stamp);
            }
            // ^ The last good list stays; the failure shows until a read succeeds.
            Err(error) => self.error = Some(error),
        }
        true
    }

    pub fn select(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    fn open_detail(&mut self) {
        self.detail = self.rows.get(self.selected).map(|row| row.id);
        self.scroll = 0;
        self.confirm = None;
        self.notice = None;
    }

    /// Where `S` opens a shell: the shown or selected task's working copy with its worker's
    /// pane, else the main checkout when the chat is open or no task is listed.
    pub fn shell_target(&self) -> Result<(PathBuf, Option<u64>), String> {
        let task = match &self.chat {
            // ^ With the chat open, `S` is the task's shell while the task has the keys.
            Some(_) => self.detail.filter(|_| self.focus() == Region::Task),
            None => self
                .detail
                .or_else(|| self.rows.get(self.selected).map(|row| row.id)),
        };
        let Some(id) = task else {
            return Ok((self.main.clone(), None));
        };
        let record = u32::try_from(id)
            .map_err(anyhow::Error::from)
            .and_then(|id| store::TaskDir::open(&self.store, id))
            .and_then(|dir| dir.read_record())
            .map_err(|error| format!("t{id}: {error:#}"))?;
        if !record.working_copy.is_dir() {
            return Err(format!("t{id} has no working copy"));
        }
        Ok((record.working_copy, record.pane_id))
    }

    /// The ledger entry of the task whose detail shows.
    pub fn detail_task(&self) -> Option<&Value> {
        let id = self.detail?;
        let ledger = self.ledger.as_ref()?;
        ["open", "recent"]
            .iter()
            .filter_map(|key| ledger[key].as_array())
            .flatten()
            .find(|task| task["id"].as_u64() == Some(id))
    }

    /// The owner's actions on the task whose detail shows: the web's rule (`owner_actions`),
    /// read off the ledger row.
    pub fn detail_actions(&self) -> Vec<&'static str> {
        let Some(task) = self.detail_task() else {
            return Vec::new();
        };
        let Ok(state) = serde_json::from_value::<TaskState>(task["state"].clone()) else {
            return Vec::new();
        };
        let waiting = serde_json::from_value::<WaitOn>(task["waiting"]["on"].clone()).ok();
        view::owner_actions(state, waiting, task["status"]["kind"].as_str())
    }

    /// One key in a task's detail. False when it is not an action's key. A land or a hand
    /// asks first (`y` runs it, `n` or `Esc` drops it); `a` opens the manager chat with the
    /// task's number in the message line, as the web's Answer does.
    pub fn detail_key(&mut self, action: &Action) -> bool {
        let Some(id) = self.detail else {
            return false;
        };
        self.notice = None;
        if let Some(what) = self.confirm {
            if matches!(action, Action::ConfirmYes | Action::Select) {
                self.confirm = None;
                self.run(id, what);
            } else if matches!(
                action,
                Action::NextAttention | Action::InputEscape | Action::Quit
            ) {
                self.confirm = None;
            }
            return true;
        }
        let actions = self.detail_actions();
        let offered =
            |names: &[&'static str]| names.iter().copied().find(|name| actions.contains(name));
        match action {
            Action::Land => self.confirm = offered(&["land"]),
            Action::ConfirmHalt => self.confirm = offered(&["handback", "hand"]),
            Action::NextActive => {
                if actions.contains(&"answer") {
                    // ^ Beside the chat, the answer goes to that chat's message line.
                    if self.chat.is_none() {
                        self.open_chat(None);
                    }
                    if let Some(chat) = &mut self.chat {
                        chat.mention(id);
                    }
                    self.region = Region::Chat;
                }
            }
            _ => return false,
        }
        true
    }

    /// Show task `id` beside the chat, keys on it; a number the ledger does not list says so in
    /// the chat.
    pub fn open_task(&mut self, id: u64) {
        self.detail = Some(id);
        self.scroll = 0;
        self.confirm = None;
        self.notice = None;
        self.region = Region::Task;
        // ^ Before the first read lands, `apply` checks the number.
        if self.ledger.is_some() && self.detail_task().is_none() {
            self.task_gone(id);
        }
    }

    /// Task `id`, shown, is not in the ledger: close it, and say so in a chat it shows beside.
    fn task_gone(&mut self, id: u64) {
        self.detail = None;
        self.region = Region::Chat;
        if let Some(chat) = &mut self.chat {
            chat.notice = Some((format!("No task #{id} in this project"), true));
        }
    }

    /// A task shows beside the open chat (its ledger row is read).
    pub fn task_beside(&self) -> bool {
        self.chat.is_some() && self.detail_task().is_some()
    }

    /// The region that takes the keys while the chat is open: the rail, the task (once its row
    /// is read), else the chat.
    pub fn focus(&self) -> Region {
        match self.region {
            Region::Rail => Region::Rail,
            Region::Task if self.task_beside() => Region::Task,
            _ => Region::Chat,
        }
    }

    /// The rail shows beside the chat (the pane is `RAIL_MIN` wide).
    pub fn rail_shown(&self) -> bool {
        self.width.get() >= RAIL_MIN
    }

    /// The task beside the chat is drawn: beside it from `BESIDE_MIN`, else in the chat's place
    /// while it has the keys.
    pub fn task_drawn(&self) -> bool {
        self.task_beside() && (self.width.get() >= BESIDE_MIN || self.focus() == Region::Task)
    }

    /// One key on the task beside the chat: `Esc` closes it, `tab` gives the keys on, `j`/`k`
    /// scroll, and the detail's actions run (`detail_key`). The chat takes typing (its input
    /// mode), so the detail's letters come as characters: `L`, `H`, `a`, and `y`/`n` for a
    /// question.
    pub fn task_key(&mut self, action: &Action) {
        if self.confirm.is_none() {
            match action {
                Action::InputEscape | Action::Quit => {
                    self.detail = None;
                    self.region = Region::Chat;
                    return;
                }
                // ^ The cycle of `tab`: chat, task, rail (when drawn), chat.
                Action::InputTab => {
                    self.region = if self.rail_shown() {
                        Region::Rail
                    } else {
                        Region::Chat
                    };
                    return;
                }
                Action::NavigateDown | Action::InputChar('j') => {
                    self.scroll = self.scroll.saturating_add(1);
                    return;
                }
                Action::NavigateUp | Action::InputChar('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                    return;
                }
                _ => {}
            }
        }
        let action = match action {
            Action::InputChar('L') => &Action::Land,
            Action::InputChar('H') => &Action::ConfirmHalt,
            Action::InputChar('a') => &Action::NextActive,
            Action::InputChar('y') => &Action::ConfirmYes,
            Action::InputChar('n') => &Action::NextAttention,
            action => action,
        };
        self.detail_key(action);
    }

    /// The rail's items, built at each ledger read (`rail_list`).
    pub fn rail_items(&self) -> &[RailItem] {
        &self.rail_list
    }

    /// The main chat's name, as the ledger marks it.
    fn main_chat(&self) -> Option<&str> {
        self.ledger.as_ref().and_then(main_chat)
    }

    /// A chat by its key: the main chat's name is the manager (none), as `open_chat(None)`
    /// opens it.
    fn chat_key(&self, chat: Option<&str>) -> Option<String> {
        chat.filter(|name| Some(*name) != self.main_chat())
            .map(str::to_string)
    }

    /// The rail item of the open chat.
    pub fn rail_current(&self) -> Option<usize> {
        let open = self.chat_key(self.chat.as_ref()?.chat.as_deref());
        self.rail_list.iter().position(|item| match &item.target {
            RailTarget::Manager => open.is_none(),
            RailTarget::Chat(name) => open.as_deref() == Some(name.as_str()),
            RailTarget::ForYou => false,
        })
    }

    /// `^g`: the keys go to the rail, its open chat selected.
    pub fn focus_rail(&mut self) {
        self.rail = self.rail_current().unwrap_or(0);
        self.region = Region::Rail;
    }

    /// One key on the rail: `j`/`k` move, `Enter` opens the item (another chat closes the task
    /// beside this one), `Esc` or `tab` gives the keys back to the chat.
    pub fn rail_key(&mut self, action: &Action) {
        let last = self.rail_list.len().saturating_sub(1);
        match action {
            Action::NavigateDown | Action::InputChar('j') => self.rail = (self.rail + 1).min(last),
            Action::NavigateUp | Action::InputChar('k') => self.rail = self.rail.saturating_sub(1),
            Action::InputEscape | Action::InputTab => self.region = Region::Chat,
            Action::Select => {
                self.rail = self.rail.min(last);
                self.region = Region::Chat;
                if self.rail_current() == Some(self.rail) {
                    return;
                }
                let target = self
                    .rail_list
                    .get(self.rail)
                    .map(|item| item.target.clone());
                self.detail = None;
                match target {
                    Some(RailTarget::Manager) => self.open_chat(None),
                    Some(RailTarget::Chat(name)) => self.open_chat(Some(name)),
                    Some(RailTarget::ForYou) => {
                        self.stash_draft();
                        self.chat = None;
                        self.set_you(true);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// Run the owner action `what` of task `id` as the CLI does, off the UI thread; its
    /// answer comes through `poll`.
    fn run(&mut self, id: u64, what: &'static str) {
        if self.acting.is_some() {
            return;
        }
        let doing = match what {
            "land" => "Landing",
            "hand" => "Taking over",
            _ => "Handing back",
        };
        let argv = view::action_argv(what, id);
        let project = self.project.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        self.acting = Some(format!("{doing} #{id}…"));
        self.answer = Some(rx);
        std::thread::spawn(move || {
            let answer = std::env::current_exe()
                .map_err(|error| error.to_string())
                .and_then(|wsx| owner::run_as_owner(&wsx, &project, argv));
            let _ = tx.send(match answer {
                Ok(done) => (
                    done["message"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("#{id} {what} done")),
                    false,
                ),
                Err(error) => (format!("#{id} not done: {error}"), true),
            });
        });
    }
}

/// The state word of a ledger entry: "paused" for a worker whose turn ended with a status
/// (`status.kind`), else its state (docs/tasks.md "Status").
pub fn state_word(task: &Value) -> String {
    match task["status"]["kind"].as_str() {
        Some("paused") => "paused",
        _ => task["state"].as_str().unwrap_or_default(),
    }
    .to_string()
}

/// The open tasks as the ledger has them (wsx-core `task::chains`: bands, then chains, a task
/// before the tasks that wait for it), each under its band's heading; then the listed chats and
/// the dismissed count; then the recent landed and closed tasks, newest first. Nothing here sorts
/// or counts again.
pub fn rows(ledger: &Value) -> Vec<TaskRow> {
    let list = |key: &str| ledger[key].as_array().cloned().unwrap_or_default();
    let open = list("open");
    let row = |task: &Value, section| TaskRow {
        id: task["id"].as_u64().unwrap_or_default(),
        chat: String::new(),
        title: task["title"].as_str().unwrap_or_default().to_string(),
        state: state_word(task),
        // ^ The words of wsx-core `task::status` (docs/tasks.md "Status").
        // ^ With where it goes (`flow.phrase`: held, after, step, children, into), as the web's row.
        status: [&task["status"]["label"], &task["flow"]["phrase"]]
            .iter()
            .filter_map(|word| word.as_str().filter(|word| !word.is_empty()))
            .collect::<Vec<_>>()
            .join(" · "),
        counts: Default::default(),
        section,
        root: task["flow"]["place"]["root"].as_u64().unwrap_or_default(),
        depth: task["flow"]["place"]["depth"].as_u64().unwrap_or_default() as u8,
    };
    let projects = |chat: &Value| -> Vec<String> {
        chat["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_string))
            .collect()
    };
    let chat = |chat: &Value| TaskRow {
        id: 0,
        title: chat["shown"].as_str().unwrap_or_default().to_string(),
        chat: chat["name"].as_str().unwrap_or_default().to_string(),
        state: chat["state"].as_str().unwrap_or_default().to_string(),
        // ^ The chat's own model first, as the room's heading shows it.
        status: [
            wsx_core::task::view::agent_label(chat),
            // ^ A chat that runs in more projects than this one names them (`chats::rows`).
            Some(projects(chat))
                .filter(|names| names.len() > 1)
                .map(|names| names.join(", ")),
            chat["goal"].as_str().map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" · "),
        counts: serde_json::from_value(chat["counts"].clone()).unwrap_or_default(),
        section: Section::Chats,
        root: 0,
        depth: 0,
    };
    // ^ The chats as Home lists them: those with something for the owner first, then the newest
    // message; the dismissed ones (`listed`, `chats_home::listed`) are one `+N dismissed` row.
    let mut chats = list("chats");
    chats.sort_by_key(|chat| {
        (
            chat["counts"]["needs"].as_u64().unwrap_or(0) == 0,
            std::cmp::Reverse(chat["last_unix_ms"].as_u64().unwrap_or(0)),
        )
    });
    let (listed, dismissed): (Vec<_>, Vec<_>) = chats
        .iter()
        .partition(|chat| chat["listed"].as_bool().unwrap_or(true));
    // ^ The owner's folders (set on the web's Home) are headings of their own: a folder where its
    // first chat is, the chats of no folder last under "No folder" once a folder exists (wsx-core `chats_home::sections`).
    let listed = wsx_core::task::chats_home::sections(listed, |chat| {
        chat["folder"].as_str().map(str::to_string)
    });
    let headed = listed.iter().any(|section| section.name.is_some());
    let dismissed =
        (!dismissed.is_empty()).then(|| TaskRow::note(format!("+{} dismissed", dismissed.len())));
    // ^ The For you list is one row here: "N wait for you" opens it. A task that needs the owner
    // stays in its chain under Waiting (the chain rows keep their place), as the ledger lists it.
    let you = list("for_you");
    let entry = (!you.is_empty()).then(|| TaskRow {
        id: 0,
        title: format!("{} wait for you", you.len()),
        chat: String::new(),
        state: String::new(),
        status: String::new(),
        counts: Default::default(),
        section: Section::Entry,
        root: 0,
        depth: 0,
    });
    entry
        .into_iter()
        .chain(open.iter().map(|task| {
            row(
                task,
                match task["flow"]["place"]["band"].as_str() {
                    Some("waiting") => Section::Waiting,
                    _ => Section::Moving,
                },
            )
        }))
        .chain(
            listed
                .into_iter()
                .flat_map(|section| {
                    section
                        .name
                        .or_else(|| headed.then(|| "No folder".to_string()))
                        .map(|name| TaskRow::note(format!("▾ {name}")))
                        .into_iter()
                        .chain(
                            section
                                .items
                                .into_iter()
                                .flat_map(|found| std::iter::once(chat(found))),
                        )
                        .collect::<Vec<_>>()
                })
                .chain(dismissed),
        )
        .chain(list("recent").iter().map(|task| row(task, Section::Recent)))
        .collect()
}

/// The For you list (`ledger.for_you`, wsx-core `needs::for_you`): the items that wait for the
/// owner across the open chats, by chat; a task item keeps its number, the rest open their chat.
pub fn rows_you(ledger: &Value) -> Vec<TaskRow> {
    let mut items = ledger["for_you"].as_array().cloned().unwrap_or_default();
    items.sort_by_key(|item| item["chat"].as_str().unwrap_or_default().to_string());
    items
        .iter()
        .map(|item| {
            let kind = item["kind"].as_str().unwrap_or_default();
            let task = matches!(kind, "approval" | "ask" | "check")
                .then_some(0)
                .or(item["task"].as_u64())
                .unwrap_or_default();
            TaskRow {
                id: task,
                title: item["label"].as_str().unwrap_or_default().to_string(),
                chat: item["chat"].as_str().unwrap_or_default().to_string(),
                state: kind.to_string(),
                status: item["chat"].as_str().unwrap_or_default().to_string(),
                counts: Default::default(),
                section: Section::You,
                root: 0,
                depth: 0,
            }
        })
        .collect()
}

/// The open tasks of `id`'s chain, in the list's order (the rows with its head's number).
pub fn chain_of(rows: &[TaskRow], id: u64) -> Vec<&TaskRow> {
    let Some(root) = rows.iter().find(|row| row.id == id && row.root != 0) else {
        return Vec::new();
    };
    rows.iter().filter(|row| row.root == root.root).collect()
}

/// The events message that told of `view`'s last run's end, for the routine detail's Enter
/// (`chat::run_messages`); none for a run that made no message or a project without a manager.
pub fn run_message(
    project: &str,
    main: &std::path::Path,
    view: &wsx_core::routine::RoutineView,
) -> Option<u64> {
    let run = view.latest_run.as_ref()?;
    let dir = store::ChatDir::main(&store::project_id(project, main)).ok()??;
    wsx_core::task::chat::run_messages(&dir, &[run]).pop()?
}

/// The main chat's name in `ledger`'s chats.
fn main_chat(ledger: &Value) -> Option<&str> {
    ledger["chats"]
        .as_array()?
        .iter()
        .find(|chat| chat["main"].as_bool() == Some(true))?["name"]
        .as_str()
}

/// The chat rail (docs/tasks.md "TUI"): the manager (with the main chat's counts), the open
/// chats as the list orders them with their counts, and For you with the number that waits, as
/// the list's "N wait for you" row counts it.
fn rail_list(ledger: Option<&Value>) -> Vec<RailItem> {
    let mut items = vec![RailItem {
        title: "Manager".into(),
        counts: Default::default(),
        target: RailTarget::Manager,
    }];
    let mut waiting = 0;
    if let Some(ledger) = ledger {
        let main = main_chat(ledger);
        for row in rows(ledger) {
            if row.section != Section::Chats || row.chat.is_empty() || row.state != "open" {
                continue;
            }
            if Some(row.chat.as_str()) == main {
                items[0].counts = row.counts;
                continue;
            }
            items.push(RailItem {
                title: row.title,
                counts: row.counts,
                target: RailTarget::Chat(row.chat),
            });
        }
        waiting = ledger["for_you"].as_array().map_or(0, Vec::len);
    }
    items.push(RailItem {
        title: "For you".into(),
        counts: wsx_core::task::needs::ChatCounts {
            needs: waiting,
            ..Default::default()
        },
        target: RailTarget::ForYou,
    });
    items
}
