//! The TUI's panel view: a project's panels, or wsxd's built-in machine panel and every project's
//! machine panels, one at a time, read from wsxd off the UI thread.
//! It asks for a run on open, on `R`, and when the result is older than the panel's `refresh`.
//! ref: docs/panels.md
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::panel::{self, Action, PanelInfo, PanelScope, PanelStatus};

/// A view read while a run is in flight is read again this often.
const RUNNING_POLL: Duration = Duration::from_secs(1);
/// A read that failed is tried again this often.
const RETRY: Duration = Duration::from_secs(10);
/// The age on screen is drawn again this often.
const REDRAW: Duration = Duration::from_secs(1);

/// One panel and the project that declares it; none for a panel built into wsxd.
pub struct Entry {
    pub project: Option<String>,
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
    /// The projects to list (none: wsxd's built-in panels), and whether only their machine
    /// panels count (`M`).
    sources: Vec<Option<(String, PathBuf)>>,
    pub machine: bool,
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
        Self::open(
            format!("{project} panels"),
            vec![Some((project, path))],
            false,
        )
    }

    /// wsxd's built-in machine panels, then the machine panels of these projects.
    pub fn machine(projects: Vec<(String, PathBuf)>) -> Self {
        let sources = std::iter::once(None).chain(projects.into_iter().map(Some));
        Self::open("Machine panels".into(), sources.collect(), true)
    }

    fn open(title: String, sources: Vec<Option<(String, PathBuf)>>, machine: bool) -> Self {
        let mut view = Self {
            title,
            sources,
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

    /// Ask wsxd for the panels of every source; a machine view keeps the machine-scoped ones.
    /// A source that fails is skipped, unless none answered.
    fn fetch_list(&mut self) {
        let sources = self.sources.clone();
        let machine = self.machine;
        self.fetch(move || {
            let client = wsx_core::runtime::Client::local();
            let (mut entries, mut notice, mut failure) = (Vec::new(), None, None);
            for source in sources {
                let (project, path) = source.unzip();
                match panel::request(&client, path.as_deref(), Action::List) {
                    Ok(panel::Response::Panels {
                        panels,
                        notice: found,
                    }) => {
                        notice = notice.or(found.filter(|_| !machine));
                        entries.extend(
                            panels
                                .into_iter()
                                .filter(|info| !machine || info.scope == PanelScope::Machine)
                                .map(|info| Entry {
                                    project: project.clone(),
                                    path: path.clone(),
                                    info,
                                }),
                        );
                    }
                    Ok(_) => failure = failure.or(Some("unexpected wsxd answer".to_string())),
                    Err(message) => failure = failure.or(Some(message)),
                }
            }
            match failure {
                Some(message) if entries.is_empty() => Err(message),
                _ => Ok(Answer::List(entries, notice)),
            }
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
                    self.fetch(move || {
                        let client = wsx_core::runtime::Client::local();
                        match panel::request(
                            &client,
                            path.as_deref(),
                            Action::Get {
                                name: info.name,
                                run,
                            },
                        )? {
                            panel::Response::Panel(view) => Ok(Answer::Panel(view)),
                            _ => Err("unexpected wsxd answer".into()),
                        }
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
