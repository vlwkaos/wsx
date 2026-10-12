//! Pane view documents: bounded, ephemeral, carried across a live handoff.
//! ^ docs/terminal-view.md: wsxd owns the documents; clients fetch a body on demand.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wsx_core::runtime::{
    valid_view_page_name, ApiError, PaneId, PaneView, PaneViewPage, PluginSurface, ViewBlock,
    DEFAULT_VIEW_PAGE, MAX_VIEW_BYTES, MAX_VIEW_PAGES, MAX_VIEW_TITLE_BYTES,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Page {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    pub revision: u64,
    pub markdown: String,
    /// Renderer-plugin output for one width; recomputed on demand after a handoff.
    #[serde(skip)]
    pub rendered: Option<(u16, Vec<ViewBlock>)>,
}

impl Page {
    pub fn summary(&self) -> PaneViewPage {
        PaneViewPage {
            name: self.name.clone(),
            title: self.title.clone(),
            revision: self.revision,
            lines: u32::try_from(self.markdown.lines().count()).unwrap_or(u32::MAX),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaneEntry {
    pane_id: PaneId,
    /// Newest first.
    pages: Vec<Page>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct Store {
    #[serde(default)]
    last_revision: u64,
    #[serde(default)]
    panes: Vec<PaneEntry>,
}

fn invalid(message: &str) -> ApiError {
    ApiError::new("invalid_view", message)
}

fn has_control(value: &str, allow_layout: bool) -> bool {
    value.chars().any(|character| {
        character.is_control() && !(allow_layout && matches!(character, '\n' | '\t'))
    })
}

pub(super) fn page_name(page: Option<String>) -> Result<String, ApiError> {
    let name = page.unwrap_or_else(|| DEFAULT_VIEW_PAGE.into());
    if valid_view_page_name(&name) {
        Ok(name)
    } else {
        Err(invalid(
            "page name must be 1..32 characters of letters, digits, '-', '_' or '.'",
        ))
    }
}

impl Store {
    /// Replace or add one page. The oldest page is dropped beyond `MAX_VIEW_PAGES`.
    pub fn set(
        &mut self,
        pane_id: PaneId,
        page: Option<String>,
        title: Option<String>,
        markdown: String,
    ) -> Result<(), ApiError> {
        let name = page_name(page)?;
        let title = title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty());
        if title
            .as_deref()
            .is_some_and(|title| title.len() > MAX_VIEW_TITLE_BYTES || has_control(title, false))
        {
            return Err(invalid("title must be at most 120 bytes of printable text"));
        }
        if markdown.trim().is_empty() {
            return Err(invalid(
                "document is empty; use `wsx view clear` to remove a page",
            ));
        }
        if markdown.len() > MAX_VIEW_BYTES {
            return Err(invalid("document is larger than 65536 bytes"));
        }
        if has_control(&markdown, true) {
            return Err(invalid("document contains terminal control characters"));
        }
        self.last_revision = self.last_revision.saturating_add(1);
        let index = match self.panes.iter().position(|entry| entry.pane_id == pane_id) {
            Some(index) => index,
            None => {
                self.panes.push(PaneEntry {
                    pane_id,
                    pages: Vec::new(),
                });
                self.panes.len() - 1
            }
        };
        let pages = &mut self.panes[index].pages;
        pages.retain(|page| page.name != name);
        pages.insert(
            0,
            Page {
                name,
                title,
                revision: self.last_revision,
                markdown,
                rendered: None,
            },
        );
        pages.truncate(MAX_VIEW_PAGES);
        Ok(())
    }

    /// Remove one page, or every page of the pane. Returns whether anything changed.
    pub fn clear(&mut self, pane_id: PaneId, page: Option<String>) -> Result<bool, ApiError> {
        let name = page.map(|page| page_name(Some(page))).transpose()?;
        let Some(index) = self.panes.iter().position(|entry| entry.pane_id == pane_id) else {
            return Ok(false);
        };
        let pages = &mut self.panes[index].pages;
        let before = pages.len();
        match name {
            Some(name) => pages.retain(|page| page.name != name),
            None => pages.clear(),
        }
        let changed = pages.len() != before;
        if pages.is_empty() {
            self.panes.remove(index);
        }
        Ok(changed)
    }

    pub fn remove_pane(&mut self, pane_id: PaneId) {
        self.panes.retain(|entry| entry.pane_id != pane_id);
    }

    pub fn retain_panes(&mut self, live: &HashSet<PaneId>) {
        self.panes.retain(|entry| live.contains(&entry.pane_id));
    }

    /// The named page, or the newest page when `page` is `None`.
    pub fn page(&self, pane_id: PaneId, page: Option<&str>) -> Option<&Page> {
        let pages = &self
            .panes
            .iter()
            .find(|entry| entry.pane_id == pane_id)?
            .pages;
        match page {
            Some(name) => pages.iter().find(|page| page.name == name),
            None => pages.first(),
        }
    }

    pub fn cache_rendered(
        &mut self,
        pane_id: PaneId,
        name: &str,
        revision: u64,
        columns: u16,
        blocks: Vec<ViewBlock>,
    ) {
        if let Some(page) = self
            .panes
            .iter_mut()
            .find(|entry| entry.pane_id == pane_id)
            .and_then(|entry| entry.pages.iter_mut().find(|page| page.name == name))
            .filter(|page| page.revision == revision)
        {
            page.rendered = Some((columns, blocks));
        }
    }

    pub fn summaries(&self, live: &HashSet<PaneId>) -> Vec<PaneView> {
        let mut views = self
            .panes
            .iter()
            .filter(|entry| live.contains(&entry.pane_id))
            .map(|entry| PaneView {
                pane_id: entry.pane_id,
                surface: PluginSurface::TerminalTop,
                pages: entry.pages.iter().map(Page::summary).collect(),
            })
            .collect::<Vec<_>>();
        views.sort_by_key(|view| view.pane_id);
        views
    }
}
