// Layout orchestration

pub mod ansi;
pub mod config_modal;
pub mod confirm;
pub mod git_popup;
pub mod input;
pub mod picker;
pub mod preview;
pub mod workspace_tree;

use crate::app::{App, Mode};
use crate::model::workspace::Selection;
use crate::ui::{
    config_modal::render_config_modal,
    confirm::render_confirm,
    git_popup::render_git_popup,
    input::render_input,
    preview::{
        render_empty_preview, render_project_preview, render_session_preview,
        render_worktree_preview,
    },
    workspace_tree::{compute_scroll, render_tree},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
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
    let main_area = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(sb_height),
    );
    let status_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(sb_height),
        area.width,
        sb_height,
    );

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
    render_tree(
        frame,
        chunks[0],
        &app.workspace,
        app.tree_selected,
        app.tree_scroll,
        is_move_mode,
    );

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
            if let Some((worktree, title)) = app.workspace.projects.get(pi).and_then(|p| {
                p.worktrees.get(wi).map(|wt| {
                    let title = format!("{} › {}", p.name, wt.display_name());
                    (wt.clone(), title)
                })
            }) {
                render_worktree_preview(frame, preview_area, &worktree, &title);
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
        Mode::GitPopup { project_idx: pi, .. } => {
            let def = app
                .workspace
                .projects
                .get(*pi)
                .map(|p| p.default_branch.clone())
                .unwrap_or_else(|| "main".to_string());
            render_git_popup(frame, area, &def);
        }
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
        Mode::GitPopup { .. } => "GIT",
    }
}

