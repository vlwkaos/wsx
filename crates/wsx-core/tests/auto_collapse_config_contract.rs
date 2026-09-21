use wsx_core::config::global::{
    project_has_activity_within, AutoCollapsePolicy, GlobalConfig, ADAPTIVE_COLLAPSE_MAX_HOURS,
    ADAPTIVE_COLLAPSE_MIN_HOURS,
};

const HOUR_MS: u64 = 60 * 60 * 1_000;
const DAY_MS: u64 = 24 * HOUR_MS;

#[test]
fn omitted_auto_collapse_setting_defaults_to_adaptive_24_hours() {
    let config: GlobalConfig = toml::from_str("").expect("empty TOML must decode");

    assert_eq!(
        config.auto_collapse,
        AutoCollapsePolicy::Adaptive {
            base_hours: ADAPTIVE_COLLAPSE_MIN_HOURS,
        }
    );
}

#[test]
fn legacy_positive_setting_migrates_to_flat_without_changing_its_window() {
    let config: GlobalConfig =
        toml::from_str("auto_collapse_after_hours = 7\n").expect("legacy setting must decode");
    let serialized = toml::to_string(&config).expect("config must serialize");
    let decoded: GlobalConfig = toml::from_str(&serialized).expect("serialized config must decode");

    assert_eq!(config.auto_collapse, AutoCollapsePolicy::Flat { hours: 7 });
    assert_eq!(decoded.auto_collapse, config.auto_collapse);
    assert!(!serialized.contains("auto_collapse_after_hours"));
    assert!(serialized.contains("mode = \"flat\""));
    assert!(serialized.contains("hours = 7"));
}

#[test]
fn legacy_zero_setting_migrates_to_disabled() {
    let config: GlobalConfig =
        toml::from_str("auto_collapse_after_hours = 0\n").expect("legacy zero must decode");
    let serialized = toml::to_string(&config).expect("config must serialize");
    let decoded: GlobalConfig = toml::from_str(&serialized).expect("serialized config must decode");

    assert_eq!(config.auto_collapse, AutoCollapsePolicy::Disabled);
    assert_eq!(decoded.auto_collapse, AutoCollapsePolicy::Disabled);
    assert!(serialized.contains("mode = \"disabled\""));
}

#[test]
fn canonical_flat_disabled_and_adaptive_policies_round_trip() {
    for policy in [
        AutoCollapsePolicy::Disabled,
        AutoCollapsePolicy::Flat { hours: 36 },
        AutoCollapsePolicy::Adaptive { base_hours: 48 },
    ] {
        let config = GlobalConfig {
            auto_collapse: policy,
            ..GlobalConfig::default()
        };
        let encoded = toml::to_string(&config).unwrap();
        let decoded: GlobalConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.auto_collapse, policy);
    }
}

#[test]
fn adaptive_bounds_and_ambiguous_legacy_mix_fail_closed() {
    for text in [
        "[auto_collapse]\nmode = 'adaptive'\nbase_hours = 23\n",
        "[auto_collapse]\nmode = 'adaptive'\nbase_hours = 673\n",
        "[auto_collapse]\nmode = 'flat'\nhours = 0\n",
        "auto_collapse_after_hours = 24\n[auto_collapse]\nmode = 'disabled'\n",
    ] {
        assert!(toml::from_str::<GlobalConfig>(text).is_err(), "{text}");
    }
    assert_eq!(ADAPTIVE_COLLAPSE_MAX_HOURS, 24 * 28);
}

#[test]
fn invalid_legacy_auto_collapse_types_remain_toml_parse_errors() {
    for text in [
        "auto_collapse_after_hours = '24'\n",
        "auto_collapse_after_hours = -1\n",
        "auto_collapse_after_hours = 24.0\n",
        "auto_collapse_after_hours = true\n",
    ] {
        assert!(toml::from_str::<GlobalConfig>(text).is_err(), "{text}");
    }
}

#[test]
fn activity_exactly_at_a_flat_boundary_is_active() {
    let config = GlobalConfig {
        auto_collapse: AutoCollapsePolicy::Flat { hours: 7 },
        ..GlobalConfig::default()
    };
    let window_ms = config
        .auto_collapse_window_ms()
        .expect("flat policy has a fixed window");
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
fn maximum_flat_setting_has_a_saturating_window() {
    let config = GlobalConfig {
        auto_collapse: AutoCollapsePolicy::Flat { hours: u64::MAX },
        ..GlobalConfig::default()
    };

    assert_eq!(config.auto_collapse_window_ms(), Some(u64::MAX));
}
