// Layout orchestration

pub mod config_modal;
pub mod confirm;
pub mod group_manager;
pub mod input;
pub mod layout;
pub mod notice;
pub mod picker;
pub mod preview;
pub mod routine_editor;
pub mod theme;
pub mod workspace_nav;
pub mod workspace_tree;

use crate::app::{App, GroupManagerPurpose, Mode, SPINNER_FRAMES};
use crate::ui::{
    config_modal::render_config_modal,
    confirm::render_confirm,
    group_manager::{render_group_manager, GroupManagerView},
    input::render_input,
    layout::{FrameLayout, TerminalLayout},
    preview::{
        render_empty_preview, render_project_preview, render_terminal_breadcrumb,
        render_terminal_preview, render_worktree_preview,
    },
    workspace_nav::{fit_group_strip, SidebarLayout, WORKSPACE_HEADER_TITLE},
    workspace_tree::{compute_scroll, render_tree, TreeView},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::{SystemTime, UNIX_EPOCH};
use wsx_core::{config::global::project_is_recent, model::workspace::Selection};

fn git_remote_status_color(behind: usize, ahead: usize) -> Color {
    match (behind, ahead) {
        (0, _) => theme::SUCCESS,
        (_, 0) => theme::ERROR,
        _ => theme::WARNING,
    }
}

fn compact_port_label(ports: &[u16]) -> Option<String> {
    let port = ports.first()?;
    let more = ports.len().saturating_sub(1);
    Some(if more == 0 {
        format!(":{port}")
    } else {
        format!(":{port} +{more}")
    })
}

/// Center a popup of given size within `area`.
pub fn popup_center(area: Rect, w: u16, h: u16) -> Rect {
    let width = w.min(area.width);
    let height = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Place a popup in the upper third of `area`.
pub fn popup_upper(area: Rect, w: u16, h: u16) -> Rect {
    let width = w.min(area.width);
    let height = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = (area.y + area.height / 3).min(area.bottom().saturating_sub(height));
    Rect::new(x, y, width, height)
}

pub(crate) fn popup_block<'a>(title: Line<'a>, hints: Line<'a>, border_style: Style) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(hints.right_aligned())
        .border_style(border_style)
}

fn render_workspace_header(frame: &mut Frame, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            WORKSPACE_HEADER_TITLE,
            Style::default().fg(theme::TEXT).bold(),
        )),
        area,
    );
    let groups = app.config.ordered_group_keys();
    let strip = fit_group_strip(
        &groups,
        app.active_group.as_ref(),
        usize::from(area.width),
        app.group_header_scroll,
    );
    if let Some(cells) = &strip.left_cells {
        frame.render_widget(
            Paragraph::new(Span::styled("‹", theme::group_scroll_control())),
            Rect::new(area.x.saturating_add(cells.start as u16), area.y, 1, 1),
        );
    }
    for chip in strip.chips {
        let style = if matches!(chip.key, wsx_core::config::global::GroupKey::Recent) {
            theme::recent_group_chip(chip.active)
        } else {
            theme::group_chip(chip.active)
        };
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {} ", chip.label), style)),
            Rect::new(
                area.x.saturating_add(chip.cells.start as u16),
                area.y,
                chip.cells.end.saturating_sub(chip.cells.start) as u16,
                1,
            ),
        );
    }
    if let Some(cells) = &strip.right_cells {
        frame.render_widget(
            Paragraph::new(Span::styled("›", theme::group_scroll_control())),
            Rect::new(area.x.saturating_add(cells.start as u16), area.y, 1, 1),
        );
    }
}

