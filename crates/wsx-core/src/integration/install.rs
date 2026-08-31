use super::{assets, config_edit, opencode_config, paths, InstallResult, IntegrationTarget};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
fn safe_metadata(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe managed destination: {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn safe_metadata(path: &Path, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe managed destination",
        ));
    }
    Ok(())
}

fn ensure_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => return safe_metadata(path, true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("managed directory has no parent"))?;
    ensure_dir(parent)?;
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    safe_metadata(path, true)
}

fn validate_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => safe_metadata(path, false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn atomic_write(path: &Path, content: &str, executable: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("managed file has no parent"))?;
    ensure_dir(parent)?;
    validate_file(path)?;
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".wsx.{}.{}.tmp", std::process::id(), nonce));
    let result: io::Result<()> = (|| {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(if executable { 0o700 } else { 0o600 });
        let mut file = options.open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn config_path(target: IntegrationTarget, root: &Path) -> Option<PathBuf> {
    match target {
        IntegrationTarget::Claude => Some(root.join("settings.json")),
        IntegrationTarget::Codex => Some(root.join("hooks.json")),
        IntegrationTarget::Copilot => Some(root.join("settings.json")),
        IntegrationTarget::Devin => Some(root.join("config.json")),
        IntegrationTarget::Droid => Some(root.join("settings.json")),
        IntegrationTarget::Kimi => Some(root.join("config.toml")),
        IntegrationTarget::Hermes => Some(root.join("config.yaml")),
        IntegrationTarget::Qodercli | IntegrationTarget::Qwen => Some(root.join("settings.json")),
        IntegrationTarget::Cursor
        | IntegrationTarget::Mastracode
        | IntegrationTarget::AntigravityCli => Some(root.join("hooks.json")),
        _ => None,
    }
}

fn validate_distinct_omp_directory(omp_asset: &Path, pi_asset: &Path) -> io::Result<()> {
    if omp_asset.parent() == pi_asset.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "OMP and Pi resolve to the same extension directory",
        ));
    }
    Ok(())
}

pub fn install(target: IntegrationTarget) -> io::Result<InstallResult> {
    let root = paths::root(target)?;
    if target == IntegrationTarget::Omp {
        validate_distinct_omp_directory(
            &paths::asset_path(target)?,
            &paths::asset_path(IntegrationTarget::Pi)?,
        )?;
    }
    install_in(target, &root)
}

fn install_in(target: IntegrationTarget, root: &Path) -> io::Result<InstallResult> {
    match fs::symlink_metadata(root) {
        Ok(_) => safe_metadata(root, true)?,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                && target == IntegrationTarget::Mastracode =>
        {
            ensure_dir(root)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{} config directory not found at {}",
                    target,
                    root.display()
                ),
            ));
        }
        Err(error) => return Err(error),
    }

    let asset = paths::asset_path_in(root, target);
    let executable = matches!(
        target,
        IntegrationTarget::Claude
            | IntegrationTarget::Codex
            | IntegrationTarget::Copilot
            | IntegrationTarget::Devin
            | IntegrationTarget::Droid
            | IntegrationTarget::Kimi
            | IntegrationTarget::Qodercli
            | IntegrationTarget::Qwen
            | IntegrationTarget::Cursor
            | IntegrationTarget::Mastracode
            | IntegrationTarget::AntigravityCli
            | IntegrationTarget::Grok
    );
    let mut prepared = Vec::<(PathBuf, String, bool)>::new();

    if target == IntegrationTarget::Hermes {
        let directory = asset.parent().expect("Hermes asset has a parent");
        prepared.push((
            directory.join("plugin.yaml"),
            assets::HERMES_MANIFEST.into(),
            false,
        ));
    }
    if target == IntegrationTarget::Opencode {
        prepared.push((
            root.join("wsx-tui-session.js"),
            assets::OPENCODE_TUI.into(),
            false,
        ));
        let tui_config = root.join("tui.jsonc");
        let updated = opencode_config::register_tui(&read_or_empty(&tui_config)?)?;
        prepared.push((tui_config, updated, false));
    }
    if let Some(config) = config_path(target, root) {
        let old = read_or_empty(&config)?;
        let new = match target {
            IntegrationTarget::Kimi => config_edit::kimi_toml(&old, &asset),
            IntegrationTarget::Hermes => config_edit::hermes_yaml(&old),
            _ => config_edit::json_config(target, &old, &config, &asset)?,
        };
        prepared.push((config, new, false));
    }
    if target == IntegrationTarget::Codex {
        let config = root.join("config.toml");
        let new = config_edit::codex_toml(&read_or_empty(&config)?);
        prepared.push((config, new, false));
    }
    if target == IntegrationTarget::Grok {
        let config = root.join("hooks/wsx.json");
        let command = config_edit::command(&asset, "session");
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"SessionStart": [{"hooks": [{
                "type": "command", "command": command, "timeout": 10
            }]}]}
        }))
        .map_err(io::Error::other)?
            + "\n";
        prepared.push((config, body, false));
    }

    // The versioned primary asset is the status marker. Write it last so a
    // failed config edit remains detectable and retryable on the next scan.
    prepared.push((asset, assets::primary(target), executable));
    let mut written = Vec::with_capacity(prepared.len());
    for (path, content, executable) in prepared {
        atomic_write(&path, &content, executable)?;
        written.push(path);
    }
    Ok(InstallResult {
        target,
        paths: written,
    })
}

#[cfg(test)]
pub(crate) fn atomic_write_for_test(path: &Path, content: &str) -> io::Result<()> {
    atomic_write(path, content, false)
}

#[cfg(test)]
pub(crate) fn install_for_test(
    target: IntegrationTarget,
    root: &Path,
) -> io::Result<InstallResult> {
    install_in(target, root)
}

#[cfg(test)]
pub(crate) fn validate_omp_directories_for_test(
    omp_asset: &Path,
    pi_asset: &Path,
) -> io::Result<()> {
    validate_distinct_omp_directory(omp_asset, pi_asset)
}
