// Input box with cursor movement, unicode support, and path completion.

use std::path::PathBuf;

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use crate::ui::popup_upper;

pub struct InputState {
    pub buffer: String,
    pub cursor: usize, // byte offset
    pub prompt: String,
    pub completions: Vec<String>,
    pub completion_idx: Option<usize>,
    typed: String,     // last text the user typed (before completion navigation)
    path_mode: bool,
}

impl InputState {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self::make(prompt.into(), String::new(), false)
    }

    pub fn new_path(prompt: impl Into<String>, initial: String) -> Self {
        let mut s = Self::make(prompt.into(), initial, true);
        s.typed = s.buffer.clone();
        s.completions = path_completions(&s.buffer);
        s
    }

    pub fn with_value(prompt: impl Into<String>, value: String) -> Self {
        Self::make(prompt.into(), value, false)
    }

    fn make(prompt: String, value: String, path_mode: bool) -> Self {
        let cursor = value.len();
        Self {
            buffer: value.clone(),
            cursor,
            prompt,
            completions: vec![],
            completion_idx: None,
            typed: value,
            path_mode,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.typed = self.buffer.clone();
        self.completion_idx = None;
        if self.path_mode {
            self.completions = path_completions(&self.buffer);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.buffer[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buffer.drain(prev..self.cursor);
            self.cursor = prev;
            self.typed = self.buffer.clone();
            self.completion_idx = None;
            if self.path_mode {
                self.completions = path_completions(&self.buffer);
            }
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.buffer[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor < self.buffer.len() {
            let c = self.buffer[self.cursor..].chars().next().unwrap();
            self.cursor += c.len_utf8();
        }
    }

    pub fn value(&self) -> &str {
        &self.buffer
    }

    /// Move selection down; wraps around. Tab calls this too.
    pub fn select_next(&mut self) {
        if self.completions.is_empty() { return; }
        let next = match self.completion_idx {
            None => 0,
            Some(i) => (i + 1) % self.completions.len(),
        };
        self.completion_idx = Some(next);
        self.buffer = self.completions[next].clone();
        self.cursor = self.buffer.len();
        self.maybe_drill_down();
    }

    /// Move selection up. At index 0, goes back to typed text.
    pub fn select_prev(&mut self) {
        if self.completions.is_empty() { return; }
        let prev = match self.completion_idx {
            None => Some(self.completions.len().saturating_sub(1)),
            Some(0) => None,
            Some(i) => Some(i - 1),
        };
        self.completion_idx = prev;
        self.buffer = match prev {
            None => self.typed.clone(),
            Some(i) => self.completions[i].clone(),
        };
        self.cursor = self.buffer.len();
        self.maybe_drill_down();
    }

    /// If the current buffer ends with '/' and has only one child match,
    /// or was just selected as a unique completion, show children immediately.
    fn maybe_drill_down(&mut self) {
        if self.buffer.ends_with('/') {
            let children = path_completions(&self.buffer);
            if !children.is_empty() {
                self.typed = self.buffer.clone();
                self.completions = children;
                self.completion_idx = None;
            }
        }
    }

    fn display_cursor(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
}

// ── Completion logic ──────────────────────────────────────────────────────────

/// Subsequence fuzzy match. Returns score if all query chars appear in order
/// in target (case-insensitive). Higher score = better match.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() { return Some(0); }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = target.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut consecutive = 0i32;
    for (ti, &tc) in t.iter().enumerate() {
        if qi < q.len() && tc == q[qi] {
            consecutive += 1;
            score += 1 + consecutive; // base + consecutive bonus
            if ti == 0 { score += 4; } // prefix match bonus
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    if qi == q.len() { Some(score) } else { None }
}

fn path_completions(input: &str) -> Vec<String> {
    let (expanded, tilde) = expand_input(input);

    let (parent, prefix) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let p = expanded.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| expanded.clone());
        let pfx = expanded.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (p, pfx)
    };

    let Ok(rd) = std::fs::read_dir(&parent) else { return vec![] };

    let mut scored: Vec<(i32, String)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !prefix.starts_with('.') { return None; }
            let score = fuzzy_score(&prefix, &name)?;
            Some((score, display_path(&parent.join(&name), tilde)))
        })
        .collect();

    if prefix.is_empty() {
        scored.sort_by(|a, b| a.1.cmp(&b.1));
    } else {
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }

    scored.into_iter().map(|(_, path)| path).collect()
}

fn expand_input(input: &str) -> (PathBuf, bool) {
    if let Some(home) = dirs::home_dir() {
        if input.starts_with("~/") {
            return (home.join(&input[2..]), true);
        }
        if input == "~" {
            return (home, true);
        }
    }
    (PathBuf::from(if input.is_empty() { "." } else { input }), false)
}

fn display_path(path: &PathBuf, prefer_tilde: bool) -> String {
    if prefer_tilde {
        if let Some(home) = dirs::home_dir() {
            if let Ok(rel) = path.strip_prefix(&home) {
                let rel_str = rel.to_string_lossy();
                return if rel_str.is_empty() {
                    "~/".to_string()
                } else {
                    format!("~/{}/", rel_str)
                };
            }
        }
    }
    format!("{}/", path.to_string_lossy())
}

// ── Rendering ────────────────────────────────────────────────────────────────

pub fn render_input(frame: &mut Frame, area: Rect, state: &InputState, title: &str) {
    let width = area.width.min(60);
    let popup = popup_upper(area, width, 3);

    frame.render_widget(Clear, popup);

    let display = format!("{}{}", state.prompt, state.buffer);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title))
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(display).block(block);
    frame.render_widget(para, popup);

    let cursor_col = state.prompt.len() + state.display_cursor();
    let cursor_x = popup.x + 1 + cursor_col as u16;
    frame.set_cursor_position((cursor_x.min(popup.x + popup.width - 2), popup.y + 1));

    if !state.completions.is_empty() {
        let max_show = 10usize.min(state.completions.len());
        let drop_h = max_show as u16 + 2;
        let drop_y = popup.y + 3;
        if drop_y + drop_h <= area.y + area.height {
            let drop = Rect::new(popup.x, drop_y, width, drop_h);
            frame.render_widget(Clear, drop);

            let items: Vec<ListItem> = state.completions.iter().take(max_show).enumerate()
                .map(|(i, s)| {
                    let selected = state.completion_idx == Some(i);
                    let style = if selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(format!(" {} ", s)).style(style)
                })
                .collect();

            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Gray)));
            frame.render_widget(list, drop);
        }
    }
}