// ^ [[wsx UI Patterns]] Responsive layout, capability hints, and direct state projection.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.is_mobile = app.force_mobile || area.width < 60;
    let is_mobile = app.is_mobile;
    let status = status_bar_view(app);
    let layout = FrameLayout::new(area);
    let main_area = layout.content;
    app.group_header_area = layout.header;
    render_workspace_header(frame, layout.header, app);

    let (tree_area, preview_area) = if is_mobile && matches!(app.mode, Mode::Terminal { .. }) {
        (Rect::default(), main_area)
    } else if is_mobile {
        (main_area, Rect::default())
    } else {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(32), Constraint::Min(0)])
            .split(main_area);
        (chunks[0], chunks[1])
    };

    let workspace_focused = matches!(app.mode, Mode::Workspace);
    let sidebar_layout = SidebarLayout::bordered(tree_area);
    let visible_height = sidebar_layout.list.height as usize;
    app.tree_visible_height = visible_height;
    app.tree_scroll = compute_scroll(app.tree_selected, visible_height, app.tree_scroll);
    app.tree_area = tree_area;
    app.preview_area = preview_area;
    let has_terminal_preview = matches!(
        app.current_selection(),
        Selection::Session(..) | Selection::Pane(..)
    );
    let terminal_layout = has_terminal_preview.then(|| TerminalLayout::new(preview_area));
    app.terminal_area = terminal_layout.map_or(Rect::default(), |layout| layout.viewport);
    let terminal_breadcrumb_area =
        terminal_layout.map_or(Rect::default(), |layout| layout.breadcrumb);

    let is_move_mode = matches!(app.mode, Mode::Move { .. } | Mode::MoveSession { .. });
    if tree_area.width > 0 && tree_area.height > 0 {
        if workspace_focused {
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::TEXT_SUBTLE)),
                tree_area,
            );
        }
        if let Mode::GroupManager {
            selected,
            scroll,
            purpose,
        } = &app.mode
        {
            let assign_path = match purpose {
                GroupManagerPurpose::Assign { project_idx } => app
                    .workspace
                    .projects
                    .get(*project_idx)
                    .map(|p| p.path.as_path()),
                GroupManagerPurpose::Switch => None,
            };
            let now_unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let recent_project_count = app
                .workspace
                .projects
                .iter()
                .filter(|project| {
                    project_is_recent(
                        project.last_agent_active_unix_ms,
                        project.last_terminal_active_unix_ms,
                        now_unix_ms,
                    )
                })
                .count();
            render_group_manager(
                frame,
                tree_area,
                GroupManagerView {
                    selected: *selected,
                    scroll: *scroll,
                    config: &app.config,
                    active_group: app.active_group.as_ref(),
                    assign_path,
                    recent_project_count,
                },
            );
        } else {
            render_tree(
                frame,
                sidebar_layout,
                TreeView {
                    workspace: &app.workspace,
                    flat: app.flat(),
                    selected: app.tree_selected,
                    scroll_offset: app.tree_scroll,
                    is_move_mode,
                },
            );
        }
    }
    if !is_mobile && !workspace_focused && tree_area.width > 0 {
        let divider_x = tree_area.x + tree_area.width.saturating_sub(1);
        let buffer = frame.buffer_mut();
        for y in tree_area.y..tree_area.y + tree_area.height {
            buffer[(divider_x, y)].set_symbol("│");
            buffer[(divider_x, y)].set_style(Style::default().fg(theme::DIVIDER));
        }
    }

    if preview_area.width > 0 {
        match app.current_selection() {
            Selection::Session(pi, wi, si) => {
                if let Some((project, worktree, session)) =
                    app.workspace.projects.get(pi).and_then(|project| {
                        project.worktrees.get(wi).and_then(|worktree| {
                            worktree
                                .sessions
                                .get(si)
                                .map(|session| (project, worktree, session))
                        })
                    })
                {
                    render_terminal_breadcrumb(
                        frame,
                        terminal_breadcrumb_area,
                        &project.name,
                        worktree.display_name(),
                        session,
                        None,
                    );
                    render_terminal_preview(
                        frame,
                        app.terminal_area,
                        app.terminal_surface(session.pane_id, session.terminal_id),
                        matches!(app.mode, Mode::Terminal { .. }),
                    );
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::Pane(pi, wi, si, pane_idx) => {
                if let Some((project, worktree, session, pane)) =
                    app.workspace.projects.get(pi).and_then(|project| {
                        project.worktrees.get(wi).and_then(|worktree| {
                            worktree.sessions.get(si).and_then(|session| {
                                session
                                    .panes
                                    .get(pane_idx)
                                    .map(|pane| (project, worktree, session, pane))
                            })
                        })
                    })
                {
                    render_terminal_breadcrumb(
                        frame,
                        terminal_breadcrumb_area,
                        &project.name,
                        worktree.display_name(),
                        session,
                        Some(pane),
                    );
                    render_terminal_preview(
                        frame,
                        app.terminal_area,
                        app.terminal_surface(pane.pane_id, pane.terminal_id),
                        matches!(app.mode, Mode::Terminal { .. }),
                    );
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::Worktree(pi, wi) => {
                let found = app.workspace.projects.get(pi).and_then(|p| {
                    p.worktrees.get(wi).map(|wt| {
                        let title = format!("{} › {}", p.name, wt.display_name());
                        title
                    })
                });
                if let Some(title) = found {
                    if let Some(wt) = app
                        .workspace
                        .projects
                        .get(pi)
                        .and_then(|p| p.worktrees.get(wi))
                    {
                        render_worktree_preview(frame, preview_area, wt, &title);
                    } else {
                        render_empty_preview(frame, preview_area);
                    }
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::Project(pi) => {
                if let Some(project) = app.workspace.projects.get(pi) {
                    render_project_preview(frame, preview_area, project);
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::RoutinesHeader(pi) => {
                if let Some(project) = app.workspace.projects.get(pi) {
                    preview::render_routines_preview(frame, preview_area, project);
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::Routine(pi, ri) => {
                if let Some(project) = app.workspace.projects.get(pi) {
                    if let Some(routine) = project.routines.get(ri) {
                        preview::render_routine_preview(frame, preview_area, project, routine, 0);
                    } else {
                        render_empty_preview(frame, preview_area);
                    }
                } else {
                    render_empty_preview(frame, preview_area);
                }
            }
            Selection::None => render_empty_preview(frame, preview_area),
        }
    }

    render_status_bar(frame, layout.footer, app, &status);
    notice::render(frame, area, app);
    render_overlay(frame, main_area, app);
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
        Mode::Help => render_help(frame, area, app),
        Mode::RoutineEditor {
            form,
            original_name,
            can_rename,
            ..
        } => routine_editor::render(frame, area, form, original_name.is_some(), *can_rename),
        Mode::RoutineDetail {
            project_path,
            routine_name,
            scroll,
        } => {
            if let Some(project) = app
                .workspace
                .projects
                .iter()
                .find(|project| project.path == *project_path)
            {
                if let Some(routine) = project
                    .routines
                    .iter()
                    .find(|view| view.routine.name == *routine_name)
                {
                    frame.render_widget(Clear, area);
                    preview::render_routine_preview(frame, area, project, routine, *scroll);
                } else {
                    render_empty_preview(frame, area);
                }
            }
        }
        Mode::Workspace
        | Mode::Terminal { .. }
        | Mode::Move { .. }
        | Mode::MoveSession { .. }
        | Mode::Search { .. }
        | Mode::GroupManager { .. } => {}
    }
}

struct StatusBarView {
    mode: &'static str,
    badge: theme::ModeBadge,
    hints: Vec<String>,
    global_hints: Vec<String>,
}

fn status_bar_view(app: &App) -> StatusBarView {
    let (mode, badge, hints) = match &app.mode {
        Mode::Workspace => {
            let mut hints = match app.current_selection() {
                Selection::Project(_)
                    if app.active_group == Some(wsx_core::config::global::GroupKey::Recent) =>
                {
                    vec!["(e)dit", "(w)orktree", "(g)roup", "(d)remove recent"]
                }
                Selection::Project(_) if app.config.groups.is_empty() => {
                    vec!["(e)dit", "(w)orktree", "(T)groups", "(d)unregister"]
                }
                Selection::Project(_) => {
                    vec!["(e)dit", "(w)orktree", "(g)roup", "(d)unregister"]
                }
                Selection::Worktree(..) => vec!["(s)ession", "(d)elete", "(?)help"],
                Selection::Session(..) => vec!["(C)interrupt", "(d)close", "(?)help"],
                Selection::Pane(..) => {
                    vec!["(|)split", "(-)split", "(C)interrupt", "(d)close"]
                }
                Selection::RoutinesHeader(_) => vec!["(u)new", "(?)help"],
                Selection::Routine(..) => vec!["(e)dit", "(d)elete", "(?)help"],
                Selection::None => vec!["(p)add project", "(?)help"],
            }
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
            hints.extend(["(i)idle", "(a)active", "(n)attention"].map(str::to_string));
            ("WORKSPACE", theme::ModeBadge::Navigation, hints)
        }
        Mode::Terminal { .. } => {
            let mut hints = vec![app.terminal_workspace_hint()];
            if let Some(quit) = app.terminal_quit_hint() {
                hints.push(quit);
            }
            ("TERMINAL", theme::ModeBadge::Terminal, hints)
        }
        Mode::Input { .. } => ("INPUT", theme::ModeBadge::Input, Vec::new()),
        Mode::Confirm { .. } => ("CONFIRM", theme::ModeBadge::Confirm, Vec::new()),
        Mode::Config { .. } => ("CONFIG", theme::ModeBadge::Config, Vec::new()),
        Mode::Move { .. } | Mode::MoveSession { .. } => (
            "MOVE",
            theme::ModeBadge::Move,
            vec!["(j/k)reorder".into(), "Esc: done".into()],
        ),
        Mode::Help => ("HELP", theme::ModeBadge::Info, Vec::new()),
        Mode::Search { .. } => (
            "SEARCH",
            theme::ModeBadge::Input,
            vec!["Enter: next".into(), "Esc: exit".into()],
        ),
        Mode::GroupManager { purpose, .. } => match purpose {
            GroupManagerPurpose::Switch => (
                "GROUPS",
                theme::ModeBadge::Config,
                vec![
                    "(j/k)navigate".into(),
                    "(Space)toggle".into(),
                    "(a)dd".into(),
                    "(r)ename".into(),
                    "(d)elete".into(),
                    "Esc: back".into(),
                ],
            ),
            GroupManagerPurpose::Assign { .. } => (
                "GROUPS",
                theme::ModeBadge::Config,
                vec![
                    "(j/k)navigate".into(),
                    "(Space)toggle".into(),
                    "Esc: close".into(),
                ],
            ),
        },
        Mode::RoutineEditor { .. } => ("ROUTINE", theme::ModeBadge::Routine, Vec::new()),
        Mode::RoutineDetail { .. } => (
            "DETAIL",
            theme::ModeBadge::Info,
            vec!["(j/k)scroll".into(), "Esc: close".into()],
        ),
    };
    let global_hints = if matches!(app.mode, Mode::Workspace) {
        vec!["(,)config".into(), "(q)quit".into()]
    } else {
        Vec::new()
    };
    StatusBarView {
        mode,
        badge,
        hints,
        global_hints,
    }
}

fn fit_hints(hints: &[String], available_width: usize) -> String {
    let mut visible = String::new();
    for hint in hints {
        let candidate = if visible.is_empty() {
            hint.clone()
        } else {
            format!("{visible}  {hint}")
        };
        if Line::from(candidate.as_str()).width() > available_width {
            break;
        }
        visible = candidate;
    }
    visible
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App, view: &StatusBarView) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mode_text = format!(" {} ", view.mode);
    let badge_width = Line::from(mode_text.as_str()).width();
    let badge_style = theme::mode_badge(view.badge);

    let activity = if app.is_busy() {
        let labels = app
            .jobs
            .iter()
            .map(|job| job.label.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        Some((
            format!(
                " {} {} ",
                SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()],
                labels
            ),
            Style::default().fg(theme::WORKING),
        ))
    } else {
        app.update_available.as_ref().map(|version| {
            (
                format!(" update v{version} "),
                Style::default().fg(theme::WARNING).bold(),
            )
        })
    };
    let activity_width = activity
        .as_ref()
        .map(|(text, _)| Line::from(text.as_str()).width())
        .unwrap_or(0);
    let global_available = usize::from(area.width).saturating_sub(badge_width + activity_width + 1);
    let global = fit_hints(&view.global_hints, global_available.saturating_sub(2));
    let global_text = if global.is_empty() {
        String::new()
    } else {
        format!(" {global} ")
    };
    let global_width = Line::from(global_text.as_str()).width();
    let available =
        usize::from(area.width).saturating_sub(badge_width + global_width + activity_width + 1);
    let hint_text = if let Mode::Search { query, .. } = &app.mode {
        format!(" /{query}_")
    } else {
        let fitted = fit_hints(&view.hints, available.saturating_sub(1));
        if fitted.is_empty() {
            String::new()
        } else {
            format!(" {fitted}")
        }
    };
    let left_width = badge_width + Line::from(hint_text.as_str()).width();
    let pad = usize::from(area.width).saturating_sub(left_width + global_width + activity_width);
    let mut spans = vec![
        Span::styled(mode_text, badge_style),
        Span::styled(hint_text, Style::default().fg(theme::TEXT_MUTED)),
        Span::raw(" ".repeat(pad)),
        Span::styled(global_text, Style::default().fg(theme::TEXT_MUTED)),
    ];
    if let Some((text, style)) = activity {
        spans.push(Span::styled(text, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.clamp(40, 64);
    let height = area.height.clamp(12, 40);
    let popup = popup_center(area, width, height);

    frame.render_widget(Clear, popup);

    const ENTRIES: &[&str] = &[
        " Navigation",
        "  j/k / ↑↓     Navigate tree",
        "  h/l / ←→     Collapse/expand",
        "  Enter         Project/Worktree: toggle  |  Session: Terminal mode",
        "",
        " Project",
        "  p             Add project (path: prompt)",
        "  u             Create routine",
        "  m             Move project (reorder list)",
        "  g             Assign project group",
        "  d             Unregister project",
        "  c             Clean merged worktrees (batch)",
        "  e             View/edit wsx.config.yml",
        "",
        " Worktree",
        "  w             Add worktree (branch: prompt)",
        "  s             New persistent session (optional init command)",
        "  r             Set alias",
        "  d             Delete worktree + kill all sessions",
        "  c             Clean this worktree if merged",
        "  e             View/edit wsx.config.yml",
        "",
        " Session",
        "  Enter         Enter Terminal mode",
        "  C             Send Ctrl+C to session",
        "  r             Rename",
        "  d             Kill session",
        "  x             Toggle ⊘ mute (local to wsx; interaction clears it)",
        "",
        " Terminal mode",
        "$terminal_escape",
        "$terminal_quit",
        "$terminal_literal",
        "",
        " Groups (optional)",
        "  T             Open scrollable Groups sidebar",
        "  { / }         Switch to previous/next group",
        "  g             Assign selected project to a group",
        "  a/r/d · J/K   Add/rename/delete · reorder in Groups",
        "",
        " Global",
        "  [ / ]         Jump to prev / next project",
        "  a / A         Jump to next / prev active session (◉)",
        "  n / N         Jump to next / prev session needing attention (●)",
        "  R             Refresh",
        "  ,             Edit global config",
        "  ?             Help",
        "  q             Quit TUI",
        "  Q             Hard quit TUI and wsxd",
    ];

    let inner_width = (width as usize).saturating_sub(2);
    let lines: Vec<Line> = ENTRIES
        .iter()
        .flat_map(|entry| {
            let text = match *entry {
                "$terminal_escape" => Some(format!(
                    "  {:<14} Focus Workspace",
                    app.terminal_escape_label()
                )),
                "$terminal_quit" => app
                    .terminal_quit_label()
                    .map(|label| format!("  {label:<14} Quit TUI")),
                "$terminal_literal" => Some(format!(
                    "  {:<14} Send literal prefix",
                    app.terminal_literal_escape_label()
                )),
                other => Some(other.to_string()),
            };
            text.map_or_else(Vec::new, |text| help_wrap_line(&text, inner_width))
        })
        .collect();

    let block = popup_block(
        Line::from(" Help "),
        Line::styled(" Esc close ", Style::default().fg(theme::TEXT_MUTED)),
        Style::default().fg(theme::ACCENT),
    );
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

#[cfg(test)]
mod tests {
    use super::{fit_hints, git_remote_status_color, theme};

    #[test]
    fn git_remote_status_uses_actionable_semantic_colors() {
        assert_eq!(git_remote_status_color(0, 0), theme::SUCCESS);
        assert_eq!(git_remote_status_color(0, 2), theme::SUCCESS);
        assert_eq!(git_remote_status_color(2, 0), theme::ERROR);
        assert_eq!(git_remote_status_color(2, 1), theme::WARNING);
    }

    #[test]
    fn status_hints_stop_before_exceeding_one_line() {
        let hints = vec![
            "(C)interrupt".to_string(),
            "(d)close".to_string(),
            "(?)help".to_string(),
        ];
        assert_eq!(fit_hints(&hints, 29), "(C)interrupt  (d)close");
        assert_eq!(fit_hints(&hints, 5), "");
    }
}
