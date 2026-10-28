//! The TUI's task list: the project's ledger, read off the UI thread and read
//! again when a file of its task store changes. ref: docs/tasks.md "TUI"
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::task::{
    ledger,
    store::{self, Stamp},
};

/// How often the store's files are compared with the last read.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// A check's answer: none when no stamp changed, else the new stamps and the ledger read.
type Read = Option<(Stamp, Result<Value, String>)>;

/// One row: an open task in attention order, an open or asked role, then a recent landed or
/// closed task. A role row has no number; its title is the role's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub id: u64,
    pub title: String,
    pub state: String,
    /// What it waits for, or a role's brief; empty when nothing.
    pub waits: String,
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
    pub ledger: Option<Value>,
    pub rows: Vec<TaskRow>,
    pub error: Option<String>,
    pub selected: usize,
    /// The task whose detail shows, by number, and its scroll.
    pub detail: Option<u64>,
    pub scroll: u16,
    /// The project's manager chat, opened over the list with `m`.
    pub chat: Option<Box<crate::task_chat::ManagerChat>>,
    stamp: Option<Stamp>,
    pending: Option<mpsc::Receiver<Read>>,
    last_check: Instant,
}

impl TaskList {
    pub fn new(project: String, main: PathBuf) -> Self {
        let store = store::project_id(&project, &main);
        let mut list = Self {
            project,
            main,
            store,
            ledger: None,
            rows: Vec::new(),
            error: None,
            selected: 0,
            detail: None,
            scroll: 0,
            chat: None,
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
        let (project, main, store, last) = (
            self.project.clone(),
            self.main.clone(),
            self.store.clone(),
            self.stamp.clone(),
        );
        self.pending = Some(rx);
        self.last_check = Instant::now();
        std::thread::spawn(move || {
            let now = store::stamp(&store);
            let read = (last.as_ref() != Some(&now)).then(|| {
                let ledger = ledger::build(&project, &main).map_err(|error| format!("{error:#}"));
                (now, ledger)
            });
            let _ = tx.send(read);
        });
    }

    /// Open the manager's chat, or a role's (docs/tasks.md "Roles").
    pub fn open_chat(&mut self, role: Option<String>) {
        self.chat = Some(Box::new(crate::task_chat::ManagerChat::new(
            self.project.clone(),
            self.main.clone(),
            role,
        )));
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
        waits: waits(task),
        section,
    };
    let role = |role: &Value| TaskRow {
        id: 0,
        title: role["name"].as_str().unwrap_or_default().to_string(),
        state: role["state"].as_str().unwrap_or_default().to_string(),
        waits: role["brief"].as_str().unwrap_or_default().to_string(),
        section: Section::Roles,
    };
    open.iter()
        .map(|task| row(task, Section::Open))
        .chain(list("roles").iter().map(role))
        .chain(list("recent").iter().map(|task| row(task, Section::Recent)))
        .collect()
}

/// What a task waits for, in a few words: `owner: <reason>` or the reason it is held.
pub fn waits(task: &Value) -> String {
    if let Some(on) = task["waiting"]["on"].as_str() {
        let reason = task["waiting"]["reason"].as_str().unwrap_or_default();
        return if reason.is_empty() {
            on.to_string()
        } else {
            format!("{on}: {reason}")
        };
    }
    task["hold"]
        .as_str()
        .map(|hold| format!("try first: {hold}"))
        .unwrap_or_default()
}
