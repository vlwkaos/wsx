use wsx_core::config::global::GlobalConfig;
use wsx_core::runtime::{AgentInfo, Capabilities};

#[test]
fn global_config_default_enables_agent_restore() {
    assert!(GlobalConfig::default().resume_agents_on_restore);
}

#[test]
fn omitted_agent_restore_setting_defaults_to_enabled() {
    let config: GlobalConfig = toml::from_str("").expect("empty TOML must decode");

    assert!(config.resume_agents_on_restore);
}

#[test]
fn disabled_agent_restore_setting_round_trips_explicitly() {
    let config: GlobalConfig =
        toml::from_str("resume_agents_on_restore = false\n").expect("explicit setting must decode");

    assert!(!config.resume_agents_on_restore);
    assert!(toml::to_string(&config)
        .expect("config must serialize")
        .contains("resume_agents_on_restore = false"),);
}

#[test]
fn legacy_agent_info_defaults_missing_session_reference() {
    let agent: AgentInfo = serde_json::from_str(
        r#"{
            "id": 17,
            "provider": "pi",
            "state": "idle",
            "conversation_id": "legacy-conversation",
            "capabilities": {},
            "source": "legacy"
        }"#,
    )
    .expect("legacy agent JSON must decode");

    assert_eq!(agent.session_ref, None);
    assert_eq!(
        agent.conversation_id.as_deref(),
        Some("legacy-conversation")
    );
}

#[test]
fn legacy_capabilities_default_missing_agent_session_restore_to_disabled() {
    let capabilities: Capabilities = serde_json::from_str(
        r#"{
            "pane_splits": true,
            "plugins": true,
            "agent_reports": true,
            "listening_ports": false,
            "process_restore": false
        }"#,
    )
    .expect("legacy capabilities JSON must decode");

    assert!(!capabilities.agent_session_restore);
}
