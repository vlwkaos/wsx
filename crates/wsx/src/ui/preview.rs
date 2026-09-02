// Right preview pane — git info, session capture, project summary

use crate::session_state::{self, AppSessionState};
use asched_core::routine::{ipc::RoutineView, Trigger};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};
use wsx_core::{
    config::global::PortVisibility,
    model::workspace::{
        FetchFailReason, PaneInfo, Project, SessionInfo, SubmoduleCommitState, WorktreeInfo,
    },
};

use super::{compact_port_label, git_remote_status_color, theme, workspace_tree::agent_state_icon};

pub struct TerminalBreadcrumbView<'a> {
    pub project: &'a str,
    pub worktree: &'a str,
    pub session: &'a SessionInfo,
    pub pane: Option<&'a PaneInfo>,
    pub port_visibility: PortVisibility,
    pub animation_frame: usize,
}

pub fn render_terminal_breadcrumb(frame: &mut Frame, area: Rect, view: TerminalBreadcrumbView<'_>) {
    let TerminalBreadcrumbView {
        project,
        worktree,
        session,
        pane,
        port_visibility,
        animation_frame,
    } = view;
    let (agent, state, ports, outcome_acknowledged, foreground_job) = pane.map_or_else(
        || {
            (
                session.agent.as_deref(),
                session.agent_status,
                session.listening_ports(),
                session.outcome_acknowledged,
                !session.is_agentic() && session.has_foreground_job(),
            )
        },
        |pane| {
            (
                pane.agent.as_deref(),
                pane.agent_status,
                pane.listening_ports.clone(),
                pane.outcome_acknowledged,
                pane.agent.is_none() && pane.foreground_job,
            )
        },
    );
    let (icon, icon_color) = agent_state_icon(
        state,
        session.muted,
        outcome_acknowledged,
        foreground_job,
        animation_frame,
    );
    let mut spans = vec![
        Span::styled(
            format!(" {project}"),
            Style::default().fg(theme::TEXT).bold(),
        ),
        Span::styled(" › ", Style::default().fg(theme::TEXT_SUBTLE)),
        Span::styled(worktree.to_string(), Style::default().fg(theme::ACCENT)),
        Span::styled(" › ", Style::default().fg(theme::TEXT_SUBTLE)),
        Span::styled(
            session.display_name.clone(),
            Style::default().fg(theme::TEXT),
        ),
    ];
    if let Some(pane) = pane {
        spans.push(Span::styled(" › ", Style::default().fg(theme::TEXT_SUBTLE)));
        spans.push(Span::styled(
            pane.label.clone(),
            Style::default().fg(theme::TEXT_MUTED),
        ));
    }
    spans.push(Span::styled(
        format!("  {icon}"),
        Style::default().fg(icon_color),
    ));
    if let Some(agent_label) = session_state::agent_label(agent) {
        spans.push(Span::styled(agent_label, theme::agent_label()));
    }
    let port_width =
        usize::from(area.width).saturating_sub(Line::from(spans.clone()).width().saturating_add(2));
    let is_agentic = session.is_agentic();
    if let Some(label) = port_visibility
        .shows_session(is_agentic)
        .then(|| compact_port_label(&ports, port_width))
        .flatten()
    {
        spans.push(Span::styled(
            format!("  {label}"),
            Style::default().fg(theme::ACCENT),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn render_worktree_preview(
    frame: &mut Frame,
    area: Rect,
    worktree: &WorktreeInfo,
    title: &str,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::NONE)
        .padding(Padding::new(2, 1, 1, 1))
        .title(format!(" {} ", title))
        .title_style(Style::default().bold());

    let label_style = Style::default().fg(theme::TEXT_SUBTLE);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Branch:  ", label_style),
            Span::styled(
                worktree.branch.clone(),
                Style::default().fg(theme::ACCENT).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Path:    ", label_style),
            Span::styled(
                worktree.path.to_string_lossy().to_string(),
                Style::default().fg(theme::TEXT),
            ),
        ]),
    ];

    let ports = worktree.listening_ports();
    if !ports.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Ports:   ", label_style),
            Span::styled(
                ports
                    .iter()
                    .map(|port| format!(":{port}"))
                    .collect::<Vec<_>>()
                    .join("  "),
                Style::default().fg(theme::ACCENT),
            ),
        ]));
    }

    if let Some(info) = &worktree.git_info {
        // ── Remote tracking ──────────────────────────────────────────────────
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Remote:", label_style)));
        if let Some(remote) = &info.remote_branch {
            let status_text = match (info.behind, info.ahead) {
                (0, 0) => "in sync".to_string(),
                (b, a) if b > 0 && a > 0 => {
                    format!("↓{} ↑{}  diverged — pull first", b, a)
                }
                (b, _) if b > 0 => format!("↓{}  pull needed", b),
                (_, a) => format!("↑{}  ready to push", a),
            };
            let status_style =
                Style::default().fg(git_remote_status_color(info.behind, info.ahead));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} — ", remote),
                    Style::default().fg(theme::TEXT_MUTED),
                ),
                Span::styled(status_text, status_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "  no upstream tracking branch",
                Style::default().fg(theme::TEXT_SUBTLE),
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
                Span::styled("  ⚠ fetch failed: ", Style::default().fg(theme::ERROR)),
                Span::styled(reason_text, Style::default().fg(theme::WARNING)),
            ]));
            if let Some(last) = worktree.last_fetched {
                let interval = 60u64 * 2u64.pow(worktree.fetch_fail_count.min(4));
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
                    Style::default().fg(theme::TEXT_SUBTLE),
                )));
            }
        } else if worktree.fetch_failed {
            lines.push(Line::from(Span::styled(
                "  ⚠ fetch failed",
                Style::default().fg(theme::ERROR),
            )));
        }

        // ── Local changes ─────────────────────────────────────────────────────
        lines.push(Line::from(""));
        if info.modified_files.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("Local:   ", label_style),
                Span::styled("clean", Style::default().fg(theme::SUCCESS)),
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
                    Style::default().fg(theme::WARNING),
                ),
            ]));
            for f in info.modified_files.iter().take(5) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", f),
                    Style::default().fg(theme::WARNING),
                )));
            }
            if info.modified_files.len() > 5 {
                lines.push(Line::from(Span::styled(
                    format!("  … {} more", info.modified_files.len() - 5),
                    Style::default().fg(theme::TEXT_SUBTLE),
                )));
            }
        }

        // ── Nested Git sources ────────────────────────────────────────────────
        match &info.submodules {
            None => {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("Submodules: ", label_style),
                    Span::styled("status unavailable", Style::default().fg(theme::ERROR)),
                ]));
            }
            Some(submodules) if !submodules.is_empty() => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Submodules:", label_style)));
                for submodule in submodules {
                    let (state, mut color) = match submodule.commit_state {
                        SubmoduleCommitState::InSync => ("in sync", theme::SUCCESS),
                        SubmoduleCommitState::CommitChanged => {
                            ("commit differs from parent", theme::WARNING)
                        }
                        SubmoduleCommitState::Uninitialized => ("not initialized", theme::WARNING),
                        SubmoduleCommitState::Conflict => ("conflicted", theme::ERROR),
                    };
                    if submodule.commit_state == SubmoduleCommitState::InSync
                        && (submodule.modified_content || submodule.untracked_content)
                    {
                        color = theme::WARNING;
                    }
                    let mut details = vec![state];
                    if submodule.modified_content {
                        details.push("modified content");
                    }
                    if submodule.untracked_content {
                        details.push("untracked content");
                    }
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {} — ", submodule.path),
                            Style::default().fg(theme::TEXT_MUTED),
                        ),
                        Span::styled(details.join(", "), Style::default().fg(color)),
                    ]));
                }
            }
            Some(_) => {}
        }

        if !info.subtrees.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Subtrees:", label_style)));
            for subtree in &info.subtrees {
                let (status, color) = if subtree.modified_files.is_empty() {
                    ("clean".to_string(), theme::SUCCESS)
                } else {
                    (
                        format!(
                            "{} local change{}",
                            subtree.modified_files.len(),
                            if subtree.modified_files.len() == 1 {
                                ""
                            } else {
                                "s"
                            }
                        ),
                        theme::WARNING,
                    )
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} — ", subtree.path),
                        Style::default().fg(theme::TEXT_MUTED),
                    ),
                    Span::styled(status, Style::default().fg(color)),
                ]));
                for file in subtree.modified_files.iter().take(3) {
                    lines.push(Line::from(Span::styled(
                        format!("    {file}"),
                        Style::default().fg(theme::WARNING),
                    )));
                }
                if subtree.modified_files.len() > 3 {
                    lines.push(Line::from(Span::styled(
                        format!("    … {} more", subtree.modified_files.len() - 3),
                        Style::default().fg(theme::TEXT_SUBTLE),
                    )));
                }
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
                        Style::default().fg(theme::WARNING),
                    ),
                    Span::styled(c.message.clone(), Style::default().fg(theme::TEXT)),
                ]));
            }
        }
    }

    if !worktree.sessions.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Sessions:",
            Style::default().fg(theme::TEXT_SUBTLE),
        )));
        for s in &worktree.sessions {
            let dot = if session_state::derive(s).app_state() == AppSessionState::NeedsAttention {
                " ●"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                format!("  {}{}", s.display_name, dot),
                Style::default().fg(theme::SUCCESS),
            )));
        }
    }

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

