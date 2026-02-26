// Layout orchestration

pub mod ansi;
pub mod workspace_tree;
pub mod preview;
pub mod input;
pub mod picker;
pub mod confirm;
pub mod config_modal;

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use crate::app::{App, Mode};
use crate::model::workspace::Selection;
use crate::ui::{
    confirm::render_confirm,
    config_modal::render_config_modal,
    input::render_input,
    preview::{render_empty_preview, render_project_preview, render_session_preview, render_worktree_preview},
    workspace_tree::{compute_scroll, render_tree},
};

/// Center a popup of given size within `area`.
pub fn popup_center(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Place a popup in the upper third of `area`.
pub fn popup_upper(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + area.height / 3;
    Rect::new(x, y, w, h)
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let sb_height = status_bar_height(app, area.width);
    let main_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(sb_height));
    let status_area = Rect::new(area.x, area.y + area.height.saturating_sub(sb_height), area.width, sb_height);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(0)])
        .split(main_area);

    let visible_height = chunks[0].height.saturating_sub(2) as usize;
    app.tree_visible_height = visible_height;
    app.tree_scroll = compute_scroll(app.tree_selected, visible_height, app.tree_scroll);
    app.tree_area = chunks[0];
    app.preview_area = chunks[1];

    let is_move_mode = matches!(app.mode, Mode::Move { .. } | Mode::MoveSession { .. });
    render_tree(frame, chunks[0], &app.workspace, app.tree_selected, app.tree_scroll, is_move_mode);

    let preview_area = chunks[1];
    match app.current_selection() {
        Selection::Session(pi, wi, si) => {
            if let Some((sess, title)) = app.workspace.projects.get(pi).and_then(|p| {
                let wt = p.worktrees.get(wi)?;
                let sess = wt.sessions.get(si)?;
                let title = format!("{} › {} › {}", p.name, wt.display_name(), sess.display_name);
                Some((sess.clone(), title))
            }) {
                render_session_preview(frame, preview_area, &sess, &title);
            } else {
                render_empty_preview(frame, preview_area);
            }
        }
        Selection::Worktree(pi, wi) => {
            if let Some((project, worktree, title)) = app.workspace.projects.get(pi)
                .and_then(|p| p.worktrees.get(wi).map(|wt| {
                    let title = format!("{} › {}", p.name, wt.display_name());
                    (p.clone(), wt.clone(), title)
                }))
            {
                render_worktree_preview(frame, preview_area, &project, &worktree, &title);
            } else {
                render_empty_preview(frame, preview_area);
            }
        }
        Selection::Project(pi) => {
            if let Some(project) = app.workspace.projects.get(pi).cloned() {
                render_project_preview(frame, preview_area, &project);
            } else {
                render_empty_preview(frame, preview_area);
            }
        }
        Selection::None => render_empty_preview(frame, preview_area),
    }

    render_status_bar(frame, status_area, app);
    render_overlay(frame, main_area, app);
    if app.loading {
        render_loading(frame, main_area);
    }
}

fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    match &mut app.mode {
        Mode::Input { context, state } => {
            let title = context.title();
            render_input(frame, area, state, title);
        }
        Mode::Confirm { message, .. } => {
            let msg = message.clone();
            render_confirm(frame, area, &msg);
        }
        Mode::Config { project_idx } => {
            let pi = *project_idx;
            if let Some(project) = app.workspace.projects.get(pi) {
                let config = project.config.clone().unwrap_or_default();
                let name = project.name.clone();
                render_config_modal(frame, area, &config, &name);
            }
        }
        Mode::Help => render_help(frame, area),
        Mode::Normal | Mode::Move { .. } | Mode::MoveSession { .. } | Mode::Search { .. } => {}
    }
}

fn get_mode_label(app: &App) -> &'static str {
    match &app.mode {
        Mode::Normal => "NORMAL",
        Mode::Input { .. } => "INPUT",
        Mode::Confirm { .. } => "CONFIRM",
        Mode::Config { .. } => "CONFIG",
        Mode::Move { .. } | Mode::MoveSession { .. } => "MOVE",
        Mode::Help => "HELP",
        Mode::Search { .. } => "SEARCH",
    }
}

fn build_hints(app: &App) -> String {
    let global = "(/)search  (n)ext (N)prev pending  (e)config  (?)help  (q)uit";
    match &app.mode {
        Mode::Normal => match app.current_selection() {
            Selection::Project(_) =>
                format!("(m)ove  (w)orktree  (d)el  (c)lean  ·  {}", global),
            Selection::Worktree(_, _) =>
                format!("(s)ession  (o)run  (r)alias  (d)el  ·  (w)orktree  (c)lean  ·  {}", global),
            Selection::Session(pi, wi, si) => {
                let active = app.workspace.projects.get(pi)
                    .and_then(|p| p.worktrees.get(wi))
                    .and_then(|w| w.sessions.get(si))
                    .map(|s| s.last_activity.map(|t| t.elapsed().as_secs() < crate::app::IDLE_SECS).unwrap_or(false))
                    .unwrap_or(false);
                let dismiss = if active { "" } else { "(x)dismiss  ·  " };
                format!("(m)ove  (r)ename  (d)kill  ·  {}(C-a d)detach  ·  (s)ession  (o)run  ·  (w)orktree  (c)lean  ·  {}", dismiss, global)
            }
            Selection::None => "(p) add project".to_string(),
        },
        Mode::Input { .. } => "Esc: cancel".to_string(),
        Mode::Confirm { .. } => "(y)es  (n)o".to_string(),
        Mode::Config { .. } => "(e)dit .gtrignore  Esc: close".to_string(),
        Mode::Move { .. } | Mode::MoveSession { .. } => "(j/k) reorder  Esc: done".to_string(),
        Mode::Help => "Esc: close".to_string(),
        Mode::Search { .. } => unreachable!(),
    }
}

