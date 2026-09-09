pub mod review;

use serde::Serialize;
use std::{
    collections::HashSet,
    env, fs, io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wsx_core::runtime::{
    PaneId, PluginManifest, PluginSidecarDescriptor, PluginSidecarView, PluginViewPayload,
    WorktreeId, WSX_PLUGIN_VIEW_ENV,
};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PLUGINS: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_VIEW_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_VIEW_ROWS: usize = 512;
const MAX_VIEW_TEXT_BYTES: usize = 4096;
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
    let mut counts = std::collections::HashMap::new();
    for plugin in &plugins {
        *counts.entry(plugin.id.clone()).or_insert(0usize) += 1;
    }
    plugins.retain(|plugin| counts[&plugin.id] == 1);
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
        || manifest.worktree_review.as_ref().is_some_and(|review| {
            review.api_version != wsx_core::runtime::REVIEW_API_VERSION
                || review.comparisons.as_slice()
                    != [wsx_core::runtime::ReviewComparison::WorkingAgainstHead]
        })
        || manifest.events.len() > 32
        || manifest.events.iter().any(|event| !valid_token(event))
        || manifest.sidecar.as_ref().is_some_and(|sidecar| {
            !(80..=500).contains(&sidecar.minimum_columns)
                || !(20..=120).contains(&sidecar.preferred_width)
                || sidecar.minimum_columns < sidecar.preferred_width.saturating_add(61)
                || !(500..=60_000).contains(&sidecar.refresh_ms)
        })
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

pub fn sidecars(plugins: &[PluginManifest]) -> Vec<PluginSidecarDescriptor> {
    let mut sidecars = plugins
        .iter()
        .filter(|plugin| plugin.enabled)
        .filter_map(|plugin| {
            plugin.sidecar.clone().map(|spec| PluginSidecarDescriptor {
                plugin_id: plugin.id.clone(),
                title: plugin.name.clone(),
                spec,
            })
        })
        .collect::<Vec<_>>();
    sidecars.sort_by(|left, right| {
        right
            .spec
            .priority
            .cmp(&left.spec.priority)
            .then_with(|| left.plugin_id.cmp(&right.plugin_id))
    });
    let mut seen = HashSet::new();
    sidecars.retain(|sidecar| seen.insert(sidecar.plugin_id.clone()));
    sidecars
}

#[derive(Serialize)]
struct PluginViewContext<'a> {
    api_version: u32,
    pane_id: PaneId,
    worktree_id: WorktreeId,
    worktree_path: &'a Path,
    columns: u16,
    rows: u16,
    generation: u64,
}

pub struct RenderContext<'a> {
    pub pane_id: PaneId,
    pub worktree_id: WorktreeId,
    pub worktree_path: &'a Path,
    pub epoch: u64,
    pub columns: u16,
    pub rows: u16,
    pub generation: u64,
}

pub fn render(
    plugin: &PluginManifest,
    request: RenderContext<'_>,
) -> io::Result<PluginSidecarView> {
    let context = serde_json::to_string(&PluginViewContext {
        api_version: 1,
        pane_id: request.pane_id,
        worktree_id: request.worktree_id,
        worktree_path: request.worktree_path,
        columns: request.columns,
        rows: request.rows,
        generation: request.generation,
    })
    .map_err(io::Error::other)?;
    let mut command = Command::new(&plugin.command[0]);
    command
        .args(&plugin.command[1..])
        .env(WSX_PLUGIN_VIEW_ENV, context)
        .stdin(Stdio::null());
    let output =
        wsx_core::git::output_with_timeout_limit(&mut command, TIMEOUT, MAX_VIEW_OUTPUT_BYTES)?;
    if !output.status.success() {
        return Err(io::Error::other("plugin view command failed"));
    }
    let payload: PluginViewPayload = serde_json::from_slice(&output.stdout)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    validate_payload(&payload, request.rows)?;
    Ok(PluginSidecarView {
        plugin_id: plugin.id.clone(),
        title: plugin.name.clone(),
        epoch: request.epoch,
        pane_id: request.pane_id,
        worktree_id: request.worktree_id,
        generation: request.generation,
        payload,
    })
}

