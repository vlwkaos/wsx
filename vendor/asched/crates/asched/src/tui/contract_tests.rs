use super::*;
use asched_core::routine::{Capabilities, Routine};
use crossterm::event::KeyEventKind;
use ratatui::backend::TestBackend;
use std::path::PathBuf;

fn project(name: &str) -> Project {
    Project {
        name: name.into(),
        working_dir: PathBuf::from(format!("/projects/{name}")),
    }
}

fn routine_view(enabled: bool, running: bool, can_toggle: bool) -> RoutineView {
    let mut capabilities = Capabilities::for_running(running);
    capabilities.can_toggle_enabled = can_toggle;
    RoutineView {
        routine: Routine {
            name: "daily".into(),
            trigger: Trigger::Cron("0 9 * * *".into()),
            command: vec!["echo".into(), "hello world".into()],
            prompt: "prompt text".into(),
            enabled,
        },
        capabilities,
        next_run_epoch: Some(123),
        latest_run: None,
        recent_runs: vec![],
    }
}

fn rendered(model: &Model, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, model)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn contains_words(screen: &str, expected: &[&str]) -> bool {
    let screen = screen.to_lowercase();
    expected
        .iter()
        .all(|word| screen.contains(&word.to_lowercase()))
}

fn routine_row(project_name: &str, enabled: bool, running: bool, can_toggle: bool) -> Row {
    Row::Routine {
        project: project(project_name),
        revision: 7,
        view: Box::new(routine_view(enabled, running, can_toggle)),
    }
}

fn routine_model(enabled: bool, running: bool, can_toggle: bool) -> Model {
    Model {
        rows: vec![routine_row("alpha", enabled, running, can_toggle)],
        ..Default::default()
    }
}

fn press(character: char, model: &mut Model) -> bool {
    dispatch(
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
        model,
    )
}

#[test]
fn given_refresh_key_when_dispatched_then_refresh_is_queued_without_starting_io() {
    let mut model = Model::default();

    let exit = press('r', &mut model);

    assert_eq!(
        (
            exit,
            model.refresh_requested,
            model.announce_refresh,
            model.action_in_flight,
            model.pending_action.is_none(),
        ),
        (false, true, true, false, true)
    );
}

#[test]
fn given_runnable_routine_when_space_is_dispatched_then_run_is_queued_in_progress() {
    let mut model = routine_model(true, false, true);

    let exit = press(' ', &mut model);

    assert!(
        !exit
            && !model.action_in_flight
            && model.status == "action in progress"
            && matches!(
                model.pending_action.as_ref(),
                Some((project, Action::Run { name }))
                    if project.name == "alpha" && name == "daily"
            )
    );
}

#[test]
fn given_cancellable_routine_when_space_is_dispatched_then_cancel_is_queued_in_progress() {
    let mut model = routine_model(true, true, true);

    let exit = press(' ', &mut model);

    assert!(
        !exit
            && !model.action_in_flight
            && model.status == "action in progress"
            && matches!(
                model.pending_action.as_ref(),
                Some((project, Action::Cancel { name }))
                    if project.name == "alpha" && name == "daily"
            )
    );
}

#[test]
fn given_disabled_toggleable_routine_when_enable_key_is_dispatched_then_enable_is_queued_in_progress(
) {
    let mut model = routine_model(false, false, true);

    let exit = press('e', &mut model);

    assert!(
        !exit
            && !model.action_in_flight
            && model.status == "action in progress"
            && matches!(
                model.pending_action.as_ref(),
                Some((
                    project,
                    Action::SetEnabled {
                        revision: 7,
                        name,
                        enabled: true,
                    },
                )) if project.name == "alpha" && name == "daily"
            )
    );
}

#[test]
fn given_enabled_toggleable_routine_when_enable_key_is_dispatched_then_disable_is_queued_in_progress(
) {
    let mut model = routine_model(true, false, true);

    let exit = press('e', &mut model);

    assert!(
        !exit
            && !model.action_in_flight
            && model.status == "action in progress"
            && matches!(
                model.pending_action.as_ref(),
                Some((
                    project,
                    Action::SetEnabled {
                        revision: 7,
                        name,
                        enabled: false,
                    },
                )) if project.name == "alpha" && name == "daily"
            )
    );
}

#[test]
fn given_successful_action_worker_result_when_received_then_busy_clears_and_refresh_is_requested() {
    let (sender, receiver) = mpsc::channel();
    sender.send(WorkerResult::Action(Ok(()))).unwrap();
    let mut model = Model {
        action_in_flight: true,
        ..Default::default()
    };
    let mut refresh_in_flight = false;
    let mut dirty = false;

    receive_worker_results(&receiver, &mut model, &mut refresh_in_flight, &mut dirty);

    assert_eq!(
        (
            model.action_in_flight,
            model.status.as_str(),
            model.refresh_requested,
            refresh_in_flight,
            dirty,
        ),
        (false, "action completed", true, false, true)
    );
}

