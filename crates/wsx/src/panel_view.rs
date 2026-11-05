//! The TUI's panel view: a project's panels, one at a time, read from wsxd off the UI thread.
//! It asks for a run on open, on `R`, and when the result is older than the panel's `refresh`.
//! ref: docs/panels.md
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};
use wsx_core::panel::{self, Action, PanelInfo, PanelStatus, Response};

/// A view read while a run is in flight is read again this often.
const RUNNING_POLL: Duration = Duration::from_secs(1);
/// A read that failed is tried again this often.
const RETRY: Duration = Duration::from_secs(10);
/// The age on screen is drawn again this often.
const REDRAW: Duration = Duration::from_secs(1);

pub struct PanelsView {
    pub project: String,
    path: PathBuf,
    /// None until wsxd answered the list.
    pub panels: Option<Vec<PanelInfo>>,
    pub notice: Option<String>,
    pub selected: usize,
    pub shown: Option<panel::PanelView>,
    pub error: Option<String>,
    pub scroll: u16,
    /// ^ The furthest scroll and the height of the last frame, which `j` uses.
    pub max_scroll: Cell<u16>,
    pending: Option<mpsc::Receiver<Result<Response, String>>>,
    /// The next read starts a run.
    run_next: bool,
    read_at: Instant,
    last_poll: Instant,
    drawn: Instant,
}

impl PanelsView {
    pub fn new(project: String, path: PathBuf) -> Self {
        let mut view = Self {
            project,
            path,
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
        view.fetch(Action::List);
        view
    }

    fn fetch(&mut self, action: Action) {
        let (tx, rx) = mpsc::sync_channel(1);
        let path = self.path.clone();
        self.pending = Some(rx);
        self.last_poll = Instant::now();
        std::thread::spawn(move || {
            let client = wsx_core::runtime::Client::local();
            let _ = tx.send(panel::request(&client, &path, action));
        });
    }

    fn current(&self) -> Option<&PanelInfo> {
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
                        Ok(Response::Panels { panels, notice }) => {
                            self.panels = Some(panels);
                            self.notice = notice;
                            self.error = None;
                        }
                        Ok(Response::Panel(view)) => {
                            self.shown = Some(view);
                            self.read_at = Instant::now();
                            self.error = None;
                        }
                        Ok(Response::Error(message)) | Err(message) => self.error = Some(message),
                    }
                }
            }
        }
        if self.pending.is_none() {
            if let Some(info) = self.current().cloned() {
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
                    self.fetch(Action::Get {
                        name: info.name,
                        run,
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
