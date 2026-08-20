//! Minimal project/routine control surface.
//! ref: README.md#tui

mod theme;

use crate::service::{registry, send_tui_action, send_tui_observation, terminal_safe};
use anyhow::Result;
use asched_core::routine::ipc::{Action, Response, RoutineView};
use asched_core::routine::Trigger;
use asched_core::Project;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
enum Row {
    Project {
        project: Project,
        error: Option<String>,
    },
    Routine {
        project: Project,
        revision: u64,
        view: Box<RoutineView>,
    },
}

impl Row {
    fn identity(&self) -> (String, Option<String>) {
        match self {
            Self::Project { project, .. } => (project.name.clone(), None),
            Self::Routine { project, view, .. } => {
                (project.name.clone(), Some(view.routine.name.clone()))
            }
        }
    }
}

#[derive(Default)]
struct Model {
    rows: Vec<Row>,
    selected: usize,
    status: String,
    refresh_requested: bool,
    announce_refresh: bool,
    pending_action: Option<(Project, Action)>,
    action_in_flight: bool,
}

impl Model {
    fn replace_rows(&mut self, rows: Vec<Row>, announce: bool) {
        let selected = self.rows.get(self.selected).map(Row::identity);
        self.rows = rows;
        self.selected = selected
            .and_then(|identity| self.rows.iter().position(|row| row.identity() == identity))
            .unwrap_or(0)
            .min(self.rows.len().saturating_sub(1));
        if announce {
            self.status = "refreshed".into();
        }
    }

    fn request_refresh(&mut self, announce: bool) {
        self.refresh_requested = true;
        self.announce_refresh |= announce;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
    }

    fn selected_routine(&self) -> Option<(&Project, u64, &RoutineView)> {
        match self.rows.get(self.selected) {
            Some(Row::Routine {
                project,
                revision,
                view,
            }) => Some((project, *revision, view)),
            _ => None,
        }
    }

    fn run_or_cancel(&mut self) {
        let Some((project, _, view)) = self.selected_routine() else {
            self.status = "select a routine first".into();
            return;
        };
        let project = project.clone();
        let action = if view.capabilities.can_cancel {
            Action::Cancel {
                name: view.routine.name.clone(),
            }
        } else if view.capabilities.can_run {
            Action::Run {
                name: view.routine.name.clone(),
            }
        } else {
            self.status = "selected routine cannot run or cancel".into();
            return;
        };
        self.apply(&project, action);
    }

    fn toggle_enabled(&mut self) {
        let Some((project, revision, view)) = self.selected_routine() else {
            self.status = "select a routine first".into();
            return;
        };
        if !view.capabilities.can_toggle_enabled {
            self.status = "selected routine cannot change enabled state".into();
            return;
        }
        let project = project.clone();
        let name = view.routine.name.clone();
        let enabled = !view.routine.enabled;
        self.apply(
            &project,
            Action::SetEnabled {
                revision,
                name,
                enabled,
            },
        );
    }

    fn apply(&mut self, project: &Project, action: Action) {
        if self.action_in_flight || self.pending_action.is_some() {
            self.status = "another action is still in progress".into();
            return;
        }
        self.pending_action = Some((project.clone(), action));
        self.status = "action in progress".into();
    }
}

enum WorkerResult {
    Refresh(Result<Vec<Row>, String>),
    Action(Result<(), String>),
}

