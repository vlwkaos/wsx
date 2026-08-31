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
        IntegrationTarget::Qodercli => &["qodercli"],
        IntegrationTarget::Qwen => &["qwen"],
        IntegrationTarget::Cursor => &["cursor-agent"],
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

fn available_on_path(target: IntegrationTarget, path: Option<std::ffi::OsString>) -> bool {
    path.as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .any(|dir| {
            commands(target)
                .iter()
                .any(|cmd| executable(&dir.join(cmd)))
        })
}

pub fn is_available(target: IntegrationTarget) -> bool {
    if available_on_path(target, std::env::var_os("PATH")) {
        return true;
    }
    match target {
        IntegrationTarget::Codex => paths::root(target).is_ok_and(|root| {
            glob::glob(&format!(
                "{}/packages/standalone/releases/*/bin/codex",
                root.display()
            ))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|path| executable(&path))
        }),
        IntegrationTarget::Hermes if cfg!(windows) => paths::root(target).is_ok_and(|root| {
            [
                root.join("hermes.exe"),
                root.join("bin/hermes.exe"),
                root.join("Scripts/hermes.exe"),
            ]
            .iter()
            .any(|path| executable(path))
        }),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) fn available_on_path_for_test(target: IntegrationTarget, path: &Path) -> bool {
    available_on_path(target, Some(path.as_os_str().to_owned()))
}
