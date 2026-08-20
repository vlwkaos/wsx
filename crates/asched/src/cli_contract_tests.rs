use super::*;
use clap::Parser;
use std::os::fd::AsRawFd;

#[test]
fn given_nonnumeric_startup_descriptor_when_validated_then_it_is_rejected() {
    assert!(validate_startup_descriptor(OsStr::new("not-a-number")).is_err());
}

#[test]
fn given_negative_startup_descriptor_when_validated_then_it_is_rejected() {
    assert!(validate_startup_descriptor(OsStr::new("-1")).is_err());
}

#[test]
fn given_stdio_startup_descriptors_when_validated_then_each_is_rejected() {
    assert!([0, 1, 2]
        .into_iter()
        .all(
            |descriptor| validate_startup_descriptor(OsStr::new(&descriptor.to_string())).is_err()
        ));
}

#[test]
fn given_closed_positive_startup_descriptor_when_validated_then_it_is_rejected() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli-contract-closed-descriptor");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::create(&path).unwrap();
    let descriptor = file.as_raw_fd();
    drop(file);

    let result = validate_startup_descriptor(OsStr::new(&descriptor.to_string()));
    let _ = std::fs::remove_file(path);

    assert!(result.is_err());
}

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
