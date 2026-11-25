//! wsx's own folders (docs/daemon-discovery.md "Folders"). `WSX_HOME`, an absolute path, holds
//! them all: `config/` (the config, the web sites, the plugins, the owner's `wsx.config.yml`),
//! `state/` (the task store and the daemon's socket) and `cache/`. Without it they are the
//! platform's, as before. A second wsx on one machine (`scripts/dev.sh`) sets only `WSX_HOME`:
//! `HOME` stays the owner's, so every agent it runs keeps its own settings and sign-in.
use std::path::PathBuf;

pub const WSX_HOME_ENV: &str = "WSX_HOME";

/// `WSX_HOME` when it is an absolute path; an empty or relative value is ignored.
pub fn root() -> Option<PathBuf> {
    std::env::var_os(WSX_HOME_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The env value of a folder variable when it is an absolute path.
fn absolute(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The folder of `config-v2.toml`: `$WSX_HOME/config`, else the platform's (`~/Library/Application
/// Support/wsx` on macOS).
pub fn config_dir() -> Option<PathBuf> {
    root()
        .map(|root| root.join("config"))
        .or_else(|| dirs::config_dir().map(|dir| dir.join("wsx")))
}

/// The folder of the web sites (`web/`), the plugins (`plugins/`) and the owner's
/// `wsx.config.yml`: `$WSX_HOME/config`, else `${XDG_CONFIG_HOME:-$HOME/.config}/wsx`.
pub fn user_config_dir() -> Option<PathBuf> {
    root().map(|root| root.join("config")).or_else(|| {
        absolute("XDG_CONFIG_HOME")
            .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
            .map(|dir| dir.join("wsx"))
    })
}

/// Every folder that holds a wsx config on this machine: this wsx's, and the owner's platform
/// ones when `WSX_HOME` moves this wsx's away. An agent's boundary denies them all.
pub fn all_config_dirs() -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = config_dir().into_iter().chain(user_config_dir()).collect();
    if root().is_some() {
        all.extend(dirs::config_dir().map(|dir| dir.join("wsx")));
        all.extend(
            absolute("XDG_CONFIG_HOME")
                .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
                .map(|dir| dir.join("wsx")),
        );
    }
    all.dedup();
    all
}

/// The state folder: the task store and, unless `WSX_SOCKET` moves it, the daemon's socket.
/// `$WSX_HOME/state`, else `$XDG_STATE_HOME/wsx`, else `~/.local/state/wsx`. An empty or relative
/// value is ignored, as the XDG base directory spec says, so it never depends on the current
/// directory.
pub fn state_dir() -> PathBuf {
    root().map(|root| root.join("state")).unwrap_or_else(|| {
        absolute("XDG_STATE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local/state")))
            .unwrap_or_else(std::env::temp_dir)
            .join("wsx")
    })
}

/// The cache folder: `$WSX_HOME/cache`, else the platform's (`~/Library/Caches/wsx` on macOS).
pub fn cache_dir() -> Option<PathBuf> {
    root()
        .map(|root| root.join("cache"))
        .or_else(|| dirs::cache_dir().map(|dir| dir.join("wsx")))
}
