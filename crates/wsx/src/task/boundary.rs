//! The agent boundary: one macOS Seatbelt profile per role, applied by the task host and the
//! manager host to the whole agent process tree (Bash, file tools, MCP servers, hooks), so any
//! adapter runs in its own no-prompt mode inside it. A deny-list: reads and writes are open
//! except what other tasks, wsx and the owner's secrets own; network, loopback and PTYs stay
//! open. A signal reaches only a process of the same sandbox, so one agent's `pkill` cannot
//! end another agent, the manager, wsxd or the owner's processes. An untrusted agent (Pi with
//! any model, Claude on another endpoint) gets the strict profile of `wsx_core::task::policy`
//! instead: the same denials plus writes only in its own folders, the network only to the
//! loopback and its own provider, and no privilege tools. ref: docs/tasks.md "Permissions"
// ^ Claude Code (https://code.claude.com/docs/en/sandboxing) and Codex
// (https://developers.openai.com/codex/security) confine commands the same way on macOS:
// `sandbox-exec -p <profile>` with a generated Seatbelt profile.

use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use wsx_core::task::policy::{Policy, Role, DENIED_EXEC};
use wsx_core::task::{copy, store, TaskRecord};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Home folders that hold the owner's secrets: no agent reads or writes them.
const SECRETS: [&str; 3] = [".ssh", ".aws", ".gnupg"];
/// What a strict profile hides besides the secrets above.
const STRICT_SECRETS: [&str; 4] = [".config/gh", ".netrc", ".npmrc", ".claude"];
/// What a strict agent writes outside its folders: the null device, terminals and PTYs.
const DEV_WRITE: &str = r"^/dev/(null|zero|tty|ttys[0-9]+|ptmx|fd/[0-9]+)$";

/// Pi's built-in providers: the endpoint each calls and the variable that holds its key.
const PI_PROVIDERS: [(&str, &str, &str); 2] = [
    (
        "anthropic",
        "https://api.anthropic.com",
        "ANTHROPIC_API_KEY",
    ),
    ("openai", "https://api.openai.com", "OPENAI_API_KEY"),
];
/// The macOS resolver's socket: a strict agent that reaches a remote provider looks its name up.
const RESOLVER_SOCKET: &str = "/private/var/run/mDNSResponder";

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
    /// The project overview and the temporary files an editor writes beside it.
    own_prefix: Vec<PathBuf>,
    policy: Policy,
    /// The main repository's git folder: a strict worker commits there.
    common: PathBuf,
    /// The adapter's own state folder (sessions) and the credential file in it. A strict
    /// agent writes the folder and reads not the file.
    adapter_home: Option<(PathBuf, &'static str)>,
    /// The host's socket, the one Unix socket a strict agent may reach.
    broker: Vec<PathBuf>,
    /// The port of the agent's own provider when it is not on this machine: the one place
    /// besides the loopback a strict agent connects to (Seatbelt cannot name a remote host).
    remote: Vec<u16>,
    /// The one `*_API_KEY` variable the agent's provider needs.
    provider_key: Option<&'static str>,
    /// A built-in Pi provider signs in with the adapter's credential file: it stays readable.
    credential_open: bool,
}

impl Boundary {
    /// A worker: its own copy and task folder; never the default branch's ref: wsx lands.
    /// A child's default branch is its parent's branch; the branch the parent lands on is
    /// denied too. ref: docs/tasks.md "Parent tasks"
    pub fn worker(record: &TaskRecord, own: &Path, policy: Policy) -> Result<Self> {
        let common = copy::git_common_dir(&record.project_path)?;
        let mut branches = vec![record.default_branch.clone()];
        if let Some(parent) = record.parent {
            let parent = store::TaskDir::open(&record.project_id, parent)?.read_record()?;
            branches.push(parent.default_branch);
        }
        let mut boundary = Self::new(&record.project_path, own, policy)?;
        boundary.deny_write_prefix = branches
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .flat_map(|branch| [common.join(&branch), common.join("logs").join(&branch)])
            .collect();
        boundary.own.push(real(&record.working_copy));
        Ok(boundary)
    }

    /// The manager: its folder and the project overview.
    // ^ It may move the default branch: when a land refuses on the owner's uncommitted changes
    // in the main checkout, it commits them on main after the owner's yes (docs/tasks.md
    // "Manager"), which writes main's ref and reflog.
    pub fn manager(main: &Path, own: &Path, overview: &Path, policy: Policy) -> Result<Self> {
        let mut boundary = Self::new(main, own, policy)?;
        boundary.own_prefix.push(real(overview));
        Ok(boundary)
    }

