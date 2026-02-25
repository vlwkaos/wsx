// Layout orchestration

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

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let main_area = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
    let status_area = Rect::new(area.x, area.y + area.height.saturating_sub(1), area.width, 1);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(0)])
        .split(main_area);

    // Update scroll to match visible height
    let visible_height = chunks[0].height.saturating_sub(2) as usize;
    app.tree_scroll = compute_scroll(app.tree_selected, visible_height, app.tree_scroll);

    render_tree(frame, chunks[0], &app.workspace, app.tree_selected, app.tree_scroll);

    // Preview
    let sel = app.current_selection();
    match &sel {
        Selection::Session(pi, wi, si) => {
            let (pi, wi, si) = (*pi, *wi, *si);
            if let Some(sess) = app.workspace.projects.get(pi)
                .and_then(|p| p.worktrees.get(wi))
                .and_then(|w| w.sessions.get(si))
            {
                let sess = sess.clone();
                render_session_preview(frame, chunks[1], &sess);
            } else {
                render_empty_preview(frame, chunks[1]);
            }
        }
        Selection::Worktree(pi, wi) => {
            let (pi, wi) = (*pi, *wi);
            let data = app.workspace.projects.get(pi).map(|p| {
                (p.clone(), p.worktrees.get(wi).cloned())
            });
            if let Some((project, Some(worktree))) = data {
                render_worktree_preview(frame, chunks[1], &project, &worktree);
            } else {
                render_empty_preview(frame, chunks[1]);
            }
        }
        Selection::Project(pi) => {
            let pi = *pi;
            if let Some(project) = app.workspace.projects.get(pi) {
                let project = project.clone();
                render_project_preview(frame, chunks[1], &project);
            } else {
                render_empty_preview(frame, chunks[1]);
            }
        }
        Selection::None => render_empty_preview(frame, chunks[1]),
    }

    render_status_bar(frame, status_area, app);
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
        Mode::Help => render_help(frame, area),
        Mode::Normal => {}
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mode_label = match &app.mode {
        Mode::Normal => "NORMAL",
        Mode::Input { .. } => "INPUT",
        Mode::Confirm { .. } => "CONFIRM",
        Mode::Config { .. } => "CONFIG",
        Mode::Help => "HELP",
    };

    let sel = app.current_selection();
    let hints = match &app.mode {
        Mode::Normal => match sel {
            Selection::Project(_) => "j/k:nav  Enter:toggle  w:worktree  d:del  c:clean  e:config",
            Selection::Worktree(_, _) => "j/k:nav  Enter:toggle  s:session  o:run  N:alias  d:del  e:config",
            Selection::Session(_, _, _) => "j/k:nav  Enter:attach  N:rename  d:kill",
            Selection::None => "p:add project",
        },
        Mode::Input { .. } => "Enter:confirm  Esc:cancel",
        Mode::Confirm { .. } => "y:yes  n:no",
        Mode::Config { .. } => "Esc:close",
        Mode::Help => "Esc/q:close",
    };

    let common = "  R:refresh  ?:help  q:quit";
    let full_hints = if matches!(app.mode, Mode::Normal) {
        format!("{}{}", hints, common)
    } else {
        hints.to_string()
    };

    let msg = app.status_message.as_deref().unwrap_or("");
    let (mode_fg, mode_bg) = if app.loading {
        (Color::Black, Color::Magenta)
    } else {
        (Color::Black, Color::Yellow)
    };
    let mode_text = if app.loading {
        " [WORKING…] ".to_string()
    } else {
        format!(" [{}] ", mode_label)
    };
    let spans = vec![
        Span::styled(mode_text, Style::default().fg(mode_fg).bg(mode_bg).bold()),
        Span::styled(format!(" {}", full_hints), Style::default().fg(Color::Gray).bg(Color::Black)),
        if msg.is_empty() {
            Span::raw("")
        } else {
            Span::styled(format!("  {}", msg), Style::default().fg(Color::Cyan).bg(Color::Black))
        },
    ];

    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = area.width.min(64).max(40);
    let height = area.height.min(28).max(12);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let text = concat!(
        " Navigation\n",
        "  j/k / ↑↓     Navigate tree\n",
        "  h/l / ←→     Collapse/expand\n",
        "  Enter         Project/Worktree: toggle  |  Session: attach\n",
        "\n",
        " Project\n",
        "  p             Add project (path: prompt)\n",
        "  d             Unregister project\n",
        "  c             Clean merged worktrees\n",
        "  e             View .gtrconfig\n",
        "\n",
        " Worktree\n",
        "  w             Add worktree (branch: prompt)\n",
        "  s             New persistent session (optional init command)\n",
        "  o             Open ephemeral run (session dies on exit, attaches)\n",
        "  N             Set alias\n",
        "  d             Delete worktree + kill all sessions\n",
        "  e             View .gtrconfig\n",
        "\n",
        " Session\n",
        "  Enter         Attach\n",
        "  N             Rename\n",
        "  d             Kill session\n",
        "\n",
        " Global\n",
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
