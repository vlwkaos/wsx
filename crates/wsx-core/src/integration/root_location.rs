//! One declaration for machine-local installation and portable hook references.
use super::IntegrationTarget;
use std::path::PathBuf;
use std::{ffi::OsString, io};

pub(super) struct RootLocation {
    variables: &'static [&'static str],
    home: &'static [&'static str],
    suffix: &'static [&'static str],
}

impl RootLocation {
    pub(super) fn resolve(
        &self,
        home: impl Fn() -> io::Result<PathBuf>,
        expand: impl Fn(OsString) -> io::Result<PathBuf>,
    ) -> io::Result<PathBuf> {
        let value = self
            .variables
            .iter()
            .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()));
        let mut path = if let Some(value) = value {
            expand(value)?
        } else {
            let mut path = home()?;
            for part in self.home {
                path.push(part);
            }
            path
        };
        for part in self.suffix {
            path.push(part);
        }
        Ok(path)
    }

    pub(super) fn shell_assignment(&self) -> String {
        let mut value = "${HOME:?HOME is unset}".to_string();
        for part in self.home {
            value.push('/');
            value.push_str(part);
        }
        for variable in self.variables.iter().rev() {
            value = format!("${{{variable}:-{value}}}");
        }
        let mut command = format!("wsx_hook_root=\"{value}\"; ");
        // ^ Parameter expansion is data, never eval. Match installer tilde expansion.
        command.push_str("case \"$wsx_hook_root\" in '~') wsx_hook_root=\"$HOME\";; '~/'*) wsx_hook_root=\"$HOME/${wsx_hook_root#\\~/}\";; esac; ");
        for part in self.suffix {
            command.push_str(&format!("wsx_hook_root=\"$wsx_hook_root/{part}\"; "));
        }
        command
    }
}

pub(super) fn location(target: IntegrationTarget) -> Option<RootLocation> {
    use IntegrationTarget::*;
    let (variables, home, suffix): (&[&str], &[&str], &[&str]) = match target {
        Pi => (&["PI_CODING_AGENT_DIR"], &[".pi", "agent"], &[]),
        Claude => (&["CLAUDE_CONFIG_DIR"], &[".claude"], &[]),
        Codex => (&["CODEX_HOME"], &[".codex"], &[]),
        Copilot => (&["COPILOT_HOME"], &[".copilot"], &[]),
        Devin => (&["XDG_CONFIG_HOME"], &[".config"], &["devin"]),
        Droid => (&[], &[".factory"], &[]),
        Kimi => (&["KIMI_CODE_HOME"], &[".kimi-code"], &[]),
        Opencode => (&[], &[".config", "opencode"], &[]),
        Kilo => (&[], &[".config", "kilo"], &[]),
        Hermes => (&["HERMES_HOME"], &[".hermes"], &[]),
        Qodercli => (&["QODER_CONFIG_DIR"], &[".qoder"], &[]),
        Qwen => (&["QWEN_HOME"], &[".qwen"], &[]),
        Cursor => (&["CURSOR_CONFIG_DIR"], &[".cursor"], &[]),
        Mastracode => (&[], &[".mastracode"], &[]),
        AntigravityCli => (&["ANTIGRAVITY_CLI_CONFIG_DIR"], &[".gemini", "config"], &[]),
        Grok => (&["GROK_CONFIG_DIR", "GROK_HOME"], &[".grok"], &[]),
        // OMP's relative PI_CONFIG_DIR behavior is intentionally unchanged; it has no shell hook binding.
        Omp => return None,
    };
    Some(RootLocation {
        variables,
        home,
        suffix,
    })
}
