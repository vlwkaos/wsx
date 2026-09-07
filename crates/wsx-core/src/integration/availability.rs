use super::{paths, IntegrationTarget};
use std::path::Path;

fn commands(target: IntegrationTarget) -> &'static [&'static str] {
    match target {
        IntegrationTarget::Pi => &["pi"],
        IntegrationTarget::Omp => &["omp"],
        IntegrationTarget::Claude => &["claude"],
        IntegrationTarget::Codex => &["codex"],
        IntegrationTarget::Copilot => &["copilot"],
        IntegrationTarget::Devin => &["devin"],
        IntegrationTarget::Droid => &["droid"],
        IntegrationTarget::Kimi => &["kimi"],
        IntegrationTarget::Opencode => &["opencode"],
        IntegrationTarget::Kilo => &["kilo", "kilo-code"],
        IntegrationTarget::Hermes => &["hermes"],
        IntegrationTarget::Qodercli => &["qoder", "qodercli"],
        IntegrationTarget::Qwen => &["qwen"],
        IntegrationTarget::Cursor => &["agent", "cursor-agent"],
        IntegrationTarget::Mastracode => &["mastracode"],
        IntegrationTarget::AntigravityCli => &["agy"],
        IntegrationTarget::Grok => &["grok"],
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

fn command_on_path(
    target: IntegrationTarget,
    path: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    path.as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .find_map(|dir| {
            commands(target)
                .iter()
                .map(|command| dir.join(command))
                .find(|path| executable(path))
        })
}

pub(crate) fn command_path(target: IntegrationTarget) -> Option<std::path::PathBuf> {
    if let Some(path) = command_on_path(target, std::env::var_os("PATH")) {
        return Some(path);
    }
    match target {
        IntegrationTarget::Codex => paths::root(target).ok().and_then(|root| {
            glob::glob(&format!(
                "{}/packages/standalone/releases/*/bin/codex",
                root.display()
            ))
            .ok()?
            .filter_map(Result::ok)
            .find(|path| executable(path))
        }),
        IntegrationTarget::Hermes if cfg!(windows) => paths::root(target).ok().and_then(|root| {
            [
                root.join("hermes.exe"),
                root.join("bin/hermes.exe"),
                root.join("Scripts/hermes.exe"),
            ]
            .into_iter()
            .find(|path| executable(path))
        }),
        _ => None,
    }
}

pub fn is_available(target: IntegrationTarget) -> bool {
    command_path(target).is_some()
}

#[cfg(test)]
pub(crate) fn available_on_path_for_test(target: IntegrationTarget, path: &Path) -> bool {
    command_on_path(target, Some(path.as_os_str().to_owned())).is_some()
}
