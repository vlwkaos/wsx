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

/// One row: an open task in attention order, an open chat, then a recent landed or
/// closed task. A chat row has no number; its title is the chat's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: u64,
    pub title: String,
    pub state: String,
    /// The status label (wsx-core `task::status`), or a chat's goal; empty when nothing.
    pub status: String,
    /// A chat's pill counts, marked after its name; zero for a task.
    pub counts: wsx_core::task::needs::ChatCounts,
    pub section: Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Open,
    Chats,
    Recent,
}

impl Section {
    pub fn heading(self) -> &'static str {
        match self {
            Self::Open => "Open",
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
    /// The task whose detail shows, by number, and its scroll.
    pub detail: Option<u64>,
    pub scroll: u16,
    /// The day rows show tokens, not cost (`u`).
    pub tokens: bool,
    /// The agent the spend lines and the day rows count (`a`): an adapter or
    /// `adapter/model` of `usage_all.agents`; none for all.
    pub agent: Option<String>,
    /// The project's manager chat, opened over the list with `m`.
    pub chat: Option<Box<crate::task_chat::ManagerChat>>,
    /// The New chat form, opened with `n` over the list.
    pub new_chat: Option<crate::task_new_chat::NewChat>,
    /// The project's overview, opened with `o` over the list or the chat.
    pub overview: Option<Overview>,
    /// The `?` popup over the list, the detail or the chat.
    pub help: Option<crate::ui::help::HelpView>,
    /// A land or a hand of the task shown, waiting for `y` (the action's name in `owner_actions`).
    pub confirm: Option<&'static str>,
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
            help: None,
            confirm: None,
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
        let usage_due = self
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
        self.chat = Some(Box::new(crate::task_chat::ManagerChat::new(
            self.project.clone(),
            self.main.clone(),
            chat,
        )));
    }

    /// `P` in the chat: the manager chat of the registered project `name`, in place of this
    /// view. False when no registered project has that name.
    pub fn switch_to(&mut self, name: &str) -> bool {
        let Some((name, main)) = self
            .projects
            .iter()
            .find(|(other, _)| other == name)
            .cloned()
        else {
            return false;
        };
        let mut list = Self::new(name, main, std::mem::take(&mut self.projects));
        list.open_chat(None);
        *self = list;
        true
    }

    /// `n`: the New chat form, whose `default` agent is the project's manager's.
    pub fn open_new_chat(&mut self) {
        let default = wsx_core::config::project::load_task_config(&self.main)
            .ok()
            .and_then(|config| {
                wsx_core::task::view::agent_label(&wsx_core::task::view::default_agent(&config))
            });
        self.new_chat = Some(crate::task_new_chat::NewChat::new(&self.store, default));
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
            .map(|row| row.title.as_str())
    }

    /// `Enter`: an open chat, else a task's detail.
    pub fn open_selected(&mut self) {
        match (self.selected_chat(), self.rows.get(self.selected)) {
            (Some(chat), _) => self.open_chat(Some(chat.to_string())),
            (None, Some(row)) if row.section != Section::Chats => self.open_detail(),
            _ => {}
        }
    }

