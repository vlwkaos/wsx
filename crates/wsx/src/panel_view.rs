//! The TUI's panel view: a project's panels one at a time, or the machine card (wsxd's built-in
//! panel with every project's machine panels, `panel::machine_view`), read from wsxd off the UI thread.
//! It asks for a run on open, on `R`, and when the result is older than the panel's `refresh`.
//! ref: docs/panels.md
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::panel::{self, PanelInfo, PanelScope, PanelStatus};

/// A view read while a run is in flight is read again this often.
const RUNNING_POLL: Duration = Duration::from_secs(1);
/// A read that failed is tried again this often.
const RETRY: Duration = Duration::from_secs(10);
/// The age on screen is drawn again this often.
const REDRAW: Duration = Duration::from_secs(1);

/// One panel of the project; none for the machine card.
pub struct Entry {
    path: Option<PathBuf>,
    pub info: PanelInfo,
}

/// What a finished read brings.
enum Answer {
    List(Vec<Entry>, Option<String>),
    Panel(panel::PanelView),
}

pub struct PanelsView {
    pub title: String,
    /// The project to list; none for the machine card (`M`).
    source: Option<PathBuf>,
    /// The projects whose machine panels join the machine card.
    machine: Option<Vec<(String, PathBuf)>>,
    /// None until wsxd answered the list.
    pub panels: Option<Vec<Entry>>,
    pub notice: Option<String>,
    pub selected: usize,
    pub shown: Option<panel::PanelView>,
    pub error: Option<String>,
    pub scroll: u16,
    /// ^ The furthest scroll and the height of the last frame, which `j` uses.
    pub max_scroll: Cell<u16>,
    pending: Option<mpsc::Receiver<Result<Answer, String>>>,
    /// The next read starts a run.
    run_next: bool,
    read_at: Instant,
    last_poll: Instant,
    drawn: Instant,
}

impl PanelsView {
    /// One project's panels.
    pub fn new(project: String, path: PathBuf) -> Self {
        Self::open(format!("{project} panels"), Some(path), None)
    }

    /// wsxd's built-in machine panel with the machine panels of these projects.
    pub fn machine(projects: Vec<(String, PathBuf)>) -> Self {
        Self::open("Machine".into(), None, Some(projects))
    }

    fn open(
        title: String,
        source: Option<PathBuf>,
        machine: Option<Vec<(String, PathBuf)>>,
    ) -> Self {
        let mut view = Self {
            title,
            source,
            machine,
            panels: None,
            notice: None,
            selected: 0,
            shown: None,
            error: None,
            scroll: 0,
            max_scroll: Cell::new(0),
            pending: None,
            run_next: true,
            read_at: Instant::now(),
            last_poll: Instant::now(),
            drawn: Instant::now(),
        };
        view.fetch_list();
        view
    }

    fn fetch(&mut self, job: impl FnOnce() -> Result<Answer, String> + Send + 'static) {
        let (tx, rx) = mpsc::sync_channel(1);
        self.pending = Some(rx);
        self.last_poll = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    /// Ask wsxd for the project's panels, or for the built-in machine panel.
    fn fetch_list(&mut self) {
        let path = self.source.clone();
        let machine = self.machine.is_some();
        self.fetch(move || {
            let client = wsx_core::runtime::Client::local();
            let (panels, notice) = panel::list(&client, path.as_deref())?;
            let entries = (panels.into_iter())
                .filter(|info| !machine || info.scope == PanelScope::Machine)
                .take(if machine { 1 } else { usize::MAX })
                .map(|info| Entry {
                    path: path.clone(),
                    info,
                })
                .collect();
            Ok(Answer::List(entries, notice.filter(|_| !machine)))
        });
    }

    fn current(&self) -> Option<&Entry> {
        self.panels.as_ref()?.get(self.selected)
    }

    /// Milliseconds since the shown result finished, by wsxd's clock plus the time since it was read.
    pub fn age_ms(&self) -> u64 {
        self.shown.as_ref().map_or(0, |shown| {
            shown.age_ms + self.read_at.elapsed().as_millis() as u64
        })
    }

    /// Take a finished read and start the next one when due. True when the view changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("the read stopped".into());
                    return true;
                }
                Ok(answer) => {
                    self.pending = None;
                    changed = true;
                    match answer {
                        Ok(Answer::List(entries, notice)) => {
                            self.panels = Some(entries);
                            self.notice = notice;
                            self.error = None;
                        }
                        Ok(Answer::Panel(view)) => {
                            self.shown = Some(view);
                            self.read_at = Instant::now();
                            self.error = None;
                        }
                        Err(message) => self.error = Some(message),
                    }
                }
            }
        }
        if self.pending.is_none() {
            if let Some((path, info)) = self.current().map(|e| (e.path.clone(), e.info.clone())) {
                let waited = self.last_poll.elapsed();
                let (run, due) = match &self.shown {
                    None => (true, self.error.is_none() || waited >= RETRY),
                    Some(shown) => (
                        self.run_next,
                        self.run_next
                            || waited >= RUNNING_POLL
                                && (shown.running
                                    || shown.run.status == PanelStatus::Pending
                                    || info.refresh > 0 && self.age_ms() >= info.refresh * 1000),
                    ),
                };
                if due {
                    self.run_next = false;
                    let machine = self.machine.clone();
                    self.fetch(move || {
                        let client = wsx_core::runtime::Client::local();
                        if let Some(projects) = machine {
                            return panel::machine_view(&client, &projects, run).map(Answer::Panel);
                        }
                        panel::get(&client, path.as_deref(), info.name, run).map(Answer::Panel)
                    });
                }
            }
        }
        if self.drawn.elapsed() >= REDRAW {
            self.drawn = Instant::now();
            changed = true;
        }
        changed
    }

    /// `Tab`, `l`, `h`: the next or the previous panel, run at once.
    pub fn select(&mut self, delta: isize) {
        let count = self.panels.as_ref().map_or(0, Vec::len);
        if count < 2 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.shown = None;
        self.scroll = 0;
        self.run_next = true;
    }

    /// `R`: run the shown panel now.
    pub fn refresh(&mut self) {
        self.run_next = true;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).clamp(0, self.max_scroll.get() as i32);
        self.scroll = next as u16;
    }
}