#[test]
fn given_failed_action_worker_result_when_received_then_busy_clears_and_error_requests_refresh() {
    let (sender, receiver) = mpsc::channel();
    sender
        .send(WorkerResult::Action(Err("action failed".into())))
        .unwrap();
    let mut model = Model {
        action_in_flight: true,
        ..Default::default()
    };
    let mut refresh_in_flight = false;
    let mut dirty = false;

    receive_worker_results(&receiver, &mut model, &mut refresh_in_flight, &mut dirty);

    assert_eq!(
        (
            model.action_in_flight,
            model.status.as_str(),
            model.refresh_requested,
            refresh_in_flight,
            dirty,
        ),
        (false, "action failed", true, false, true)
    );
}

#[test]
fn given_reordered_refresh_worker_rows_when_received_then_selection_identity_is_preserved_and_busy_clears(
) {
    let mut model = Model {
        rows: vec![
            Row::Project {
                project: project("alpha"),
                error: None,
            },
            routine_row("alpha", true, false, true),
            routine_row("beta", true, false, true),
        ],
        selected: 2,
        announce_refresh: true,
        ..Default::default()
    };
    let refreshed = vec![
        Row::Project {
            project: project("beta"),
            error: None,
        },
        routine_row("beta", true, false, true),
        Row::Project {
            project: project("alpha"),
            error: None,
        },
        routine_row("alpha", true, false, true),
    ];
    let (sender, receiver) = mpsc::channel();
    sender.send(WorkerResult::Refresh(Ok(refreshed))).unwrap();
    let mut refresh_in_flight = true;
    let mut dirty = false;

    receive_worker_results(&receiver, &mut model, &mut refresh_in_flight, &mut dirty);

    assert_eq!(
        (
            model.rows.len(),
            model.selected,
            model.rows[model.selected].identity(),
            model.status.as_str(),
            refresh_in_flight,
            model.announce_refresh,
            dirty,
        ),
        (
            4,
            1,
            ("beta".into(), Some("daily".into())),
            "refreshed",
            false,
            false,
            true,
        )
    );
}

#[test]
fn given_empty_model_when_moving_selection_then_selection_remains_zero() {
    let mut model = Model::default();

    model.move_selection(1);
    model.move_selection(-1);

    assert_eq!(model.selected, 0);
}

#[test]
fn given_bounded_rows_when_navigating_beyond_edges_then_selection_is_clamped() {
    let mut model = Model {
        rows: vec![
            Row::Project {
                project: project("alpha"),
                error: None,
            },
            Row::Project {
                project: project("beta"),
                error: None,
            },
        ],
        ..Default::default()
    };

    model.move_selection(-1);
    model.move_selection(99);

    assert_eq!(model.selected, 1);
}

#[test]
fn given_stale_out_of_range_selection_when_moved_then_selection_is_clamped_to_existing_rows() {
    let mut model = Model {
        rows: vec![
            Row::Project {
                project: project("alpha"),
                error: None,
            },
            Row::Project {
                project: project("beta"),
                error: None,
            },
        ],
        selected: usize::MAX,
        ..Default::default()
    };

    model.move_selection(0);

    assert_eq!(model.selected, 1);
}

#[test]
fn given_nonpress_key_event_when_dispatched_then_it_is_ignored() {
    let mut model = Model {
        rows: vec![
            Row::Project {
                project: project("alpha"),
                error: None,
            },
            Row::Project {
                project: project("beta"),
                error: None,
            },
        ],
        ..Default::default()
    };
    let key = KeyEvent::new_with_kind(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Release);

    let exit = dispatch(key, &mut model);

    assert_eq!((exit, model.selected), (false, 0));
}

#[test]
fn given_quit_keys_when_dispatched_then_exit_is_requested() {
    let keys = [
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ];

    let exits = keys
        .into_iter()
        .all(|key| dispatch(key, &mut Model::default()));

    assert!(exits);
}

#[test]
fn given_empty_model_when_rendered_then_registration_guidance_is_visible() {
    let screen = rendered(&Model::default(), 80, 16);

    assert!(contains_words(&screen, &["projects", "registered"]));
}

#[test]
fn given_project_refresh_error_when_rendered_then_project_and_error_are_visible() {
    let model = Model {
        rows: vec![Row::Project {
            project: project("alpha"),
            error: Some("daemon unavailable".into()),
        }],
        ..Default::default()
    };

    let screen = rendered(&model, 80, 16);

    assert!(contains_words(
        &screen,
        &["alpha", "/projects/alpha", "daemon", "unavailable"]
    ));
}

#[test]
fn given_running_routine_when_rendered_then_state_details_and_cancel_hint_are_visible() {
    let model = Model {
        rows: vec![Row::Routine {
            project: project("alpha"),
            revision: 4,
            view: Box::new(routine_view(true, true, true)),
        }],
        ..Default::default()
    };

    let screen = rendered(&model, 100, 18);

    assert!(contains_words(
        &screen,
        &["daily", "running", "4", "cancel", "enable/disable"]
    ));
}

#[test]
fn given_stale_selection_when_rendered_then_rendering_remains_safe() {
    let model = Model {
        rows: vec![Row::Project {
            project: project("alpha"),
            error: None,
        }],
        selected: usize::MAX,
        ..Default::default()
    };

    let screen = rendered(&model, 80, 16);

    assert!(contains_words(&screen, &["alpha", "projects"]));
}

#[test]
fn given_one_cell_terminal_when_rendered_then_rendering_remains_bounded() {
    let screen = rendered(&Model::default(), 1, 1);

    assert_eq!(screen.chars().count(), 1);
}
