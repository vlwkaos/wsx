// Layout orchestration

pub mod config_modal;
pub mod confirm;
pub mod global_settings;
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
    global_settings::render as render_global_settings,
    group_manager::{render_group_manager, GroupManagerView},
    input::render_input,
    layout::{terminal_sidebar_width, FrameLayout, TerminalLayout, EXPANDED_SIDEBAR_WIDTH},
    preview::{
        render_empty_preview, render_project_preview, render_terminal_breadcrumb,
        render_terminal_preview, render_worktree_preview, TerminalBreadcrumbView,
    },
    workspace_nav::{fit_group_strip, SidebarLayout, WORKSPACE_HEADER_TITLE},
    workspace_tree::{compute_scroll, render_compact_tree, render_tree, CompactTreeView, TreeView},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use wsx_core::{config::global::TerminalSidebar, model::workspace::Selection};

fn git_remote_status_color(behind: usize, ahead: usize) -> Color {
    match (behind, ahead) {
        (0, _) => theme::SUCCESS,
        (_, 0) => theme::ERROR,
        _ => theme::WARNING,
    }
}

fn compact_port_label(ports: &[u16], max_width: usize) -> Option<String> {
    let mut best = None;
    for shown in 1..=ports.len() {
        let mut label = ports[..shown]
            .iter()
            .map(|port| format!(":{port}"))
            .collect::<Vec<_>>()
            .join(" ");
        let remaining = ports.len() - shown;
        if remaining > 0 {
            label.push_str(&format!(" +{remaining}"));
        }
        if Line::from(label.as_str()).width() > max_width {
            break;
        }
        best = Some(label);
    }
    best
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
        &app.active_group,
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
        let style = theme::group_chip(chip.active);
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

    let terminal_mode = matches!(app.mode, Mode::Terminal { .. });
    let compact_terminal =
        !is_mobile && terminal_mode && app.config.terminal_sidebar == TerminalSidebar::Compact;
    let (tree_area, preview_area) = if is_mobile && terminal_mode {
        (Rect::default(), main_area)
    } else if is_mobile {
        (main_area, Rect::default())
    } else {
        let sidebar_width = if terminal_mode {
            terminal_sidebar_width(app.config.terminal_sidebar)
        } else {
            EXPANDED_SIDEBAR_WIDTH
        };
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(0)])
            .split(main_area);
        (chunks[0], chunks[1])
    };

    let workspace_focused = matches!(app.mode, Mode::Workspace);
    let sidebar_layout = if compact_terminal {
        SidebarLayout::compact_rail(tree_area)
    } else {
        SidebarLayout::bordered(tree_area)
    };
    let visible_height = sidebar_layout.list.height as usize;
    app.tree_visible_height = visible_height;
    if app.tree_scroll_manual {
        (app.tree_scroll, app.tree_selected) = workspace_tree::scroll_viewport(
            app.tree_scroll,
            app.tree_selected,
            visible_height,
            app.flat().len(),
            0,
        );
    } else {
        app.tree_scroll = compute_scroll(app.tree_selected, visible_height, app.tree_scroll);
    }
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
            render_group_manager(
                frame,
                tree_area,
                GroupManagerView {
                    selected: *selected,
                    scroll: *scroll,
                    config: &app.config,
                    active_group: &app.active_group,
                    assign_path,
                },
            );
        } else {
            let stale_projects = app.stale_project_indices();
            if compact_terminal {
                render_compact_tree(
                    frame,
                    sidebar_layout,
                    CompactTreeView {
                        workspace: &app.workspace,
                        flat: app.flat(),
                        stale_projects: &stale_projects,
                        selected: app.tree_selected,
                        scroll_offset: app.tree_scroll,
                        animation_frame: app.spinner_frame,
                    },
                );
            } else {
                render_tree(
                    frame,
                    sidebar_layout,
                    TreeView {
                        workspace: &app.workspace,
                        flat: app.flat(),
                        stale_projects: &stale_projects,
                        selected: app.tree_selected,
                        scroll_offset: app.tree_scroll,
                        is_move_mode,
                        port_visibility: app.config.port_visibility,
                        animation_frame: app.spinner_frame,
                    },
                );
            }
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
                        TerminalBreadcrumbView {
                            project: &project.name,
                            worktree: worktree.display_name(),
                            session,
                            pane: None,
                            port_visibility: app.config.port_visibility,
                            animation_frame: app.spinner_frame,
                        },
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
                        TerminalBreadcrumbView {
                            project: &project.name,
                            worktree: worktree.display_name(),
                            session,
                            pane: Some(pane),
                            port_visibility: app.config.port_visibility,
                            animation_frame: app.spinner_frame,
                        },
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
        Mode::GlobalSettings { form } => render_global_settings(frame, area, form),
        Mode::Help => render_help(frame, area, app),
        Mode::RoutinePresetPicker { selected, .. } => {
            routine_editor::render_preset_picker(frame, area, *selected)
        }
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
    release: StatusReleaseView,
}

struct StatusReleaseView {
    current: &'static str,
    update: Option<String>,
    visible: bool,
}

