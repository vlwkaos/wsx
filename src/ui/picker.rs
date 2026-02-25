// Simple list picker overlay (no fuzzy filtering). Reserved for future use.
#![allow(dead_code)]

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
};
use crate::ui::popup_center;

pub struct PickerState {
    pub title: String,
    pub items: Vec<String>,
    pub list_state: ListState,
}

impl PickerState {
    pub fn new(title: impl Into<String>, items: Vec<String>) -> Self {
        let mut list_state = ListState::default();
        if !items.is_empty() {
            list_state.select(Some(0));
        }
        Self { title: title.into(), items, list_state }
    }

    pub fn navigate_up(&mut self) {
        if self.items.is_empty() { return; }
        let i = self.list_state.selected().unwrap_or(0);
        let next = if i == 0 { self.items.len() - 1 } else { i - 1 };
        self.list_state.select(Some(next));
    }

    pub fn navigate_down(&mut self) {
        if self.items.is_empty() { return; }
        let i = self.list_state.selected().unwrap_or(0);
        let next = (i + 1) % self.items.len();
        self.list_state.select(Some(next));
    }

    pub fn selected_item(&self) -> Option<&str> {
        let i = self.list_state.selected()?;
        self.items.get(i).map(|s| s.as_str())
    }
}

pub fn render_picker(frame: &mut Frame, area: Rect, state: &mut PickerState) {
    let width = area.width.min(60).max(30);
    let height = area.height.min(20).max(6);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = state.items.iter().map(|s| ListItem::new(s.as_str())).collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", state.title))
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan));

    frame.render_stateful_widget(list, popup, &mut state.list_state);
}
