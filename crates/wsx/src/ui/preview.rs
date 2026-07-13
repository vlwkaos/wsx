// Right preview pane — git info, session capture, project summary

use crate::session_state::{self, AppSessionState};
use crate::ui::ansi;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use wsx_core::model::workspace::{FetchFailReason, Project, SessionInfo, WorktreeInfo};
use wsx_core::routine::ipc::RoutineView;

pub fn render_worktree_preview(
    frame: &mut Frame,
    area: Rect,
    worktree: &WorktreeInfo,
    title: &str,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title))
        .title_style(Style::default().bold());

    let label_style = Style::default().fg(Color::Rgb(120, 120, 140));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Branch:  ", label_style),
            Span::styled(
                worktree.branch.clone(),
                Style::default().fg(Color::Rgb(100, 200, 255)).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Path:    ", label_style),
            Span::styled(
                worktree.path.to_string_lossy().to_string(),
                Style::default().fg(Color::Rgb(200, 200, 210)),
            ),
        ]),
    ];

    if let Some(info) = &worktree.git_info {
        // ── Remote tracking ──────────────────────────────────────────────────
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Remote:", label_style)));
        if let Some(remote) = &info.remote_branch {
            let (status_text, status_style) = match (info.behind, info.ahead) {
                (0, 0) => (
                    "in sync".to_string(),
                    Style::default().fg(Color::Rgb(100, 200, 100)),
                ),
                (b, a) if b > 0 && a > 0 => (
                    format!("↓{} ↑{}  diverged — pull first", b, a),
                    Style::default().fg(Color::Magenta),
                ),
                (b, _) if b > 0 => (
                    format!("↓{}  pull needed", b),
                    Style::default().fg(Color::Red),
                ),
                (_, a) => (
                    format!("↑{}  ready to push", a),
                    Style::default().fg(Color::Cyan),
                ),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} — ", remote),
                    Style::default().fg(Color::Rgb(180, 180, 200)),
                ),
                Span::styled(status_text, status_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "  no upstream tracking branch",
                Style::default().fg(Color::DarkGray),
            )));
        }

        // Fetch failure warning block
        if let Some(reason) = &worktree.fetch_fail_reason {
            let reason_text = match reason {
                FetchFailReason::Auth => "credentials rejected",
                FetchFailReason::Timeout => "timed out",
                FetchFailReason::Network => "network error",
            };
            lines.push(Line::from(vec![
                Span::styled("  ⚠ fetch failed: ", Style::default().fg(Color::Red)),
                Span::styled(reason_text, Style::default().fg(Color::Yellow)),
            ]));
            if let Some(last) = worktree.last_fetched {
                let interval = 60u64 * 2u64.pow(worktree.fetch_fail_count.min(4) as u32);
                let elapsed = last.elapsed().as_secs();
                let remaining = interval.saturating_sub(elapsed);
                let retry_text = if remaining < 5 {
                    format!("retrying soon (attempt {})", worktree.fetch_fail_count)
                } else if remaining >= 60 {
                    format!(
                        "retrying in {}m (attempt {})",
                        remaining / 60,
                        worktree.fetch_fail_count
                    )
                } else {
                    format!(
                        "retrying in {}s (attempt {})",
                        remaining, worktree.fetch_fail_count
                    )
                };
                lines.push(Line::from(Span::styled(
                    format!("    {}", retry_text),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        } else if worktree.fetch_failed {
            lines.push(Line::from(Span::styled(
                "  ⚠ fetch failed",
                Style::default().fg(Color::Red),
            )));
        }

        // ── Local changes ─────────────────────────────────────────────────────
        lines.push(Line::from(""));
        if info.modified_files.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Local:   ", label_style),
                Span::styled("clean", Style::default().fg(Color::Rgb(100, 200, 100))),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("Local:   ", label_style),
                Span::styled(
                    format!(
                        "{} file{} modified",
                        info.modified_files.len(),
                        if info.modified_files.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            for f in info.modified_files.iter().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", f),
                    Style::default().fg(Color::Rgb(255, 150, 80)),
                )));
            }
            if info.modified_files.len() > 5 {
                lines.push(Line::from(Span::styled(
                    format!("  … {} more", info.modified_files.len() - 5),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        // ── Recent commits ────────────────────────────────────────────────────
        if !info.recent_commits.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Commits:", label_style)));
            for c in &info.recent_commits {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", c.hash),
                        Style::default().fg(Color::Rgb(255, 180, 80)),
                    ),
                    Span::styled(
                        c.message.clone(),
                        Style::default().fg(Color::Rgb(210, 210, 220)),
                    ),
                ]));
            }
        }
    }

    if !worktree.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Sessions:",
            Style::default().fg(Color::Rgb(120, 120, 140)),
        )));
        for s in &worktree.sessions {
            let dot = if session_state::derive(s).app_state() == AppSessionState::NeedsAttention {
                " ●"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                format!("  {}{}", s.display_name, dot),
                Style::default().fg(Color::Rgb(100, 220, 130)),
            )));
        }
    }

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

