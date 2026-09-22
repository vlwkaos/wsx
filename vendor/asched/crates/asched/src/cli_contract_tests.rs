use super::*;
use clap::Parser;

#[test]
fn given_repeated_project_flags_when_parsed_then_order_and_duplicates_are_preserved() {
    let parsed = Cli::try_parse_from([
        "asched",
        "routine",
        "list",
        "--project",
        "alpha",
        "--project",
        "beta",
        "--project",
        "alpha",
    ])
    .unwrap();

    assert!(matches!(
        parsed.command,
        Some(Command::Routine {
            command: RoutineCommand::List { project, .. }
        }) if project == ["alpha", "beta", "alpha"]
    ));
}

#[test]
fn given_event_trigger_when_add_is_parsed_then_kind_is_preserved() {
    let parsed = Cli::try_parse_from([
        "asched",
        "routine",
        "add",
        "watch",
        "--project",
        "alpha",
        "--event",
        "filesystem.changed",
        "--arg",
        "/bin/true",
    ])
    .unwrap();

    assert!(matches!(
        parsed.command,
        Some(Command::Routine {
            command: RoutineCommand::Add {
                cron: None,
                event: Some(kind),
                ..
            }
        }) if kind == "filesystem.changed"
    ));
}

#[test]
fn given_fire_arguments_when_parsed_then_generic_event_fields_are_preserved() {
    let parsed = Cli::try_parse_from([
        "asched",
        "routine",
        "fire",
        "--project",
        "alpha",
        "--kind",
        "filesystem.changed",
        "--event-id",
        "delivery-1",
        "--payload",
        "{}",
    ])
    .unwrap();

    assert!(matches!(
        parsed.command,
        Some(Command::Routine {
            command: RoutineCommand::Fire {
                project,
                kind,
                event_id,
                payload: Some(payload),
                ..
            }
        }) if project == "alpha"
            && kind == "filesystem.changed"
            && event_id == "delivery-1"
            && payload == "{}"
    ));
}

#[test]
fn given_repeated_direct_args_when_parsed_then_each_argv_item_is_preserved() {
    let parsed = Cli::try_parse_from([
        "asched",
        "routine",
        "add",
        "daily",
        "--project",
        "alpha",
        "--cron",
        "0 0 * * *",
        "--arg",
        "/bin/echo",
        "--arg",
        "--literal",
    ])
    .unwrap();

    assert!(matches!(
        parsed.command,
        Some(Command::Routine {
            command: RoutineCommand::Add { argv, .. }
        }) if argv == ["/bin/echo", "--literal"]
    ));
}
