//! State of the pane view band and reader. No I/O and no rendering.
//! ^ docs/terminal-view.md: the panel overlays Terminal mode and never resizes the PTY.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{layout::Rect, text::Line};
use std::collections::HashMap;
use wsx_core::runtime::{PaneId, PaneView, PaneViewPage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewOpen {
    #[default]
    Closed,
    /// Top rows of the terminal; keys still reach the terminal.
    Band,
    /// Full terminal viewport; keys scroll the document.
    Reader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchKey {
    pub pane_id: PaneId,
    pub page: String,
    pub revision: u64,
    pub columns: u16,
}

pub struct LoadedView {
    pub key: FetchKey,
    pub lines: Vec<Line<'static>>,
}

#[derive(Default)]
pub struct ViewPanel {
    open: ViewOpen,
    pane: Option<PaneId>,
    /// `None` follows the newest page.
    page: Option<String>,
    scroll: usize,
    /// Rendered pages of the current pane, so turning a page needs no fetch.
    loaded: Vec<LoadedView>,
    pub pending: Option<FetchKey>,
    /// A key that failed is not requested again until the document changes.
    pub failed: Option<FetchKey>,
    seen: HashMap<PaneId, u64>,
    /// Set by rendering; the same rectangle hit-tests the mouse.
    pub area: Rect,
}

pub const BAND_WHEEL_LINES: usize = 2;
const MIN_TERMINAL_ROWS: u16 = 6;

/// Rows the band covers, including its one status row. Zero when it cannot fit.
pub fn band_height(content_lines: usize, terminal_rows: u16) -> u16 {
    if terminal_rows < MIN_TERMINAL_ROWS {
        return 0;
    }
    let limit = usize::from((terminal_rows / 3).max(3));
    u16::try_from((content_lines.max(1) + 1).min(limit)).unwrap_or(0)
}

fn newest(view: &PaneView) -> u64 {
    view.pages
        .iter()
        .map(|page| page.revision)
        .max()
        .unwrap_or(0)
}

pub enum ReaderKey {
    Close,
    Handled,
}

impl ViewPanel {
    pub fn open_for(&self, pane_id: PaneId) -> ViewOpen {
        if self.pane == Some(pane_id) {
            self.open
        } else {
            ViewOpen::Closed
        }
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn page<'a>(&self, view: &'a PaneView) -> Option<&'a PaneViewPage> {
        self.page
            .as_deref()
            .filter(|_| self.pane == Some(view.pane_id))
            .and_then(|name| view.pages.iter().find(|page| page.name == name))
            .or_else(|| view.pages.first())
    }

    pub fn page_position(&self, view: &PaneView) -> (usize, usize) {
        let current = self.page(view).map(|page| page.name.as_str());
        let index = view
            .pages
            .iter()
            .position(|page| Some(page.name.as_str()) == current)
            .unwrap_or(0);
        (index + 1, view.pages.len())
    }

    pub fn fetch_key(&self, view: &PaneView, columns: u16) -> Option<FetchKey> {
        self.page(view).map(|page| FetchKey {
            pane_id: view.pane_id,
            page: page.name.clone(),
            revision: page.revision,
            columns,
        })
    }

    /// The first page that is not rendered yet: the shown page, then the others.
    pub fn next_fetch(&self, view: &PaneView, columns: u16) -> Option<FetchKey> {
        let shown = self.fetch_key(view, columns)?;
        std::iter::once(shown.clone())
            .chain(view.pages.iter().map(|page| FetchKey {
                page: page.name.clone(),
                revision: page.revision,
                ..shown.clone()
            }))
            .find(|key| {
                self.failed.as_ref() != Some(key)
                    && !self.loaded.iter().any(|loaded| loaded.key == *key)
            })
    }

    /// Store a rendered page when it still belongs to `view` at this width.
    pub fn store(&mut self, view: &PaneView, columns: u16, loaded: LoadedView) -> bool {
        let current = loaded.key.pane_id == view.pane_id
            && loaded.key.columns == columns
            && view
                .pages
                .iter()
                .any(|page| page.name == loaded.key.page && page.revision == loaded.key.revision);
        if current {
            self.loaded.retain(|other| {
                other.key.pane_id == view.pane_id
                    && other.key.page != loaded.key.page
                    && view.pages.iter().any(|page| page.name == other.key.page)
            });
            self.loaded.push(loaded);
        }
        current
    }

    /// Lines for `key`. Until a new revision or width is rendered, the previous
    /// rendering of the same page stays on screen.
    pub fn lines(&self, key: &FetchKey) -> Option<&[Line<'static>]> {
        self.loaded
            .iter()
            .find(|loaded| loaded.key == *key)
            .or_else(|| {
                self.loaded
                    .iter()
                    .find(|loaded| loaded.key.pane_id == key.pane_id && loaded.key.page == key.page)
            })
            .map(|loaded| loaded.lines.as_slice())
    }

    pub fn unread(&self, view: &PaneView) -> bool {
        newest(view) > self.seen.get(&view.pane_id).copied().unwrap_or(0)
    }

    fn show(&mut self, view: &PaneView, open: ViewOpen) {
        if self.pane != Some(view.pane_id) {
            self.pane = Some(view.pane_id);
            self.page = None;
            self.scroll = 0;
        }
        self.open = open;
        self.seen.insert(view.pane_id, newest(view));
    }

    /// Prefix+v: closed, then band, then reader, then band again.
    pub fn toggle(&mut self, view: &PaneView) {
        let next = match self.open_for(view.pane_id) {
            ViewOpen::Closed | ViewOpen::Reader => ViewOpen::Band,
            ViewOpen::Band => ViewOpen::Reader,
        };
        self.show(view, next);
    }

    pub fn close(&mut self) {
        self.open = ViewOpen::Closed;
    }

    /// A new revision arrived. Opens the band only when the user enabled auto-open.
    pub fn document_changed(&mut self, view: &PaneView, selected: bool, auto_open: bool) {
        if !selected {
            return;
        }
        match self.open_for(view.pane_id) {
            ViewOpen::Closed if auto_open && self.unread(view) => self.show(view, ViewOpen::Band),
            ViewOpen::Closed => {}
            ViewOpen::Band | ViewOpen::Reader => {
                self.seen.insert(view.pane_id, newest(view));
            }
        }
    }

    /// Drop state for panes whose view is gone.
    pub fn retain(&mut self, views: &[PaneView]) {
        self.seen
            .retain(|pane_id, _| views.iter().any(|view| view.pane_id == *pane_id));
        if self
            .pane
            .is_some_and(|pane_id| !views.iter().any(|view| view.pane_id == pane_id))
        {
            *self = Self {
                seen: std::mem::take(&mut self.seen),
                ..Self::default()
            };
        }
    }

    pub fn clamp(&mut self, total: usize, visible: usize) {
        self.scroll = self.scroll.min(total.saturating_sub(visible));
    }

    pub fn scroll_by(&mut self, delta: isize, total: usize, visible: usize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp(total, visible);
    }

    fn turn_page(&mut self, view: &PaneView, delta: isize) {
        let (position, count) = self.page_position(view);
        if count < 2 {
            return;
        }
        let next = (position - 1).saturating_add_signed(delta).min(count - 1);
        self.page = Some(view.pages[next].name.clone());
        self.scroll = 0;
    }

    /// Reader keys never reach the terminal.
    pub fn reader_key(
        &mut self,
        key: KeyEvent,
        view: &PaneView,
        total: usize,
        visible: usize,
    ) -> ReaderKey {
        if key.kind == KeyEventKind::Release {
            return ReaderKey::Handled;
        }
        let page = isize::try_from(visible.saturating_sub(1).max(1)).unwrap_or(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return ReaderKey::Close,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1, total, visible),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1, total, visible),
            KeyCode::Char(' ') | KeyCode::PageDown => self.scroll_by(page, total, visible),
            KeyCode::Char('b') | KeyCode::PageUp => self.scroll_by(-page, total, visible),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = total.saturating_sub(visible),
            KeyCode::Char(']') => self.turn_page(view, 1),
            KeyCode::Char('[') => self.turn_page(view, -1),
            _ => {}
        }
        ReaderKey::Handled
    }

    pub fn leave_reader_if_open(&mut self) {
        if self.open == ViewOpen::Reader {
            self.leave_reader();
        }
    }

    /// Reader `q`/Esc returns to the band.
    pub fn leave_reader(&mut self) {
        self.open = ViewOpen::Band;
        self.scroll = 0;
    }
}
