use super::{availability, paths, InstallStatus, IntegrationMetadata, IntegrationTarget};
use std::fs;
use std::io;

fn version(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        line.trim()
            .trim_start_matches('/')
            .trim_start_matches('#')
            .trim()
            .strip_prefix("WSX_INTEGRATION_VERSION=")?
            .trim()
            .parse()
            .ok()
    })
}

pub fn metadata(target: IntegrationTarget) -> io::Result<IntegrationMetadata> {
    let root = paths::root(target)?;
    metadata_in(target, &root, availability::is_available(target))
}

fn metadata_in(
    target: IntegrationTarget,
    root: &std::path::Path,
    available: bool,
) -> io::Result<IntegrationMetadata> {
    let installed_version = fs::read_to_string(paths::asset_path_in(root, target))
        .ok()
        .and_then(|content| version(&content));
    let mut install_status = match installed_version {
        None => InstallStatus::Missing,
        Some(version) if version >= target.expected_version() => InstallStatus::Current,
        Some(_) => InstallStatus::Outdated,
    };
    if target == IntegrationTarget::Opencode && install_status == InstallStatus::Current {
        let tui_current = fs::read_to_string(root.join("wsx-tui-session.js"))
            .ok()
            .and_then(|content| version(&content))
            .is_some_and(|version| version >= target.expected_version());
        let configured = fs::read_to_string(root.join("tui.jsonc"))
            .is_ok_and(|content| content.contains("./wsx-tui-session.js"));
        if !tui_current || !configured {
            install_status = InstallStatus::Outdated;
        }
    }
    if target == IntegrationTarget::Grok && install_status == InstallStatus::Current {
        let configured = fs::read_to_string(root.join("hooks/wsx.json"))
            .is_ok_and(|content| content.contains("wsx-agent-status.sh"));
        if !configured {
            install_status = InstallStatus::Outdated;
        }
    }
    Ok(IntegrationMetadata {
        target,
        cli_value: target.cli_value(),
        label: target.label(),
        lifecycle: target.lifecycle(),
        available,
        install_status,
        installed_version,
        expected_version: target.expected_version(),
    })
}

pub fn scan() -> io::Result<Vec<IntegrationMetadata>> {
    IntegrationTarget::ALL.into_iter().map(metadata).collect()
}

fn needs_install(metadata: &IntegrationMetadata) -> bool {
    metadata.available && metadata.install_status != InstallStatus::Current
}

pub fn scan_needing_install() -> io::Result<Vec<IntegrationMetadata>> {
    Ok(scan()?.into_iter().filter(needs_install).collect())
}

#[cfg(test)]
pub(crate) fn metadata_for_test(
    target: IntegrationTarget,
    root: &std::path::Path,
) -> io::Result<IntegrationMetadata> {
    metadata_in(target, root, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wsx-core-integration-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
    }

    #[test]
    fn parses_markers() {
        assert_eq!(version("// WSX_INTEGRATION_VERSION=10\n"), Some(10));
        assert_eq!(version("# WSX_INTEGRATION_VERSION=5"), Some(5));
    }

    #[test]
    fn stale_adapter_without_agent_does_not_need_install() {
        let root = test_root("stale-opencode");
        let asset = paths::asset_path_in(&root, IntegrationTarget::Opencode);
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        fs::write(&asset, "// WSX_INTEGRATION_VERSION=1\n").unwrap();

        let metadata = metadata_in(IntegrationTarget::Opencode, &root, false).unwrap();

        assert_eq!(metadata.install_status, InstallStatus::Outdated);
        assert!(!metadata.available);
        assert!(!needs_install(&metadata));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detected_agent_without_adapter_needs_install() {
        let root = test_root("missing-pi");

        let metadata = metadata_in(IntegrationTarget::Pi, &root, true).unwrap();

        assert_eq!(metadata.install_status, InstallStatus::Missing);
        assert!(metadata.available);
        assert!(needs_install(&metadata));
    }
}