fn validate_payload(payload: &PluginViewPayload, rows: u16) -> io::Result<()> {
    let limit = usize::from(rows).min(MAX_VIEW_ROWS);
    if payload.rows.len() > limit
        || payload.remaining > 1000
        || payload
            .empty
            .as_deref()
            .is_some_and(|text| !valid_text(text))
        || payload.rows.iter().any(|row| {
            !valid_text(&row.badge)
                || row.badge.len() > 16
                || !valid_text(&row.primary)
                || row.primary.is_empty()
                || row
                    .secondary
                    .as_deref()
                    .is_some_and(|text| !valid_text(text))
                || !valid_text(&row.value)
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin view output is invalid",
        ));
    }
    Ok(())
}

fn valid_text(value: &str) -> bool {
    value.len() <= MAX_VIEW_TEXT_BYTES && !value.chars().any(char::is_control)
}

pub(crate) fn valid_token(value: &str) -> bool {
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
    use std::os::unix::fs::PermissionsExt;
    use wsx_core::runtime::{PluginSidecarSpec, PluginSurface};

    fn sidecar_manifest(id: &str, priority: i32) -> PluginManifest {
        PluginManifest {
            api_version: 1,
            id: id.into(),
            name: id.into(),
            command: vec!["/unused".into()],
            worktree_review: None,
            events: Vec::new(),
            enabled: true,
            sidecar: Some(PluginSidecarSpec {
                surface: PluginSurface::TerminalRight,
                priority,
                minimum_columns: 120,
                preferred_width: 36,
                refresh_ms: 2_000,
            }),
        }
    }

    #[test]
    fn sidecars_are_priority_ordered_with_stable_ids() {
        let plugins = vec![
            sidecar_manifest("beta", 1),
            sidecar_manifest("alpha", 1),
            sidecar_manifest("lower", 0),
        ];
        assert_eq!(
            sidecars(&plugins)
                .into_iter()
                .map(|sidecar| sidecar.plugin_id)
                .collect::<Vec<_>>(),
            ["alpha", "beta", "lower"]
        );
    }

    #[test]
    fn executable_sidecar_returns_one_bounded_validated_view() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".work")
            .join(format!("plugin-view-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let executable = root.join("view.sh");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' '{\"empty\":\"ready\",\"rows\":[],\"remaining\":0}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();
        let mut plugin = sidecar_manifest("view", 0);
        plugin.command = vec![executable.to_string_lossy().into_owned()];
        let view = render(
            &plugin,
            RenderContext {
                pane_id: PaneId(1),
                worktree_id: WorktreeId(2),
                worktree_path: &root,
                epoch: 7,
                columns: 36,
                rows: 10,
                generation: 3,
            },
        )
        .unwrap();
        assert_eq!(view.epoch, 7);
        assert_eq!(view.payload.empty.as_deref(), Some("ready"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plugin_payload_rejects_controls_and_unbounded_rows() {
        let row = wsx_core::runtime::PluginViewRow {
            badge: " M".into(),
            primary: "file.rs".into(),
            secondary: None,
            value: "+1".into(),
            tone: Default::default(),
        };
        assert!(validate_payload(
            &PluginViewPayload {
                empty: None,
                rows: vec![row.clone()],
                remaining: 0,
            },
            1,
        )
        .is_ok());
        assert!(validate_payload(
            &PluginViewPayload {
                empty: None,
                rows: vec![wsx_core::runtime::PluginViewRow {
                    primary: "bad\nrow".into(),
                    ..row
                }],
                remaining: 0,
            },
            1,
        )
        .is_err());
    }

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