pub fn run() -> Result<()> {
    let mut session = TerminalSession::enter()?;
    let mut model = Model::default();
    model.request_refresh(false);
    let (sender, receiver) = mpsc::channel();
    let mut refresh_in_flight = false;
    let mut last_refresh = Instant::now();
    let mut dirty = true;
    loop {
        receive_worker_results(&receiver, &mut model, &mut refresh_in_flight, &mut dirty);
        if model.refresh_requested && !refresh_in_flight && !model.action_in_flight {
            model.refresh_requested = false;
            refresh_in_flight = true;
            spawn_refresh(sender.clone());
        }
        if !model.action_in_flight {
            if let Some((project, action)) = model.pending_action.take() {
                model.action_in_flight = true;
                spawn_action(sender.clone(), project, action);
            }
        }
        if dirty {
            session.terminal.draw(|frame| render(frame, &model))?;
            dirty = false;
        }
        let wait = REFRESH_INTERVAL
            .saturating_sub(last_refresh.elapsed())
            .min(Duration::from_millis(250));
        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
                if dispatch(key, &mut model) {
                    break;
                }
                dirty = true;
            }
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            model.request_refresh(false);
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn receive_worker_results(
    receiver: &Receiver<WorkerResult>,
    model: &mut Model,
    refresh_in_flight: &mut bool,
    dirty: &mut bool,
) {
    while let Ok(result) = receiver.try_recv() {
        match result {
            WorkerResult::Refresh(result) => {
                *refresh_in_flight = false;
                let announce = std::mem::take(&mut model.announce_refresh);
                match result {
                    Ok(rows) => model.replace_rows(rows, announce),
                    Err(error) => model.status = error,
                }
            }
            WorkerResult::Action(result) => {
                model.action_in_flight = false;
                model.status = match result {
                    Ok(()) => "action completed".into(),
                    Err(error) => error,
                };
                model.request_refresh(false);
            }
        }
        *dirty = true;
    }
}

fn spawn_refresh(sender: Sender<WorkerResult>) {
    std::thread::spawn(move || {
        let result = load_rows().map_err(|error| error.to_string());
        let _ = sender.send(WorkerResult::Refresh(result));
    });
}

fn load_rows() -> Result<Vec<Row>> {
    let projects = registry()?.load()?.projects;
    let results = std::thread::scope(|scope| {
        projects
            .into_iter()
            .map(|project| {
                scope.spawn(move || {
                    let response = send_tui_observation(&project.working_dir, Action::List);
                    (project, response)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join())
            .collect::<Vec<_>>()
    });
    let mut rows = Vec::new();
    for result in results {
        let (project, response) =
            result.map_err(|_| anyhow::anyhow!("routine refresh worker panicked"))?;
        match response {
            Ok(Response::Routines { revision, routines }) => {
                rows.push(Row::Project {
                    project: project.clone(),
                    error: None,
                });
                rows.extend(routines.into_iter().map(|view| Row::Routine {
                    project: project.clone(),
                    revision,
                    view: Box::new(view),
                }));
            }
            Ok(_) => rows.push(Row::Project {
                project,
                error: Some("unexpected daemon response".into()),
            }),
            Err(error) => rows.push(Row::Project {
                project,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(rows)
}

fn spawn_action(sender: Sender<WorkerResult>, project: Project, action: Action) {
    std::thread::spawn(move || {
        let result = send_tui_action(&project.working_dir, action)
            .map(|_| ())
            .map_err(|error| error.to_string());
        let _ = sender.send(WorkerResult::Action(result));
    });
}

fn dispatch(key: KeyEvent, model: &mut Model) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => true,
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => true,
        (KeyCode::Down | KeyCode::Char('j'), _) => {
            model.move_selection(1);
            false
        }
        (KeyCode::Up | KeyCode::Char('k'), _) => {
            model.move_selection(-1);
            false
        }
        (KeyCode::Home | KeyCode::Char('g'), _) => {
            model.selected = 0;
            false
        }
        (KeyCode::End | KeyCode::Char('G'), _) => {
            model.selected = model.rows.len().saturating_sub(1);
            false
        }
        (KeyCode::Char('r'), _) => {
            model.request_refresh(true);
            false
        }
        (KeyCode::Char(' '), _) => {
            model.run_or_cancel();
            false
        }
        (KeyCode::Char('e'), _) => {
            model.toggle_enabled();
            false
        }
        _ => false,
    }
}

fn render(frame: &mut Frame<'_>, model: &Model) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("asched", theme::title()),
            Span::raw("  project routines"),
        ])),
        outer[0],
    );

    let direction = if outer[1].width < 72 {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let body = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(outer[1]);
    render_tree(frame, body[0], model);
    render_details(frame, body[1], model);
    render_footer(frame, outer[2], model);
}

fn render_tree(frame: &mut Frame<'_>, area: Rect, model: &Model) {
    let items = if model.rows.is_empty() {
        vec![ListItem::new(
            "No projects. Use: asched project add NAME DIR",
        )]
    } else {
        model
            .rows
            .iter()
            .map(|row| match row {
                Row::Project { project, error } => {
                    let suffix = error.as_ref().map(|_| "  !").unwrap_or("");
                    ListItem::new(format!("{}{}", terminal_safe(&project.name), suffix)).style(
                        if error.is_some() {
                            theme::error()
                        } else {
                            theme::project()
                        },
                    )
                }
                Row::Routine { view, .. } => {
                    let state = if view.capabilities.can_cancel {
                        "running"
                    } else if view.routine.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    ListItem::new(format!(
                        "  {}  {}",
                        terminal_safe(&view.routine.name),
                        state
                    ))
                    .style(theme::routine(view.routine.enabled))
                }
            })
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Projects"))
        .highlight_style(theme::selected())
        .highlight_symbol("> ");
    let mut state =
        ListState::default().with_selected((!model.rows.is_empty()).then_some(model.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_details(frame: &mut Frame<'_>, area: Rect, model: &Model) {
    let lines = match model.rows.get(model.selected) {
        Some(Row::Project { project, error }) => {
            let mut lines = vec![
                Line::styled(terminal_safe(&project.name), theme::title()),
                Line::raw(terminal_safe(&project.working_dir.to_string_lossy())),
            ];
            if let Some(error) = error {
                lines.push(Line::raw(""));
                lines.push(Line::styled(terminal_safe(error), theme::error()));
            }
            lines
        }
        Some(Row::Routine {
            project,
            revision,
            view,
        }) => routine_details(project, *revision, view),
        None => vec![
            Line::styled("No registered projects", theme::title()),
            Line::raw("Register one with the CLI, then press r."),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn routine_details(project: &Project, revision: u64, view: &RoutineView) -> Vec<Line<'static>> {
    let latest = view
        .latest_run
        .as_ref()
        .map(|run| format!("{:?} at {}", run.status, run.started_epoch))
        .unwrap_or_else(|| "never".into());
    vec![
        Line::styled(terminal_safe(&view.routine.name), theme::title()),
        Line::raw(format!("project   {}", terminal_safe(&project.name))),
        Line::raw(format!(
            "trigger   {}",
            terminal_safe(&match &view.routine.trigger {
                Trigger::Cron(expression) => format!("cron:{expression}"),
                Trigger::Event { kind } => format!("event:{kind}"),
            })
        )),
        Line::raw(format!("enabled   {}", view.routine.enabled)),
        Line::raw(format!("revision  {revision}")),
        Line::raw(format!("command   {}", format_argv(&view.routine.command))),
        Line::raw(format!("next run  {}", format_epoch(view.next_run_epoch))),
        Line::raw(format!("latest    {latest}")),
        Line::raw(""),
        Line::raw(terminal_safe(&view.routine.prompt)),
    ]
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, model: &Model) {
    let mut hints = vec!["q quit", "j/k move", "r refresh"];
    if let Some((_, _, view)) = model.selected_routine() {
        if view.capabilities.can_cancel {
            hints.push("space cancel");
        } else if view.capabilities.can_run {
            hints.push("space run");
        }
        if view.capabilities.can_toggle_enabled {
            hints.push("e enable/disable");
        }
    }
    let status = if model.status.is_empty() {
        hints.join("  ")
    } else {
        format!("{}  |  {}", hints.join("  "), terminal_safe(&model.status))
    };
    frame.render_widget(Paragraph::new(status).style(theme::footer()), area);
}

fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}

fn format_epoch(epoch: Option<i64>) -> String {
    epoch
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".into())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod contract_tests;
