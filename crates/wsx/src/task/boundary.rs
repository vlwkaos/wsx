//! The agent boundary: one macOS Seatbelt profile per role, applied by the task host and the
//! manager host to the whole agent process tree (Bash, file tools, MCP servers, hooks), so any
//! adapter runs in its own no-prompt mode inside it. A deny-list: reads and writes are open
//! except what other tasks, wsx and the owner's secrets own; network, loopback and PTYs stay
//! open. ref: docs/tasks.md "Permissions"
// ^ Claude Code (https://code.claude.com/docs/en/sandboxing) and Codex
// (https://developers.openai.com/codex/security) confine commands the same way on macOS:
// `sandbox-exec -p <profile>` with a generated Seatbelt profile.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use wsx_core::task::{copy, store, TaskRecord};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Home folders that hold the owner's secrets: no agent reads or writes them.
const SECRETS: [&str; 3] = [".ssh", ".aws", ".gnupg"];

/// What one role may not touch. The last matching Seatbelt rule wins, so `own` (allowed)
/// comes after the state folder (denied).
pub struct Boundary {
    deny_read: Vec<PathBuf>,
    deny_write: Vec<PathBuf>,
    /// A file and its `.lock`, as git writes a ref.
    deny_write_prefix: Vec<PathBuf>,
    /// Other tasks' copies `<parent>/<repo>-t<n>`, also those created after the launch.
    deny_copies: Option<(PathBuf, String)>,
    own: Vec<PathBuf>,
    /// The project page and the temporary files an editor writes beside it.
    own_prefix: Vec<PathBuf>,
}

impl Boundary {
    /// A worker: its own copy and task folder; never the default branch's ref: wsx lands.
    pub fn worker(record: &TaskRecord, own: &Path) -> Result<Self> {
        let common = copy::git_common_dir(&record.project_path)?;
        let branch = format!("refs/heads/{}", record.default_branch);
        let mut boundary = Self::new(&record.project_path, own)?;
        boundary.deny_write_prefix = vec![common.join(&branch), common.join("logs").join(&branch)];
        boundary.own.push(real(&record.working_copy));
        Ok(boundary)
    }

    /// The manager: its folder and the project page.
    // ^ It may move the default branch: when a land refuses on the owner's uncommitted changes
    // in the main checkout, it commits them on main after the owner's yes (docs/tasks.md
    // "Manager"), which writes main's ref and reflog.
    pub fn manager(main: &Path, own: &Path, page: &Path) -> Result<Self> {
        let mut boundary = Self::new(main, own)?;
        boundary.own_prefix.push(real(page));
        Ok(boundary)
    }

    fn new(main: &Path, own: &Path) -> Result<Self> {
        if !cfg!(target_os = "macos") {
            anyhow::bail!(
                "a task agent runs only inside wsx's macOS Seatbelt boundary; this platform has none yet"
            );
        }
        let home = dirs::home_dir().context("no home folder")?;
        let common = copy::git_common_dir(main)?;
        let secrets: Vec<PathBuf> = SECRETS.iter().map(|name| real(&home.join(name))).collect();
        let mut deny_write = secrets.clone();
        // ^ Writes only: the agent's own sign-in reads the login keychain in its process.
        deny_write.push(real(&home.join("Library/Keychains")));
        deny_write.extend(config_dirs().iter().map(|dir| real(dir)));
        deny_write.push(real(&store::state_dir()));
        // ^ The main repository's hooks run, and its config applies, in the owner's own git.
        deny_write.extend([real(&common.join("hooks")), real(&common.join("config"))]);
        let deny_copies = match (main.parent(), main.file_name()) {
            (Some(parent), Some(name)) => {
                Some((real(parent), format!("{}-t", name.to_string_lossy())))
            }
            _ => None,
        };
        Ok(Self {
            deny_read: secrets,
            deny_write,
            deny_write_prefix: Vec::new(),
            deny_copies,
            own: vec![real(own)],
            own_prefix: Vec::new(),
        })
    }

    /// The Seatbelt profile: allow everything, then the denials, then the role's own paths.
    pub fn profile(&self) -> String {
        let mut profile = String::from("(version 1)\n(allow default)\n");
        let mut rule = |verb: &str, filter: String| profile += &format!("({verb} {filter})\n");
        for path in &self.deny_read {
            rule("deny file-read*", subpath(path));
        }
        for path in &self.deny_write {
            rule("deny file-write*", subpath(path));
        }
        for path in &self.deny_write_prefix {
            rule(
                "deny file-write*",
                regex(&format!("^{}(\\.lock)?$", escape(path))),
            );
        }
        if let Some((parent, prefix)) = &self.deny_copies {
            let copies = format!("^{}/{}[0-9]+(/|$)", escape(parent), regex_escape(prefix));
            rule("deny file-write*", regex(&copies));
        }
        for path in &self.own {
            rule("allow file-write*", subpath(path));
        }
        for path in &self.own_prefix {
            rule("allow file-write*", regex(&format!("^{}", escape(path))));
        }
        profile
    }

    /// `program` inside the boundary; the caller adds its arguments and environment.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(self.profile()).arg(program);
        command
    }
}

/// Where wsx keeps its configuration: the platform folder and the XDG one.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = dirs::config_dir().into_iter().collect();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")));
    dirs.extend(xdg);
    let mut dirs: Vec<PathBuf> = dirs.into_iter().map(|dir| dir.join("wsx")).collect();
    dirs.dedup();
    dirs
}

/// The path as Seatbelt matches it: symlinks resolved (`/var` is `/private/var` on macOS),
/// through its nearest existing parent when it does not exist yet.
fn real(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn subpath(path: &Path) -> String {
    let text = path.display().to_string();
    format!(
        "subpath \"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn regex(pattern: &str) -> String {
    format!("regex #\"{pattern}\"")
}

fn escape(path: &Path) -> String {
    regex_escape(&path.display().to_string())
}

fn regex_escape(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if "\\.^$|?*+()[]{}\"".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
