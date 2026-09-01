// Input box with cursor movement, unicode support, and path/project completion.

use std::path::{Path, PathBuf};

use crate::ui::{popup_block, popup_upper, theme};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};

pub struct InputState {
    pub buffer: String,
    pub cursor: usize, // byte offset
    pub prompt: String,
    pub completions: Vec<String>,
    pub completion_idx: Option<usize>,
    typed: String, // last text the user typed (before completion navigation)
    path_mode: bool,
    project_search: bool,
    // Snapshot of the app's repo cache at modal open. Stays static for the
    // lifetime of the modal; the App owns the live scanner and refreshes
    // between opens.
    scan_dirs: Vec<String>,
}

impl InputState {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self::make(prompt.into(), String::new(), false)
    }

    /// Open with a pre-populated snapshot from the app's repo cache. The
    /// app refreshes its cache in the background; the modal sees results
    /// captured at open time. Type a path with `/` or `~` to bypass the
    /// cache entirely and use live filesystem completion.
    pub fn new_project_search(prompt: impl Into<String>, cached_repos: Vec<String>) -> Self {
        let mut s = Self::make(prompt.into(), String::new(), false);
        s.project_search = true;
        s.completions = project_search_completions("", &cached_repos);
        s.scan_dirs = cached_repos;
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
            project_search: false,
            scan_dirs: vec![],
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.typed = self.buffer.clone();
        self.completion_idx = None;
        if self.project_search {
            self.completions = project_search_completions(&self.buffer, &self.scan_dirs);
        } else if self.path_mode {
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
            if self.project_search {
                self.completions = project_completions(&self.buffer, &self.scan_dirs);
            } else if self.path_mode {
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
        if self.completions.is_empty() {
            return;
        }
        let next = match self.completion_idx {
            None => 0,
            Some(i) => (i + 1) % self.completions.len(),
        };
        self.completion_idx = Some(next);
        self.buffer = self.completions[next].clone();
        self.cursor = self.buffer.len();
    }

    /// Move selection up. At index 0, goes back to typed text.
    pub fn select_prev(&mut self) {
        if self.completions.is_empty() {
            return;
        }
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
    }

    fn display_cursor(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
}

// ── Completion logic ──────────────────────────────────────────────────────────

/// Subsequence fuzzy match. Returns score if all query chars appear in order
/// in target (case-insensitive). Higher score = better match.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = target.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut qi = 0;
    let mut score = 0i32;
    let mut consecutive = 0i32;
    for (ti, &tc) in t.iter().enumerate() {
        if qi < q.len() && tc == q[qi] {
            consecutive += 1;
            score += 1 + consecutive; // base + consecutive bonus
            if ti == 0 {
                score += 4;
            } // prefix match bonus
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    if qi == q.len() {
        Some(score)
    } else {
        None
    }
}

/// Path-like input (contains `/` or starts with `~`) gets filesystem
/// completion — the background scanner can miss repos outside `~`, below
/// `MAX_SCAN_DEPTH`, or under `SKIP_DIRS`. Bare names stay on fuzzy match
/// against the cached scan. `register_project` validates "is a git repo"
/// on submit, so filesystem suggestions can include any directory.
fn project_search_completions(query: &str, dirs: &[String]) -> Vec<String> {
    if query.contains('/') || query.starts_with('~') {
        path_completions(query)
    } else {
        project_completions(query, dirs)
    }
}

fn project_completions(query: &str, dirs: &[String]) -> Vec<String> {
    if query.is_empty() {
        let mut refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
        refs.sort_unstable();
        return refs.iter().map(|s| s.to_string()).collect();
    }
    let mut scored: Vec<(i32, &str)> = dirs
        .iter()
        .filter_map(|d| {
            let rel = d.strip_prefix("~/").unwrap_or(d);
            let rel = rel.strip_suffix('/').unwrap_or(rel);
            fuzzy_score(query, rel).map(|s| (s, d.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored.into_iter().map(|(_, d)| d.to_string()).collect()
}

fn path_completions(input: &str) -> Vec<String> {
    let (expanded, tilde) = expand_input(input);

    let (parent, prefix) = if input.ends_with('/') {
        (expanded.clone(), String::new())
    } else {
        let p = expanded
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| expanded.clone());
        let pfx = expanded
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        (p, pfx)
    };

    let Ok(rd) = std::fs::read_dir(&parent) else {
        return vec![];
    };

    let mut scored: Vec<(i32, String)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
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
        if let Some(stripped) = input.strip_prefix("~/") {
            return (home.join(stripped), true);
        }
        if input == "~" {
            return (home, true);
        }
    }
    (
        PathBuf::from(if input.is_empty() { "." } else { input }),
        false,
    )
}

fn display_path(path: &Path, prefer_tilde: bool) -> String {
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

    let max_show = if state.completions.is_empty() {
        0
    } else {
        5usize.min(state.completions.len())
    };
    let scroll_offset = match state.completion_idx {
        Some(i) if i >= max_show => i - max_show + 1,
        _ => 0,
    };

    // Single box: input line + completion rows inside one border
    let popup_h = 3 + max_show as u16;
    let popup = popup_upper(area, width, popup_h);

    frame.render_widget(Clear, popup);

    let hints = Line::from(vec![
        Span::styled(" Enter confirm", Style::default().fg(theme::TEXT)),
        Span::styled("  Esc cancel ", Style::default().fg(theme::TEXT_MUTED)),
    ]);
    let block = popup_block(
        Line::from(format!(" {} ", title)),
        hints,
        Style::default().fg(theme::ACCENT),
    );
    frame.render_widget(block, popup);
    if popup.width < 2 || popup.height < 2 {
        return;
    }

    // Input line
    let display = format!("{}{}", state.prompt, state.buffer);
    let input_row = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        1.min(popup.height.saturating_sub(1)),
    );
    frame.render_widget(Paragraph::new(display), input_row);

    let cursor_col = state.prompt.len() + state.display_cursor();
    let cursor_x = popup.x + 1 + cursor_col as u16;
    frame.set_cursor_position((
        cursor_x.min(popup.x + popup.width.saturating_sub(2)),
        popup.y.saturating_add(1),
    ));

    // Completion items — left-aligned with where user types
    let prompt_w = state.prompt.chars().count() as u16;
    let comp_x = popup.x + 1 + prompt_w;
    let comp_w = popup.width.saturating_sub(2 + prompt_w);

    if max_show > 0 {
        for (vis_idx, (orig_idx, s)) in state
            .completions
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(max_show)
            .enumerate()
        {
            let y = popup.y + 2 + vis_idx as u16;
            let selected = state.completion_idx == Some(orig_idx);
            let style = if selected {
                theme::accent_selection()
            } else {
                Style::default().fg(theme::TEXT_MUTED)
            };
            let row = Rect::new(comp_x, y, comp_w, 1);
            frame.render_widget(Paragraph::new(s.as_str()).style(style), row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_actions_render_on_the_bottom_border() {
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let state = InputState::new("> ");

        terminal
            .draw(|frame| render_input(frame, frame.area(), &state, "Name"))
            .unwrap();

        let rows = (0..12)
            .map(|y| {
                (0..60)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let bottom_y = rows
            .iter()
            .position(|row| row.contains("Enter confirm"))
            .expect("input hints");
        let bottom = &rows[bottom_y];
        assert!(bottom.contains("Esc cancel"), "{bottom:?}");
        assert_eq!(
            terminal.backend().buffer()[(0, bottom_y as u16)].symbol(),
            "└"
        );
        assert_eq!(
            terminal.backend().buffer()[(59, bottom_y as u16)].symbol(),
            "┘"
        );
        for (y, row) in rows.iter().enumerate() {
            if y != bottom_y {
                assert!(!row.contains("Enter confirm"), "{row:?}");
                assert!(!row.contains("Esc cancel"), "{row:?}");
            }
        }
    }

    #[test]
    fn narrow_input_render_is_safe() {
        let backend = ratatui::backend::TestBackend::new(20, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let state = InputState::new("> ");

        terminal
            .draw(|frame| render_input(frame, frame.area(), &state, "Name"))
            .unwrap();
    }
}
