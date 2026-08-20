use super::*;

#[test]
fn given_escape_and_newline_when_made_terminal_safe_then_no_control_character_is_emitted() {
    let safe = terminal_safe("\u{1b}[31mline\nnext");

    assert_eq!(
        (safe.as_str(), safe.chars().any(char::is_control)),
        ("\\u{1b}[31mline\\nnext", false)
    );
}

#[test]
fn given_tui_timeout_configuration_when_compared_to_cancellation_bound_then_observations_stay_short_and_mutations_stay_safe(
) {
    assert!(
        TUI_REFRESH_TIMEOUT <= Duration::from_millis(500)
            && TUI_ACTION_TIMEOUT >= Duration::from_secs(5)
            && TUI_ACTION_TIMEOUT > TUI_REFRESH_TIMEOUT
    );
}
