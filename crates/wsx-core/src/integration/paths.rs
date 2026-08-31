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
fn env_or(var: &str, default: &[&str]) -> io::Result<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return expanded(v);
    }
    let mut p = home()?;
    for part in default {
        p.push(part);
    }
    Ok(p)
}

pub(crate) fn root(target: IntegrationTarget) -> io::Result<PathBuf> {
    Ok(match target {
        IntegrationTarget::Pi => env_or("PI_CODING_AGENT_DIR", &[".pi", "agent"])?,
        IntegrationTarget::Omp => {
            if let Some(v) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
                expanded(v)?
            } else {
                home()?
                    .join(
                        std::env::var_os("PI_CONFIG_DIR")
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| ".omp".into()),
                    )
                    .join("agent")
            }
        }
        IntegrationTarget::Claude => env_or("CLAUDE_CONFIG_DIR", &[".claude"])?,
        IntegrationTarget::Codex => env_or("CODEX_HOME", &[".codex"])?,
        IntegrationTarget::Copilot => env_or("COPILOT_HOME", &[".copilot"])?,
        IntegrationTarget::Devin => {
            if let Some(v) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                expanded(v)?.join("devin")
            } else {
                home()?.join(".config/devin")
            }
        }
        IntegrationTarget::Droid => home()?.join(".factory"),
        IntegrationTarget::Kimi => env_or("KIMI_CODE_HOME", &[".kimi-code"])?,
        IntegrationTarget::Opencode => home()?.join(".config/opencode"),
        IntegrationTarget::Kilo => home()?.join(".config/kilo"),
        IntegrationTarget::Hermes => env_or("HERMES_HOME", &[".hermes"])?,
        IntegrationTarget::Qodercli => env_or("QODER_CONFIG_DIR", &[".qoder"])?,
        IntegrationTarget::Qwen => env_or("QWEN_HOME", &[".qwen"])?,
        IntegrationTarget::Cursor => env_or("CURSOR_CONFIG_DIR", &[".cursor"])?,
        IntegrationTarget::Mastracode => home()?.join(".mastracode"),
        IntegrationTarget::AntigravityCli => {
            env_or("ANTIGRAVITY_CLI_CONFIG_DIR", &[".gemini", "config"])?
        }
        IntegrationTarget::Grok => {
            if let Some(v) = std::env::var_os("GROK_CONFIG_DIR").filter(|v| !v.is_empty()) {
                expanded(v)?
            } else {
                env_or("GROK_HOME", &[".grok"])?
            }
        }
    })
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
