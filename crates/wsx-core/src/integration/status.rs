use super::{availability, paths, InstallStatus, IntegrationMetadata, IntegrationTarget};
use std::fs;
use std::io;
use std::process::Command;

const CODEX_COMPATIBILITY_NOTE: &str = "Requires Codex 0.150 or newer";

fn codex_version(output: &str) -> Option<(u32, u32, u32)> {
    let value = output.split_whitespace().find(|part| {
        part.chars().next().is_some_and(|ch| ch.is_ascii_digit()) && part.contains('.')
    })?;
    let mut parts = value.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next().unwrap_or("0").parse().ok()?,
    ))
}

fn codex_version_supported(output: &str) -> bool {
    codex_version(output).is_some_and(|version| version >= (0, 150, 0))
}

pub(crate) fn compatibility(
    target: IntegrationTarget,
    available: bool,
) -> (bool, Option<&'static str>) {
    if target != IntegrationTarget::Codex || !available {
        return (true, None);
    }
    let compatible = availability::command_path(target)
        .and_then(|path| Command::new(path).arg("--version").output().ok())
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|output| codex_version_supported(&output));
    (
        compatible,
        (!compatible).then_some(CODEX_COMPATIBILITY_NOTE),
    )
}

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
    let available = availability::is_available(target);
    let (compatible, compatibility_note) = compatibility(target, available);
    metadata_in(target, &root, available, compatible, compatibility_note)
}

fn metadata_in(
    target: IntegrationTarget,
    root: &std::path::Path,
    available: bool,
    compatible: bool,
    compatibility_note: Option<&'static str>,
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
        compatible,
        compatibility_note,
        install_status,
        installed_version,
        expected_version: target.expected_version(),
    })
}

pub fn scan() -> io::Result<Vec<IntegrationMetadata>> {
    IntegrationTarget::ALL.into_iter().map(metadata).collect()
}

#[cfg(test)]
pub(crate) fn metadata_for_test(
    target: IntegrationTarget,
    root: &std::path::Path,
) -> io::Result<IntegrationMetadata> {
    metadata_in(target, root, false, true, None)
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
    fn parses_markers_and_codex_versions() {
        assert_eq!(version("// WSX_INTEGRATION_VERSION=10\n"), Some(10));
        assert_eq!(version("# WSX_INTEGRATION_VERSION=5"), Some(5));
        assert_eq!(codex_version("codex-cli 0.150.0\n"), Some((0, 150, 0)));
        assert_eq!(codex_version("codex 1.2\n"), Some((1, 2, 0)));
        assert_eq!(codex_version("unknown\n"), None);
        assert!(!codex_version_supported("codex-cli 0.149.9\n"));
        assert!(codex_version_supported("codex-cli 0.150.0\n"));
    }

    #[test]
    fn stale_adapter_without_agent_does_not_need_install() {
        let root = test_root("stale-opencode");
        let asset = paths::asset_path_in(&root, IntegrationTarget::Opencode);
        fs::create_dir_all(asset.parent().unwrap()).unwrap();
        fs::write(&asset, "// WSX_INTEGRATION_VERSION=1\n").unwrap();

        let metadata = metadata_in(IntegrationTarget::Opencode, &root, false, true, None).unwrap();

        assert_eq!(metadata.install_status, InstallStatus::Outdated);
        assert!(!metadata.available);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detected_agent_without_adapter_needs_install() {
        let root = test_root("missing-pi");

        let metadata = metadata_in(IntegrationTarget::Pi, &root, true, true, None).unwrap();

        assert_eq!(metadata.install_status, InstallStatus::Missing);
        assert!(metadata.available);
        assert!(metadata.compatible);
    }
}