    fn new(main: &Path, own: &Path, policy: Policy) -> Result<Self> {
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
        // ^ wsxd runs the routines written there outside this boundary (docs/tasks.md "Routines").
        deny_write.push(real(&wsx_core::routine::store::root()));
        // ^ The main repository's hooks run, and its config applies, in the owner's own git.
        deny_write.extend([real(&common.join("hooks")), real(&common.join("config"))]);
        let deny_copies = match (main.parent(), main.file_name()) {
            (Some(parent), Some(name)) => {
                Some((real(parent), format!("{}-t", name.to_string_lossy())))
            }
            _ => None,
        };
        let mut deny_read = secrets;
        if policy.strict() {
            // ^ What the strict profile adds to the secrets: the owner's other credentials
            // and wsx's own config (docs/design/local-model-fallback.md 5.3).
            deny_read.extend(STRICT_SECRETS.iter().map(|name| real(&home.join(name))));
            deny_read.push(real(&home.join("Library/Keychains")));
            deny_read.extend(config_dirs().iter().map(|dir| real(dir)));
        }
        Ok(Self {
            deny_read,
            deny_write,
            deny_write_prefix: Vec::new(),
            deny_copies,
            own: vec![real(own)],
            own_prefix: Vec::new(),
            policy,
            common: real(&common),
            adapter_home: None,
            broker: Vec::new(),
            remote: Vec::new(),
            provider_key: None,
            credential_open: false,
        })
    }