fn build_hints(app: &App) -> String {
    let global = "(/)search  (a)ctive  ·  (n)ext (N)prev pending  ·  (e)config  (?)help";
    match &app.mode {
        Mode::Normal => match app.current_selection() {
            Selection::Project(_) => format!("(m)ove  (w)orktree  (d)el  (c)lean  ·  {}", global),
            Selection::Worktree(_, _) => format!(
                "(s)ession  (r)alias  (d)el  ·  (w)orktree  (c)lean  ·  {}",
                global
            ),
            Selection::Session(pi, wi, si) => {
                let active = app
                    .workspace
                    .projects
                    .get(pi)
                    .and_then(|p| p.worktrees.get(wi))
                    .and_then(|w| w.sessions.get(si))
                    .map(|s| {
                        s.last_activity
                            .map(|t| t.elapsed().as_secs() < crate::app::IDLE_SECS)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                let dismiss = if active { "" } else { "(x)dismiss  ·  " };
                format!("(m)ove  (r)ename  (d)kill  ·  {}(S)send cmd  (C)ctrl-c  ·  (C-a d)detach  ·  (s)ession  ·  (w)orktree  (c)lean  ·  {}", dismiss, global)
            }
            Selection::None => "(p) add project".to_string(),
        },
        Mode::Input { .. } => "Esc: cancel".to_string(),
        Mode::Confirm { .. } => "(y)es  (n)o".to_string(),
        Mode::Config { .. } => "(e)dit .gtrignore  Esc: close".to_string(),
        Mode::Move { .. } | Mode::MoveSession { .. } => "(j/k) reorder  Esc: done".to_string(),
        Mode::Help => "Esc: close".to_string(),
        Mode::Search { .. } => unreachable!(),
        Mode::GitPopup { .. } => {
            "(p)ull  (P)ush  (r)pull-rebase  (m)erge-from  (M)erge-into  Esc: close".to_string()
        }
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
            let candidate = format!("{}  {}", current, group);
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
            Span::styled(
                " [/] ",
                Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
            ),
            Span::styled(format!(" {}_", query), Style::default().fg(Color::White)),
            Span::styled(
                "  Enter: next  Esc: exit",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let label = get_mode_label(app);
    let mode_text = format!(" [{}] ", label);
    let badge_width = mode_text.len();
    let badge_style = Style::default().fg(Color::Black).bg(Color::Yellow).bold();

    let ver = concat!(" v", env!("CARGO_PKG_VERSION"), " ");
    let ver_style = Style::default().fg(Color::DarkGray);

    let msg = app.status_message.as_deref().unwrap_or("");
    if !msg.is_empty() {
        let left = format!(" {}", msg);
        let left_len = badge_width + left.len();
        let pad = (area.width as usize).saturating_sub(left_len + ver.len());
        let spans = vec![
            Span::styled(mode_text, badge_style),
            Span::styled(left, Style::default().fg(Color::Cyan)),
            Span::raw(" ".repeat(pad)),
            Span::styled(ver, ver_style),
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
        let left = format!(" {}", text);
        let left_len = badge_width + left.len();
        let pad = (area.width as usize).saturating_sub(left_len + ver.len());
        let spans = vec![
            Span::styled(mode_text, badge_style),
            Span::styled(left, hint_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(ver, ver_style),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    } else {
        let indent = " ".repeat(badge_width);
        let mut text_lines: Vec<Line> = vec![Line::from(vec![
            Span::styled(mode_text, badge_style),
            Span::styled(format!(" {}", hint_lines[0]), hint_style),
        ])];
        let last = hint_lines.len() - 1;
        for (i, hl) in hint_lines[1..].iter().enumerate() {
            let left = format!(" {}", hl);
            if i + 1 == last {
                let left_len = badge_width + left.len();
                let pad = (area.width as usize).saturating_sub(left_len + ver.len());
                text_lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(left, hint_style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(ver, ver_style),
                ]));
            } else {
                text_lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(left, hint_style),
                ]));
            }
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

    const ENTRIES: &[&str] = &[
        " Navigation",
        "  j/k / ↑↓     Navigate tree",
        "  h/l / ←→     Collapse/expand",
        "  Enter         Project/Worktree: toggle  |  Session: attach",
        "",
        " Project",
        "  p             Add project (path: prompt)",
        "  m             Move project (reorder list)",
        "  d             Unregister project",
        "  c             Clean merged worktrees (batch)",
        "  e             View .gtrconfig",
        "",
        " Worktree",
        "  w             Add worktree (branch: prompt)",
        "  s             New persistent session (optional init command)",
        "  r             Set alias",
        "  d             Delete worktree + kill all sessions",
        "  c             Clean this worktree if merged",
        "  e             View .gtrconfig",
        "",
        " Session",
        "  Enter         Attach",
        "  S             Send command to session",
        "  C             Send Ctrl+C to session",
        "  r             Rename",
        "  d             Kill session",
        "  x             Dismiss ● (suppress running-app notification) / toggle ⊘ mute",
        "",
        " Inside Session (tmux)",
        "  Ctrl+a d      Detach (return to wsx)",
        "  Ctrl+a ?      tmux help",
        "",
        " Global",
        "  [ / ]         Jump to prev / next project",
        "  a             Jump to next active session (◉)",
        "  n / N         Jump to next / prev session needing attention (●)",
        "  R             Refresh",
        "  ?             Help",
        "  q             Quit",
    ];

    let inner_width = (width as usize).saturating_sub(2);
    let lines: Vec<Line> = ENTRIES
        .iter()
        .flat_map(|entry| help_wrap_line(entry, inner_width))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, popup);
}

/// Wrap a help entry, indenting continuation lines to align with the description column.
fn help_wrap_line(line: &str, width: usize) -> Vec<Line<'static>> {
    // Find where the description starts: first run of 2+ spaces after a non-space char
    // (following the 2-char indent). Key lines look like "  key     description".
    let desc_col = if line.starts_with("  ") && !line[2..].starts_with(' ') {
        let rest = &line[2..];
        let mut found = None;
        let mut in_spaces = false;
        let mut space_start = 0;
        for (i, c) in rest.char_indices() {
            if c == ' ' {
                if !in_spaces {
                    space_start = i;
                    in_spaces = true;
                }
            } else {
                if in_spaces && i - space_start >= 2 {
                    found = Some(i);
                    break;
                }
                in_spaces = false;
            }
        }
        found.map(|i| 2 + i) // byte offset of description start
    } else {
        None
    };

    let Some(desc_byte) = desc_col else {
        return vec![Line::from(line.to_owned())];
    };

    // Measure key column display width (chars, treating all as 1-wide)
    let key_display: usize = line[..desc_byte].chars().count();
    let desc_text = &line[desc_byte..];
    let desc_width = width.saturating_sub(key_display);

    if desc_text.len() <= desc_width {
        return vec![Line::from(line.to_owned())];
    }

    // Word-wrap the description
    let indent = " ".repeat(key_display);
    let key_part = line[..desc_byte].to_owned();
    let mut result = Vec::new();
    let mut remaining = desc_text;
    let mut first = true;

    while !remaining.is_empty() {
        let avail = if first {
            desc_width
        } else {
            width.saturating_sub(key_display)
        };
        let (chunk, rest) = split_at_word(remaining, avail);
        if first {
            result.push(Line::from(format!("{}{}", key_part, chunk)));
            first = false;
        } else {
            result.push(Line::from(format!("{}{}", indent, chunk)));
        }
        remaining = rest.trim_start();
    }
    result
}

/// Split `s` at a word boundary no longer than `max_chars`. Returns (chunk, remainder).
fn split_at_word(s: &str, max_chars: usize) -> (&str, &str) {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return (s, "");
    }
    // Find byte offset of max_chars-th char
    let end_byte = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    // Walk back to last space
    if let Some(space) = s[..end_byte].rfind(' ') {
        (&s[..space], &s[space..])
    } else {
        (&s[..end_byte], &s[end_byte..])
    }
}
