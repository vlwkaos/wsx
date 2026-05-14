use crate::config::global::GlobalConfig;
use crate::ui::popup_center;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render_tab_manager(
    frame: &mut Frame,
    area: Rect,
    selected: usize,
    config: &GlobalConfig,
    active_tab: Option<&str>,
) {
    let tabs = config.ordered_tabs();
    let h = (tabs.len() as u16 + 4).min(area.height);
    let popup = popup_center(area, 42, h);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Tabs ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    for (i, tab) in tabs.iter().enumerate() {
        let name = tab.unwrap_or("default");
        let count = if let Some(tab_str) = tab {
            config
                .projects
                .iter()
                .filter(|p| p.tab.as_deref() == Some(tab_str))
                .count()
        } else {
            config.projects.iter().filter(|p| p.tab.is_none()).count()
        };
        let is_cur_tab = tab.as_deref() == active_tab;
        let cursor = if i == selected { "▶" } else { " " };
        let active_mark = if is_cur_tab { "●" } else { " " };
        let text = format!("{} {} {} ({})", cursor, active_mark, name, count);
        let style = if i == selected {
            Style::default().fg(Color::Black).bg(Color::Yellow).bold()
        } else if is_cur_tab {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