pub fn render_terminal_preview(
    frame: &mut Frame,
    area: Rect,
    terminal: Option<&wsx_core::runtime::TerminalFrame>,
    interactive: bool,
) {
    render_terminal_frame(frame, area, terminal, interactive);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalRowProjection {
    source_start: u16,
    len: u16,
}

impl TerminalRowProjection {
    fn target_row(self, source_row: u16) -> Option<u16> {
        let projected_row = source_row.checked_sub(self.source_start)?;
        (projected_row < self.len).then_some(projected_row)
    }
}

// ^ [[wsx UI Patterns]] Frame projection may crop authoritative rows, but never resize them.
fn terminal_row_projection(source_rows: u16, target_rows: u16) -> TerminalRowProjection {
    TerminalRowProjection {
        source_start: source_rows.saturating_sub(target_rows),
        len: source_rows.min(target_rows),
    }
}

fn render_terminal_frame(
    frame: &mut Frame,
    area: Rect,
    terminal: Option<&wsx_core::runtime::TerminalFrame>,
    interactive: bool,
) {
    frame.render_widget(Clear, area);
    let inner = area;
    let Some(terminal) = terminal else { return };
    let rows = terminal_row_projection(terminal.rows, inner.height);
    let visible_cols = inner.width.min(terminal.cols);
    let buffer = frame.buffer_mut();
    for projected_y in 0..rows.len {
        let source_y = rows.source_start + projected_y;
        let target_y = projected_y;
        let selection = terminal
            .selection
            .binary_search_by_key(&source_y, |selection| selection.row)
            .ok()
            .map(|index| terminal.selection[index]);
        for x in 0..visible_cols {
            let Some(cell) = terminal
                .cells
                .get(usize::from(source_y) * usize::from(terminal.cols) + usize::from(x))
            else {
                continue;
            };
            let target = &mut buffer[(inner.x + x, inner.y + target_y)];
            project_terminal_cell(target, cell);
            if selection
                .is_some_and(|selection| (selection.start_col..=selection.end_col).contains(&x))
            {
                target.set_style(theme::terminal_selection());
            }
        }
    }
    if interactive && terminal.cursor.visible && terminal.cursor.x < visible_cols {
        if let Some(cursor_y) = rows.target_row(terminal.cursor.y) {
            frame.set_cursor_position((inner.x + terminal.cursor.x, inner.y + cursor_y));
        }
    }
}

fn project_terminal_cell(target: &mut ratatui::buffer::Cell, source: &wsx_core::runtime::Cell) {
    let mut style = Style::default();
    if let Some([r, g, b]) = source.fg {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if let Some([r, g, b]) = source.bg {
        style = style.bg(Color::Rgb(r, g, b));
    }
    let mut modifiers = Modifier::empty();
    if source.modifiers.bold {
        modifiers |= Modifier::BOLD;
    }
    if source.modifiers.italic {
        modifiers |= Modifier::ITALIC;
    }
    if source.modifiers.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if source.modifiers.inverse {
        modifiers |= Modifier::REVERSED;
    }
    if source.modifiers.dim {
        modifiers |= Modifier::DIM;
    }
    if source.modifiers.strike {
        modifiers |= Modifier::CROSSED_OUT;
    }
    let symbol = if source.symbol.is_empty() {
        " "
    } else {
        &source.symbol
    };
    target
        .set_symbol(symbol)
        .set_style(style.add_modifier(modifiers))
        .set_skip(matches!(
            source.width,
            wsx_core::runtime::CellWidth::SpacerTail
        ));
}

pub fn render_project_preview(frame: &mut Frame, area: Rect, project: &Project) {
    frame.render_widget(Clear, area);
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("Path:  ", Style::default().fg(theme::TEXT_MUTED)),
            Span::styled(
                project.path.to_string_lossy().to_string(),
                Style::default().fg(theme::TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled("Branch: ", Style::default().fg(theme::TEXT_MUTED)),
            Span::styled(
                project.default_branch.clone(),
                Style::default().fg(theme::ACCENT),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Worktrees:",
            Style::default().fg(theme::TEXT_MUTED),
        )),
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
                Style::default().fg(theme::ACCENT),
            ),
            Span::styled(
                format!(
                    "  ({} session{}){}",
                    sess_count,
                    if sess_count == 1 { "" } else { "s" },
                    activity
                ),
                Style::default().fg(theme::TEXT_MUTED),
            ),
        ]));
    }

    if project.worktrees.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no worktrees)",
            Style::default().fg(theme::TEXT_MUTED),
        )));
    }

    let block = Block::default()
        .borders(Borders::NONE)
        .padding(Padding::new(2, 1, 1, 1))
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
        .borders(Borders::NONE)
        .padding(Padding::new(2, 1, 1, 1))
        .title(" Preview ")
        .title_style(Style::default().fg(theme::TEXT_MUTED));
    let para = Paragraph::new("Select a project, worktree, or session")
        .style(Style::default().fg(theme::TEXT_MUTED))
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
                    Style::default().fg(theme::ACCENT).bold(),
                ),
                Span::raw(format!(
                    "  {}  last: {last}",
                    routine_trigger_label(&view.routine.trigger)
                )),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::NONE)
                    .padding(Padding::new(2, 1, 1, 1))
                    .title(format!(" {} › sched ", project.name)),
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
    let label = Style::default().fg(theme::TEXT_SUBTLE);
    let next = view
        .next_run_epoch
        .map(format_epoch)
        .unwrap_or_else(|| "unavailable".into());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Schedule:", label),
            Span::raw(if view.routine.enabled {
                " enabled"
            } else {
                " disabled"
            }),
        ]),
        Line::from(vec![
            Span::styled("Trigger: ", label),
            Span::raw(routine_trigger_label(&view.routine.trigger)),
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
                Style::default().fg(theme::ACCENT).bold(),
            ),
            Line::raw(run.final_output.clone()),
        ]);
        if view.recent_runs.len() > 1 {
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Recent history",
                Style::default().fg(theme::ACCENT).bold(),
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
            Style::default().fg(theme::TEXT_SUBTLE),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::NONE)
                    .padding(Padding::new(2, 1, 1, 1))
                    .title(format!(" {} › {} ", project.name, view.routine.name)),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn routine_trigger_label(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron(cron) => cron.clone(),
        Trigger::Event { kind } => format!("event:{kind}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use wsx_core::{
        model::workspace::{GitInfo, SubmoduleInfo, SubtreeInfo},
        runtime::{
            AgentState, Cell, CellWidth, Cursor, PaneId, PaneLayout, SessionId, TerminalFrame,
            TerminalId, TerminalSelectionRange,
        },
    };

    fn terminal_frame(cells: Vec<Cell>) -> TerminalFrame {
        TerminalFrame {
            pane_id: PaneId(1),
            terminal_id: TerminalId(2),
            revision: 1,
            cols: 3,
            rows: 1,
            cells,
            cursor: Cursor {
                x: 0,
                y: 0,
                visible: false,
                blinking: false,
                shape: 0,
            },
            selection: Vec::new(),
        }
    }

    fn terminal_frame_rows(rows: &[&str]) -> TerminalFrame {
        let cells = rows
            .iter()
            .flat_map(|row| {
                assert_eq!(row.chars().count(), 3);
                row.chars().map(|symbol| Cell {
                    symbol: symbol.to_string(),
                    ..Cell::default()
                })
            })
            .collect();
        let mut frame = terminal_frame(cells);
        frame.rows = rows.len() as u16;
        frame
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, area: Rect, y: u16) -> String {
        (area.x..area.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn worktree_preview_separates_submodules_and_configured_subtrees() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let worktree = WorktreeInfo {
            name: "main".into(),
            branch: "main".into(),
            path: "/repo".into(),
            is_main: true,
            alias: None,
            sessions: vec![SessionInfo {
                session_id: SessionId(1),
                pane_id: PaneId(1),
                terminal_id: TerminalId(1),
                agent: Some("codex".into()),
                display_name: "agent".into(),
                agent_status: AgentState::Working,
                revision: 1,
                layout: PaneLayout::Leaf { pane_id: PaneId(1) },
                panes: vec![PaneInfo {
                    pane_id: PaneId(1),
                    terminal_id: TerminalId(1),
                    label: "terminal".into(),
                    agent: Some("codex".into()),
                    agent_status: AgentState::Working,
                    revision: 1,
                    exited: false,
                    listening_ports: vec![5173],
                    foreground_job: false,
                    outcome_acknowledged: false,
                }],
                muted: false,
                outcome_acknowledged: false,
            }],
            expanded: true,
            git_info: Some(GitInfo {
                recent_commits: vec![],
                modified_files: vec!["ordinary.txt".into()],
                submodules: Some(vec![SubmoduleInfo {
                    path: "vendor/module".into(),
                    commit_state: SubmoduleCommitState::InSync,
                    modified_content: true,
                    untracked_content: false,
                }]),
                subtrees: vec![SubtreeInfo {
                    path: "vendor/asched".into(),
                    modified_files: vec![],
                }],
                ahead: 0,
                behind: 0,
                remote_branch: Some("origin/main".into()),
            }),
            fetch_failed: false,
            fetch_fail_count: 0,
            fetch_fail_reason: None,
            last_fetched: None,
            git_info_fetched_at: None,
        };

        terminal
            .draw(|frame| render_worktree_preview(frame, frame.area(), &worktree, "wsx"))
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Ports:"), "{text:?}");
        assert!(text.contains(":5173"), "{text:?}");
        assert!(text.contains("Local:"), "{text:?}");
        assert!(text.contains("1 file modified"), "{text:?}");
        assert!(text.contains("Submodules:"), "{text:?}");
        assert!(text.contains("vendor/module"), "{text:?}");
        assert!(text.contains("modified content"), "{text:?}");
        assert!(text.contains("Subtrees:"), "{text:?}");
        assert!(text.contains("vendor/asched"), "{text:?}");
    }

    #[test]
    fn terminal_breadcrumb_shows_agent_and_ports_without_state_words() {
        let backend = TestBackend::new(64, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let session = SessionInfo {
            session_id: SessionId(1),
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            agent: Some("codex".into()),
            display_name: "review".into(),
            agent_status: AgentState::Idle,
            revision: 1,
            layout: PaneLayout::Leaf { pane_id: PaneId(1) },
            panes: vec![PaneInfo {
                pane_id: PaneId(1),
                terminal_id: TerminalId(1),
                label: "terminal".into(),
                agent: Some("codex".into()),
                agent_status: AgentState::Idle,
                revision: 1,
                exited: false,
                listening_ports: vec![3000, 5173],
                foreground_job: false,
                outcome_acknowledged: false,
            }],
            muted: false,
            outcome_acknowledged: false,
        };

        terminal
            .draw(|frame| {
                render_terminal_breadcrumb(
                    frame,
                    frame.area(),
                    TerminalBreadcrumbView {
                        project: "wsx",
                        worktree: "main",
                        session: &session,
                        pane: None,
                        port_visibility: PortVisibility::All,
                        animation_frame: 0,
                    },
                )
            })
            .unwrap();

        let text = terminal.backend().buffer().content()[..64]
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("wsx › main › review"));
        assert!(text.contains("(codex)"));
        assert!(text.contains(":3000 :5173"));
        assert!(!text.contains("idle"));

        let backend = TestBackend::new(64, 1);
        let mut default_policy = Terminal::new(backend).unwrap();
        default_policy
            .draw(|frame| {
                render_terminal_breadcrumb(
                    frame,
                    frame.area(),
                    TerminalBreadcrumbView {
                        project: "wsx",
                        worktree: "main",
                        session: &session,
                        pane: None,
                        port_visibility: PortVisibility::NonAgentic,
                        animation_frame: 0,
                    },
                )
            })
            .unwrap();
        let text = default_policy
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains(":3000"));
    }

    #[test]
    fn terminal_breadcrumb_omits_identity_for_an_ordinary_shell() {
        let backend = TestBackend::new(48, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let session = SessionInfo {
            session_id: SessionId(1),
            pane_id: PaneId(1),
            terminal_id: TerminalId(1),
            agent: None,
            display_name: "shell-session".into(),
            agent_status: AgentState::Unknown,
            revision: 1,
            layout: PaneLayout::Leaf { pane_id: PaneId(1) },
            panes: vec![],
            muted: false,
            outcome_acknowledged: false,
        };

        terminal
            .draw(|frame| {
                render_terminal_breadcrumb(
                    frame,
                    frame.area(),
                    TerminalBreadcrumbView {
                        project: "wsx",
                        worktree: "main",
                        session: &session,
                        pane: None,
                        port_visibility: PortVisibility::All,
                        animation_frame: 0,
                    },
                )
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("shell-session"));
        assert!(!text.contains("unknown"));
        assert!(!text.contains(" · shell"));
    }

    #[test]
    fn terminal_row_projection_top_aligns_short_frames_and_tail_crops_tall_frames() {
        let short = terminal_row_projection(2, 4);
        assert_eq!(
            short,
            TerminalRowProjection {
                source_start: 0,
                len: 2,
            }
        );
        assert_eq!(short.target_row(0), Some(0));
        assert_eq!(short.target_row(1), Some(1));

        let cropped = terminal_row_projection(4, 2);
        assert_eq!(
            cropped,
            TerminalRowProjection {
                source_start: 2,
                len: 2,
            }
        );
        assert_eq!(cropped.target_row(0), None);
        assert_eq!(cropped.target_row(2), Some(0));
        assert_eq!(cropped.target_row(3), Some(1));

        assert_eq!(
            terminal_row_projection(4, 0),
            TerminalRowProjection {
                source_start: 4,
                len: 0,
            }
        );
    }

    #[test]
    fn workspace_preview_top_aligns_a_shorter_terminal_frame() {
        let backend = TestBackend::new(5, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let terminal_frame = terminal_frame_rows(&["abc", "def"]);
        let area = Rect::new(1, 1, 3, 4);

        terminal
            .draw(|frame| render_terminal_frame(frame, area, Some(&terminal_frame), false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer_row(buffer, area, 1), "abc");
        assert_eq!(buffer_row(buffer, area, 2), "def");
        assert_eq!(buffer_row(buffer, area, 3), "   ");
        assert_eq!(buffer_row(buffer, area, 4), "   ");
    }

    #[test]
    fn workspace_preview_crops_a_taller_terminal_frame_from_the_top() {
        let backend = TestBackend::new(3, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let terminal_frame = terminal_frame_rows(&["abc", "def", "ghi", "jkl"]);

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer_row(buffer, buffer.area, 0), "ghi");
        assert_eq!(buffer_row(buffer, buffer.area, 1), "jkl");
    }

    #[test]
    fn interactive_terminal_handoff_keeps_a_mismatched_frame_bottom_anchored() {
        let backend = TestBackend::new(3, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let terminal_frame = terminal_frame_rows(&["abc", "def", "ghi", "jkl"]);

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer_row(buffer, buffer.area, 0), "ghi");
        assert_eq!(buffer_row(buffer, buffer.area, 1), "jkl");
    }

    #[test]
    fn interactive_terminal_matching_baseline_keeps_its_top_origin() {
        let backend = TestBackend::new(3, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let terminal_frame = terminal_frame_rows(&["abc", "def"]);

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer_row(buffer, buffer.area, 0), "abc");
        assert_eq!(buffer_row(buffer, buffer.area, 1), "def");
    }

    #[test]
    fn interactive_terminal_cursor_follows_the_projected_source_row() {
        let backend = TestBackend::new(3, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut terminal_frame = terminal_frame_rows(&["abc", "def"]);
        terminal_frame.cursor = Cursor {
            x: 1,
            y: 1,
            visible: true,
            blinking: false,
            shape: 0,
        };

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), true))
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(1, 1));
    }

    #[test]
    fn terminal_selection_overlay_follows_cropped_source_rows_and_wide_cells() {
        let backend = TestBackend::new(3, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut terminal_frame = terminal_frame_rows(&["abc", "def", "界 x", "jkl"]);
        terminal_frame.cells[6].width = CellWidth::Wide;
        terminal_frame.cells[7].symbol.clear();
        terminal_frame.cells[7].width = CellWidth::SpacerTail;
        terminal_frame.selection = vec![TerminalSelectionRange {
            row: 2,
            start_col: 0,
            end_col: 1,
        }];

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let selection_style = theme::terminal_selection();
        assert_eq!(buffer[(0, 0)].bg, selection_style.bg.unwrap());
        assert_eq!(buffer[(2, 0)].bg, Color::Reset);
        assert_eq!(buffer[(0, 1)].bg, Color::Reset);
    }

    #[test]
    fn terminal_projection_keeps_default_background_transparent() {
        let backend = TestBackend::new(3, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let terminal_frame = terminal_frame(vec![
            Cell {
                symbol: "a".into(),
                ..Cell::default()
            },
            Cell {
                symbol: "b".into(),
                bg: Some([1, 2, 3]),
                ..Cell::default()
            },
            Cell {
                symbol: "c".into(),
                ..Cell::default()
            },
        ]);

        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&terminal_frame), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(1, 0)].bg, Color::Rgb(1, 2, 3));
        assert_eq!(buffer[(2, 0)].bg, Color::Reset);
    }

    #[test]
    fn terminal_projection_preserves_wide_tail_and_clears_it_after_replacement() {
        let backend = TestBackend::new(9, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let wide = terminal_frame(vec![
            Cell {
                symbol: "界".into(),
                width: CellWidth::Wide,
                ..Cell::default()
            },
            Cell {
                width: CellWidth::SpacerTail,
                ..Cell::default()
            },
            Cell {
                symbol: "x".into(),
                ..Cell::default()
            },
        ]);
        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&wide), true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_eq!(buffer[(8, 3)].bg, Color::Reset);
        let mut projected = ratatui::buffer::Cell::default();
        project_terminal_cell(&mut projected, &wide.cells[1]);
        assert!(projected.skip);

        let narrow = terminal_frame(vec![
            Cell {
                symbol: "a".into(),
                ..Cell::default()
            },
            Cell {
                symbol: "b".into(),
                ..Cell::default()
            },
            Cell {
                symbol: "x".into(),
                ..Cell::default()
            },
        ]);
        terminal
            .draw(|frame| render_terminal_frame(frame, frame.area(), Some(&narrow), true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "a");
        assert_eq!(buffer[(1, 0)].symbol(), "b");
        project_terminal_cell(&mut projected, &narrow.cells[1]);
        assert!(!projected.skip);
    }
}