// Split hints at "  ·  " scope separators to fit within `available_width` chars per line.
fn wrap_hints(hints: &str, available_width: usize) -> Vec<String> {
    let groups: Vec<&str> = hints.split("  ·  ").collect();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for group in &groups {
        if current.is_empty() {
            current = group.to_string();
        } else {
            let candidate = format!("{}  ·  {}", current, group);
            if candidate.len() <= available_width {
                current = candidate;
            } else {
                lines.push(current);
                current = group.to_string();
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn status_bar_height(app: &App, width: u16) -> u16 {
    if matches!(app.mode, Mode::Search { .. }) || app.status_message.is_some() {
        return 1;
    }
    let label = get_mode_label(app);
    let badge_width = label.len() + 4; // " [LABEL] "
    let available = (width as usize).saturating_sub(badge_width + 1);
    let lines = wrap_hints(&build_hints(app), available);
    (lines.len() as u16).max(1)
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // Search mode gets its own full-bar treatment
    if let Mode::Search { query, .. } = &app.mode {
        let spans = vec![
            Span::styled(" [/] ", Style::default().fg(Color::Black).bg(Color::Cyan).bold()),
            Span::styled(format!(" {}_", query), Style::default().fg(Color::White)),
            Span::styled("  Enter: next  Esc: exit", Style::default().fg(Color::DarkGray)),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let label = get_mode_label(app);
    let mode_text = format!(" [{}] ", label);
    let badge_width = mode_text.len();
    let badge_style = Style::default().fg(Color::Black).bg(Color::Yellow).bold();

    let msg = app.status_message.as_deref().unwrap_or("");
    if !msg.is_empty() {
        let spans = vec![
            Span::styled(mode_text, badge_style),
            Span::styled(format!(" {}", msg), Style::default().fg(Color::Cyan)),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let hints = build_hints(app);
    let available = (area.width as usize).saturating_sub(badge_width + 1);
    let hint_lines = wrap_hints(&hints, available);
    let hint_style = Style::default().fg(Color::Gray);

    if hint_lines.len() <= 1 || area.height < 2 {
        let text = hint_lines.first().map(|s| s.as_str()).unwrap_or(&hints);
        let spans = vec![
            Span::styled(mode_text, badge_style),
            Span::styled(format!(" {}", text), hint_style),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else {
        let indent = " ".repeat(badge_width);
        let mut text_lines: Vec<Line> = vec![Line::from(vec![
            Span::styled(mode_text, badge_style),
            Span::styled(format!(" {}", hint_lines[0]), hint_style),
        ])];
        for hl in &hint_lines[1..] {
            text_lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(format!(" {}", hl), hint_style),
            ]));
        }
        frame.render_widget(Paragraph::new(Text::from(text_lines)), area);
    }
}

fn render_loading(frame: &mut Frame, area: Rect) {
    let popup = popup_center(area, 20, 3);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let para = Paragraph::new("  ⏳ Working…")
        .block(block)
        .style(Style::default().fg(Color::Magenta).bold());
    frame.render_widget(para, popup);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = area.width.min(64).max(40);
    let height = area.height.min(40).max(12);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    let text = concat!(
        " Navigation\n",
        "  j/k / ↑↓     Navigate tree\n",
        "  h/l / ←→     Collapse/expand\n",
        "  Enter         Project/Worktree: toggle  |  Session: attach\n",
        "\n",
        " Project\n",
        "  p             Add project (path: prompt)\n",
        "  m             Move project (reorder list)\n",
        "  d             Unregister project\n",
        "  c             Clean merged worktrees (batch)\n",
        "  e             View .gtrconfig\n",
        "\n",
        " Worktree\n",
        "  w             Add worktree (branch: prompt)\n",
        "  s             New persistent session (optional init command)\n",
        "  o             Open ephemeral run (session dies on exit, attaches)\n",
        "  r             Set alias\n",
        "  d             Delete worktree + kill all sessions\n",
        "  c             Clean this worktree if merged\n",
        "  e             View .gtrconfig\n",
        "\n",
        " Session\n",
        "  Enter         Attach\n",
        "  r             Rename\n",
        "  d             Kill session\n",
        "  x             Dismiss ● (suppress running-app notification) / toggle ⊘ mute\n",
        "\n",
        " Inside Session (tmux)\n",
        "  Ctrl+a d      Detach (return to wsx)\n",
        "  Ctrl+a ?      tmux help\n",
        "\n",
        " Global\n",
        "  n / N         Jump to next / prev session needing attention (●)\n",
        "  R             Refresh\n",
        "  ?             Help\n",
        "  q             Quit\n",
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}