pub fn render_session_preview(
    frame: &mut Frame,
    area: Rect,
    session: &SessionInfo,
    title: &str,
    parsed: Option<&ratatui::text::Text<'static>>,
) {
    frame.render_widget(Clear, area);
    let activity = if session_state::derive(session).app_state() == AppSessionState::NeedsAttention
    {
        " ●"
    } else {
        ""
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {}{} ", title, activity))
        .title_style(Style::default().bold());

    // Use cached parsed ANSI text when available; fall back to parsing inline.
    let fallback;
    let text: &ratatui::text::Text<'_> = match parsed {
        Some(t) => t,
        None => {
            fallback = session
                .pane_capture
                .as_deref()
                .map(ansi::parse)
                .unwrap_or_else(|| "(no capture)".into());
            &fallback
        }
    };
    let inner_h = area.height.saturating_sub(2) as usize; // minus borders
    let scroll = text.lines.len().saturating_sub(inner_h) as u16;
    let para = Paragraph::new(text.clone())
        .block(block)
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

pub fn render_project_preview(frame: &mut Frame, area: Rect, project: &Project) {
    frame.render_widget(Clear, area);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("Path:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                project.path.to_string_lossy().to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("Branch: ", Style::default().fg(Color::Gray)),
            Span::styled(
                project.default_branch.clone(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled("Worktrees:", Style::default().fg(Color::Gray))),
    ];

    for wt in &project.worktrees {
        let main_mark = if wt.is_main { "* " } else { "  " };
        let sess_count = wt.sessions.len();
        let activity = if wt
            .sessions
            .iter()
            .any(|s| session_state::derive(s).app_state() == AppSessionState::NeedsAttention)
        {
            " ●"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}{}", main_mark, wt.display_name()),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!(
                    "  ({} session{}){}",
                    sess_count,
                    if sess_count == 1 { "" } else { "s" },
                    activity
                ),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    if project.worktrees.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no worktrees)",
            Style::default().fg(Color::Gray),
        )));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", project.name))
        .title_style(Style::default().bold());

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

pub fn render_empty_preview(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Preview ")
        .title_style(Style::default().fg(Color::Gray));
    let para = Paragraph::new("Select a project, worktree, or session")
        .style(Style::default().fg(Color::Gray))
        .block(block);
    frame.render_widget(para, area);
}

pub fn render_routines_preview(frame: &mut Frame, area: Rect, project: &Project) {
    frame.render_widget(Clear, area);
    let lines = project
        .routines
        .iter()
        .map(|view| {
            let last = view
                .latest_run
                .as_ref()
                .map(|run| format!("{:?}", run.status))
                .unwrap_or_else(|| "never".into());
            Line::from(vec![
                Span::styled(
                    format!("◇ {}", view.routine.name),
                    Style::default().fg(Color::Magenta).bold(),
                ),
                Span::raw(format!("  {}  last: {last}", view.routine.cron)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} › Routines ", project.name)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub fn render_routine_preview(
    frame: &mut Frame,
    area: Rect,
    project: &Project,
    view: &RoutineView,
    scroll: u16,
) {
    frame.render_widget(Clear, area);
    let label = Style::default().fg(Color::DarkGray);
    let next = view
        .next_run_epoch
        .map(format_epoch)
        .unwrap_or_else(|| "unavailable".into());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Cron:    ", label),
            Span::raw(view.routine.cron.clone()),
        ]),
        Line::from(vec![Span::styled("Next:    ", label), Span::raw(next)]),
        Line::from(vec![
            Span::styled("Command: ", label),
            Span::raw(serde_json::to_string(&view.routine.command).unwrap_or_default()),
        ]),
        Line::from(vec![
            Span::styled("Prompt:  ", label),
            Span::raw(view.routine.prompt.clone()),
        ]),
        Line::raw(""),
    ];
    let mut actions = Vec::new();
    if view.capabilities.can_edit {
        actions.push(if view.capabilities.can_rename {
            "edit (rename allowed)"
        } else {
            "edit"
        });
    }
    if view.capabilities.can_delete {
        actions.push(if view.capabilities.can_cancel {
            "delete (cancels active run)"
        } else {
            "delete"
        });
    }
    lines.push(Line::from(vec![
        Span::styled("Actions: ", label),
        Span::raw(if actions.is_empty() {
            "unavailable".to_string()
        } else {
            actions.join(", ")
        }),
    ]));
    lines.push(Line::raw(""));
    if let Some(run) = &view.latest_run {
        lines.extend([
            Line::from(vec![
                Span::styled("Last:    ", label),
                Span::raw(format!(
                    "{}  {:?}",
                    format_epoch(run.started_epoch),
                    run.status
                )),
            ]),
            Line::from(vec![
                Span::styled("stdout:  ", label),
                Span::raw(run.stdout_path.to_string_lossy().to_string()),
            ]),
            Line::from(vec![
                Span::styled("stderr:  ", label),
                Span::raw(run.stderr_path.to_string_lossy().to_string()),
            ]),
            Line::raw(""),
            Line::styled(
                "Final agent output",
                Style::default().fg(Color::Magenta).bold(),
            ),
            Line::raw(run.final_output.clone()),
        ]);
        if view.recent_runs.len() > 1 {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Recent history",
                Style::default().fg(Color::Magenta).bold(),
            ));
            for previous in view.recent_runs.iter().rev().skip(1).take(5) {
                lines.push(Line::raw(format!(
                    "{}  {:?}  {}",
                    format_epoch(previous.started_epoch),
                    previous.status,
                    previous.stdout_path.display()
                )));
            }
        }
    } else {
        lines.push(Line::styled(
            "No run history",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} › {} ", project.name, view.routine.name)),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn format_epoch(epoch: i64) -> String {
    let timestamp = epoch as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    unsafe {
        libc::localtime_r(&timestamp, local.as_mut_ptr());
    }
    let local = unsafe { local.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min
    )
}
