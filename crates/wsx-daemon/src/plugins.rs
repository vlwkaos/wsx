use std::{
    env, fs, io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wsx_core::runtime::PluginManifest;

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PLUGINS: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(3);

pub fn discover() -> Vec<PluginManifest> {
    plugin_dir()
        .and_then(|dir| discover_in(&dir).ok())
        .unwrap_or_default()
}

fn plugin_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("wsx/plugins"))
}

fn discover_in(dir: &Path) -> io::Result<Vec<PluginManifest>> {
    let directory = fs::symlink_metadata(dir)?;
    if directory.file_type().is_symlink()
        || !directory.is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe plugin directory",
        ));
    }
    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    paths.truncate(MAX_PLUGINS);
    let mut plugins = Vec::new();
    for path in paths {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_MANIFEST_BYTES
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(mut manifest) = serde_json::from_slice::<PluginManifest>(&bytes) else {
            continue;
        };
        if let Some(executable) = manifest.command.first_mut() {
            let candidate = Path::new(executable);
            if !candidate.is_absolute() {
                let Some(parent) = path.parent() else {
                    continue;
                };
                let Ok(resolved) = resolve_relative_executable(parent, candidate) else {
                    continue;
                };
                *executable = resolved.to_string_lossy().into_owned();
            }
        }
        if validate(&manifest).is_ok() {
            plugins.push(manifest);
        }
    }
    Ok(plugins)
}

fn resolve_relative_executable(base: &Path, candidate: &Path) -> io::Result<PathBuf> {
    let base = fs::canonicalize(base)?;
    let mut current = base.clone();
    for component in candidate.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => current.push(part),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "plugin executable escapes its manifest directory",
                ));
            }
        }
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "plugin executable path contains a symlink",
            ));
        }
    }
    let resolved = fs::canonicalize(current)?;
    if !resolved.starts_with(&base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "plugin executable escapes its manifest directory",
        ));
    }
    Ok(resolved)
}

pub fn validate(manifest: &PluginManifest) -> Result<(), &'static str> {
    if manifest.api_version != 1
        || !valid_token(&manifest.id)
        || manifest.name.trim().is_empty()
        || manifest.name.len() > 128
        || manifest.command.is_empty()
        || manifest.command.len() > 32
        || manifest
            .command
            .iter()
            .any(|part| part.is_empty() || part.len() > 4096 || part.as_bytes().contains(&0))
        || manifest.events.len() > 32
        || manifest.events.iter().any(|event| !valid_token(event))
    {
        return Err("invalid plugin manifest");
    }
    let executable = Path::new(&manifest.command[0]);
    let metadata = fs::symlink_metadata(executable).map_err(|_| "plugin executable unavailable")?;
    if !executable.is_absolute()
        || metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        return Err("untrusted plugin executable");
    }
    Ok(())
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn emit(plugins: &[PluginManifest], name: &str, payload: &str) {
    for plugin in plugins.iter().filter(|plugin| {
        plugin.enabled
            && plugin
                .events
                .iter()
                .any(|event| event == name || event == "*")
    }) {
        let Ok(mut child) = Command::new(&plugin.command[0])
            .args(&plugin.command[1..])
            .env("WSX_EVENT_JSON", payload)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_are_bounded() {
        assert!(valid_token("session.created"));
        assert!(!valid_token("bad/event"));
        assert!(!valid_token(""));
    }

    #[test]
    fn relative_executables_cannot_escape_or_follow_symlinks() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work")
            .join(format!("plugin-path-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        assert!(resolve_relative_executable(&root, Path::new("../outside")).is_err());

        let executable = root.join("tool");
        fs::write(&executable, "tool").unwrap();
        std::os::unix::fs::symlink(&executable, root.join("link")).unwrap();
        assert!(resolve_relative_executable(&root, Path::new("link")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
