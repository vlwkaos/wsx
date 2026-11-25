// Input box with cursor movement, unicode support, and project completion (wsx_core::project_search).

use crate::ui::{popup_block, popup_upper, theme};
use ratatui::{
    prelude::*,
    widgets::{Clear, Paragraph},
};
use wsx_core::project_search::suggestions;

pub struct InputState {
    pub buffer: String,
    pub cursor: usize, // byte offset
    pub prompt: String,
    pub completions: Vec<String>,
    pub completion_idx: Option<usize>,
    typed: String, // last text the user typed (before completion navigation)
    project_search: bool,
    // Snapshot of the app's repo cache at modal open. Stays static for the
    // lifetime of the modal; the App owns the live scanner and refreshes
    // between opens.
    scan_dirs: Vec<String>,
}

impl InputState {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self::make(prompt.into(), String::new())
    }

    /// Open with a pre-populated snapshot from the app's repo cache. The
    /// app refreshes its cache in the background; the modal sees results
    /// captured at open time. Type a path with `/` or `~` to bypass the
    /// cache entirely and use live filesystem completion.
    pub fn new_project_search(prompt: impl Into<String>, cached_repos: Vec<String>) -> Self {
        let mut s = Self::make(prompt.into(), String::new());
        s.project_search = true;
        s.completions = suggestions("", &cached_repos);
        s.scan_dirs = cached_repos;
        s
    }

    pub fn with_value(prompt: impl Into<String>, value: String) -> Self {
        Self::make(prompt.into(), value)
    }

    fn make(prompt: String, value: String) -> Self {
        let cursor = value.len();
        Self {
            buffer: value.clone(),
            cursor,
            prompt,
            completions: vec![],
            completion_idx: None,
            typed: value,
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
            self.completions = suggestions(&self.buffer, &self.scan_dirs);
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
                self.completions = suggestions(&self.buffer, &self.scan_dirs);
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

    pub fn display_cursor(&self) -> usize {
        self.buffer[..self.cursor].chars().count()
    }
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
    fn narrow_input_keeps_its_prompt() {
        // ^ At 20 columns the box degrades without an underflow and the field's prompt stays.
        let backend = ratatui::backend::TestBackend::new(20, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let state = InputState::new("> ");

        terminal
            .draw(|frame| render_input(frame, frame.area(), &state, "Name"))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("> "), "{screen:?}");
    }
}
