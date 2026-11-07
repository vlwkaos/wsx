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
    ledger, now_unix_ms,
    store::{self, Stamp},
    usage,
};

/// How often the store's files are compared with the last read.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// A check's answer: none when no stamp changed, else the new stamps and the ledger read.
type Read = Option<(Stamp, Result<Value, String>)>;

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

/// One row: an open task in attention order, an open or asked role, then a recent landed or
/// closed task. A role row has no number; its title is the role's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: u64,
    pub title: String,
    pub state: String,
    /// The status label (wsx-core `task::status`), or a role's brief; empty when nothing.
    pub status: String,
    pub section: Section,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Open,
    Roles,
    Recent,
}

impl Section {
    pub fn heading(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Roles => "Roles",
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
    /// The project's overview, opened with `o` over the list or the chat.
    pub overview: Option<Overview>,
    stamp: Option<Stamp>,
    pending: Option<mpsc::Receiver<Read>>,
    last_check: Instant,
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
            overview: None,
            stamp: None,
            pending: None,
            last_check: Instant::now(),
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
        self.pending = Some(rx);
        self.last_check = Instant::now();
        std::thread::spawn(move || {
            let now = store::stamp(&store);
            let read = (last.as_ref() != Some(&now)).then(|| {
                let mut ledger =
                    ledger::build(&project, &main).map_err(|error| format!("{error:#}"));
                // ^ Every project's spend summed, read with the ledger: it follows this
                // project's changes, not the others'. Both count the chosen agent's turns.
                // ref: docs/tasks.md "TUI"
                if let Ok(ledger) = &mut ledger {
                    let stores: Vec<(String, String)> = projects
                        .iter()
                        .map(|(name, path)| (name.clone(), store::project_id(name, path)))
                        .collect();
                    let now = now_unix_ms();
                    ledger["usage"] = usage::all(&[(project, store)], now, &filter);
                    ledger["usage_all"] = usage::all(&stores, now, &filter);
                }
                (now, ledger)
            });
            let _ = tx.send(read);
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
        self.check();
    }

    /// Open the manager's chat, or a role's (docs/tasks.md "Roles").
    pub fn open_chat(&mut self, role: Option<String>) {
        self.chat = Some(Box::new(crate::task_chat::ManagerChat::new(
            self.project.clone(),
            self.main.clone(),
            role,
        )));
    }

    /// `o`: the overview as it is now; a missing file or a failed read shows as words.
    pub fn open_overview(&mut self) {
        self.overview = Some(Overview::new(
            store::read_overview(&self.store).map_err(|error| format!("{error:#}")),
        ));
    }

    /// The selected row's open role, whose chat `Enter` opens.
    pub fn selected_role(&self) -> Option<&str> {
        self.rows
            .get(self.selected)
            .filter(|row| row.section == Section::Roles && row.state == "open")
            .map(|row| row.title.as_str())
    }

    /// `Enter`: an open role's chat, else a task's detail; an asked role waits for the owner.
    pub fn open_selected(&mut self) {
        match (self.selected_role(), self.rows.get(self.selected)) {
            (Some(role), _) => self.open_chat(Some(role.to_string())),
            (None, Some(row)) if row.section != Section::Roles => self.open_detail(),
            _ => {}
        }
    }

    /// Take a finished read and start the next check when due. True when the view changed.
    /// An open chat reads instead of the list.
    pub fn poll(&mut self) -> bool {
        if let Some(chat) = &mut self.chat {
            return chat.poll();
        }
        if let Some(rx) = &self.pending {
            return match rx.try_recv() {
                Ok(read) => {
                    self.pending = None;
                    read.is_some_and(|(stamp, ledger)| self.apply(stamp, ledger))
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
        false
    }

    fn apply(&mut self, stamp: Stamp, ledger: Result<Value, String>) -> bool {
        match ledger {
            Ok(ledger) => {
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
}

/// Open tasks first, waiting on the owner, failed, ready, checking, then working; then the
/// open and asked roles; then the recent landed and closed tasks, newest first as the
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
    open.sort_by_key(|task| rank(task["state"].as_str().unwrap_or_default()));
    let row = |task: &Value, section| TaskRow {
        id: task["id"].as_u64().unwrap_or_default(),
        title: task["title"].as_str().unwrap_or_default().to_string(),
        state: task["state"].as_str().unwrap_or_default().to_string(),
        // ^ The words of wsx-core `task::status` (docs/tasks.md "Status").
        // ^ With the reason of a wait for another task (`status.wait`), as the web's row.
        status: [&task["status"]["label"], &task["status"]["wait"]]
            .iter()
            .filter_map(|word| word.as_str().filter(|word| !word.is_empty()))
            .collect::<Vec<_>>()
            .join(" · "),
        section,
    };
    let role = |role: &Value| TaskRow {
        id: 0,
        title: role["name"].as_str().unwrap_or_default().to_string(),
        state: role["state"].as_str().unwrap_or_default().to_string(),
        status: role["brief"].as_str().unwrap_or_default().to_string(),
        section: Section::Roles,
    };
    open.iter()
        .map(|task| row(task, Section::Open))
        .chain(list("roles").iter().map(role))
        .chain(list("recent").iter().map(|task| row(task, Section::Recent)))
        .collect()
}