fn status_bar_view(app: &App) -> StatusBarView {
    let (mode, badge, hints) = match &app.mode {
        Mode::Workspace => {
            let mut hints = match app.current_selection() {
                Selection::Project(_) if app.config.groups.is_empty() => vec![
                    "(e)dit",
                    "(w)orktree",
                    "(u)routine",
                    "(T)groups",
                    "(d)unregister",
                ],
                Selection::Project(_) => vec![
                    "(e)dit",
                    "(w)orktree",
                    "(u)routine",
                    "(g)roup",
                    "(d)unregister",
                ],
                Selection::Worktree(..) => {
                    vec!["(s)ession", "(u)routine", "(d)elete", "(?)help"]
                }
                Selection::Session(..) => {
                    vec!["(C)interrupt", "(u)routine", "(d)close", "(?)help"]
                }
                Selection::Pane(..) => vec![
                    "(|)split",
                    "(-)split",
                    "(C)interrupt",
                    "(u)routine",
                    "(d)close",
                ],
                Selection::RoutinesHeader(_) => vec!["(u)new", "(?)help"],
                Selection::Routine(project_idx, routine_idx) => {
                    let view = &app.workspace.projects[project_idx].routines[routine_idx];
                    let mut hints = Vec::new();
                    if view.capabilities.can_edit {
                        hints.push("(e)dit");
                    }
                    if view.capabilities.can_delete {
                        hints.push("(d)elete");
                    }
                    hints.push("(?)help");
                    hints
                }
                Selection::None => vec!["(p)add project", "(?)help"],
            }
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
            hints.extend(["(i)dle iter.", "(a)ctive iter.", "(n)eeds"].map(str::to_string));
            ("WORKSPACE", theme::ModeBadge::Navigation, hints)
        }
        Mode::Terminal { .. } => (
            "TERMINAL",
            theme::ModeBadge::Terminal,
            vec![app.terminal_workspace_hint()],
        ),
        Mode::Input { .. } => ("INPUT", theme::ModeBadge::Input, Vec::new()),
        Mode::Confirm { .. } => ("CONFIRM", theme::ModeBadge::Confirm, Vec::new()),
        Mode::Config { .. } | Mode::GlobalSettings { .. } => {
            ("CONFIG", theme::ModeBadge::Config, Vec::new())
        }
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
        Mode::RoutinePresetPicker { .. } => ("RUNNER", theme::ModeBadge::Routine, Vec::new()),
        Mode::RoutineEditor { .. } => ("ROUTINE", theme::ModeBadge::Routine, Vec::new()),
        Mode::RoutineDetail { .. } => (
            "DETAIL",
            theme::ModeBadge::Info,
            vec!["(j/k)scroll".into(), "Esc: close".into()],
        ),
    };
    // ^ Global quit hints stay right-aligned; single-key labels complete the word after the key.
    let global_hints = match &app.mode {
        Mode::Workspace => vec!["(,)config".into(), "(q)uit".into()],
        Mode::Terminal { .. } => app.terminal_quit_hint().into_iter().collect(),
        _ => Vec::new(),
    };
    StatusBarView {
        mode,
        badge,
        hints,
        global_hints,
        release: StatusReleaseView {
            current: env!("CARGO_PKG_VERSION"),
            update: app.update_available.clone(),
            visible: app.config.show_release_status,
        },
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

fn status_release_line(view: &StatusReleaseView, available_width: usize) -> Line<'static> {
    if !view.visible {
        return Line::default();
    }
    let current = format!(" v{} ", view.current);
    if let Some(update) = &view.update {
        let update = format!("↑ v{update} ");
        if Line::from(format!("{current}{update}")).width() <= available_width {
            return Line::from(vec![
                Span::styled(current.clone(), Style::default().fg(theme::TEXT_MUTED)),
                Span::styled(update, Style::default().fg(theme::WARNING).bold()),
            ]);
        }
    }
    if Line::from(current.as_str()).width() <= available_width {
        return Line::from(Span::styled(
            current,
            Style::default().fg(theme::TEXT_MUTED),
        ));
    }
    Line::default()
}

fn status_regions(area: Rect, release_width: usize) -> (Rect, Rect) {
    let release_width = u16::try_from(release_width)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let content_width = area.width.saturating_sub(release_width);
    (
        Rect::new(area.x, area.y, content_width, area.height),
        Rect::new(
            area.x.saturating_add(content_width),
            area.y,
            release_width,
            area.height,
        ),
    )
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App, view: &StatusBarView) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mode_text = format!(" {} ", view.mode);
    let badge_width = Line::from(mode_text.as_str()).width();
    let context_min_width = if let Mode::Search { query, .. } = &app.mode {
        Line::from(format!(" /{query}_")).width()
    } else {
        view.hints
            .first()
            .map(|hint| Line::from(format!(" {hint}")).width())
            .unwrap_or(0)
    };
    let release = status_release_line(
        &view.release,
        usize::from(area.width).saturating_sub(badge_width + context_min_width),
    );
    let release_width = release.width();
    let (content_area, release_area) = status_regions(area, release_width);
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
        None
    };
    let activity_width = activity
        .as_ref()
        .map(|(text, _)| Line::from(text.as_str()).width())
        .unwrap_or(0);
    let content_width = usize::from(content_area.width);
    let global_available =
        content_width.saturating_sub(badge_width + activity_width + context_min_width);
    let global = fit_hints(&view.global_hints, global_available.saturating_sub(2));
    let global_text = if global.is_empty() {
        String::new()
    } else {
        format!(" {global} ")
    };
    let global_width = Line::from(global_text.as_str()).width();
    let available = content_width.saturating_sub(badge_width + global_width + activity_width + 1);
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
    let pad = content_width.saturating_sub(left_width + global_width + activity_width);
    let mut spans = vec![
        Span::styled(mode_text, badge_style),
        Span::styled(hint_text, Style::default().fg(theme::TEXT_MUTED)),
        Span::raw(" ".repeat(pad)),
        Span::styled(global_text, Style::default().fg(theme::TEXT_MUTED)),
    ];
    if let Some((text, style)) = activity {
        spans.push(Span::styled(text, style));
    }
    if content_area.width > 0 {
        frame.render_widget(Paragraph::new(Line::from(spans)), content_area);
    }
    if release_area.width > 0 {
        frame.render_widget(
            Paragraph::new(release).alignment(Alignment::Right),
            release_area,
        );
    }
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
        "  x             Acknowledge done, otherwise toggle ⊘ mute",
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
        "  ,             Open global settings",
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
    use super::{
        compact_port_label, fit_hints, git_remote_status_color, status_regions,
        status_release_line, theme, StatusReleaseView,
    };

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn git_remote_status_uses_actionable_semantic_colors() {
        assert_eq!(git_remote_status_color(0, 0), theme::SUCCESS);
        assert_eq!(git_remote_status_color(0, 2), theme::SUCCESS);
        assert_eq!(git_remote_status_color(2, 0), theme::ERROR);
        assert_eq!(git_remote_status_color(2, 1), theme::WARNING);
    }

    #[test]
    fn compact_port_labels_show_actual_ports_that_fit() {
        let ports = [5173, 8081, 9000];

        assert_eq!(
            compact_port_label(&ports, 18).as_deref(),
            Some(":5173 :8081 :9000")
        );
        assert_eq!(compact_port_label(&ports, 13).as_deref(), Some(":5173 +2"));
        assert_eq!(compact_port_label(&ports, 7), None);
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

    #[test]
    fn status_release_always_shows_the_running_version() {
        let view = StatusReleaseView {
            current: "0.20.0",
            update: None,
            visible: true,
        };

        let current = status_release_line(&view, 80);
        assert_eq!(line_text(&current), " v0.20.0 ");
        assert_eq!(current.spans[0].style.fg, Some(theme::TEXT_MUTED));
    }

    #[test]
    fn status_release_shows_current_and_available_versions() {
        let view = StatusReleaseView {
            current: "0.20.0",
            update: Some("0.21.0".into()),
            visible: true,
        };

        let full = status_release_line(&view, 80);
        assert_eq!(line_text(&full), " v0.20.0 ↑ v0.21.0 ");
        assert_eq!(full.spans[0].style.fg, Some(theme::TEXT_MUTED));
        assert_eq!(full.spans[1].style.fg, Some(theme::WARNING));
    }

    #[test]
    fn narrow_status_drops_update_before_running_version() {
        let view = StatusReleaseView {
            current: "0.20.0",
            update: Some("0.21.0".into()),
            visible: true,
        };

        assert_eq!(line_text(&status_release_line(&view, 13)), " v0.20.0 ");
        assert_eq!(line_text(&status_release_line(&view, 9)), " v0.20.0 ");
        assert!(status_release_line(&view, 8).spans.is_empty());
    }

    #[test]
    fn hidden_release_status_reserves_no_footer_space() {
        let view = StatusReleaseView {
            current: "0.20.0",
            update: Some("0.21.0".into()),
            visible: false,
        };

        assert!(status_release_line(&view, 80).spans.is_empty());
    }

    #[test]
    fn rust_ui_sources_reject_duplicated_parenthesized_mnemonics() {
        fn rust_files(directory: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    rust_files(&path, files);
                } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                    files.push(path);
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut violations = Vec::new();
        for path in files {
            let source = std::fs::read(&path).unwrap();
            for (index, window) in source.windows(4).enumerate() {
                if window[0] == b'('
                    && window[1].is_ascii_alphabetic()
                    && window[2] == b')'
                    && window[1].eq_ignore_ascii_case(&window[3])
                {
                    let line = source[..index]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count()
                        + 1;
                    violations.push(format!("{}:{line}", path.display()));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "mnemonic key is repeated after its parenthesized prefix: {violations:?}"
        );
    }

    #[test]
    fn release_region_stays_at_the_bottom_right_boundary() {
        let area = ratatui::layout::Rect::new(4, 9, 80, 1);
        let (content, release) = status_regions(area, 14);

        assert_eq!(content, ratatui::layout::Rect::new(4, 9, 66, 1));
        assert_eq!(release, ratatui::layout::Rect::new(70, 9, 14, 1));
        assert_eq!(release.right(), area.right());
    }
}