    /// The adapter's state folder: `CLAUDE_CONFIG_DIR` or `~/.claude` for `claude`,
    /// `PI_CODING_AGENT_DIR` or `~/.pi/agent` for `pi`. `claude_dir`: the profile's folder.
    pub fn adapter(mut self, adapter: &str, claude_dir: Option<&Path>) -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let env = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        self.adapter_home = match adapter {
            "claude" => Some((
                claude_dir
                    .map(Path::to_path_buf)
                    .or_else(|| env("CLAUDE_CONFIG_DIR"))
                    .unwrap_or_else(|| home.join(".claude")),
                ".credentials.json",
            )),
            "pi" => Some((
                env("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi/agent")),
                "auth.json",
            )),
            _ => None,
        }
        .map(|(dir, credential)| (real(&dir), credential));
        self
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// The `*_API_KEY` variable a strict agent keeps: its provider's, no other.
    pub fn provider_key(&self) -> Option<&'static str> {
        self.provider_key
    }

    /// The endpoint of the agent's provider, which a strict profile lets it reach besides the
    /// loopback: a Pi model `provider/id`'s `baseUrl` in the adapter's `models.json` or the
    /// built-in provider's own, else `ANTHROPIC_BASE_URL` for another adapter. A provider
    /// wsx cannot name reaches nothing. ref: docs/design/local-model-fallback.md 5.3 (P6)
    pub fn reach(mut self, adapter: &str, model: &str, env: &[(String, OsString)]) -> Self {
        let (url, builtin) = if adapter == "pi" {
            let provider = model.split_once('/').map_or("", |(provider, _)| provider);
            let builtin = PI_PROVIDERS.iter().find(|(name, ..)| *name == provider);
            let configured = self
                .adapter_home
                .as_ref()
                .and_then(|(dir, _)| models_json_url(dir, provider));
            (
                configured.or(builtin.map(|(_, url, _)| url.to_string())),
                builtin,
            )
        } else {
            let base = env.iter().find(|(name, _)| name == "ANTHROPIC_BASE_URL");
            (
                base.map(|(_, url)| url.to_string_lossy().into_owned()),
                None,
            )
        };
        self.remote = url.as_deref().and_then(remote_port).into_iter().collect();
        self.provider_key = builtin.map(|(_, _, key)| *key);
        self.credential_open = builtin.is_some() && !self.remote.is_empty();
        self
    }

    /// The host's broker socket, reachable from a strict profile.
    pub fn broker(mut self, socket: &Path) -> Self {
        // ^ Both spellings: Seatbelt may match the path as given or as resolved.
        self.broker = vec![socket.to_path_buf(), real(socket)];
        self.broker.dedup();
        self
    }

    /// The Seatbelt profile: allow everything, then the denials, then the role's own paths.
    // ^ Each `sandbox-exec` is its own sandbox, so `same-sandbox` keeps an agent's signals to
    // its own tree (daemonised children included) though every worker shares this text.
    // Incident 2026-11-05: one worker's `pkill -f` ended three other workers.
    pub(super) fn profile(&self) -> String {
        let mut profile = String::from(
            "(version 1)\n(allow default)\n(deny signal)\n(allow signal (target same-sandbox))\n",
        );
        let strict = self.policy.strict();
        let mut rule = |verb: &str, filter: String| profile += &format!("({verb} {filter})\n");
        if strict {
            // ^ Writes only where the role works; the denials below still win over these.
            rule("deny file-write*", String::new());
            rule("allow file-write*", regex(DEV_WRITE));
            if let Some(tmp) = std::env::var_os("TMPDIR").filter(|tmp| !tmp.is_empty()) {
                rule("allow file-write*", subpath(&real(Path::new(&tmp))));
            }
            // ^ Only a worker commits; the manager's commit on main is for Claude alone.
            if self.policy.role == Role::Worker {
                rule("allow file-write*", subpath(&self.common));
            }
            if let Some((dir, _)) = &self.adapter_home {
                rule("allow file-write*", subpath(dir));
            }
        }
        for path in &self.deny_read {
            // ^ The adapter's own folder stays readable but for its credential file.
            if self
                .adapter_home
                .as_ref()
                .is_some_and(|(dir, _)| dir == path)
            {
                continue;
            }
            rule("deny file-read*", subpath(path));
        }
        if let Some((dir, credential)) = self
            .adapter_home
            .as_ref()
            .filter(|_| strict && !self.credential_open)
        {
            rule(
                "deny file-read*",
                format!("(literal \"{}\")", dir.join(credential).display()),
            );
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
        if strict {
            rule("deny network*", String::new());
            rule(
                "allow network-outbound",
                "(remote ip \"localhost:*\")".into(),
            );
            rule("allow network-inbound", "(local ip \"localhost:*\")".into());
            rule("allow network-bind", "(local ip \"localhost:*\")".into());
            // ^ `sandbox-exec` takes only `*` or `localhost` as the host of an address.
            for port in &self.remote {
                rule(
                    "allow network-outbound",
                    format!("(remote ip \"*:{port}\")"),
                );
            }
            if !self.remote.is_empty() {
                rule(
                    "allow network-outbound",
                    unix_socket(Path::new(RESOLVER_SOCKET)),
                );
            }
            for broker in &self.broker {
                rule("allow network-outbound", unix_socket(broker));
            }
            // ^ wsxd starts any command outside this boundary for whoever speaks its socket.
            rule(
                "deny network-outbound",
                unix_socket(&real(&wsx_core::runtime::default_socket_path())),
            );
            for program in DENIED_EXEC {
                rule("deny process-exec", format!("(literal \"{program}\")"));
            }
        }
        profile
    }

    /// `program` inside the boundary; the caller adds its arguments and environment.
    /// Fails with sandbox-exec's own words when it cannot apply the profile here, so the
    /// agent start fails with the reason instead of an agent that ends at once.
    pub fn command(&self, program: &str) -> Result<Command> {
        let profile = self.profile();
        let tried = Command::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(&profile)
            .arg("/usr/bin/true")
            .output()
            .with_context(|| format!("cannot run {SANDBOX_EXEC}"))?;
        if !tried.status.success() {
            let said = String::from_utf8_lossy(&tried.stderr).trim().to_string();
            // ^ Seatbelt does not start inside another sandbox: sandbox_apply answers EPERM.
            let hint = if said.contains("Operation not permitted") {
                "; this host runs inside another sandbox, where wsx's boundary cannot start: \
                 start wsxd outside it"
            } else {
                ""
            };
            anyhow::bail!(
                "{SANDBOX_EXEC} cannot apply the boundary ({}: {said}){hint}",
                tried.status
            );
        }
        let mut command = Command::new(SANDBOX_EXEC);
        command.arg("-p").arg(profile).arg(program);
        Ok(command)
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
        "(subpath \"{}\")",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn unix_socket(path: &Path) -> String {
    let text = path.display().to_string();
    format!(
        "(remote unix-socket (path-literal \"{}\"))",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// `providers.<provider>.baseUrl` of the Pi folder's `models.json`.
fn models_json_url(dir: &Path, provider: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("models.json")).ok()?;
    let models: serde_json::Value = serde_json::from_str(&text).ok()?;
    let url = models.get("providers")?.get(provider)?.get("baseUrl")?;
    url.as_str().map(str::to_string)
}

/// The port a base URL names, when its host is not this machine's loopback.
fn remote_port(url: &str) -> Option<u16> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = match authority.strip_prefix('[') {
        Some(inner) => inner.split_once(']')?,
        None => authority
            .rsplit_once(':')
            .map_or((authority, ""), |(h, p)| (h, p)),
    };
    let port = match port.trim_start_matches(':') {
        "" if scheme == "http" => 80,
        "" => 443,
        port => port.parse().ok()?,
    };
    let loopback =
        host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.");
    (!host.is_empty() && !loopback).then_some(port)
}

fn regex(pattern: &str) -> String {
    format!("(regex #\"{pattern}\")")
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
