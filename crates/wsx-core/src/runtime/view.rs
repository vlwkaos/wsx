//! Pane view documents and fenced-block renderer plugins. See docs/terminal-view.md.
use super::{PaneId, PluginSurface, PluginTone};
use serde::{Deserialize, Serialize};

pub const DEFAULT_VIEW_PAGE: &str = "main";
pub const MAX_VIEW_BYTES: usize = 64 * 1024;
pub const MAX_VIEW_PAGES: usize = 4;
pub const MAX_VIEW_TITLE_BYTES: usize = 120;
pub const MAX_VIEW_PAGE_NAME_BYTES: usize = 32;
pub const MAX_VIEW_COLUMNS: u16 = 500;
pub const VIEW_RENDERER_API_VERSION: u32 = 1;
pub const MAX_VIEW_BLOCK_LINES: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneViewPage {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Increases with every accepted `pane_view_set`; survives a live handoff.
    pub revision: u64,
    pub lines: u32,
}

/// Pages are ordered newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneView {
    pub pane_id: PaneId,
    pub surface: PluginSurface,
    pub pages: Vec<PaneViewPage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSpan {
    pub text: String,
    #[serde(default)]
    pub tone: PluginTone,
}

/// Plugin output for the fenced block at `index` in `fenced_blocks` order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewBlock {
    pub index: usize,
    pub lines: Vec<Vec<ViewSpan>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneViewDocument {
    pub pane_id: PaneId,
    pub surface: PluginSurface,
    pub page: PaneViewPage,
    pub columns: u16,
    pub markdown: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ViewBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRendererSpec {
    pub api_version: u32,
    /// Fenced-block languages this plugin renders, for example `mermaid`.
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRenderRequest {
    pub api_version: u32,
    pub language: String,
    pub source: String,
    pub columns: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRenderResponse {
    pub lines: Vec<Vec<ViewSpan>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub fn valid_view_page_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_VIEW_PAGE_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One line of a view document as both wsxd and the TUI must classify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewLine<'a> {
    Text(&'a str),
    /// Opening fence of block `index`; `language` is the first info-string word.
    FenceOpen {
        index: usize,
        language: &'a str,
    },
    Code(&'a str),
    FenceClose,
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = rest.bytes().take_while(|byte| *byte == marker).count();
    (length >= 3).then(|| (marker, length, &rest[length..]))
}

/// Classify every line. An unclosed fence runs to the end of the document.
pub fn view_lines(markdown: &str) -> Vec<ViewLine<'_>> {
    let mut lines = Vec::new();
    let mut open: Option<(u8, usize)> = None;
    let mut next_index = 0;
    for line in markdown.lines() {
        match open {
            Some((marker, length)) => {
                let closes = fence_marker(line).is_some_and(|(candidate, count, rest)| {
                    candidate == marker && count >= length && rest.trim().is_empty()
                });
                if closes {
                    open = None;
                    lines.push(ViewLine::FenceClose);
                } else {
                    lines.push(ViewLine::Code(line));
                }
            }
            None => match fence_marker(line) {
                // A backtick fence cannot carry a backtick in its info string.
                Some((marker, length, info)) if marker != b'`' || !info.contains('`') => {
                    let index = next_index;
                    next_index += 1;
                    open = Some((marker, length));
                    lines.push(ViewLine::FenceOpen {
                        index,
                        language: info.split_whitespace().next().unwrap_or(""),
                    });
                }
                _ => lines.push(ViewLine::Text(line)),
            },
        }
    }
    lines
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedBlock {
    pub index: usize,
    pub language: String,
    pub source: String,
}

pub fn fenced_blocks(markdown: &str) -> Vec<FencedBlock> {
    let mut blocks: Vec<FencedBlock> = Vec::new();
    for line in view_lines(markdown) {
        match line {
            ViewLine::FenceOpen { index, language } => blocks.push(FencedBlock {
                index,
                language: language.to_ascii_lowercase(),
                source: String::new(),
            }),
            ViewLine::Code(text) => {
                if let Some(block) = blocks.last_mut() {
                    block.source.push_str(text);
                    block.source.push('\n');
                }
            }
            ViewLine::Text(_) | ViewLine::FenceClose => {}
        }
    }
    blocks
}
