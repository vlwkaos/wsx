use wsx_core::config::global::{project_has_activity_within, GlobalConfig};

const HOUR_MS: u64 = 60 * 60 * 1_000;
const DAY_MS: u64 = 24 * HOUR_MS;

#[test]
fn omitted_auto_collapse_setting_defaults_to_24_hours() {
    let config: GlobalConfig = toml::from_str("").expect("empty TOML must decode");

    assert_eq!(config.auto_collapse_after_hours, 24);
}

#[test]
fn positive_auto_collapse_setting_round_trips_through_toml() {
    let config: GlobalConfig =
        toml::from_str("auto_collapse_after_hours = 7\n").expect("positive setting must decode");
    let serialized = toml::to_string(&config).expect("config must serialize");
    let decoded: GlobalConfig = toml::from_str(&serialized).expect("serialized config must decode");

    assert_eq!(decoded.auto_collapse_after_hours, 7);
    assert!(serialized.contains("auto_collapse_after_hours = 7"));
}

#[test]
fn zero_auto_collapse_setting_decodes_and_round_trips() {
    let config: GlobalConfig =
        toml::from_str("auto_collapse_after_hours = 0\n").expect("zero setting must decode");
    let serialized = toml::to_string(&config).expect("config must serialize");
    let decoded: GlobalConfig = toml::from_str(&serialized).expect("serialized config must decode");

    assert_eq!(decoded.auto_collapse_after_hours, 0);
    assert!(serialized.contains("auto_collapse_after_hours = 0"));
}

#[test]
fn invalid_auto_collapse_setting_types_remain_toml_parse_errors() {
    let string_value = toml::from_str::<GlobalConfig>("auto_collapse_after_hours = '24'\n");
    let negative_value = toml::from_str::<GlobalConfig>("auto_collapse_after_hours = -1\n");
    let float_value = toml::from_str::<GlobalConfig>("auto_collapse_after_hours = 24.0\n");
    let bool_value = toml::from_str::<GlobalConfig>("auto_collapse_after_hours = true\n");

    assert!(string_value.is_err());
    assert!(negative_value.is_err());
    assert!(float_value.is_err());
    assert!(bool_value.is_err());
}

#[test]
fn activity_exactly_at_the_configured_boundary_is_active() {
    let config = GlobalConfig {
        auto_collapse_after_hours: 7,
        ..GlobalConfig::default()
    };
    let window_ms = config
        .auto_collapse_window_ms()
        .expect("positive automatic-collapse window");
    let now = 30 * DAY_MS;
    let exactly_at_boundary = now - window_ms;

    assert!(project_has_activity_within(
        Some(exactly_at_boundary),
        None,
        now,
        window_ms,
    ));
    assert!(!project_has_activity_within(
        Some(exactly_at_boundary - 1),
        None,
        now,
        window_ms,
    ));
}

#[test]
fn largest_toml_auto_collapse_setting_decodes_and_round_trips() {
    let config: GlobalConfig = toml::from_str("auto_collapse_after_hours = 9223372036854775807\n")
        .expect("largest TOML integer setting must decode");

    assert_eq!(config.auto_collapse_after_hours, i64::MAX as u64);
    let serialized = toml::to_string(&config).expect("maximum TOML setting must serialize");
    let decoded: GlobalConfig = toml::from_str(&serialized).expect("serialized config must decode");
    assert_eq!(decoded.auto_collapse_after_hours, i64::MAX as u64);
}

#[test]
fn maximum_programmatic_auto_collapse_setting_has_a_safe_window() {
    let config = GlobalConfig {
        auto_collapse_after_hours: u64::MAX,
        ..GlobalConfig::default()
    };

    assert_eq!(config.auto_collapse_window_ms(), Some(u64::MAX));
}
