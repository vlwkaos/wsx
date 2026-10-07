use super::IntegrationTarget;
use std::io;
use std::path::{Path, PathBuf};

fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or_else(|| io::Error::other("cannot determine home directory"))
}
fn expanded(value: std::ffi::OsString) -> io::Result<PathBuf> {
    let p = PathBuf::from(value);
    let Some(s) = p.to_str() else { return Ok(p) };
    if s == "~" {
        return home();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    Ok(p)
}
pub(crate) fn root(target: IntegrationTarget) -> io::Result<PathBuf> {
    if let Some(location) = super::root_location::location(target) {
        return location.resolve(home, expanded);
    }
    // OMP keeps its distinct relative PI_CONFIG_DIR rules.
    if let Some(v) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
        expanded(v)
    } else {
        Ok(home()?
            .join(
                std::env::var_os("PI_CONFIG_DIR")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| ".omp".into()),
            )
            .join("agent"))
    }
}

pub(crate) fn shell_root_assignment(target: IntegrationTarget) -> String {
    super::root_location::location(target)
        .expect("shell hook targets have a declared root")
        .shell_assignment()
}

pub(crate) fn asset_path(target: IntegrationTarget) -> io::Result<PathBuf> {
    Ok(asset_path_in(&root(target)?, target))
}

pub(crate) fn asset_path_in(root: &Path, target: IntegrationTarget) -> PathBuf {
    match target {
        IntegrationTarget::Pi => root.join("extensions/wsx-agent-status.ts"),
        IntegrationTarget::Omp => root.join("extensions/wsx-omp-agent-status.ts"),
        IntegrationTarget::Claude => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Codex => root.join("wsx-agent-status.sh"),
        IntegrationTarget::Copilot => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Devin => root.join("wsx-agent-status.sh"),
        IntegrationTarget::Droid => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Kimi => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Opencode => root.join("plugins/wsx-agent-status.js"),
        IntegrationTarget::Kilo => root.join("plugin/wsx-agent-status.js"),
        IntegrationTarget::Hermes => root.join("plugins/wsx-agent-status/__init__.py"),
        IntegrationTarget::Qodercli => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Qwen => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Cursor => root.join("wsx-agent-status.sh"),
        IntegrationTarget::Mastracode => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::AntigravityCli => root.join("hooks/wsx-agent-status.sh"),
        IntegrationTarget::Grok => root.join("hooks/wsx-agent-status.sh"),
    }
}
