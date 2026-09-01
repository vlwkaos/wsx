//! Installation and discovery for wsx-owned coding-agent adapters.
//!
//! Vendor hook schemas are mirrored here, but assets only invoke `wsx agent
//! report` and trust lifecycle events emitted by each provider.

// ^ [[Session Model]] crates/wsx-core/src/integration/mod.rs -> crates/wsx-core/src/runtime/domain.rs, integrations/pi/wsx-agent-status.ts
mod assets;
mod availability;
mod config_edit;
mod install;
mod model;
mod opencode_config;
mod paths;
pub mod resume;
mod status;

pub use availability::is_available;
pub use install::install;
pub use model::{
    InstallResult, InstallStatus, IntegrationMetadata, IntegrationTarget, LifecycleCapability,
};
pub use status::{metadata, scan, scan_needing_install};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wsx-core-integration-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn enum_is_complete() {
        assert_eq!(IntegrationTarget::ALL.len(), 17);
        assert_eq!(
            IntegrationTarget::ALL.map(IntegrationTarget::cli_value),
            [
                "pi",
                "omp",
                "claude",
                "codex",
                "copilot",
                "devin",
                "droid",
                "kimi",
                "opencode",
                "kilo",
                "hermes",
                "qodercli",
                "qwen",
                "cursor",
                "mastracode",
                "antigravity-cli",
                "grok"
            ]
        );
    }
    #[test]
    fn availability_and_status_are_path_injected() {
        let root = test_root("availability");
        let executable = root.join("claude");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(availability::available_on_path_for_test(
            IntegrationTarget::Claude,
            &root
        ));
        assert_eq!(
            status::metadata_for_test(IntegrationTarget::Claude, &root)
                .unwrap()
                .install_status,
            InstallStatus::Missing
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn safe_idempotent_install() {
        let root = test_root("pi");
        let first = install::install_for_test(IntegrationTarget::Pi, &root).unwrap();
        let before = fs::read(&first.paths[0]).unwrap();
        let second = install::install_for_test(IntegrationTarget::Pi, &root).unwrap();
        assert_eq!(first.paths, second.paths);
        assert_eq!(before, fs::read(&second.paths[0]).unwrap());
        assert_eq!(
            status::metadata_for_test(IntegrationTarget::Pi, &root)
                .unwrap()
                .install_status,
            InstallStatus::Current
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_target_installs_idempotently_in_isolation() {
        let parent = test_root("all-targets");
        for target in IntegrationTarget::ALL {
            let root = parent.join(target.cli_value());
            fs::create_dir_all(&root).unwrap();
            let first = install::install_for_test(target, &root).unwrap();
            let second = install::install_for_test(target, &root).unwrap();
            assert_eq!(first.paths, second.paths, "{target}");
            assert_eq!(
                status::metadata_for_test(target, &root)
                    .unwrap()
                    .install_status,
                InstallStatus::Current,
                "{target}"
            );
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn omp_rejects_the_pi_extension_directory() {
        let shared = test_root("shared-agent");
        let omp = paths::asset_path_in(&shared, IntegrationTarget::Omp);
        let pi = paths::asset_path_in(&shared, IntegrationTarget::Pi);

        let error = install::validate_omp_directories_for_test(&omp, &pi).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        fs::remove_dir_all(shared).unwrap();
    }

    #[test]
    fn invalid_config_does_not_publish_a_current_version_marker() {
        let root = test_root("invalid-config");
        fs::write(root.join("settings.json"), "[]\n").unwrap();

        assert!(install::install_for_test(IntegrationTarget::Claude, &root).is_err());
        assert!(!paths::asset_path_in(&root, IntegrationTarget::Claude).exists());
        assert_eq!(
            status::metadata_for_test(IntegrationTarget::Claude, &root)
                .unwrap()
                .install_status,
            InstallStatus::Missing
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unsafe_destinations() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = test_root("unsafe");
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        let link = dir.join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            install::atomic_write_for_test(&link, "x")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let bad = dir.join("bad");
        fs::write(&bad, "x").unwrap();
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            install::atomic_write_for_test(&bad, "x")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn representative_config_shapes() {
        let p = PathBuf::from("target/wsx hook.sh");
        let cfg = PathBuf::from("x.json");
        let nested =
            config_edit::json_config(IntegrationTarget::Claude, r#"{"keep":1}"#, &cfg, &p).unwrap();
        assert!(nested.contains("SessionStart") && nested.contains("\"keep\": 1"));
        assert_eq!(
            config_edit::json_config(IntegrationTarget::Claude, &nested, &cfg, &p).unwrap(),
            nested
        );
        assert!(config_edit::json_config(IntegrationTarget::Claude, "[]", &cfg, &p).is_err());
        let direct = config_edit::json_config(IntegrationTarget::Copilot, "{}", &cfg, &p).unwrap();
        assert!(direct.contains("\"bash\""));
        let simple = config_edit::json_config(IntegrationTarget::Cursor, "{}", &cfg, &p).unwrap();
        assert!(simple.contains("sessionStart"));
        let flat = config_edit::json_config(IntegrationTarget::Mastracode, "{}", &cfg, &p).unwrap();
        assert!(flat.contains("PermissionRequest"));
        let named = config_edit::json_config(
            IntegrationTarget::AntigravityCli,
            r#"{"other":{}}"#,
            &cfg,
            &p,
        )
        .unwrap();
        assert!(named.contains("PreInvocation") && named.contains("other"));
        let codex = config_edit::codex_toml("model = \"x\"\n");
        assert!(codex.contains("[features]\nhooks = true"));
        let kimi = config_edit::kimi_toml("model = \"x\"\n", &p);
        assert!(kimi.contains("PermissionRequest") && kimi.contains("model"));
        let yaml = config_edit::hermes_yaml("theme: dark\n");
        assert!(yaml.contains("wsx-agent-status") && yaml.contains("theme"));
    }

    #[test]
    fn primary_assets_report_version_and_native_session_ids() {
        for target in IntegrationTarget::ALL {
            let asset = assets::primary(target);
            let marker = format!("WSX_INTEGRATION_VERSION={}", target.expected_version());
            assert!(asset.contains(&marker), "{target}: missing {marker}");
            assert!(
                asset.contains("--session-id"),
                "{target}: missing --session-id"
            );
            assert!(
                !asset.contains("--conversation-id"),
                "{target}: unexpected --conversation-id"
            );
        }
    }

    #[test]
    fn pi_and_omp_primary_assets_prefer_session_paths_and_report_lifecycle() {
        for target in [IntegrationTarget::Pi, IntegrationTarget::Omp] {
            let asset = assets::primary(target);
            let path = asset
                .find("--session-path")
                .expect("session path assertion requires a path branch");
            let id = asset
                .find("--session-id")
                .expect("session path assertion requires an ID branch");
            assert!(path < id, "{target}: path branch must precede ID branch");
            assert!(
                asset.contains(&format!("\"--provider\", \"{}\"", target.cli_value())),
                "{target}: missing exact provider reporting"
            );
            assert!(
                asset.contains("--lifecycle"),
                "{target}: missing lifecycle reporting"
            );
        }
    }

    #[test]
    fn opencode_assets_use_tui_routing_and_argv_reporting() {
        assert!(assets::OPENCODE_TUI.contains("api.route.current"));
        assert!(assets::OPENCODE_TUI.contains("execFile"));
        assert!(!assets::OPENCODE_TUI.contains("createConnection"));
        let updated = opencode_config::register_tui("{\n  // keep\n  \"plugin\": []\n}\n").unwrap();
        assert!(updated.contains("// keep"));
        assert!(updated.contains("./wsx-tui-session.js"));
    }
}