    /// Take a finished read and start the next check when due. True when the view changed.
    /// An open chat reads instead of the list.
    pub fn poll(&mut self) -> bool {
        if let Some(chat) = &mut self.chat {
            return chat.poll();
        }
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
                    .map(|row| (row.id, row.title.clone()));
                self.rows = rows(&ledger);
                // ^ The selection and the detail follow their task, not its position.
                self.selected = selected
                    .and_then(|(id, title)| {
                        self.rows
                            .iter()
                            .position(|row| row.id == id && (id != 0 || row.title == title))
                    })
                    .unwrap_or(self.selected)
                    .min(self.rows.len().saturating_sub(1));
                if self
                    .detail
                    .is_some_and(|id| !self.rows.iter().any(|row| row.id == id))
                {
                    self.detail = None;
                }
                self.ledger = Some(ledger);
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
            Some(_) => None,
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
    /// read off the ledger row, the project's trial and its `tasks.preview`.
    pub fn detail_actions(&self) -> Vec<&'static str> {
        let (Some(task), Some(ledger)) = (self.detail_task(), &self.ledger) else {
            return Vec::new();
        };
        let Ok(state) = serde_json::from_value::<TaskState>(task["state"].clone()) else {
            return Vec::new();
        };
        let waiting = serde_json::from_value::<WaitOn>(task["waiting"]["on"].clone()).ok();
        let trial = &ledger["deploys"]["trial"];
        let stage = if trial["task"].as_u64() == self.detail {
            trial["stage"].as_str()
        } else {
            None
        };
        view::owner_actions(
            state,
            waiting,
            task["status"]["kind"].as_str(),
            stage,
            ledger["preview"].as_bool().unwrap_or(false),
        )
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
            Action::Land => self.confirm = offered(&["pass", "land"]),
            Action::ConfirmHalt => self.confirm = offered(&["handback", "hand"]),
            Action::GroupManager => {
                if let Some(what) = offered(&["trial_end", "try"]) {
                    self.run(id, what);
                }
            }
            Action::NextActive => {
                if actions.contains(&"answer") {
                    self.open_chat(None);
                    if let Some(chat) = &mut self.chat {
                        chat.mention(id);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// Run the owner action `what` of task `id` as the CLI does, off the UI thread; its
    /// answer comes through `poll`.
    fn run(&mut self, id: u64, what: &'static str) {
        if self.acting.is_some() {
            return;
        }
        let (doing, verb, flag) = match what {
            "land" => ("Landing", "land", None),
            "pass" => ("Landing", "land", Some("--tried")),
            "hand" => ("Taking over", "hand", None),
            "handback" => ("Handing back", "handback", None),
            "trial_end" => ("Ending the trial of", "trial", Some("--end")),
            _ => ("Starting the preview of", "preview", None),
        };
        let mut argv = vec!["task".to_string(), verb.into(), id.to_string()];
        argv.extend(flag.map(String::from));
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
                        .or_else(|| done["url"].as_str().map(|url| format!("Preview: {url}")))
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

/// Open tasks first, waiting on the owner, failed, ready, checking, then working; then the
/// open chats; then the recent landed and closed tasks, newest first as the
/// ledger has them.
pub fn rows(ledger: &Value) -> Vec<TaskRow> {
    let rank = |state: &str| match state {
        "waiting" => 0,
        "failed" => 1,
        "ready" => 2,
        "checking" => 3,
        _ => 4,
    };
    let list = |key: &str| ledger[key].as_array().cloned().unwrap_or_default();
    let mut open = list("open");
    open.sort_by_key(|task| rank(&state_word(task)));
    let row = |task: &Value, section| TaskRow {
        id: task["id"].as_u64().unwrap_or_default(),
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
    };
    let chat = |chat: &Value| TaskRow {
        id: 0,
        title: chat["name"].as_str().unwrap_or_default().to_string(),
        state: chat["state"].as_str().unwrap_or_default().to_string(),
        // ^ The chat's own model first, as the room's heading shows it.
        status: [
            wsx_core::task::view::agent_label(chat),
            chat["goal"].as_str().map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" · "),
        counts: serde_json::from_value(chat["counts"].clone()).unwrap_or_default(),
        section: Section::Chats,
    };
    open.iter()
        .map(|task| row(task, Section::Open))
        .chain(list("chats").iter().map(chat))
        .chain(list("recent").iter().map(|task| row(task, Section::Recent)))
        .collect()
}

/// The events message that told of `view`'s last run's end, for the routine detail's Enter
/// (`chat::run_messages`); none for a run that made no message or a project without a manager.
pub fn run_message(
    project: &str,
    main: &std::path::Path,
    view: &wsx_core::routine::RoutineView,
) -> Option<u64> {
    let run = view.latest_run.as_ref()?;
    let dir = store::ManagerDir::existing(&store::project_id(project, main)).ok()??;
    wsx_core::task::chat::run_messages(&dir, &[run]).pop()?
}
