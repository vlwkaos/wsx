use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntegrationTarget {
    Pi,
    Omp,
    Claude,
    Codex,
    Copilot,
    Devin,
    Droid,
    Kimi,
    Opencode,
    Kilo,
    Hermes,
    Qodercli,
    Qwen,
    Cursor,
    Mastracode,
    AntigravityCli,
    Grok,
}

impl IntegrationTarget {
    pub const ALL: [Self; 17] = [
        Self::Pi,
        Self::Omp,
        Self::Claude,
        Self::Codex,
        Self::Copilot,
        Self::Devin,
        Self::Droid,
        Self::Kimi,
        Self::Opencode,
        Self::Kilo,
        Self::Hermes,
        Self::Qodercli,
        Self::Qwen,
        Self::Cursor,
        Self::Mastracode,
        Self::AntigravityCli,
        Self::Grok,
    ];

    pub const fn cli_value(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Devin => "devin",
            Self::Droid => "droid",
            Self::Kimi => "kimi",
            Self::Opencode => "opencode",
            Self::Kilo => "kilo",
            Self::Hermes => "hermes",
            Self::Qodercli => "qodercli",
            Self::Qwen => "qwen",
            Self::Cursor => "cursor",
            Self::Mastracode => "mastracode",
            Self::AntigravityCli => "antigravity-cli",
            Self::Grok => "grok",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::Omp => "OMP",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Copilot => "GitHub Copilot CLI",
            Self::Devin => "Devin CLI",
            Self::Droid => "Factory Droid",
            Self::Kimi => "Kimi Code",
            Self::Opencode => "OpenCode",
            Self::Kilo => "Kilo Code",
            Self::Hermes => "Hermes Agent",
            Self::Qodercli => "Qoder CLI",
            Self::Qwen => "Qwen Code",
            Self::Cursor => "Cursor Agent",
            Self::Mastracode => "MastraCode",
            Self::AntigravityCli => "Antigravity CLI",
            Self::Grok => "Grok CLI",
        }
    }

    pub const fn lifecycle(self) -> LifecycleCapability {
        match self {
            Self::Pi
            | Self::Omp
            | Self::Claude
            | Self::Kimi
            | Self::Opencode
            | Self::Kilo
            | Self::Mastracode => LifecycleCapability::Authoritative,
            _ => LifecycleCapability::IdentityOnly,
        }
    }

    pub const fn expected_version(self) -> u32 {
        match self {
            Self::Pi | Self::Omp => 11,
            Self::Claude => 10,
            Self::Codex => 9,
            Self::Copilot | Self::Droid | Self::Qodercli => 4,
            Self::Devin | Self::AntigravityCli => 3,
            Self::Mastracode => 4,
            Self::Kimi => 9,
            Self::Opencode => 13,
            Self::Kilo => 7,
            Self::Hermes => 6,
            Self::Qwen | Self::Cursor | Self::Grok => 2,
        }
    }
}

impl fmt::Display for IntegrationTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.cli_value())
    }
}

impl FromStr for IntegrationTarget {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|target| target.cli_value() == value)
            .ok_or_else(|| format!("unknown integration target: {value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCapability {
    Authoritative,
    IdentityOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallStatus {
    Missing,
    Current,
    Outdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationMetadata {
    pub target: IntegrationTarget,
    pub cli_value: &'static str,
    pub label: &'static str,
    pub lifecycle: LifecycleCapability,
    pub available: bool,
    pub install_status: InstallStatus,
    pub installed_version: Option<u32>,
    pub expected_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub target: IntegrationTarget,
    pub paths: Vec<std::path::PathBuf>,
}
