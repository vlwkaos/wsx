//! One validated site per process. See docs/web-client.md for the file format.
//! ^ The site file is the only source of panels and capability bits; the browser
//! never selects them.
use crate::theme::Theme;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Panel {
    Chat,
    Files,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ChatSend,
    AgentWrite,
    SessionStart,
    FilesRead,
    FilesWrite,
    TerminalView,
    TerminalInput,
    MediaUpload,
    MediaDownload,
    Push,
}

impl Capability {
    /// The panel that must be enabled for this capability to be reachable.
    fn panel(self) -> Option<Panel> {
        match self {
            Self::ChatSend | Self::AgentWrite | Self::SessionStart => Some(Panel::Chat),
            Self::FilesRead | Self::FilesWrite => Some(Panel::Files),
            Self::TerminalView | Self::TerminalInput => Some(Panel::Terminal),
            Self::MediaUpload | Self::MediaDownload | Self::Push => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteFile {
    id: String,
    name: String,
    #[serde(default)]
    base_path: String,
    port: u16,
    public_origin: String,
    panels: Vec<Panel>,
    /// A built-in theme name, or the absolute path of a theme directory or file.
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default)]
    icon: Option<IconFile>,
    access: AccessFile,
    #[serde(default)]
    media: MediaLimits,
    #[serde(default)]
    push: Option<PushFile>,
    #[serde(default)]
    terminal: TerminalFile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct TerminalFile {
    /// Join character pairs such as `=>` and `!=` in the terminal font.
    ligatures: bool,
}

impl Default for TerminalFile {
    fn default() -> Self {
        Self { ligatures: true }
    }
}

fn default_theme() -> String {
    "default".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IconFile {
    png_192: PathBuf,
    png_512: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessFile {
    projects: ProjectsFile,
    capabilities: Vec<Capability>,
    #[serde(default)]
    agent_command: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProjectsFile {
    All(String),
    Paths(Vec<PathBuf>),
}

/// The projects a site may reach. Listed paths are canonical directories.
#[derive(Debug, Clone)]
pub enum Access {
    /// Every project wsxd manages, now and later.
    All,
    Projects(Vec<PathBuf>),
}

impl Access {
    pub fn grants(&self, project: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Projects(paths) => {
                fs::canonicalize(project).is_ok_and(|path| paths.contains(&path))
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediaLimits {
    pub max_file_bytes: u64,
    pub quota_bytes: u64,
    pub ttl_seconds: u64,
}

impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 25 * 1024 * 1024,
            quota_bytes: 1024 * 1024 * 1024,
            ttl_seconds: 7 * 24 * 3600,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushFile {
    subject: String,
    private_key: String,
    #[serde(default = "default_push_origins")]
    service_origins: Vec<String>,
}

fn default_push_origins() -> Vec<String> {
    [
        "https://fcm.googleapis.com",
        "https://updates.push.services.mozilla.com",
        "https://web.push.apple.com",
    ]
    .map(String::from)
    .to_vec()
}

#[derive(Debug, Clone)]
pub struct PushSettings {
    pub subject: String,
    pub private_key: String,
    pub service_origins: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Origin {
    /// Exact `scheme://host[:port]` the browser sends as `Origin`.
    pub origin: String,
    /// Exact `host[:port]` the browser sends as `Host`.
    pub host: String,
    pub secure: bool,
}

pub struct Site {
    pub id: String,
    pub name: String,
    /// Empty for an alt-port root site, otherwise `/segment[/segment]` without a trailing slash.
    pub base: String,
    pub port: u16,
    pub public: Origin,
    pub panels: Vec<Panel>,
    pub theme: Theme,
    pub access: Access,
    pub capabilities: Vec<Capability>,
    pub agent_command: Vec<String>,
    pub media: MediaLimits,
    pub push: Option<PushSettings>,
    pub terminal_ligatures: bool,
    pub state_dir: PathBuf,
}

impl Site {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot read site file {}", path.display()))?;
        ensure!(metadata.is_file(), "site file must be a regular file");
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "site file must be owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o077 == 0,
            "site file must be owner-only (chmod 600 {})",
            path.display()
        );
        ensure!(metadata.len() <= 64 * 1024, "site file is too large");
        let file: SiteFile =
            toml::from_str(&fs::read_to_string(path)?).context("invalid site file")?;
        Self::validate(file)
    }

    fn validate(file: SiteFile) -> Result<Self> {
        ensure!(
            slug(&file.id, 32),
            "id must be 1-32 characters of a-z, 0-9 or '-'"
        );
        ensure!(
            !file.name.is_empty()
                && file.name.chars().count() <= 40
                && !file.name.chars().any(char::is_control),
            "name must be 1-40 printable characters"
        );
        ensure!(file.port != 0, "port must be 1-65535");
        let base = normalize_base(&file.base_path)?;
        let public = parse_origin(&file.public_origin)?;
        ensure!(!file.panels.is_empty(), "panels must not be empty");
        ensure!(unique(&file.panels), "panels contains a duplicate");
        ensure!(
            unique(&file.access.capabilities),
            "capabilities contains a duplicate"
        );
        for capability in &file.access.capabilities {
            if let Some(panel) = capability.panel() {
                ensure!(
                    file.panels.contains(&panel),
                    "capability {capability:?} needs the {panel:?} panel"
                );
            }
        }
        let has = |capability| file.access.capabilities.contains(&capability);
        ensure!(
            !has(Capability::FilesWrite) || has(Capability::FilesRead),
            "files_write needs files_read"
        );
        ensure!(
            !has(Capability::TerminalInput) || has(Capability::TerminalView),
            "terminal_input needs terminal_view"
        );
        ensure!(
            !has(Capability::AgentWrite) || has(Capability::ChatSend),
            "agent_write needs chat_send"
        );
        ensure!(
            has(Capability::SessionStart) != file.access.agent_command.is_empty(),
            "session_start and agent_command must be set together"
        );
        ensure!(
            file.access
                .agent_command
                .iter()
                .all(|part| !part.is_empty() && !part.contains('\0'))
                && file.access.agent_command.len() <= 16,
            "agent_command is invalid"
        );
        let access = match file.access.projects {
            ProjectsFile::All(word) => {
                ensure!(
                    word == "all",
                    "access.projects must be \"all\" or a list of paths"
                );
                Access::All
            }
            ProjectsFile::Paths(paths) => {
                ensure!(
                    (1..=32).contains(&paths.len()),
                    "access.projects must list 1-32 paths"
                );
                let mut projects = Vec::new();
                for path in paths {
                    ensure!(
                        path.is_absolute(),
                        "project path {} must be absolute",
                        path.display()
                    );
                    let project = fs::canonicalize(&path)
                        .with_context(|| format!("project path {} is missing", path.display()))?;
                    ensure!(
                        project.is_dir(),
                        "project path {} must be a directory",
                        path.display()
                    );
                    projects.push(project);
                }
                ensure!(unique(&projects), "access.projects contains a duplicate");
                Access::Projects(projects)
            }
        };
        let media = file.media;
        ensure!(
            (1..=512 * 1024 * 1024).contains(&media.max_file_bytes)
                && media.quota_bytes >= media.max_file_bytes
                && (1..=366 * 24 * 3600).contains(&media.ttl_seconds),
            "media limits are out of range"
        );
        let icons = file
            .icon
            .map(|icon| -> Result<_> {
                Ok((read_png(&icon.png_192, 192)?, read_png(&icon.png_512, 512)?))
            })
            .transpose()?;
        let theme = Theme::load(&file.theme, icons)?;
        let push = file
            .push
            .map(|push| -> Result<_> {
                ensure!(has(Capability::Push), "[push] needs the push capability");
                ensure!(
                    push.subject.len() <= 256
                        && (push.subject.starts_with("mailto:")
                            || push.subject.starts_with("https://"))
                        && !push.subject.chars().any(char::is_whitespace),
                    "push.subject must be a mailto: or https: contact"
                );
                ensure!(
                    !push.service_origins.is_empty() && push.service_origins.len() <= 16,
                    "push.service_origins must list 1-16 origins"
                );
                let mut origins = Vec::new();
                for origin in &push.service_origins {
                    origins.push(parse_origin(origin)?.origin);
                }
                Ok(PushSettings {
                    subject: push.subject,
                    private_key: push.private_key,
                    service_origins: origins,
                })
            })
            .transpose()?;
        ensure!(
            push.is_some() || !has(Capability::Push),
            "the push capability needs a [push] section"
        );
        let state_dir = wsx_core::runtime::default_socket_path()
            .parent()
            .context("wsx state directory is unavailable")?
            .join("web")
            .join(&file.id);
        Ok(Self {
            id: file.id,
            name: file.name,
            base,
            port: file.port,
            public,
            panels: file.panels,
            theme,
            access,
            capabilities: file.access.capabilities,
            agent_command: file.access.agent_command,
            media,
            push,
            terminal_ligatures: file.terminal.ligatures,
            state_dir,
        })
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn shows(&self, panel: Panel) -> bool {
        self.panels.contains(&panel)
    }

    /// Cookie and service-worker scope: always ends with a slash.
    pub fn scope(&self) -> String {
        format!("{}/", self.base)
    }

    pub fn cookie_name(&self) -> String {
        format!("wsx_web_{}", self.id.replace('-', "_"))
    }
}

fn unique<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .all(|(index, item)| !items[..index].contains(item))
}

fn slug(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn normalize_base(value: &str) -> Result<String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    ensure!(trimmed.starts_with('/'), "base_path must start with '/'");
    ensure!(trimmed.len() <= 128, "base_path is too long");
    for segment in trimmed[1..].split('/') {
        ensure!(
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "base_path segment {segment:?} is invalid"
        );
    }
    Ok(trimmed.to_owned())
}

/// Accept only `https://host[:port]`, or `http://` for a loopback host.
pub fn parse_origin(value: &str) -> Result<Origin> {
    let (scheme, authority) = value
        .split_once("://")
        .with_context(|| format!("origin {value:?} needs a scheme"))?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().context("origin port")?)),
        None => (authority, None),
    };
    ensure!(
        !host.is_empty()
            && host.len() <= 253
            && !host.starts_with(['.', '-'])
            && host.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'-')),
        "origin {value:?} must be scheme://lowercase-host[:port] with no path"
    );
    let loopback = matches!(host, "127.0.0.1" | "localhost");
    let default_port = match scheme {
        "https" => 443,
        "http" if loopback => 80,
        "http" => bail!("origin {value:?} must use https unless it is loopback"),
        _ => bail!("origin {value:?} has an unsupported scheme"),
    };
    ensure!(port != Some(0), "origin port must be 1-65535");
    let host = match port {
        Some(port) if port != default_port => format!("{host}:{port}"),
        _ => host.to_owned(),
    };
    Ok(Origin {
        origin: format!("{scheme}://{host}"),
        host,
        secure: scheme == "https",
    })
}

fn read_png(path: &Path, size: u32) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("cannot read icon {}", path.display()))?;
    crate::theme::check_png(&bytes, size).with_context(|| path.display().to_string())?;
    Ok(bytes)
}
