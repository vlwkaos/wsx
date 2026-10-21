use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::session_state;
use wsx_core::{
    config::global::{project_matches_group, GlobalConfig, GroupKey},
    model::workspace::{Project, WorkspaceState},
    ops, runtime,
};

#[derive(Clone, Default, ValueEnum)]
pub enum Format {
    #[default]
    Normal,
    Compact,
}

#[derive(Parser)]
#[command(
    name = "wsx",
    version,
    about = "Project-first worktree and terminal manager"
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Portrait/mobile mode: full-width tree, no preview panel
    #[arg(long)]
    pub mobile: bool,
    // ^ docs/daemon-live-upgrade.md: set by an older TUI that re-executed into this binary.
    #[arg(long, hide = true, value_name = "VERSION")]
    pub followed_from: Option<String>,
    #[arg(long, hide = true, requires = "followed_from", value_name = "PANE_ID")]
    pub follow_pane: Option<u64>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Print workspace state
    Status {
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        subcommand: WorktreeCmd,
    },
    /// Session operations
    Session {
        #[command(subcommand)]
        subcommand: SessionCmd,
    },
    /// Group operations
    Group {
        #[command(subcommand)]
        subcommand: GroupCmd,
    },
    /// Report normalized state from a trusted agent adapter
    Agent {
        #[command(subcommand)]
        subcommand: AgentCmd,
    },
    /// Inspect or reload trusted executable plugins
    Plugin {
        #[command(subcommand)]
        subcommand: PluginCmd,
    },
    /// Inspect the wsx terminal daemon without starting it
    Runtime {
        #[command(subcommand)]
        subcommand: RuntimeCmd,
    },
    /// Control the wsx terminal daemon without starting it
    Daemon {
        #[command(subcommand)]
        subcommand: DaemonCmd,
    },
    /// Manage machine-local scheduled routines
    Routine {
        #[command(subcommand)]
        subcommand: RoutineCmd,
    },
    /// Host one paired web site for remote agent work (docs/web-client.md)
    Web {
        #[command(subcommand)]
        subcommand: WebCmd,
    },
    /// Headless worker tasks: new, list, show, steer, add, allow, stop, close (docs/tasks.md)
    Task {
        #[command(subcommand)]
        subcommand: crate::task::TaskCmd,
    },
}

#[derive(Subcommand)]
pub enum WebCmd {
    /// Serve one site on its loopback port until interrupted
    Serve {
        #[command(flatten)]
        site: WebSite,
    },
    /// Print a one-time code that pairs one device with the site
    Pair {
        #[command(flatten)]
        site: WebSite,
    },
    /// List paired devices, or revoke one
    Devices {
        #[command(flatten)]
        site: WebSite,
        /// Device ID to revoke
        #[arg(long)]
        revoke: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print a new Web Push key pair for a site file
    Vapid,
}

/// Which site a `wsx web` command is for (docs/web-client.md "Site file").
#[derive(clap::Args)]
pub struct WebSite {
    /// Owner-only site file, or a site name in ~/.config/wsx/web/; omit it when that folder has one site
    #[arg(value_name = "SITE")]
    name: Option<String>,
    /// Same as SITE
    #[arg(long = "site", value_name = "SITE", conflicts_with = "name")]
    flag: Option<String>,
}

impl WebSite {
    fn named(&self) -> Option<&str> {
        self.flag.as_deref().or(self.name.as_deref())
    }
}

impl Command {
    /// True when the caller asked for JSON, so stderr must stay free of notices.
    pub fn json_output(&self) -> bool {
        match self {
            Command::Status { json, .. } => *json,
            Command::Worktree { subcommand } => matches!(
                subcommand,
                WorktreeCmd::Create { json: true, .. } | WorktreeCmd::List { json: true, .. }
            ),
            Command::Session { subcommand } => matches!(
                subcommand,
                SessionCmd::Create { json: true, .. }
                    | SessionCmd::Delete { json: true, .. }
                    | SessionCmd::Restart { json: true, .. }
                    | SessionCmd::List { json: true, .. }
            ),
            Command::Agent { subcommand } => matches!(
                subcommand,
                AgentCmd::Request { json: true, .. }
                    | AgentCmd::Start(AgentStartArgs { json: true, .. })
                    | AgentCmd::Inspect { json: true, .. }
                    | AgentCmd::Wait { json: true, .. }
                    | AgentCmd::Continue { json: true, .. }
                    | AgentCmd::Cancel { json: true, .. }
                    | AgentCmd::Exchanges { json: true, .. }
            ),
            Command::Plugin { subcommand } => {
                matches!(subcommand, PluginCmd::List { json: true })
            }
            Command::Daemon { subcommand } => matches!(
                subcommand,
                DaemonCmd::Stop { json: true, .. } | DaemonCmd::Restart { json: true }
            ),
            Command::Task { subcommand } => subcommand.json_output(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AgentStateArg {
    Unknown,
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl From<AgentStateArg> for runtime::AgentState {
    fn from(value: AgentStateArg) -> Self {
        match value {
            AgentStateArg::Unknown => Self::Unknown,
            AgentStateArg::Idle => Self::Idle,
            AgentStateArg::Working => Self::Working,
            AgentStateArg::Blocked => Self::Blocked,
            AgentStateArg::Done => Self::Done,
            AgentStateArg::Error => Self::Error,
        }
    }
}

#[derive(Subcommand)]
pub enum AgentCmd {
    /// Discover exact agent targets with bounded provider-native persisted history
    Context {
        session: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=32))]
        limit: u32,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=32))]
        messages: u32,
        #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u32).range(1..=16384))]
        bytes: u32,
        /// Return candidate identities and readiness without reading native history
        #[arg(long)]
        metadata_only: bool,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Install an agent lifecycle integration
    Install {
        #[arg(value_parser = parse_integration_target)]
        integration: wsx_core::integration::IntegrationTarget,
    },
    /// Mark the current managed pane's retained agent as detached
    Detach,
    /// Submit an authoritative provider report for one pane
    Report {
        pane: String,
        #[arg(long)]
        provider: String,
        #[arg(long, value_enum)]
        state: AgentStateArg,
        #[arg(long, conflicts_with_all = ["session_id", "session_path"])]
        conversation_id: Option<String>,
        #[arg(long, conflicts_with = "session_path")]
        session_id: Option<String>,
        #[arg(long)]
        session_path: Option<String>,
        #[arg(long, requires = "session_id")]
        transcript_path: Option<String>,
        #[arg(long, hide = true)]
        wake_token: Option<String>,
        #[arg(long)]
        prompt: bool,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        lifecycle: bool,
        /// Retain resume metadata but mark the agent process as no longer attached
        #[arg(long)]
        detached: bool,
        #[arg(long, hide = true)]
        presence_id: Option<String>,
        #[arg(long, hide = true)]
        escape_interrupts: bool,
        #[arg(long, hide = true)]
        exchange_receipts: bool,
        /// The hook reports which prompt each turn takes (see --received)
        #[arg(long, hide = true)]
        prompt_receipts: bool,
        /// The prompt this turn took: `<exchange>:<round>` of a wsx round, or `other`
        #[arg(long, hide = true, value_parser = parse_prompt_received)]
        received: Option<runtime::PromptReceived>,
        /// The agent compacts its conversation (on a Working report)
        #[arg(long, hide = true)]
        compacting: bool,
    },
    #[command(hide = true)]
    PresenceRenew {
        pane: String,
        #[arg(long)]
        presence_id: String,
    },
    #[command(hide = true)]
    WakeHeartbeat {
        pane: String,
        #[arg(long)]
        prompt_id: String,
    },
    /// Deliver a bounded provider-neutral request to an explicit agent session
    Request {
        session: String,
        #[command(flatten)]
        prompt: PromptSource,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long)]
        writer: bool,
        #[arg(long = "write-claim", requires = "writer")]
        write_claims: Vec<PathBuf>,
        /// Stash an idle Claude draft before delivery (never cancels a running agent)
        #[arg(long)]
        stash_draft: bool,
        /// Deliver once per key and pane: a repeat returns the existing exchange
        #[arg(long, value_name = "TEXT")]
        key: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Start an agent on a branch in one call: worktree, session, readiness and request
    Start(AgentStartArgs),
    /// Submit an adapter-observed receipt for the exact current exchange round
    #[command(hide = true)]
    ExchangeReceipt {
        exchange: runtime::AgentExchangeId,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        round: u32,
        #[arg(long, value_parser = parse_exchange_receipt)]
        receipt: runtime::AgentExchangeReceipt,
        #[arg(long, requires = "input_id")]
        delivery_sha256: Option<String>,
        #[arg(long, requires = "delivery_sha256")]
        input_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect one exchange and optionally include its bounded terminal frame
    Inspect {
        exchange: runtime::AgentExchangeId,
        #[arg(long)]
        frame: bool,
        #[arg(long)]
        json: bool,
    },
    /// Wait until one exchange, or with --any the first of several, is blocked or finished
    Wait {
        exchanges: Vec<runtime::AgentExchangeId>,
        /// Return the first of the named exchanges, or of the active ones in -p/-w, that
        /// reaches a wait boundary
        #[arg(long)]
        any: bool,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(long)]
        frame: bool,
        /// Add the agent's final message of this round from its saved history
        #[arg(long)]
        result: bool,
        /// Largest result text in bytes
        #[arg(long, default_value_t = 16384, requires = "result",
              value_parser = clap::value_parser!(u32).range(1..=65536))]
        result_bytes: u32,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Deliver a follow-up round to a terminal exchange
    Continue {
        exchange: runtime::AgentExchangeId,
        #[command(flatten)]
        prompt: PromptSource,
        /// Stash an idle Claude draft before delivery
        #[arg(long)]
        stash_draft: bool,
        /// Deliver this round once per key: a repeat returns the exchange unchanged
        #[arg(long, value_name = "TEXT")]
        key: Option<String>,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Request cancellation of one active exchange
    Cancel {
        exchange: runtime::AgentExchangeId,
        #[arg(long)]
        json: bool,
    },
    /// List retained exchanges, optionally scoped to one session or pane
    Exchanges {
        session: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
}

// ^ docs/agent-orchestration.md "Start a worker in one call".
#[derive(clap::Args)]
pub struct AgentStartArgs {
    /// Branch of the worktree; the worktree is created when it is missing
    branch: String,
    #[command(flatten)]
    prompt: PromptSource,
    #[arg(short, long)]
    project: Option<String>,
    /// Start a new branch from this commit, tag, or branch instead of the default branch
    #[arg(long)]
    from: Option<String>,
    /// Command that starts the agent in the new session
    /// (default: the project's worktree.defaultSession.command)
    #[arg(long, value_name = "COMMAND")]
    agent: Option<String>,
    /// Session name; an existing session of this name in the worktree is used
    /// (default: the first word of the agent command)
    #[arg(long)]
    name: Option<String>,
    #[arg(long, default_value_t = 600)]
    timeout: u64,
    /// Seconds to wait until the agent can take a prompt
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
    ready_timeout: u64,
    #[arg(long)]
    writer: bool,
    #[arg(long = "write-claim", requires = "writer")]
    write_claims: Vec<PathBuf>,
    /// Deliver once per key and pane: a repeat returns the existing exchange
    #[arg(long, value_name = "TEXT")]
    key: Option<String>,
    /// Answer yes when the agent asks whether to trust the worktree folder. The agent
    /// saves that decision and then runs the folder's own settings and hooks
    #[arg(long)]
    accept_folder_trust: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List accepted plugin manifests
    List {
        #[arg(long)]
        json: bool,
    },
    /// Reload manifests from the owner-controlled plugin directory
    Reload,
}

#[derive(Subcommand)]
pub enum RuntimeCmd {
    /// Report socket, protocol, epoch, revision, and resource counts
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Stop wsxd and end the programs running in its sessions; saved session commands restart on next launch
    Stop {
        /// Stop even when sessions are running
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Replace wsxd with the installed build; sessions, shells and agents keep running
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Clear the shared crash budget and explicitly recover wsxd
    Recover,
}

#[derive(Subcommand)]
pub enum RoutineCmd {
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Show {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Add {
        name: String,
        #[arg(long)]
        cron: String,
        #[arg(long = "arg", required = true)]
        command: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Edit {
        name: String,
        #[arg(long)]
        new_name: Option<String>,
        #[arg(long)]
        cron: String,
        #[arg(long = "arg", required = true)]
        command: Vec<String>,
        #[arg(long, default_value = "")]
        prompt: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Delete {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Enable cron scheduling for a routine
    Enable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Disable cron scheduling without cancelling an active run
    Disable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
    },
    Run {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Cancel an active routine run
    Cancel {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    Logs {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Fire a provider-neutral event for matching routines
    Fire {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        event_id: String,
        #[arg(long, default_value = "null")]
        payload: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum GroupCmd {
    /// List groups
    Ls,
    /// Create a group
    Create { name: String },
    /// Rename a group
    Rename { old: String, new_name: String },
    /// Add a project to a group
    Add { group: String, project: String },
    /// Remove a project from a group
    Remove { group: String, project: String },
}

#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Create a worktree and its configured default session
    Create {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
        /// Start a new branch from this commit, tag, or branch instead of the default branch
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a clean worktree and its sessions (refuses uncommitted work; keeps an unmerged branch)
    Delete {
        branch: String,
        #[arg(short, long)]
        project: Option<String>,
    },
    /// List worktrees
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
}

/// Exactly one prompt source: the argument, `-` for standard input, or a file.
#[derive(clap::Args)]
pub struct PromptSource {
    /// Prompt text, or `-` to read the prompt from standard input
    prompt: Option<String>,
    /// Read the prompt from this file
    #[arg(long, value_name = "PATH")]
    prompt_file: Option<PathBuf>,
}

impl PromptSource {
    /// Read the prompt and refuse an empty or oversized one before any daemon call.
    /// ^ One trailing line end of a file or a pipe is not part of the prompt: wsxd adds Enter.
    pub(crate) fn read(self, limit: usize) -> Result<String> {
        use std::io::Read;
        let bounded = |reader: &mut dyn Read, source: &str| -> Result<String> {
            let mut bytes = Vec::new();
            // Two bytes past the limit prove an oversized prompt without reading all of it.
            reader
                .take(limit as u64 + 2)
                .read_to_end(&mut bytes)
                .with_context(|| format!("cannot read the prompt from {source}"))?;
            let mut text = String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("the prompt from {source} is not UTF-8 text"))?;
            if text.ends_with('\n') {
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
            }
            Ok(text)
        };
        let prompt = match (self.prompt, self.prompt_file) {
            (Some(_), Some(_)) => {
                bail!(
                    "give one prompt source: an argument, `-` for standard input, or --prompt-file"
                )
            }
            (None, None) => {
                bail!("a prompt is required: an argument, `-` for standard input, or --prompt-file <path>")
            }
            (Some(text), None) if text == "-" => {
                bounded(&mut std::io::stdin().lock(), "standard input")?
            }
            (Some(text), None) => text,
            (None, Some(path)) => {
                let mut file = std::fs::File::open(&path)
                    .with_context(|| format!("cannot open prompt file {}", path.display()))?;
                bounded(&mut file, &path.display().to_string())?
            }
        };
        if prompt.is_empty() {
            bail!("the prompt is empty");
        }
        if prompt.len() > limit {
            bail!("the prompt is larger than the limit of {limit} bytes");
        }
        Ok(prompt)
    }
}

#[derive(Default, clap::Args)]
pub struct SessionScope {
    /// Select or restrict a session target to one project
    #[arg(short, long)]
    project: Option<String>,
    /// Select or restrict a session target to one worktree branch, alias, or path
    #[arg(short = 'w', long)]
    worktree: Option<String>,
}

// ^ [[Direct Session Control]] Parsing and resolution live here; wsx-core ops and
// wsxd retain mutation, revision, and PTY authority.
#[derive(Subcommand)]
pub enum SessionCmd {
    /// Create a session in a known or inferred worktree
    Create {
        #[arg(long)]
        name: Option<String>,
        /// Text entered into the new shell after it starts
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Report the managed pane that runs this command
    Current {
        #[arg(long)]
        json: bool,
    },
    /// Delete a session and all of its panes
    Delete {
        session: String,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Restart an exited session pane with its saved command
    Restart {
        session: String,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Deprecated alias for send-text
    SendKeys {
        session: String,
        keys: String,
        #[arg(long)]
        no_enter: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Send literal text to a terminal pane
    SendText {
        session: String,
        text: String,
        #[arg(long)]
        no_enter: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Submit a prompt to a session's primary pane
    Prompt {
        session: String,
        #[command(flatten)]
        prompt: PromptSource,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Read a bounded window from the pane's visible semantic frame
    Peek {
        session: String,
        /// Number of visible lines to return (default: full viewport)
        #[arg(short = 'n', long)]
        lines: Option<u32>,
        /// Trim trailing blank lines
        #[arg(long)]
        trim: bool,
        /// Strip ANSI/decorations and compact for agent consumption
        #[arg(short = 'a', long)]
        agent: bool,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// Rename a session
    Rename {
        old: String,
        new_name: String,
        #[command(flatten)]
        scope: SessionScope,
    },
    /// List sessions
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(short = 'f', long, value_enum, default_value = "normal")]
        format: Format,
        #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
        group: Option<GroupKey>,
    },
}

pub fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Status {
            json,
            format,
            group,
        } => cmd_status(json, format, group.as_ref()),
        Command::Worktree { subcommand } => match subcommand {
            WorktreeCmd::Create {
                branch,
                project,
                from,
                json,
            } => cmd_worktree_create(&branch, project.as_deref(), from.as_deref(), json),
            WorktreeCmd::Delete { branch, project } => {
                cmd_worktree_delete(&branch, project.as_deref())
            }
            WorktreeCmd::List {
                project,
                json,
                format,
                group,
            } => cmd_worktree_list(project.as_deref(), json, format, group.as_ref()),
        },
        Command::Session { subcommand } => match subcommand {
            SessionCmd::Create {
                name,
                command,
                json,
                scope,
            } => cmd_session_create(scope, name, command, json),
            SessionCmd::Current { json } => cmd_session_current(json),
            SessionCmd::Delete {
                session,
                json,
                scope,
            } => cmd_session_delete(&session, &scope, json),
            SessionCmd::Restart {
                session,
                json,
                scope,
            } => cmd_session_restart(&session, json, &scope),
            SessionCmd::SendKeys {
                session: s,
                keys,
                no_enter,
                scope,
            } => cmd_session_send_text(&s, &keys, no_enter, &scope),
            SessionCmd::SendText {
                session,
                text,
                no_enter,
                scope,
            } => cmd_session_send_text(&session, &text, no_enter, &scope),
            SessionCmd::Prompt {
                session,
                prompt,
                scope,
            } => cmd_session_prompt(&session, prompt, &scope),
            SessionCmd::Peek {
                session: s,
                lines,
                trim,
                agent,
                scope,
            } => cmd_session_peek(&s, lines, trim, agent, &scope),
            SessionCmd::Rename {
                old,
                new_name,
                scope,
            } => cmd_session_rename(&old, &new_name, &scope),
            SessionCmd::List {
                project,
                json,
                format,
                group,
            } => cmd_session_list(project.as_deref(), json, format, group.as_ref()),
        },
        Command::Group { subcommand } => match subcommand {
            GroupCmd::Ls => cmd_group_ls(),
            GroupCmd::Create { name } => cmd_group_create(&name),
            GroupCmd::Rename { old, new_name } => cmd_group_rename(&old, &new_name),
            GroupCmd::Add { group, project } => cmd_group_add(&group, &project),
            GroupCmd::Remove { group, project } => cmd_group_remove(&group, &project),
        },
        Command::Agent { subcommand } => match subcommand {
            AgentCmd::Context {
                session,
                provider,
                limit,
                messages,
                bytes,
                metadata_only,
                json,
                scope,
            } => cmd_agent_context(
                session.as_deref(),
                provider.as_deref(),
                ContextOptions {
                    limit,
                    messages,
                    bytes,
                    metadata_only,
                    json,
                },
                &scope,
            ),
            AgentCmd::Install { integration } => cmd_agent_install(integration),
            AgentCmd::Detach => cmd_agent_detach(),
            AgentCmd::Report {
                pane,
                provider,
                state,
                conversation_id,
                session_id,
                session_path,
                transcript_path,
                wake_token,
                prompt,
                resume,
                lifecycle,
                detached,
                presence_id,
                escape_interrupts,
                exchange_receipts,
                prompt_receipts,
                received,
                compacting,
            } => cmd_agent_report(
                &pane,
                AgentReportOptions {
                    provider,
                    state: state.into(),
                    attached: !detached,
                    presence_id,
                    conversation_id,
                    session_id,
                    session_path,
                    transcript_path,
                    wake_token,
                    capabilities: runtime::AgentCapabilities {
                        prompt,
                        resume,
                        lifecycle,
                        escape_interrupts,
                        exchange_receipts,
                        prompt_receipts,
                    },
                    received,
                    compacting,
                },
            ),
            AgentCmd::PresenceRenew { pane, presence_id } => {
                cmd_agent_presence_renew(&pane, &presence_id)
            }
            AgentCmd::WakeHeartbeat { pane, prompt_id } => {
                cmd_agent_wake_heartbeat(&pane, &prompt_id)
            }
            AgentCmd::Request {
                session,
                prompt,
                timeout,
                writer,
                write_claims,
                stash_draft,
                key,
                json,
                scope,
            } => cmd_agent_exchange_request(
                &session,
                prompt,
                timeout,
                writer,
                write_claims,
                stash_draft,
                key,
                json,
                &scope,
            ),
            AgentCmd::Start(args) => cmd_agent_start(args),
            AgentCmd::ExchangeReceipt {
                exchange,
                round,
                receipt,
                delivery_sha256,
                input_id,
                json,
            } => cmd_agent_exchange_receipt(
                exchange,
                round,
                receipt,
                delivery_sha256,
                input_id,
                json,
            ),
            AgentCmd::Inspect {
                exchange,
                frame,
                json,
            } => cmd_agent_exchange_inspect(exchange, frame, json),
            AgentCmd::Wait {
                exchanges,
                any,
                timeout,
                frame,
                result,
                result_bytes,
                json,
                scope,
            } => cmd_agent_exchange_wait(
                exchanges,
                any,
                timeout,
                frame,
                result.then_some(result_bytes as usize),
                json,
                &scope,
            ),
            AgentCmd::Continue {
                exchange,
                prompt,
                stash_draft,
                key,
                timeout,
                json,
            } => cmd_agent_exchange_continue(exchange, prompt, timeout, stash_draft, key, json),
            AgentCmd::Cancel { exchange, json } => cmd_agent_exchange_cancel(exchange, json),
            AgentCmd::Exchanges {
                session,
                json,
                scope,
            } => cmd_agent_exchange_list(session.as_deref(), json, &scope),
        },
        Command::Plugin { subcommand } => match subcommand {
            PluginCmd::List { json } => cmd_plugin_list(json),
            PluginCmd::Reload => cmd_plugin_reload(),
        },
        Command::Runtime { subcommand } => match subcommand {
            RuntimeCmd::Status { json } => cmd_runtime_status(json),
        },
        Command::Daemon { subcommand } => match subcommand {
            DaemonCmd::Stop { force, json } => cmd_daemon_stop(force, json),
            DaemonCmd::Restart { json } => cmd_daemon_restart(json),
            DaemonCmd::Recover => cmd_daemon_recover(),
        },
        Command::Routine { subcommand } => cmd_routine(subcommand),
        Command::Task { subcommand } => crate::task::run(subcommand),
        Command::Web { subcommand } => match subcommand {
            WebCmd::Serve { site } => wsx_web::serve(site.named()),
            WebCmd::Pair { site } => wsx_web::pair(site.named()),
            WebCmd::Devices { site, revoke, json } => {
                wsx_web::devices(site.named(), revoke.as_deref(), json)
            }
            WebCmd::Vapid => wsx_web::vapid(),
        },
    }
}

fn cmd_routine(command: RoutineCmd) -> Result<()> {
    use asched_core::routine::{ipc::Action, Routine, Trigger};

    let (project_arg, action, json) = match command {
        RoutineCmd::List { project, json } => (project, Action::List, json),
        RoutineCmd::Show {
            name,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let (revision, routines) = fetch_routines(&path)?;
            let routine = routines
                .into_iter()
                .find(|view| view.routine.name == name)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            return print_routine_response(
                asched_core::routine::ipc::Response::Routine {
                    revision,
                    routine: Box::new(routine),
                },
                json,
            );
        }
        RoutineCmd::Add {
            name,
            cron,
            command,
            prompt,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            let routine = Routine {
                name,
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled: true,
            }
            .validated()?;
            return print_routine_response(
                send_routine(&path, Action::Add { revision, routine })?,
                false,
            );
        }
        RoutineCmd::Edit {
            name,
            new_name,
            cron,
            command,
            prompt,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let (current_revision, routines) = fetch_routines(&path)?;
            let enabled = routines
                .iter()
                .find(|view| view.routine.name == name)
                .map(|view| view.routine.enabled)
                .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
            let routine = Routine {
                name: new_name.unwrap_or_else(|| name.clone()),
                trigger: Trigger::Cron(cron),
                command,
                prompt,
                enabled,
            }
            .validated()?;
            return print_routine_response(
                send_routine(
                    &path,
                    Action::Edit {
                        revision: revision.unwrap_or(current_revision),
                        old_name: name,
                        routine,
                    },
                )?,
                false,
            );
        }
        RoutineCmd::Delete {
            name,
            project,
            revision,
        } => {
            let path = routine_project(project.as_deref())?;
            let revision = revision.unwrap_or(fetch_revision(&path)?);
            return print_routine_response(
                send_routine(&path, Action::Delete { revision, name })?,
                false,
            );
        }
        RoutineCmd::Enable {
            name,
            project,
            revision,
        } => return set_routine_enabled(project.as_deref(), &name, revision, true),
        RoutineCmd::Disable {
            name,
            project,
            revision,
        } => return set_routine_enabled(project.as_deref(), &name, revision, false),
        RoutineCmd::Run {
            name,
            project,
            json,
        } => (project, Action::Run { name }, json),
        RoutineCmd::Cancel { name, project } => (project, Action::Cancel { name }, false),
        RoutineCmd::Logs {
            name,
            project,
            json,
        } => (project, Action::Logs { name }, json),
        RoutineCmd::Fire {
            kind,
            event_id,
            payload,
            project,
            json,
        } => {
            let path = routine_project(project.as_deref())?;
            let payload = serde_json::from_str(&payload)
                .map_err(|error| anyhow::anyhow!("event payload must be JSON: {error}"))?;
            let response = send_routine(
                &path,
                Action::Fire {
                    kind,
                    payload,
                    event_id,
                },
            )?;
            let asched_core::routine::ipc::Response::Fire { outcome } = response else {
                bail!("routine daemon returned an unexpected fire response")
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                print_fire_outcome(&outcome);
            }
            return Ok(());
        }
    };
    let path = routine_project(project_arg.as_deref())?;
    print_routine_response(send_routine(&path, action)?, json)
}

fn routine_root() -> Result<PathBuf> {
    Ok(asched_core::registry::RegistryStore::default_root()?)
}

pub(crate) fn registered_routine_paths() -> Result<Vec<PathBuf>> {
    let registry = asched_core::registry::RegistryStore::new(routine_root()?).load()?;
    Ok(registry
        .projects
        .into_iter()
        .map(|project| project.working_dir)
        .collect())
}

fn routine_project(value: Option<&str>) -> Result<PathBuf> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(warning) = warning {
        eprintln!("warning: {warning}");
    }
    if let Some(value) = value {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
        return config
            .projects
            .iter()
            .find(|p| p.name == value)
            .map(|p| p.path.clone())
            .ok_or_else(|| anyhow::anyhow!("project '{value}' not found"));
    }
    match config.projects.as_slice() {
        [project] => Ok(project.path.clone()),
        [] => Ok(std::env::current_dir()?),
        many => bail!(
            "multiple projects — use -p to specify: {}",
            many.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn send_routine(
    project: &Path,
    action: asched_core::routine::ipc::Action,
) -> Result<asched_core::routine::ipc::Response> {
    let request = asched_core::routine::ipc::Request::new(project.to_path_buf(), action);
    // ^ wsxd hosts the asched-core scheduler while this client retains its socket protocol.
    Ok(routine_client()?.request_with_start(&request, runtime::routine_daemon_command())?)
}

fn routine_client() -> Result<asched_core::routine::RoutineClient> {
    Ok(asched_core::routine::RoutineClient::new(routine_root()?))
}

fn fetch_routines(project: &Path) -> Result<(u64, Vec<asched_core::routine::ipc::RoutineView>)> {
    match send_routine(project, asched_core::routine::ipc::Action::List)? {
        asched_core::routine::ipc::Response::Routines { revision, routines } => {
            Ok((revision, routines))
        }
        other => bail!("routine daemon returned unexpected response: {other:?}"),
    }
}

fn fetch_revision(project: &Path) -> Result<u64> {
    Ok(fetch_routines(project)?.0)
}

fn set_routine_enabled(
    project: Option<&str>,
    name: &str,
    revision: Option<u64>,
    enabled: bool,
) -> Result<()> {
    let path = routine_project(project)?;
    let (current_revision, routines) = fetch_routines(&path)?;
    let mut routine = routines
        .into_iter()
        .find(|view| view.routine.name == name)
        .map(|view| view.routine)
        .ok_or_else(|| anyhow::anyhow!("routine '{name}' not found"))?;
    routine.enabled = enabled;
    print_routine_response(
        send_routine(
            &path,
            asched_core::routine::ipc::Action::Edit {
                revision: revision.unwrap_or(current_revision),
                old_name: name.to_string(),
                routine,
            },
        )?,
        false,
    )
}

fn print_fire_outcome(outcome: &asched_core::routine::FireOutcome) {
    use asched_core::routine::{FireOutcome, RoutineFire};
    match outcome {
        FireOutcome::Deduplicated => println!("deduplicated"),
        FireOutcome::NoMatch => println!("no matching routines"),
        FireOutcome::Handled { routines } => {
            for routine in routines {
                match routine {
                    RoutineFire::Started { name } => println!("started\t{name}"),
                    RoutineFire::AlreadyRunning { name } => {
                        println!("already running\t{name}")
                    }
                }
            }
        }
    }
}

fn print_routine_response(response: asched_core::routine::ipc::Response, json: bool) -> Result<()> {
    use asched_core::routine::ipc::Response;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    match response {
        Response::Routines { revision, routines } => {
            println!("revision {revision}");
            for view in routines {
                println!(
                    "{}\t{}\t{}\t{}",
                    view.routine.name,
                    if view.routine.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    format_trigger(&view.routine.trigger),
                    format_argv(&view.routine.command)
                );
            }
        }
        Response::Routine { revision, routine } => println!(
            "revision {revision}\nname: {}\nenabled: {}\ntrigger: {}\ncommand: {}\nprompt: {}",
            routine.routine.name,
            routine.routine.enabled,
            format_trigger(&routine.routine.trigger),
            format_argv(&routine.routine.command),
            routine.routine.prompt
        ),
        Response::Runs { runs } => {
            for run in runs {
                println!(
                    "{}\t{:?}\t{}",
                    run.id,
                    run.status,
                    run.final_output.replace('\n', " ")
                );
            }
        }
        Response::Fire { outcome } => print_fire_outcome(&outcome),
        Response::Daemon { protocol, pid } => {
            println!("asched daemon pid={pid} protocol={protocol}")
        }
        Response::Ok { revision } => match revision {
            Some(revision) => println!("ok revision={revision}"),
            None => println!("ok"),
        },
        Response::Error { kind, message } => bail!("{kind}: {message}"),
    }
    Ok(())
}

fn format_trigger(trigger: &asched_core::routine::Trigger) -> String {
    match trigger {
        asched_core::routine::Trigger::Cron(cron) => cron.clone(),
        asched_core::routine::Trigger::Event { kind } => format!("event:{kind}"),
    }
}

fn format_argv(argv: &[String]) -> String {
    serde_json::to_string(argv).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod routine_output_tests {
    use super::{format_argv, Args, Command, RoutineCmd};
    use clap::Parser;

    #[test]
    fn plain_routine_argv_preserves_argument_boundaries() {
        assert_eq!(
            format_argv(&["printf".into(), "two words".into(), "".into()]),
            r#"["printf","two words",""]"#
        );
    }

    #[test]
    fn pi_routine_cli_accepts_flag_argv_as_explicit_values() {
        let parsed = Args::try_parse_from([
            "wsx",
            "routine",
            "add",
            "weekday-review",
            "--cron",
            "0 9 * * 1-5",
            "--arg=pi",
            "--arg=-p",
            "--arg={prompt}",
            "--prompt",
            "Review the project",
        ])
        .unwrap();

        let Some(Command::Routine {
            subcommand:
                RoutineCmd::Add {
                    name,
                    command,
                    prompt,
                    ..
                },
        }) = parsed.command
        else {
            panic!("expected routine add command");
        };
        assert_eq!(name, "weekday-review");
        assert_eq!(command, ["pi", "-p", "{prompt}"]);
        assert_eq!(prompt, "Review the project");
    }
}

// --- Helpers ---

pub(crate) fn load_config() -> Result<GlobalConfig> {
    let (config, warning) = GlobalConfig::load()?;
    if let Some(w) = warning {
        eprintln!("warning: {}", w);
    }
    Ok(config)
}

pub(crate) fn load_full_workspace() -> Result<(GlobalConfig, WorkspaceState)> {
    let config = load_config()?;
    let workspace = ops::load_full_workspace(&config)?;
    Ok((config, workspace))
}

fn parse_group_key(value: &str) -> std::result::Result<GroupKey, String> {
    if value.eq_ignore_ascii_case("ungrouped") {
        Ok(GroupKey::Ungrouped)
    } else {
        GroupKey::named(value)
    }
}

#[cfg(test)]
mod group_command_tests {
    use super::{resolve_projects, Args, Command, GroupCmd};
    use clap::Parser;
    use wsx_core::config::global::{GlobalConfig, GroupKey};
    use wsx_core::model::workspace::WorkspaceState;

    #[test]
    fn group_filter_accepts_named_recent_and_rejects_duplicates() {
        let args = Args::try_parse_from(["wsx", "status", "--group", "recent"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Status { group, .. })
                if group == Some(GroupKey::Named("recent".into()))
        ));
        assert!(
            Args::try_parse_from(["wsx", "status", "--group", "work", "--group", "recent"])
                .is_err()
        );
        let ungrouped = Args::try_parse_from(["wsx", "status", "--group", "UNGROUPED"]).unwrap();
        assert!(matches!(
            ungrouped.command,
            Some(Command::Status { group, .. }) if group == Some(GroupKey::Ungrouped)
        ));
        assert!(Args::try_parse_from(["wsx", "status", "--group", "Default"]).is_err());
    }

    #[test]
    fn named_group_filter_requires_configured_group_including_recent() {
        let workspace = WorkspaceState { projects: vec![] };
        let recent = GroupKey::Named("recent".into());
        let config = GlobalConfig::default();
        let error = resolve_projects(&config, &workspace, None, Some(&recent)).unwrap_err();
        assert!(error.to_string().contains("group 'recent' not found"));

        let mut config = GlobalConfig::default();
        config.groups.push("recent".into());
        assert!(resolve_projects(&config, &workspace, None, Some(&recent))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn group_add_is_canonical_and_tab_surfaces_are_removed() {
        let args = Args::try_parse_from(["wsx", "group", "add", "work", "wsx"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Group {
                subcommand: GroupCmd::Add { group, project }
            }) if group == "work" && project == "wsx"
        ));
        assert!(Args::try_parse_from(["wsx", "tab", "ls"]).is_err());
        assert!(Args::try_parse_from(["wsx", "status", "--tab", "work"]).is_err());
    }
}

#[cfg(test)]
mod session_command_tests {
    use super::{created_session_line, Args, Command, SessionCmd};
    use clap::Parser;
    use wsx_core::runtime::SessionId;

    #[test]
    fn create_accepts_direct_scope_command_and_json_output() {
        let args = Args::try_parse_from([
            "wsx",
            "session",
            "create",
            "--project",
            "api",
            "--worktree",
            "fix/timeout",
            "--name",
            "review",
            "--command",
            "pi -p 'Review timeout handling'",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Session {
                subcommand: SessionCmd::Create {
                    name: Some(name),
                    command: Some(command),
                    json: true,
                    scope,
                }
            }) if name == "review"
                && command == "pi -p 'Review timeout handling'"
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
    }

    #[test]
    fn restart_requires_a_selector_and_accepts_scope_and_json() {
        let args = Args::try_parse_from([
            "wsx",
            "session",
            "restart",
            "worker",
            "-p",
            "api",
            "-w",
            "fix/timeout",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Session {
                subcommand: SessionCmd::Restart {
                    session,
                    json: true,
                    scope,
                }
            }) if session == "worker"
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
        assert!(Args::try_parse_from(["wsx", "session", "restart"]).is_err());
        assert!(Args::try_parse_from(["wsx", "session", "restart", "worker", "--nope"]).is_err());
    }

    #[test]
    fn created_identity_is_labeled_as_a_session() {
        assert_eq!(created_session_line(SessionId(42)), "session:  42");
    }

    #[test]
    fn delete_and_prompt_accept_the_same_target_scope() {
        for argv in [
            vec![
                "wsx",
                "session",
                "delete",
                "worker",
                "-p",
                "api",
                "-w",
                "fix/timeout",
                "--json",
            ],
            vec![
                "wsx",
                "session",
                "prompt",
                "worker",
                "Run tests",
                "-p",
                "api",
                "-w",
                "fix/timeout",
            ],
        ] {
            let args = Args::try_parse_from(argv).unwrap();
            match args.command.unwrap() {
                Command::Session {
                    subcommand: SessionCmd::Delete { scope, .. },
                }
                | Command::Session {
                    subcommand: SessionCmd::Prompt { scope, .. },
                } => {
                    assert_eq!(scope.project.as_deref(), Some("api"));
                    assert_eq!(scope.worktree.as_deref(), Some("fix/timeout"));
                }
                _ => panic!("expected scoped session command"),
            }
        }
    }
}

#[cfg(test)]
mod agent_command_tests {
    use super::{AgentCmd, Args, Command};
    use clap::Parser;
    use wsx_core::integration::IntegrationTarget;

    #[test]
    fn supported_integration_has_an_explicit_install_command() {
        let args = Args::try_parse_from(["wsx", "agent", "install", "pi"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Install {
                    integration: IntegrationTarget::Pi
                }
            })
        ));
        assert!(Args::try_parse_from(["wsx", "agent", "install", "unsupported"]).is_err());
    }

    #[test]
    fn exchange_commands_parse_exact_targets_bounds_and_writer_claims() {
        let request = Args::try_parse_from([
            "wsx",
            "agent",
            "request",
            "reviewer",
            "inspect lifecycle",
            "-p",
            "api",
            "-w",
            "fix/timeout",
            "--timeout",
            "300",
            "--writer",
            "--write-claim",
            "/repo/src",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            request.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Request {
                    session,
                    prompt,
                    timeout: 300,
                    writer: true,
                    write_claims,
                    stash_draft: false,
                    key: None,
                    json: true,
                    scope,
                }
            }) if session == "reviewer"
                && prompt.prompt.as_deref() == Some("inspect lifecycle")
                && write_claims == [std::path::PathBuf::from("/repo/src")]
                && scope.project.as_deref() == Some("api")
                && scope.worktree.as_deref() == Some("fix/timeout")
        ));
        assert!(Args::try_parse_from([
            "wsx",
            "agent",
            "request",
            "reviewer",
            "inspect",
            "--write-claim",
            "/repo/src"
        ])
        .is_err());

        for argv in [
            vec!["wsx", "agent", "inspect", "17", "--frame", "--json"],
            vec!["wsx", "agent", "wait", "17", "--timeout", "30", "--frame"],
            vec![
                "wsx",
                "agent",
                "continue",
                "17",
                "challenge evidence",
                "--timeout",
                "90",
            ],
            vec!["wsx", "agent", "cancel", "17", "--json"],
            vec!["wsx", "agent", "exchanges", "reviewer", "-p", "api"],
        ] {
            assert!(Args::try_parse_from(argv).is_ok());
        }
    }

    #[test]
    fn report_accepts_escape_interrupt_capability() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "claude",
            "--state",
            "working",
            "--escape-interrupts",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    escape_interrupts: true,
                    ..
                }
            })
        ));
    }

    #[test]
    fn claude_wake_commands_accept_prompt_bound_tokens() {
        let report = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "claude",
            "--state",
            "working",
            "--wake-token",
            "prompt-123",
        ])
        .unwrap();
        assert!(matches!(
            report.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    wake_token: Some(token),
                    ..
                }
            }) if token == "prompt-123"
        ));

        let heartbeat = Args::try_parse_from([
            "wsx",
            "agent",
            "wake-heartbeat",
            "7",
            "--prompt-id",
            "prompt-123",
        ])
        .unwrap();
        assert!(matches!(
            heartbeat.command,
            Some(Command::Agent {
                subcommand: AgentCmd::WakeHeartbeat { pane, prompt_id }
            }) if pane == "7" && prompt_id == "prompt-123"
        ));
    }

    #[test]
    fn detach_targets_only_the_current_managed_pane() {
        let args = Args::try_parse_from(["wsx", "agent", "detach"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Detach
            })
        ));
        assert!(Args::try_parse_from(["wsx", "agent", "detach", "7"]).is_err());
    }

    #[test]
    fn report_accepts_explicit_detach_and_defaults_to_attached() {
        for (extra, expected) in [(&[][..], false), (&["--detached"][..], true)] {
            let argv = [
                "wsx",
                "agent",
                "report",
                "7",
                "--provider",
                "pi",
                "--state",
                "idle",
            ]
            .into_iter()
            .chain(extra.iter().copied())
            .collect::<Vec<_>>();
            let args = Args::try_parse_from(argv).unwrap();
            assert!(matches!(
                args.command,
                Some(Command::Agent {
                    subcommand: AgentCmd::Report { detached, .. }
                }) if detached == expected
            ));
        }
    }

    #[test]
    fn report_accepts_session_id_without_other_session_identifiers() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--session-id",
            "abc",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    session_id,
                    session_path,
                    conversation_id,
                    ..
                }
            }) if session_id == Some("abc".into())
                && session_path.is_none()
                && conversation_id.is_none()
        ));
    }

    #[test]
    fn report_accepts_session_path_without_other_session_identifiers() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--session-path",
            "/absolute/session.jsonl",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    session_id,
                    session_path,
                    conversation_id,
                    ..
                }
            }) if session_path == Some("/absolute/session.jsonl".into())
                && session_id.is_none()
                && conversation_id.is_none()
        ));
    }

    #[test]
    fn report_keeps_legacy_conversation_id() {
        let args = Args::try_parse_from([
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
            "--conversation-id",
            "legacy",
        ])
        .unwrap();

        assert!(matches!(
            args.command,
            Some(Command::Agent {
                subcommand: AgentCmd::Report {
                    conversation_id,
                    session_id,
                    session_path,
                    ..
                }
            }) if conversation_id == Some("legacy".into())
                && session_id.is_none()
                && session_path.is_none()
        ));
    }

    #[test]
    fn report_rejects_conflicting_session_identifiers() {
        let base = [
            "wsx",
            "agent",
            "report",
            "7",
            "--provider",
            "pi",
            "--state",
            "idle",
        ];

        for extra in [
            ["--conversation-id", "legacy", "--session-id", "abc"],
            [
                "--conversation-id",
                "legacy",
                "--session-path",
                "/absolute/session.jsonl",
            ],
            [
                "--session-id",
                "abc",
                "--session-path",
                "/absolute/session.jsonl",
            ],
        ] {
            let argv = base.into_iter().chain(extra).collect::<Vec<_>>();
            assert!(Args::try_parse_from(argv).is_err());
        }
    }
}

#[cfg(test)]
mod daemon_command_tests {
    use super::{
        incompatible_runtime_status_json, runtime_lifecycle_lines, runtime_status_state, Args,
        Command, DaemonCmd,
    };
    use clap::Parser;
    use wsx_core::runtime::{self, DaemonLifecycle, DaemonPhase};

    #[test]
    fn daemon_stop_is_a_top_level_command() {
        let args = Args::try_parse_from(["wsx", "daemon", "stop"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Daemon {
                subcommand: DaemonCmd::Stop {
                    force: false,
                    json: false
                }
            })
        ));
    }

    #[test]
    fn daemon_recover_is_an_explicit_crash_budget_override() {
        let args = Args::try_parse_from(["wsx", "daemon", "recover"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Daemon {
                subcommand: DaemonCmd::Recover
            })
        ));
    }

    fn lifecycle(phase: DaemonPhase, daemon_revision: u32) -> DaemonLifecycle {
        DaemonLifecycle {
            protocol: runtime::PROTOCOL_VERSION,
            epoch: 1,
            binary_id: "binary".into(),
            version: runtime::WSX_VERSION.into(),
            daemon_revision,
            started_unix_ms: 1,
            phase,
            live_runtimes: 0,
            active_clients: 1,
            active_tuis: 0,
            recovered_from_backup: false,
            replacement_target: None,
            replacement_target_version: String::new(),
            replacement_blockers: Vec::new(),
            last_handoff_error: None,
        }
    }

    #[test]
    fn runtime_status_distinguishes_ready_upgrade_and_deferred_states() {
        assert_eq!(runtime_status_state(None), "ready");
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::Ready,
                runtime::DAEMON_REVISION
            ))),
            "ready"
        );
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::Ready,
                runtime::DAEMON_REVISION - 1
            ))),
            "upgrade_available"
        );
        assert_eq!(
            runtime_status_state(Some(&lifecycle(
                DaemonPhase::ReplacementPending,
                runtime::DAEMON_REVISION - 1
            ))),
            "replacement_deferred"
        );
    }

    #[test]
    fn incompatible_runtime_status_exposes_legacy_lifecycle_details() {
        let mut status = lifecycle(
            DaemonPhase::ReplacementPending,
            runtime::DAEMON_REVISION - 1,
        );
        status.live_runtimes = 3;
        status.active_clients = 2;
        status.active_tuis = 1;
        status.replacement_target = Some("0.28.1:1:2:3:4".into());
        status.replacement_target_version = "0.28.1".into();
        status.replacement_blockers = vec![runtime::ReplacementBlocker::WorkingAgent];

        let json = incompatible_runtime_status_json(
            std::path::Path::new("/test/wsx.sock"),
            Some(15),
            Some(&status),
            None,
        );
        assert_eq!(json["state"], "incompatible");
        assert_eq!(json["daemon_protocol"], 15);
        assert_eq!(json["lifecycle"]["phase"], "replacement_pending");
        assert_eq!(json["lifecycle"]["live_runtimes"], 3);
        assert_eq!(json["lifecycle"]["active_clients"], 2);
        assert_eq!(json["lifecycle"]["active_tuis"], 1);
        assert_eq!(json["lifecycle"]["replacement_target"], "0.28.1:1:2:3:4");
        assert_eq!(
            json["lifecycle"]["replacement_blockers"],
            serde_json::json!(["working_agent"])
        );

        let lines = runtime_lifecycle_lines(&status);
        assert!(lines.iter().any(|line| line.contains("3 live runtimes")));
        assert!(lines.iter().any(|line| line.contains("2 clients, 1 TUIs")));
        assert!(lines
            .iter()
            .any(|line| line == "Target: 0.28.1:1:2:3:4 (version 0.28.1)"));
        assert!(lines.iter().any(|line| line == "Blockers: WorkingAgent"));
    }
}

/// Resolve projects by project name or one active group. `-p` takes precedence over `--group`.
// ^ [[wsx Architecture]] Workspace filtering has one optional group identity.
fn resolve_projects<'a>(
    config: &GlobalConfig,
    workspace: &'a WorkspaceState,
    project_name: Option<&str>,
    group: Option<&GroupKey>,
) -> Result<Vec<&'a Project>> {
    if let Some(name) = project_name {
        return Ok(vec![resolve_project(workspace, Some(name))?]);
    }
    if let Some(GroupKey::Named(name)) = group {
        if !config.named_group_exists(name) {
            bail!("group '{}' not found", name);
        }
    }
    Ok(workspace
        .projects
        .iter()
        .filter(|project| project_matches_group(config.project_groups(&project.path), group))
        .collect())
}

pub(crate) fn resolve_project<'a>(
    workspace: &'a WorkspaceState,
    name: Option<&str>,
) -> Result<&'a Project> {
    match name {
        Some(n) => workspace
            .projects
            .iter()
            .find(|p| p.name == n)
            .ok_or_else(|| anyhow::anyhow!("project '{}' not found", n)),
        None => match workspace.projects.len() {
            0 => bail!("no projects registered"),
            1 => Ok(&workspace.projects[0]),
            _ => bail!(
                "multiple projects — use -p to specify: {}",
                workspace
                    .projects
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    }
}

fn cwd_worktree<'a>(
    projects: impl Iterator<Item = &'a Project>,
    cwd: Option<&Path>,
) -> Option<(&'a Project, &'a wsx_core::model::workspace::WorktreeInfo)> {
    let cwd = cwd?;
    projects
        .flat_map(|project| {
            project
                .worktrees
                .iter()
                .map(move |worktree| (project, worktree))
        })
        .filter(|(_, worktree)| cwd == worktree.path || cwd.starts_with(&worktree.path))
        .max_by_key(|(_, worktree)| worktree.path.components().count())
}

fn resolve_creation_worktree<'a>(
    workspace: &'a WorkspaceState,
    scope: &SessionScope,
    cwd: Option<&Path>,
) -> Result<(&'a Project, &'a wsx_core::model::workspace::WorktreeInfo)> {
    let project = if let Some(name) = scope.project.as_deref() {
        resolve_project(workspace, Some(name))?
    } else if let Some((project, _)) = cwd_worktree(workspace.projects.iter(), cwd) {
        project
    } else {
        resolve_project(workspace, None)?
    };

    if let Some(selector) = scope.worktree.as_deref() {
        let matches = project
            .worktrees
            .iter()
            .filter(|worktree| {
                worktree.branch == selector
                    || worktree.alias.as_deref() == Some(selector)
                    || worktree.path == Path::new(selector)
            })
            .collect::<Vec<_>>();
        return match matches.as_slice() {
            [worktree] => Ok((project, *worktree)),
            [] => bail!(
                "worktree '{}' not found in project '{}'",
                selector,
                project.name
            ),
            _ => bail!(
                "worktree '{}' is ambiguous in project '{}'",
                selector,
                project.name
            ),
        };
    }

    if let Some((_, worktree)) = cwd_worktree(std::iter::once(project), cwd) {
        return Ok((project, worktree));
    }
    match project.worktrees.as_slice() {
        [worktree] => Ok((project, worktree)),
        [] => bail!("project '{}' has no worktrees", project.name),
        _ => bail!(
            "multiple worktrees in project '{}' — use -w to specify: {}",
            project.name,
            project
                .worktrees
                .iter()
                .map(|worktree| worktree.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn activity_label(s: &wsx_core::model::workspace::SessionInfo) -> &'static str {
    session_state::status_label(s)
}

#[cfg(test)]
mod routine_client_wiring_tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn legacy_embedded_daemon_command_is_removed() {
        assert!(Args::try_parse_from(["wsx", "routine", "daemon", "start"]).is_err());
        assert!(Args::try_parse_from(["wsx", "routine-daemon-serve"]).is_err());
    }
}

fn git_label(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.git_info
        .as_ref()
        .map(|g| format!("+{}-{}", g.ahead, g.behind))
        .unwrap_or_else(|| "-".to_string())
}

fn sessions_inline(wt: &wsx_core::model::workspace::WorktreeInfo) -> String {
    wt.sessions
        .iter()
        .map(|s| format!("{}[{}]", s.pane_id, activity_label(s)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < ncols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }
    let mut line = String::new();
    for (i, h) in headers.iter().enumerate() {
        if i + 1 < ncols {
            line.push_str(&format!("{:<width$}", h, width = widths[i] + 2));
        } else {
            line.push_str(h);
        }
    }
    println!("{}", line);
    for row in rows {
        line.clear();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < ncols {
                line.push_str(&format!("{:<width$}", cell, width = widths[i] + 2));
            } else {
                line.push_str(cell);
            }
        }
        println!("{}", line);
    }
}

// --- Command implementations ---

fn cmd_status(json: bool, format: Format, group: Option<&GroupKey>) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, None, group)?;
    if json {
        // ^ Status JSON is a top-level project array; runtime status uses an object.
        println!(
            "{}",
            serde_json::to_string_pretty(&manager_status(&projects)?)?
        );
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "git", "sessions"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees.iter().map(move |wt| {
                    vec![
                        p.name.clone(),
                        wt.branch.clone(),
                        git_label(wt),
                        sessions_inline(wt),
                    ]
                })
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    let show_groups = !config.groups.is_empty();
    for project in &projects {
        if show_groups {
            let memberships = config.project_groups(&project.path);
            let label = if memberships.is_empty() {
                "ungrouped".to_string()
            } else {
                memberships.join(",")
            };
            println!("[{}] {} {}", label, project.name, project.path.display());
        } else {
            println!("{} {}", project.name, project.path.display());
        }
        for wt in &project.worktrees {
            let git = wt
                .git_info
                .as_ref()
                .map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind))
                .unwrap_or_default();
            println!("  {}{} ({} sessions)", wt.branch, git, wt.sessions.len());
            for s in &wt.sessions {
                println!("    {:<40} {}", s.pane_id, activity_label(s));
            }
        }
    }
    Ok(())
}

/// The status projects with what a manager needs for its next step, as added keys:
/// `work` on a worktree and `exchange` on a session.
/// ^ docs/agent-orchestration.md "One status call": one exchange per session and counts
/// for a worktree keep the output bounded.
fn manager_status(projects: &[&Project]) -> Result<serde_json::Value> {
    let exchanges = list_agent_exchanges(None, RuntimeSessionScope::default())?;
    let mut output = serde_json::to_value(projects)?;
    for (project, project_row) in projects
        .iter()
        .zip(output.as_array_mut().into_iter().flatten())
    {
        // Git runs once per worktree; a few at a time keeps a large workspace quick.
        let mut work = Vec::new();
        for worktrees in project.worktrees.chunks(8) {
            std::thread::scope(|scope| {
                let jobs = worktrees
                    .iter()
                    .map(|worktree| {
                        scope.spawn(|| {
                            wsx_core::git::info::work_summary(
                                &worktree.path,
                                &project.default_branch,
                            )
                        })
                    })
                    .collect::<Vec<_>>();
                work.extend(jobs.into_iter().map(|job| job.join().ok()));
            });
        }
        let worktree_rows = project_row["worktrees"]
            .as_array_mut()
            .into_iter()
            .flatten();
        for ((worktree, worktree_row), work) in
            project.worktrees.iter().zip(worktree_rows).zip(work)
        {
            worktree_row["work"] = serde_json::to_value(work)?;
            let session_rows = worktree_row["sessions"]
                .as_array_mut()
                .into_iter()
                .flatten();
            for (session, session_row) in worktree.sessions.iter().zip(session_rows) {
                // The active exchange of the session, or else the one that changed last.
                let exchange = exchanges
                    .iter()
                    .filter(|exchange| exchange.session_id == session.session_id)
                    .max_by_key(|exchange| (!exchange.is_terminal(), exchange.updated_unix_ms));
                session_row["exchange"] = serde_json::json!(exchange.map(|exchange| {
                    serde_json::json!({
                        "id": exchange.id,
                        "pane_id": exchange.pane_id,
                        "state": exchange.state,
                        "evidence": exchange.evidence,
                        "round": exchange.round,
                        "access": exchange.access,
                        "key": exchange.key,
                        "updated_unix_ms": exchange.updated_unix_ms,
                        "deadline_unix_ms": exchange.deadline_unix_ms,
                    })
                }));
            }
        }
    }
    Ok(output)
}

fn cmd_worktree_create(
    branch: &str,
    project_name: Option<&str>,
    from: Option<&str>,
    json: bool,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    refuse_start_point_of_existing_branch(project, branch, from)?;
    let proj_config = project.config.clone().unwrap_or_default();
    let created = ops::create_configured_worktree(
        &config,
        &project.name,
        &project.path,
        from.unwrap_or(&project.default_branch),
        &proj_config,
        branch,
        wsx_core::model::workspace::WorktreeInitialSession::Shell,
    )?;
    if let Some(warning) = created.warning {
        eprintln!("warning: {warning}");
    }
    let session_id = created.session.as_ref().map(|(session_id, _)| *session_id);
    if json {
        // ^ A lookup failure must not hide the worktree that already exists.
        let pane_id = session_id.and_then(|session_id| created_session_pane(session_id).0);
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "worktree": created.path,
                "branch": branch,
                "project": project.name,
                "session_id": session_id,
                "pane_id": pane_id,
            }))?
        );
    } else {
        println!("worktree: {}", created.path.display());
        if let Some(session_id) = session_id {
            println!("{}", created_session_line(session_id));
        }
    }
    if let Some(error) = created.session_error {
        anyhow::bail!("worktree created, but its initial wsx session failed: {error}");
    }
    Ok(())
}

// ^ An existing branch is checked out as it is, so a start point cannot apply.
fn refuse_start_point_of_existing_branch(
    project: &Project,
    branch: &str,
    from: Option<&str>,
) -> Result<()> {
    if from.is_some() && wsx_core::git::worktree::branch_exists(&project.path, branch) {
        bail!("branch '{branch}' already exists; omit --from to check it out");
    }
    Ok(())
}

fn created_session_line(session_id: runtime::SessionId) -> String {
    format!("session:  {session_id}")
}

/// The pane of a session that was just created, and its agent when one already reported.
/// ^ Best effort: the session exists even when this second read fails.
fn created_session_pane(
    session_id: runtime::SessionId,
) -> (Option<runtime::PaneId>, Option<String>) {
    let Ok(snapshot) = ops::runtime_snapshot() else {
        return (None, None);
    };
    let pane_id = snapshot
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .map(|session| session.focused_pane);
    let agent = snapshot
        .panes
        .iter()
        .find(|pane| Some(pane.id) == pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| agent.attached)
        .map(|agent| agent.provider.clone());
    (pane_id, agent)
}

fn cmd_worktree_delete(branch: &str, project_name: Option<&str>) -> Result<()> {
    let (_, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, project_name)?;
    let wt = project
        .worktrees
        .iter()
        .find(|w| w.branch == branch || w.alias.as_deref() == Some(branch))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "worktree for branch '{}' not found in project '{}'",
                branch,
                project.name
            )
        })?;
    let branch_deleted = ops::delete_worktree(&project.path, &wt.path, &wt.branch)?;
    println!("deleted worktree: {}", wt.path.display());
    if !branch_deleted {
        println!(
            "kept branch {}: it has commits that are not merged",
            wt.branch
        );
    }
    Ok(())
}

fn cmd_worktree_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let worktrees: Vec<_> = projects.iter().flat_map(|p| p.worktrees.iter()).collect();
        println!("{}", serde_json::to_string_pretty(&worktrees)?);
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "git", "path"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees.iter().map(move |wt| {
                    vec![
                        p.name.clone(),
                        wt.branch.clone(),
                        git_label(wt),
                        wt.path.display().to_string(),
                    ]
                })
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            let git = wt
                .git_info
                .as_ref()
                .map(|g| format!(" [{}↑ {}↓]", g.ahead, g.behind))
                .unwrap_or_default();
            println!("{}/{}{} — {}", p.name, wt.branch, git, wt.path.display());
        }
    }
    Ok(())
}

fn parse_integration_target(
    value: &str,
) -> std::result::Result<wsx_core::integration::IntegrationTarget, String> {
    value.parse()
}

struct ContextOptions {
    limit: u32,
    messages: u32,
    bytes: u32,
    metadata_only: bool,
    json: bool,
}

// ^ Observation only: read the existing daemon and never start or replace wsxd.
pub(crate) fn existing_daemon_snapshot() -> Result<runtime::Snapshot> {
    let client = runtime::Client::new(runtime::default_socket_path());
    match client.call(&runtime::Request::Snapshot)? {
        runtime::Response::Snapshot(snapshot) => Ok(snapshot),
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("unexpected daemon snapshot"),
    }
}

fn cmd_agent_context(
    selector: Option<&str>,
    provider: Option<&str>,
    options: ContextOptions,
    requested: &SessionScope,
) -> Result<()> {
    // ^ One existing-daemon snapshot; discovery must not start or replace wsxd.
    // Native history stays local and is not exposed through the browser bridge.
    let snapshot = existing_daemon_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    let target = selector
        .map(|selector| resolve_session_in_snapshot(&snapshot, selector, scope))
        .transpose()?;
    let caller = std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .and_then(|id| id.parse::<runtime::PaneId>().ok());
    // ^ Metadata discovery must not inventory or open provider history stores.
    let mut reader = (!options.metadata_only).then(wsx_core::integration::memory::Reader::default);
    let mut candidates = Vec::new();
    let mut matched = 0;
    let mut remaining = 64 * 1024;
    for pane in &snapshot.panes {
        let Some(agent) = pane.agent.as_ref() else {
            continue;
        };
        let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == pane.session_id)
        else {
            continue;
        };
        if !session_matches_scope(&snapshot, session, scope)
            || target
                .as_ref()
                .is_some_and(|target| target.pane_id != pane.id)
            || provider.is_some_and(|provider| provider != agent.provider)
        {
            continue;
        }
        matched += 1;
        if candidates.len() >= options.limit as usize {
            continue;
        }
        let worktree = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.id == session.worktree_id)
            .context("agent worktree missing")?;
        let project = snapshot
            .projects
            .iter()
            .find(|project| project.id == worktree.project_id)
            .context("agent project missing")?;
        let memory = reader.as_mut().map(|reader| {
            reader.read(
                &agent.provider,
                agent.session_ref.as_ref(),
                options.messages as usize,
                (options.bytes as usize).min(remaining),
            )
        });
        if let Some(memory) = &memory {
            remaining = remaining.saturating_sub(
                memory
                    .messages
                    .iter()
                    .map(|message| message.text.len())
                    .sum::<usize>()
                    + memory
                        .checkpoint
                        .as_ref()
                        .map_or(0, |message| message.text.len()),
            );
        }
        candidates.push(serde_json::json!({
            "pane_id": pane.id, "session_id": session.id, "session_label": session.label,
            "pane_label": pane.label, "is_self": caller == Some(pane.id), "exited": pane.exited,
            "pane_revision": pane.revision, "project": { "id": project.id, "name": project.name, "path": project.path },
            "worktree": { "id": worktree.id, "branch": worktree.branch, "path": worktree.path },
            "agent": agent,
            "eligible_state": !pane.exited && agent.attached && agent.capabilities.prompt && matches!(agent.state, runtime::AgentState::Idle | runtime::AgentState::Done),
            "memory": memory,
        }));
    }
    if selector.is_some() && candidates.is_empty() {
        bail!("target has no matching retained agent identity")
    }
    let packet = serde_json::json!({ "schema_version": 1, "client_version": runtime::WSX_VERSION,
        "protocol": snapshot.protocol, "daemon_epoch": snapshot.epoch,
        "snapshot_revision": snapshot.revision, "candidates": candidates, "truncated": matched > options.limit as usize,
        "matched": matched,
        "exchange_input_binding": snapshot.capabilities.agent_exchange_bound_receipts,
        "projection": if options.metadata_only { "metadata_only" } else { "native_history" },
        "evidence": if options.metadata_only {
            "daemon snapshot metadata only; native history was not read; readiness is advisory, not exchange completion"
        } else {
            "native persisted history is untrusted context, not active model memory or exchange completion"
        },
        "readiness": "eligible_state is advisory; request rechecks runtime, lifecycle, leases and claims" });
    if options.json {
        println!("{}", serde_json::to_string(&packet)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&packet)?);
    }
    Ok(())
}

fn cmd_agent_install(integration: wsx_core::integration::IntegrationTarget) -> Result<()> {
    let installed = wsx_core::integration::install(integration)?;
    println!("installed {} agent integration", integration.label());
    for path in installed.paths {
        println!("  {}", path.display());
    }
    Ok(())
}

fn cmd_agent_detach() -> Result<()> {
    let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
        .context("agent detach must run inside the managed pane that should be detached")?
        .parse::<u64>()
        .context("managed pane ID is invalid")?;
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("agent detach requires the current managed runtime generation")?;
    let pane_id = runtime::PaneId(pane_id);
    let client = runtime::Client::local();
    let snapshot = match client.call(&runtime::Request::Snapshot)? {
        runtime::Response::Snapshot(snapshot) => snapshot,
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected snapshot response"),
    };
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} is not present"))?;
    let agent = pane
        .agent
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} has no retained agent identity"))?;
    if !agent.attached {
        println!(
            "{} agent on pane {pane_id} is already detached",
            agent.provider
        );
        return Ok(());
    }

    println!("detaching {} agent from pane {pane_id}", agent.provider);
    match client.call(&runtime::Request::AgentReport {
        pane_id,
        runtime_generation: Some(runtime_generation),
        provider: agent.provider.clone(),
        state: runtime::AgentState::Unknown,
        attached: false,
        presence_id: None,
        conversation_id: None,
        session_ref: None,
        wake_token: None,
        capabilities: runtime::AgentCapabilities::default(),
        received: None,
        compacting: false,
    })? {
        runtime::Response::Ack { revision } => {
            println!("detached agent at revision {revision}");
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent response"),
    }
}

/// `--received`: `<exchange>:<round>` of a wsx round, or `other`.
fn parse_prompt_received(value: &str) -> Result<runtime::PromptReceived, String> {
    if value == "other" {
        return Ok(runtime::PromptReceived::Other);
    }
    value
        .split_once(':')
        .and_then(|(exchange, round)| {
            Some(runtime::PromptReceived::Round {
                exchange: runtime::AgentExchangeId(exchange.parse().ok()?),
                round: round.parse().ok()?,
            })
        })
        .ok_or_else(|| format!("{value} is not <exchange>:<round> or other"))
}

struct AgentReportOptions {
    provider: String,
    state: runtime::AgentState,
    attached: bool,
    presence_id: Option<String>,
    conversation_id: Option<String>,
    session_id: Option<String>,
    session_path: Option<String>,
    transcript_path: Option<String>,
    wake_token: Option<String>,
    capabilities: runtime::AgentCapabilities,
    received: Option<runtime::PromptReceived>,
    compacting: bool,
}

fn cmd_agent_report(selector: &str, report: AgentReportOptions) -> Result<()> {
    let AgentReportOptions {
        provider,
        state,
        attached,
        presence_id,
        conversation_id,
        session_id,
        session_path,
        transcript_path,
        wake_token,
        capabilities,
        received,
        compacting,
    } = report;
    // ^ Adapter callbacks already have the exact pane ID. A snapshot lookup can
    // start or reconcile wsxd before SessionStart reaches its current generation.
    let pane_id = if std::env::var(runtime::WSX_PANE_ID_ENV).ok().as_deref() == Some(selector) {
        selector.parse::<runtime::PaneId>()?
    } else {
        resolve_pane(selector, &SessionScope::default())?
    };
    let session_ref = if let Some(value) = session_path {
        Some(
            runtime::AgentSessionRef::path(value)
                .ok_or_else(|| anyhow::anyhow!("invalid absolute agent session path"))?,
        )
    } else if let Some(value) = session_id {
        Some(
            runtime::AgentSessionRef::id(value)
                .ok_or_else(|| anyhow::anyhow!("invalid agent session ID"))?,
        )
    } else {
        None
    };
    let session_ref = match (session_ref, transcript_path) {
        (Some(reference), Some(path)) => Some(
            reference
                .with_transcript_path(path)
                .context("invalid absolute transcript path")?,
        ),
        (reference, None) => reference,
        (None, Some(_)) => bail!("transcript path requires an agent session identity"),
    };
    let conversation_id = conversation_id.or_else(|| {
        session_ref
            .as_ref()
            .map(|session_ref| session_ref.value.clone())
    });
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV).ok();
    // ^ A lifecycle callback observes the owning daemon; it must not bootstrap
    // a replacement while reporting the old runtime's generation.
    match runtime::Client::new(runtime::default_socket_path()).call(
        &runtime::Request::AgentReport {
            pane_id,
            runtime_generation,
            provider,
            state,
            attached,
            presence_id,
            conversation_id,
            session_ref,
            wake_token,
            capabilities,
            received,
            compacting,
        },
    )? {
        runtime::Response::Ack { revision } => {
            println!("agent report accepted at revision {revision}");
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent response"),
    }
}

// ^ crates/wsx-daemon/src/lib.rs validates the live generation and attachment
// before renewing. Do not resolve a session label or read a snapshot on this path.
fn cmd_agent_presence_renew(selector: &str, presence_id: &str) -> Result<()> {
    let pane = std::env::var(runtime::WSX_PANE_ID_ENV)
        .context("presence renewal requires a managed pane")?;
    if selector != pane {
        bail!("presence renewal must target its own managed pane");
    }
    let pane_id = runtime::PaneId(pane.parse().context("invalid managed pane ID")?);
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("presence renewal requires a managed runtime generation")?;
    match runtime::Client::local().call(&runtime::Request::AgentPresenceRenew {
        pane_id,
        runtime_generation,
        presence_id: presence_id.into(),
    })? {
        runtime::Response::Ack { .. } => Ok(()),
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected presence response"),
    }
}

// ^ docs/agent-reporting.md "Wake heartbeat": Claude starts the heartbeat from a detached
// hook shell and never ends it, so each renewal is also the check that its prompt, pane
// and wsxd still exist. A renewal is an in-memory lease update in wsxd.
const AGENT_WAKE_RENEW_INTERVAL: Duration = Duration::from_secs(5);
/// A heartbeat that cannot get an answer from wsxd for this long ends.
const AGENT_WAKE_UNANSWERED_LIMIT: Duration = Duration::from_secs(5 * 60);
const AGENT_WAKE_START_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const AGENT_WAKE_START_RETRIES: usize = 50;

fn cmd_agent_wake_heartbeat(selector: &str, prompt_id: &str) -> Result<()> {
    // ^ As `agent report`: the hook has the exact pane ID, and nothing here starts wsxd.
    let pane_id = if std::env::var(runtime::WSX_PANE_ID_ENV).ok().as_deref() == Some(selector) {
        selector.parse::<runtime::PaneId>()?
    } else {
        resolve_pane(selector, &SessionScope::default())?
    };
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .context("wake heartbeat requires a managed wsx runtime generation")?;
    let client = runtime::Client::new(runtime::default_socket_path());
    let renew = || {
        client.call(&runtime::Request::AgentWakeRenew {
            pane_id,
            runtime_generation: runtime_generation.clone(),
            wake_token: prompt_id.to_string(),
        })
    };

    let mut started = false;
    for _ in 0..AGENT_WAKE_START_RETRIES {
        match renew() {
            Ok(runtime::Response::Ack { .. }) => {
                started = true;
                break;
            }
            Ok(runtime::Response::Error(error)) if error.code == "wake_not_active" => {
                std::thread::sleep(AGENT_WAKE_START_RETRY_INTERVAL);
            }
            _ => return Ok(()),
        }
    }
    if !started {
        return Ok(());
    }

    let mut answered = Instant::now();
    loop {
        std::thread::sleep(AGENT_WAKE_RENEW_INTERVAL);
        match renew() {
            Ok(runtime::Response::Ack { .. }) => answered = Instant::now(),
            // The prompt ended or was replaced, or the pane is gone.
            Ok(_) => return Ok(()),
            // No wsxd: `call` has already waited for a live handoff to finish.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(())
            }
            Err(_) if answered.elapsed() < AGENT_WAKE_UNANSWERED_LIMIT => {}
            Err(_) => return Ok(()),
        }
    }
}

fn exchange_timeout_ms(seconds: u64) -> Result<u64> {
    seconds
        .checked_mul(1000)
        .filter(|value| *value > 0 && *value <= runtime::MAX_AGENT_EXCHANGE_TIMEOUT_MS)
        .context("exchange timeout must be between 1 second and 24 hours")
}

fn terminal_frame_text(frame: &runtime::TerminalFrame) -> String {
    frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

fn print_agent_exchange(
    exchange: &runtime::AgentExchange,
    frame: Option<&runtime::TerminalFrame>,
    json: bool,
) -> Result<()> {
    print_agent_exchange_with(exchange, frame, None, None, json)
}

/// The watched exchanges of a `wait --any`, by whether they are at a wait boundary.
struct WatchedExchanges {
    ready: Vec<runtime::AgentExchangeId>,
    pending: Vec<runtime::AgentExchangeId>,
}

/// `result` is the value of `exchange_result`. A key is absent when it was not asked for.
fn print_agent_exchange_with(
    exchange: &runtime::AgentExchange,
    frame: Option<&runtime::TerminalFrame>,
    result: Option<serde_json::Value>,
    watched: Option<&WatchedExchanges>,
    json: bool,
) -> Result<()> {
    if json {
        let mut output = serde_json::json!({
            "exchange": exchange,
            "fallback": frame.map(|frame| serde_json::json!({
                "kind": "terminal_frame",
                "text": terminal_frame_text(frame),
                "frame_revision": frame.revision,
            })),
        });
        if let Some(result) = result {
            output["result"] = result;
        }
        if let Some(watched) = watched {
            output["ready"] = serde_json::json!(watched.ready);
            output["pending"] = serde_json::json!(watched.pending);
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "exchange {} round {}: {:?}",
            exchange.id, exchange.round, exchange.state
        );
        println!("target: pane {} ({})", exchange.pane_id, exchange.provider);
        println!("evidence: {:?}", exchange.evidence);
        println!("revision: {}", exchange.revision);
        if let Some(frame) = frame {
            let text = terminal_frame_text(frame);
            if !text.is_empty() {
                println!("terminal frame (fallback):\n{text}");
            }
        }
        if let Some(result) = result {
            match result["text"].as_str() {
                Some(text) => println!("result (round {}):\n{text}", exchange.round),
                None => println!(
                    "result unavailable: {}",
                    result["status"].as_str().unwrap_or_default()
                ),
            }
        }
        if let Some(watched) = watched {
            println!("ready: {}", exchange_ids(&watched.ready));
            println!("pending: {}", exchange_ids(&watched.pending));
        }
    }
    Ok(())
}

fn exchange_ids(ids: &[runtime::AgentExchangeId]) -> String {
    if ids.is_empty() {
        return "none".into();
    }
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The agent's final message of the exchange's current round, or the reason none can be named.
/// ^ docs/agent-orchestration.md "Result of a round": the round is found in the provider's
/// saved history by the envelope line that wsxd delivered. Text from before that line is
/// never a result.
fn exchange_result(exchange: &runtime::AgentExchange, bytes: usize) -> serde_json::Value {
    let unavailable = |status: &str| serde_json::json!({ "status": status, "round": exchange.round, "text": null });
    if !matches!(
        exchange.state,
        runtime::AgentExchangeState::DoneObserved | runtime::AgentExchangeState::Completed
    ) {
        return unavailable("round_not_completed");
    }
    let Ok(snapshot) = ops::runtime_snapshot() else {
        return unavailable("daemon_unavailable");
    };
    let Some(agent) = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == exchange.pane_id)
        .and_then(|pane| pane.agent.as_ref())
        .filter(|agent| agent.id == exchange.agent_id)
    else {
        return unavailable("agent_replaced");
    };
    let marker = runtime::agent_exchange_envelope_start(exchange.id, exchange.round);
    // A provider may write its last record a moment after its agent reports done.
    let flushed_by = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = wsx_core::integration::memory::Reader::default().read_reply(
            &agent.provider,
            agent.session_ref.as_ref(),
            &marker,
            bytes,
        );
        let may_follow = matches!(
            reply.status,
            "delivery_not_in_history" | "no_message_after_delivery"
        );
        if !may_follow || Instant::now() >= flushed_by {
            return serde_json::json!({
                "status": reply.status,
                "round": exchange.round,
                "text": reply.text,
                "truncated": reply.truncated,
                "source": reply.source,
                "path": reply.path,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn agent_exchange_response(
    response: runtime::Response,
    json: bool,
) -> Result<runtime::AgentExchange> {
    match response {
        runtime::Response::AgentExchange { exchange, frame } => {
            print_agent_exchange(&exchange, frame.as_ref(), json)?;
            Ok(exchange)
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent exchange response"),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_agent_exchange_request(
    selector: &str,
    prompt: PromptSource,
    timeout: u64,
    writer: bool,
    write_claims: Vec<PathBuf>,
    stash_draft: bool,
    key: Option<String>,
    json: bool,
    scope: &SessionScope,
) -> Result<()> {
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let pane_id = resolve_pane(selector, scope)?;
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let access = if writer {
        runtime::AgentExchangeAccess::Writer
    } else {
        runtime::AgentExchangeAccess::ReadOnly
    };
    let request = exchange_create_request(
        pane_id,
        prompt,
        timeout_ms,
        access,
        write_claims,
        stash_draft,
        key,
    );
    agent_exchange_response(deliver_exchange(&request)?, json)?;
    Ok(())
}

/// The create request for these options. A key selects the keyed variant, which an
/// older wsxd refuses; without a key the request is the one every protocol-16 wsxd knows.
fn exchange_create_request(
    pane_id: runtime::PaneId,
    prompt: String,
    timeout_ms: u64,
    access: runtime::AgentExchangeAccess,
    write_claims: Vec<PathBuf>,
    stash_draft: bool,
    key: Option<String>,
) -> runtime::Request {
    match key {
        Some(key) => runtime::Request::AgentExchangeCreateKeyed {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
            key,
            stash_draft,
        },
        None if stash_draft => runtime::Request::AgentExchangeCreateStashingDraft {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
        },
        None => runtime::Request::AgentExchangeCreate {
            command: false,
            takeover: false,
            pane_id,
            prompt,
            timeout_ms,
            access,
            write_claims,
        },
    }
}

/// Send a create or continue request and say what state a failure left.
/// ^ docs/agent-orchestration.md "Idempotent delivery".
fn deliver_exchange(request: &runtime::Request) -> Result<runtime::Response> {
    let keyed = matches!(
        request,
        runtime::Request::AgentExchangeCreateKeyed { .. }
            | runtime::Request::AgentExchangeContinueKeyed { .. }
    );
    match runtime::Client::local().call(request) {
        // An older wsxd cannot decode a keyed request and answers with its parse error.
        Ok(runtime::Response::Error(error))
            if keyed
                && error.code == "invalid_json"
                && error.message.contains("unknown variant") =>
        {
            bail!(
                "unsupported_request: this wsxd is older than --key and delivered nothing; \
                 run `wsx daemon restart`, or repeat the command without --key"
            )
        }
        Ok(response) => Ok(response),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            bail!(
                "reply_timeout: wsxd did not answer in time, and the prompt may be delivered; {}",
                if keyed {
                    "repeat the command with the same --key to get the exchange"
                } else {
                    "run `wsx agent exchanges <session>` before you send it again"
                }
            )
        }
        Err(error) => Err(error.into()),
    }
}

/// What `agent start` made or found. A late failure prints the same object with `error`
/// and `next`, so the caller knows what exists and what to run.
#[derive(serde::Serialize)]
struct AgentStart {
    project: String,
    branch: String,
    worktree: PathBuf,
    worktree_created: bool,
    session_id: Option<runtime::SessionId>,
    session_label: String,
    session_created: bool,
    pane_id: Option<runtime::PaneId>,
    agent: Option<String>,
    /// True when this call answered the agent's folder trust question.
    folder_trust_accepted: bool,
    exchange: Option<runtime::AgentExchange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

// ^ docs/agent-orchestration.md "Start a worker in one call": every step uses what a
// former run left, so the same command with the same --key continues and delivers once.
fn cmd_agent_start(args: AgentStartArgs) -> Result<()> {
    // Refuse what can be refused before anything is made.
    let prompt = args.prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let timeout_ms = exchange_timeout_ms(args.timeout)?;
    let (config, workspace) = load_full_workspace()?;
    let project = resolve_project(&workspace, args.project.as_deref())?;
    let project_config = project.config.clone().unwrap_or_default();
    // ^ One owner of the command: `worktree.defaultSession.command`. `--agent` is the same
    // value for one call. A disabled default session gives no command.
    let configured = match project_config
        .initial_session(wsx_core::model::workspace::WorktreeInitialSession::Disabled)
    {
        wsx_core::model::workspace::WorktreeInitialSession::Command(command) => Some(command),
        _ => None,
    };
    let Some(command) = args
        .agent
        .or(configured)
        .filter(|command| !command.trim().is_empty())
    else {
        bail!(
            "no agent command: pass --agent <command>, or set worktree.defaultSession.command for project '{}'",
            project.name
        );
    };
    let label = args
        .name
        .filter(|name| !name.trim().is_empty())
        .or_else(|| command.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default();

    // The worktree. A failure here leaves nothing: see git::worktree::create_worktree.
    let existing = project
        .worktrees
        .iter()
        .find(|worktree| worktree.branch == args.branch);
    let (worktree, worktree_created) = match existing {
        Some(worktree) => (worktree.path.clone(), false),
        None => {
            refuse_start_point_of_existing_branch(project, &args.branch, args.from.as_deref())?;
            // No default session: the agent session below is the only one.
            let (path, warning) = ops::create_worktree(
                &project.path,
                args.from.as_deref().unwrap_or(&project.default_branch),
                &project_config,
                &args.branch,
            )?;
            if let Some(warning) = warning {
                eprintln!("warning: {warning}");
            }
            (path, true)
        }
    };
    let mut start = AgentStart {
        project: project.name.clone(),
        branch: args.branch.clone(),
        worktree,
        worktree_created,
        session_id: None,
        session_label: label,
        session_created: false,
        pane_id: None,
        agent: None,
        folder_trust_accepted: false,
        exchange: None,
        error: None,
        next: None,
    };

    let access = if args.writer {
        runtime::AgentExchangeAccess::Writer
    } else {
        runtime::AgentExchangeAccess::ReadOnly
    };
    let outcome = (|| -> std::result::Result<(), (&'static str, anyhow::Error)> {
        let snapshot = ops::runtime_snapshot().map_err(|error| ("session", error))?;
        let existing = snapshot
            .worktrees
            .iter()
            .find(|worktree| worktree.path == start.worktree)
            .and_then(|worktree| {
                snapshot.sessions.iter().find(|session| {
                    session.worktree_id == worktree.id && session.label == start.session_label
                })
            });
        let session_id = match existing {
            Some(session) => session.id,
            None => {
                start.session_created = true;
                ops::create_session(
                    &config,
                    &project.name,
                    "",
                    &start.worktree,
                    Some(start.session_label.clone()),
                    Some(command.clone()),
                )
                .map_err(|error| ("session", error))?
                .0
            }
        };
        start.session_id = Some(session_id);

        // A repeat finds the exchange of its key before it waits: its agent may be at work on it.
        if let (Some(key), Some(session)) = (args.key.as_deref(), existing) {
            start.pane_id = Some(session.focused_pane);
            let exchanges =
                list_agent_exchanges(Some(session.focused_pane), RuntimeSessionScope::default())
                    .map_err(|error| ("request", error))?;
            if let Some(exchange) = exchanges
                .into_iter()
                .find(|exchange| exchange.key.as_deref() == Some(key))
            {
                start.agent = Some(exchange.provider.clone());
                start.exchange = Some(exchange);
                return Ok(());
            }
        }

        let (pane_id, provider) = wait_for_prompt_agent(
            session_id,
            Instant::now() + Duration::from_secs(args.ready_timeout),
            args.accept_folder_trust,
            &mut start,
        )
        .map_err(|error| ("ready", error))?;
        start.agent = Some(provider);
        let request = exchange_create_request(
            pane_id,
            prompt,
            timeout_ms,
            access,
            args.write_claims,
            false,
            args.key,
        );
        match deliver_exchange(&request).map_err(|error| ("request", error))? {
            runtime::Response::AgentExchange { exchange, .. } => {
                start.exchange = Some(exchange);
                Ok(())
            }
            runtime::Response::Error(error) => Err((
                "request",
                anyhow::anyhow!("{}: {}", error.code, error.message),
            )),
            _ => Err((
                "request",
                anyhow::anyhow!("wsxd returned an unexpected agent exchange response"),
            )),
        }
    })();

    let failure = outcome.err();
    if let Some((step, error)) = &failure {
        let mut report = serde_json::json!({ "step": step, "message": error.to_string() });
        let not_ready = error.downcast_ref::<AgentNotReady>();
        if let Some(not_ready) = not_ready {
            report["reason"] = serde_json::json!(not_ready.reason);
            report["screen"] = serde_json::json!(not_ready.screen);
        }
        start.error = Some(report);
        start.next = Some(match (not_ready.map(|not_ready| not_ready.reason), start.session_id) {
            (Some(FOLDER_TRUST_QUESTION), Some(session_id)) => format!(
                "session {session_id} waits on the agent's folder trust question for {}; review that folder, then repeat this command with --accept-folder-trust",
                start.worktree.display()
            ),
            (Some(_), Some(session_id)) => format!(
                "the worktree and session {session_id} are kept; read the screen in this report or `wsx session peek {session_id} --trim`, then repeat this command"
            ),
            _ => "what is listed here is kept; repeat this command".to_owned(),
        });
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&start)?);
    } else {
        println!("worktree: {}", start.worktree.display());
        if let Some(session_id) = start.session_id {
            println!("{}", created_session_line(session_id));
        }
        if let Some(pane_id) = start.pane_id {
            println!("pane:     {pane_id}");
        }
        if let Some(exchange) = &start.exchange {
            println!(
                "exchange: {} round {}: {:?}",
                exchange.id, exchange.round, exchange.state
            );
        }
        if let Some(screen) = start
            .error
            .as_ref()
            .and_then(|error| error["screen"].as_str())
        {
            println!("screen:   last lines of the pane");
            for line in screen.lines() {
                println!("  | {line}");
            }
        }
        if let Some(next) = &start.next {
            println!("next:     {next}");
        }
    }
    match failure {
        Some((step, error)) => Err(error.context(format!("agent start stopped at step '{step}'"))),
        None => Ok(()),
    }
}

const FOLDER_TRUST_QUESTION: &str = "folder_trust_question";
const NOT_READY_SCREEN_LINES: usize = 20;
const NOT_READY_SCREEN_BYTES: usize = 2048;

/// Why `agent start` stopped before its agent could take a prompt. `screen` is the end
/// of the pane's visible screen, so the caller sees what the pane waits on in this reply.
#[derive(Debug)]
struct AgentNotReady {
    reason: &'static str,
    message: String,
    screen: Option<String>,
}

impl std::fmt::Display for AgentNotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AgentNotReady {}

/// The last non-empty lines of a screen, at most `NOT_READY_SCREEN_BYTES`.
fn screen_excerpt(screen: &str) -> String {
    let lines = screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let excerpt = lines[lines.len().saturating_sub(NOT_READY_SCREEN_LINES)..].join("\n");
    let mut start = excerpt.len().saturating_sub(NOT_READY_SCREEN_BYTES);
    while !excerpt.is_char_boundary(start) {
        start += 1;
    }
    excerpt[start..].to_owned()
}

/// Claude ("Yes, I trust this folder") and Codex ("Trust this folder?") ask this before
/// they load a folder they have not seen. No agent hook runs until it is answered.
fn asks_folder_trust(screen: &str) -> bool {
    screen
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .contains("trust this folder")
}

/// Select the trust choice of the question and confirm it. The highlighted choice is the
/// line after the agent's cursor (`❯` Claude, `›` Codex); Enter is sent only while that
/// line is the trust choice. Claude highlights "No, exit" first. False when no trust
/// choice was found.
fn answer_folder_trust(pane_id: runtime::PaneId) -> Result<bool> {
    for _ in 0..4 {
        let screen = terminal_frame_text(&pane_frame(pane_id)?);
        let choice = screen
            .lines()
            .map(str::trim_start)
            .find_map(|line| line.strip_prefix('❯').or_else(|| line.strip_prefix('›')))
            .map(|choice| choice.trim().to_lowercase());
        match choice {
            Some(choice)
                if choice.contains("trust")
                    && !choice.starts_with("no")
                    && !choice.contains("not")
                    && !choice.contains("don") =>
            {
                send_pane_bytes(pane_id, b"\r".to_vec())?;
                return Ok(true);
            }
            Some(_) => send_pane_bytes(pane_id, b"\x1b[B".to_vec())?,
            None => return Ok(false),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(false)
}

/// Wait until the session's pane has an attached agent that can take a prompt now.
/// `start.pane_id` keeps the pane ID for the report when the wait fails.
fn wait_for_prompt_agent(
    session_id: runtime::SessionId,
    deadline: Instant,
    accept_folder_trust: bool,
    start: &mut AgentStart,
) -> Result<(runtime::PaneId, String)> {
    let mut screen_read = None::<Instant>;
    loop {
        let snapshot = ops::runtime_snapshot()?;
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .context("session_missing: the session was closed")?;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| pane.id == session.focused_pane)
            .context("session_missing: the session has no pane")?;
        start.pane_id = Some(pane.id);
        let not_ready = |reason, message: String| AgentNotReady {
            reason,
            message,
            screen: pane_frame(pane.id)
                .ok()
                .map(|frame| screen_excerpt(&terminal_frame_text(&frame))),
        };
        if pane.exited {
            return Err(not_ready(
                "session_exited",
                format!("session_exited: run `wsx session restart {session_id}`"),
            )
            .into());
        }
        let agent = pane.agent.as_ref().filter(|agent| agent.attached);
        if let Some(agent) = agent.filter(|agent| {
            agent.capabilities.prompt
                && matches!(
                    agent.state,
                    runtime::AgentState::Idle | runtime::AgentState::Done
                )
        }) {
            return Ok((pane.id, agent.provider.clone()));
        }
        // ^ The trust question does not go away by itself: stop at once, or answer it
        // once when the caller asked for that.
        if agent.is_none()
            && screen_read.is_none_or(|read| read.elapsed() >= Duration::from_millis(500))
        {
            screen_read = Some(Instant::now());
            let screen = terminal_frame_text(&pane_frame(pane.id)?);
            if asks_folder_trust(&screen) {
                if !accept_folder_trust || start.folder_trust_accepted {
                    return Err(AgentNotReady {
                        reason: FOLDER_TRUST_QUESTION,
                        message: if start.folder_trust_accepted {
                            "agent_not_ready: the agent still asks whether to trust the folder"
                        } else {
                            "agent_not_ready: the agent asks whether to trust the folder"
                        }
                        .to_owned(),
                        screen: Some(screen_excerpt(&screen)),
                    }
                    .into());
                }
                if !answer_folder_trust(pane.id)? {
                    return Err(not_ready(
                        FOLDER_TRUST_QUESTION,
                        "agent_not_ready: no trust choice was found in the folder trust question"
                            .to_owned(),
                    )
                    .into());
                }
                start.folder_trust_accepted = true;
                // ^ Look again after 2.5 s: the agent needs that long to load the folder.
                screen_read = Some(Instant::now() + Duration::from_secs(2));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(match agent {
                Some(agent) if agent.capabilities.prompt => not_ready(
                    "agent_not_idle",
                    format!("agent_not_ready: {} is {:?}", agent.provider, agent.state),
                ),
                Some(agent) => not_ready(
                    "agent_cannot_prompt",
                    format!(
                        "agent_not_ready: {} is {:?} and its integration cannot take a prompt",
                        agent.provider, agent.state
                    ),
                ),
                None => not_ready(
                    "no_agent_report",
                    "agent_not_ready: no agent reported from the session".to_owned(),
                ),
            }
            .into());
        }
        // ^ Not `poll`: wsxd counts a poll without TUI data as an open TUI.
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn pane_frame(pane_id: runtime::PaneId) -> Result<runtime::TerminalFrame> {
    match runtime::Client::local().call(&runtime::Request::View {
        pane_ids: vec![pane_id],
    })? {
        runtime::Response::View { mut frames, .. } => {
            frames.pop().context("terminal frame is unavailable")
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected view response"),
    }
}

fn parse_exchange_receipt(
    value: &str,
) -> std::result::Result<runtime::AgentExchangeReceipt, String> {
    match value {
        "accepted" => Ok(runtime::AgentExchangeReceipt::Accepted),
        "completed" => Ok(runtime::AgentExchangeReceipt::Completed),
        _ => Err("receipt must be accepted or completed".into()),
    }
}

// ^ docs/agent-orchestration.md: only adapters correlate native turns with exact requests.
// wsxd owns capability, generation, round and state validation. Never bootstrap or retry a receipt.
fn cmd_agent_exchange_receipt(
    exchange_id: runtime::AgentExchangeId,
    round: u32,
    receipt: runtime::AgentExchangeReceipt,
    delivery_sha256: Option<String>,
    input_id: Option<String>,
    json: bool,
) -> Result<()> {
    let runtime_generation = std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("exchange receipt requires a managed wsx runtime generation")?;
    let request = match (delivery_sha256, input_id) {
        (Some(delivery_sha256), Some(input_id)) => {
            let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
                .ok()
                .and_then(|value| value.parse::<runtime::PaneId>().ok())
                .context("bound exchange receipt requires a managed wsx pane")?;
            runtime::Request::AgentExchangeBoundReceipt {
                exchange_id,
                pane_id,
                round,
                runtime_generation,
                delivery_sha256,
                input_id,
                receipt,
            }
        }
        (None, None) => runtime::Request::AgentExchangeReceipt {
            exchange_id,
            round,
            runtime_generation,
            receipt,
        },
        _ => bail!("bound receipt requires both delivery digest and native input ID"),
    };
    agent_exchange_response(
        runtime::Client::new(runtime::default_socket_path()).call(&request)?,
        json,
    )?;
    Ok(())
}

fn cmd_agent_exchange_inspect(
    exchange_id: runtime::AgentExchangeId,
    frame: bool,
    json: bool,
) -> Result<()> {
    let (exchange, frame) = fetch_agent_exchange(exchange_id, frame)?;
    print_agent_exchange(&exchange, frame.as_ref(), json)
}

fn current_agent_exchange(exchange_id: runtime::AgentExchangeId) -> Result<runtime::AgentExchange> {
    Ok(fetch_agent_exchange(exchange_id, false)?.0)
}

fn fetch_agent_exchange(
    exchange_id: runtime::AgentExchangeId,
    include_frame: bool,
) -> Result<(runtime::AgentExchange, Option<runtime::TerminalFrame>)> {
    match runtime::Client::local().call(&runtime::Request::AgentExchangeGet {
        exchange_id,
        include_frame,
    })? {
        runtime::Response::AgentExchange { exchange, frame } => Ok((exchange, frame)),
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent exchange response"),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_agent_exchange_wait(
    exchanges: Vec<runtime::AgentExchangeId>,
    any: bool,
    timeout: u64,
    frame: bool,
    result_bytes: Option<usize>,
    json: bool,
    scope: &SessionScope,
) -> Result<()> {
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (exchange_id, watched) = if any {
        let watched = wait_any_agent_exchange(exchanges, scope, deadline)?;
        (watched.ready[0], Some(watched))
    } else {
        let [exchange_id] = exchanges[..] else {
            bail!("wait takes one exchange; add --any for the first of several or of a -p/-w scope")
        };
        if scope.project.is_some() || scope.worktree.is_some() {
            bail!("-p and -w select exchanges only with --any");
        }
        wait_agent_exchange(exchange_id, deadline)?;
        (exchange_id, None)
    };
    let (exchange, frame) = fetch_agent_exchange(exchange_id, frame)?;
    let result = result_bytes.map(|bytes| exchange_result(&exchange, bytes));
    print_agent_exchange_with(&exchange, frame.as_ref(), result, watched.as_ref(), json)
}

fn wait_agent_exchange(exchange_id: runtime::AgentExchangeId, deadline: Instant) -> Result<()> {
    let mut exchange = current_agent_exchange(exchange_id)?;
    while !exchange.is_wait_boundary() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "wait_timeout: exchange {} remains {:?}",
                exchange.id,
                exchange.state
            );
        }
        let wait_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, 30_000);
        exchange = match runtime::Client::local().wait_agent_exchange(
            &runtime::Request::AgentExchangeWait {
                exchange_id,
                after_revision: exchange.revision,
                timeout_ms: wait_ms,
                include_frame: false,
            },
        )? {
            runtime::Response::AgentExchange { exchange, .. } => exchange,
            runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
            _ => bail!("wsxd returned an unexpected agent exchange response"),
        };
    }
    Ok(())
}

/// The most exchanges that one `wait --any` watches: each one holds a connection to wsxd.
const MAX_WATCHED_EXCHANGES: usize = 16;

/// Wait until one of the named exchanges, or one that was active in the scope at the
/// start, is at a wait boundary. `ready` is then not empty.
/// ^ docs/agent-orchestration.md "Wait for the first of several": no daemon request is
/// added. Each pending exchange gets its own `agent_exchange_wait` long-poll, which wsxd
/// answers when that exchange changes. `poll` is not used: wsxd counts a poll without
/// TUI data as an open TUI, and that would defer a daemon upgrade during the wait.
fn wait_any_agent_exchange(
    named: Vec<runtime::AgentExchangeId>,
    requested: &SessionScope,
    deadline: Instant,
) -> Result<WatchedExchanges> {
    let snapshot = ops::runtime_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    let mut watched = named;
    if watched.is_empty() {
        watched = list_agent_exchanges(None, scope)?
            .iter()
            .filter(|exchange| !exchange.is_wait_boundary())
            .map(|exchange| exchange.id)
            .collect();
        if watched.is_empty() {
            bail!("no_active_exchange: no exchange in the requested scope waits for its agent");
        }
    }
    if watched.len() > MAX_WATCHED_EXCHANGES {
        bail!("too_many_exchanges: wait --any watches at most {MAX_WATCHED_EXCHANGES} exchanges");
    }
    let sort = || -> Result<WatchedExchanges> {
        let exchanges = list_agent_exchanges(None, scope)?;
        let mut sorted = WatchedExchanges {
            ready: Vec::new(),
            pending: Vec::new(),
        };
        for id in &watched {
            let Some(exchange) = exchanges.iter().find(|exchange| exchange.id == *id) else {
                bail!("not_found: exchange {id} is not retained in the requested scope");
            };
            if exchange.is_wait_boundary() {
                sorted.ready.push(*id);
            } else {
                sorted.pending.push(*id);
            }
        }
        Ok(sorted)
    };
    let sorted = sort()?;
    if !sorted.ready.is_empty() {
        return Ok(sorted);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    for id in sorted.pending {
        let sender = sender.clone();
        // A watcher that is still waiting ends with this process.
        std::thread::spawn(move || {
            let _ = sender.send(wait_agent_exchange(id, deadline));
        });
    }
    drop(sender);
    let first = receiver
        .recv()
        .context("exchange watchers ended without an answer")?;
    let sorted = sort()?;
    match first {
        Err(error) if sorted.ready.is_empty() => {
            if error.to_string().starts_with("wait_timeout") {
                bail!(
                    "wait_timeout: no exchange of {} is at a wait boundary",
                    exchange_ids(&sorted.pending)
                );
            }
            Err(error)
        }
        _ => Ok(sorted),
    }
}

fn cmd_agent_exchange_continue(
    exchange_id: runtime::AgentExchangeId,
    prompt: PromptSource,
    timeout: u64,
    stash_draft: bool,
    key: Option<String>,
    json: bool,
) -> Result<()> {
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES)?;
    let current = current_agent_exchange(exchange_id)?;
    let expected_revision = current.revision;
    let timeout_ms = exchange_timeout_ms(timeout)?;
    let request = match key {
        Some(key) => runtime::Request::AgentExchangeContinueKeyed {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
            key,
            stash_draft,
        },
        None if stash_draft => runtime::Request::AgentExchangeContinueStashingDraft {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
        },
        None => runtime::Request::AgentExchangeContinue {
            command: false,
            takeover: false,
            exchange_id,
            expected_revision,
            prompt,
            timeout_ms,
        },
    };
    agent_exchange_response(deliver_exchange(&request)?, json)?;
    Ok(())
}

fn cmd_agent_exchange_cancel(exchange_id: runtime::AgentExchangeId, json: bool) -> Result<()> {
    let current = current_agent_exchange(exchange_id)?;
    agent_exchange_response(
        runtime::Client::local().call(&runtime::Request::AgentExchangeCancel {
            exchange_id,
            expected_revision: current.revision,
            takeover: false,
        })?,
        json,
    )?;
    Ok(())
}

/// Retained exchanges of one session or pane, or of the requested project and worktree.
fn scoped_agent_exchanges(
    selector: Option<&str>,
    requested: &SessionScope,
) -> Result<Vec<runtime::AgentExchange>> {
    let mut pane_id = None;
    let mut scope = RuntimeSessionScope::default();
    if selector.is_some() || requested.project.is_some() || requested.worktree.is_some() {
        let snapshot = ops::runtime_snapshot()?;
        scope = requested_runtime_scope(&snapshot, requested)?;
        if let Some(selector) = selector {
            pane_id = Some(resolve_session_in_snapshot(&snapshot, selector, scope)?.pane_id);
        }
    }
    list_agent_exchanges(pane_id, scope)
}

/// ^ wsxd filters by pane only. A scope without a session is applied here, on the
/// project and worktree IDs that every exchange carries.
fn list_agent_exchanges(
    pane_id: Option<runtime::PaneId>,
    scope: RuntimeSessionScope,
) -> Result<Vec<runtime::AgentExchange>> {
    match runtime::Client::local().call(&runtime::Request::AgentExchangeList { pane_id })? {
        runtime::Response::AgentExchanges(mut exchanges) => {
            exchanges.retain(|exchange| {
                scope.project_id.is_none_or(|id| exchange.project_id == id)
                    && scope
                        .worktree_id
                        .is_none_or(|id| exchange.worktree_id == id)
            });
            Ok(exchanges)
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected agent exchange list response"),
    }
}

fn cmd_agent_exchange_list(selector: Option<&str>, json: bool, scope: &SessionScope) -> Result<()> {
    let exchanges = scoped_agent_exchanges(selector, scope)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&exchanges)?);
        return Ok(());
    }
    for exchange in exchanges {
        println!(
            "{}\tpane {}\tround {}\t{:?}\t{:?}",
            exchange.id, exchange.pane_id, exchange.round, exchange.state, exchange.evidence
        );
    }
    Ok(())
}

fn cmd_plugin_list(json: bool) -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginList)? {
        runtime::Response::Plugins(plugins) if json => {
            println!("{}", serde_json::to_string_pretty(&plugins)?);
            Ok(())
        }
        runtime::Response::Plugins(plugins) => {
            for plugin in plugins {
                println!(
                    "{}\t{}\t{}",
                    plugin.id,
                    if plugin.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    plugin.name
                );
            }
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected plugin response"),
    }
}

fn cmd_plugin_reload() -> Result<()> {
    match runtime::Client::local().call(&runtime::Request::PluginReload)? {
        runtime::Response::Plugins(plugins) => {
            println!("reloaded {} plugins", plugins.len());
            Ok(())
        }
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected plugin reload response"),
    }
}

// ^ docs/daemon-live-upgrade.md: a stop ends every running program, so it needs `--force`
// while sessions are live. `None` means the daemon cannot report what it runs.
fn sessions_a_stop_would_end(client: &runtime::Client) -> Result<Option<usize>> {
    Ok(match client.status()? {
        runtime::DaemonStatus::Missing => Some(0),
        runtime::DaemonStatus::Compatible => match client.call(&runtime::Request::Snapshot)? {
            runtime::Response::Snapshot(snapshot) => Some(
                snapshot
                    .sessions
                    .iter()
                    .filter(|session| {
                        snapshot
                            .panes
                            .iter()
                            .any(|pane| pane.session_id == session.id && !pane.exited)
                    })
                    .count(),
            ),
            runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
            _ => bail!("wsxd returned an unexpected snapshot response"),
        },
        runtime::DaemonStatus::Incompatible { lifecycle, .. } => {
            lifecycle.map(|status| status.live_runtimes)
        }
    })
}

fn cmd_daemon_stop(force: bool, json: bool) -> Result<()> {
    let client = runtime::Client::local();
    let live = if force {
        None
    } else {
        match sessions_a_stop_would_end(&client)? {
            Some(0) => None,
            live => Some(live),
        }
    };
    if let Some(live) = live {
        if json {
            println!(
                "{}",
                serde_json::json!({"stopped": false, "reason": "live_sessions", "live_sessions": live})
            );
        }
        let count = live.map_or("an unknown number of".to_string(), |live| live.to_string());
        bail!(
            "wsxd not stopped: {count} running session(s) would end. `wsx daemon restart` keeps them running. Use `wsx daemon stop --force` to end them."
        );
    }
    client.shutdown()?;
    if json {
        println!("{}", serde_json::json!({"stopped": true}));
    } else {
        println!(
            "wsxd stopped and the programs in its sessions ended; saved session commands restart on next launch"
        );
    }
    Ok(())
}

fn cmd_daemon_restart(json: bool) -> Result<()> {
    let outcome = runtime::restart_daemon()?;
    let (state, live_runtimes, message) = match &outcome {
        runtime::DaemonRestart::NotRunning => (
            "not_running",
            0,
            "wsxd is not running; the next wsx command starts it".to_string(),
        ),
        runtime::DaemonRestart::Restarted { live_runtimes } => (
            "restarted",
            *live_runtimes,
            format!(
                "wsxd restarted as {}; {live_runtimes} running terminal(s) kept",
                runtime::WSX_VERSION
            ),
        ),
        runtime::DaemonRestart::Deferred {
            live_runtimes,
            blockers,
        } => (
            "deferred",
            *live_runtimes,
            format!(
                "wsxd restart is waiting. {} Sessions keep running. The restart then continues by itself.",
                crate::app::replacement_wait_reason(blockers)
            ),
        ),
        runtime::DaemonRestart::Unsupported { daemon_version } => (
            "unsupported",
            0,
            format!(
                "wsxd {daemon_version} cannot restart into the same build without ending its sessions; nothing was stopped."
            ),
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::json!({"state": state, "live_runtimes": live_runtimes, "message": message})
        );
    }
    if matches!(outcome, runtime::DaemonRestart::Unsupported { .. }) {
        bail!(message);
    }
    if !json {
        println!("{message}");
    }
    Ok(())
}

fn cmd_daemon_recover() -> Result<()> {
    runtime::recover_daemon()?;
    println!("wsxd recovered");
    Ok(())
}

fn runtime_status_state(lifecycle: Option<&runtime::DaemonLifecycle>) -> &'static str {
    match lifecycle {
        Some(status) if status.phase == runtime::DaemonPhase::ReplacementPending => {
            "replacement_deferred"
        }
        Some(status) if status.daemon_revision < runtime::DAEMON_REVISION => "upgrade_available",
        _ => "ready",
    }
}

fn incompatible_runtime_status_json(
    socket: &std::path::Path,
    daemon_protocol: Option<u32>,
    lifecycle: Option<&runtime::DaemonLifecycle>,
    lifecycle_error: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "running": true,
        "compatible": false,
        "state": "incompatible",
        "socket": socket,
        "daemon_protocol": daemon_protocol,
        "lifecycle": lifecycle,
        "lifecycle_error": lifecycle_error,
        "client": {
            "version": runtime::WSX_VERSION,
            "protocol": runtime::PROTOCOL_VERSION,
            "daemon_revision": runtime::DAEMON_REVISION,
        },
    })
}

fn runtime_lifecycle_lines(lifecycle: &runtime::DaemonLifecycle) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Lifecycle: {:?}, version {}, daemon revision {}, {} live runtimes, {} clients, {} TUIs",
            lifecycle.phase,
            if lifecycle.version.is_empty() {
                "unknown"
            } else {
                &lifecycle.version
            },
            lifecycle.daemon_revision,
            lifecycle.live_runtimes,
            lifecycle.active_clients,
            lifecycle.active_tuis,
        ),
        format!("Binary: {}", lifecycle.binary_id),
    ];
    if let Some(target) = lifecycle.replacement_target.as_deref() {
        lines.push(format!(
            "Target: {} (version {})",
            target,
            if lifecycle.replacement_target_version.is_empty() {
                "unknown"
            } else {
                &lifecycle.replacement_target_version
            }
        ));
    }
    if !lifecycle.replacement_blockers.is_empty() {
        lines.push(format!(
            "Blockers: {}",
            lifecycle
                .replacement_blockers
                .iter()
                .map(|blocker| format!("{blocker:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // ^ docs/daemon-live-upgrade.md "A failed handoff": the daemon's stderr goes nowhere.
    if let Some(failure) = &lifecycle.last_handoff_error {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        lines.push(format!(
            "Last handoff failed {}s ago: {} failed: {}",
            now.saturating_sub(failure.unix_ms) / 1000,
            failure.step,
            failure.error
        ));
    }
    lines
}

fn cmd_runtime_status(json: bool) -> Result<()> {
    let client = runtime::Client::local();
    match client.status() {
        Ok(runtime::DaemonStatus::Missing) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": false,
                        "compatible": false,
                        "state": "stopped",
                        "socket": client.socket(),
                    })
                );
            } else {
                println!("Runtime: stopped");
                println!("Socket: {}", client.socket().display());
            }
            return Ok(());
        }
        Ok(runtime::DaemonStatus::Incompatible {
            daemon_protocol,
            lifecycle,
            lifecycle_error,
        }) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&incompatible_runtime_status_json(
                        client.socket(),
                        daemon_protocol,
                        lifecycle.as_deref(),
                        lifecycle_error.as_deref(),
                    ))?
                );
            } else {
                println!("Runtime: running (incompatible)");
                println!("Socket: {}", client.socket().display());
                println!(
                    "Protocol: daemon {}, client {}",
                    daemon_protocol
                        .map(|protocol| protocol.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                    runtime::PROTOCOL_VERSION
                );
                if let Some(lifecycle) = lifecycle.as_deref() {
                    for line in runtime_lifecycle_lines(lifecycle) {
                        println!("{line}");
                    }
                }
                if let Some(error) = lifecycle_error {
                    println!("Lifecycle: unavailable ({error})");
                }
                println!("Open the matching wsx version or launch wsx to request an upgrade.");
            }
            return Ok(());
        }
        Ok(runtime::DaemonStatus::Compatible) => {}
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "running": serde_json::Value::Null,
                        "compatible": false,
                        "state": "unavailable",
                        "socket": client.socket(),
                        "error": error.to_string(),
                    })
                );
            } else {
                println!("Runtime: unavailable");
                println!("Socket: {}", client.socket().display());
                println!("Reason: {error}");
            }
            return Ok(());
        }
    }

    let snapshot = match client.call(&runtime::Request::Snapshot) {
        Ok(runtime::Response::Snapshot(snapshot)) => snapshot,
        Ok(runtime::Response::Error(error)) => bail!("{}: {}", error.code, error.message),
        Ok(_) => bail!("wsxd returned an unexpected status response"),
        Err(error) => bail!("wsxd became unavailable while reading status: {error}"),
    };
    let lifecycle = if snapshot.capabilities.lifecycle_coordination {
        match client.call(&runtime::Request::LifecycleStatus) {
            Ok(runtime::Response::Lifecycle(status)) => Some(status),
            _ => None,
        }
    } else {
        None
    };
    let state = runtime_status_state(lifecycle.as_ref());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "running": true,
                "compatible": true,
                "state": state,
                "socket": client.socket(),
                "protocol": snapshot.protocol,
                "epoch": snapshot.epoch,
                "revision": snapshot.revision,
                "projects": snapshot.projects.len(),
                "worktrees": snapshot.worktrees.len(),
                "sessions": snapshot.sessions.len(),
                "panes": snapshot.panes.len(),
                "capabilities": snapshot.capabilities,
                "lifecycle": lifecycle,
                "client": {
                    "version": runtime::WSX_VERSION,
                    "protocol": runtime::PROTOCOL_VERSION,
                    "daemon_revision": runtime::DAEMON_REVISION,
                },
            }))?
        );
    } else {
        println!(
            "Runtime: running ({state}), protocol {}, epoch {}, revision {}",
            snapshot.protocol, snapshot.epoch, snapshot.revision
        );
        println!("Socket: {}", client.socket().display());
        if let Some(lifecycle) = lifecycle {
            for line in runtime_lifecycle_lines(&lifecycle) {
                println!("{line}");
            }
        }
        println!(
            "Resources: {} projects, {} worktrees, {} sessions, {} panes",
            snapshot.projects.len(),
            snapshot.worktrees.len(),
            snapshot.sessions.len(),
            snapshot.panes.len()
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct RuntimeSessionScope {
    project_id: Option<runtime::ProjectId>,
    worktree_id: Option<runtime::WorktreeId>,
}

#[derive(Debug)]
struct ResolvedSession {
    session_id: runtime::SessionId,
    pane_id: runtime::PaneId,
    label: String,
}

fn runtime_session_scope(
    snapshot: &runtime::Snapshot,
    requested: &SessionScope,
    config: Option<&GlobalConfig>,
) -> Result<RuntimeSessionScope> {
    let project_id = requested
        .project
        .as_deref()
        .map(|name| {
            snapshot
                .projects
                .iter()
                .find(|project| project.name == name)
                .map(|project| project.id)
                .ok_or_else(|| anyhow::anyhow!("project '{}' not found", name))
        })
        .transpose()?;
    let worktree_id = requested
        .worktree
        .as_deref()
        .map(|selector| {
            let matches = snapshot
                .worktrees
                .iter()
                .filter(|worktree| project_id.is_none_or(|id| worktree.project_id == id))
                .filter(|worktree| {
                    let alias = config.and_then(|config| {
                        let project = snapshot
                            .projects
                            .iter()
                            .find(|project| project.id == worktree.project_id)?;
                        config
                            .projects
                            .iter()
                            .find(|entry| entry.path == project.path)?
                            .aliases
                            .get(&worktree.branch)
                    });
                    worktree.branch == selector
                        || worktree.path == Path::new(selector)
                        || alias.is_some_and(|alias| alias == selector)
                })
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [worktree] => Ok(worktree.id),
                [] => bail!("worktree '{}' not found in requested scope", selector),
                _ => bail!("worktree '{}' is ambiguous; add --project", selector),
            }
        })
        .transpose()?;
    Ok(RuntimeSessionScope {
        project_id,
        worktree_id,
    })
}

/// The requested scope in one snapshot. Worktree aliases need the configuration.
fn requested_runtime_scope(
    snapshot: &runtime::Snapshot,
    requested: &SessionScope,
) -> Result<RuntimeSessionScope> {
    let config = requested
        .worktree
        .as_ref()
        .map(|_| load_config())
        .transpose()?;
    runtime_session_scope(snapshot, requested, config.as_ref())
}

fn session_matches_scope(
    snapshot: &runtime::Snapshot,
    session: &runtime::Session,
    scope: RuntimeSessionScope,
) -> bool {
    let Some(worktree) = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
    else {
        return false;
    };
    scope
        .project_id
        .is_none_or(|project_id| worktree.project_id == project_id)
        && scope
            .worktree_id
            .is_none_or(|worktree_id| worktree.id == worktree_id)
}

fn resolve_session_in_snapshot(
    snapshot: &runtime::Snapshot,
    selector: &str,
    scope: RuntimeSessionScope,
) -> Result<ResolvedSession> {
    if let Ok(session_id) = selector.parse::<runtime::SessionId>() {
        if let Some(session) = snapshot
            .sessions
            .iter()
            .find(|session| session.id == session_id)
        {
            if !session_matches_scope(snapshot, session, scope) {
                bail!("session {} is outside the requested scope", session_id);
            }
            return Ok(ResolvedSession {
                session_id,
                pane_id: session.focused_pane,
                label: session.label.clone(),
            });
        }
    }
    if let Ok(pane_id) = selector.parse::<runtime::PaneId>() {
        if let Some(pane) = snapshot.panes.iter().find(|pane| pane.id == pane_id) {
            let session = snapshot
                .sessions
                .iter()
                .find(|session| session.id == pane.session_id)
                .context("pane session is unavailable")?;
            if !session_matches_scope(snapshot, session, scope) {
                bail!("pane {} is outside the requested scope", pane_id);
            }
            return Ok(ResolvedSession {
                session_id: session.id,
                pane_id,
                label: session.label.clone(),
            });
        }
    }
    let matches = snapshot
        .sessions
        .iter()
        .filter(|session| session.label == selector)
        .filter(|session| session_matches_scope(snapshot, session, scope))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok(ResolvedSession {
            session_id: session.id,
            pane_id: session.focused_pane,
            label: session.label.clone(),
        }),
        [] => bail!("session or pane not found in requested scope: {selector}"),
        _ => bail!(
            "session label is ambiguous: {selector}; add --project/--worktree or use a numeric ID"
        ),
    }
}

fn resolve_session(selector: &str, requested: &SessionScope) -> Result<ResolvedSession> {
    let snapshot = ops::runtime_snapshot()?;
    let scope = requested_runtime_scope(&snapshot, requested)?;
    resolve_session_in_snapshot(&snapshot, selector, scope)
}

fn resolve_pane(selector: &str, scope: &SessionScope) -> Result<runtime::PaneId> {
    Ok(resolve_session(selector, scope)?.pane_id)
}

#[cfg(test)]
mod session_resolution_tests {
    use super::{
        resolve_creation_worktree, resolve_session_in_snapshot, runtime_session_scope,
        RuntimeSessionScope, SessionScope,
    };
    use std::path::{Path, PathBuf};
    use wsx_core::{
        config::global::{GlobalConfig, ProjectEntry},
        model::workspace::{Project, WorkspaceState, WorktreeInfo},
        runtime::{
            Capabilities, Pane, PaneId, PaneLayout, Project as RuntimeProject, ProjectId, Session,
            SessionId, Snapshot, TerminalId, Worktree, WorktreeId,
        },
    };

    fn worktree(branch: &str, path: &str, alias: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            name: branch.into(),
            branch: branch.into(),
            path: PathBuf::from(path),
            is_main: branch == "main",
            alias: alias.map(str::to_owned),
            sessions: Vec::new(),
            expanded: false,
            git_info: None,
            fetch_failed: false,
            fetch_fail_count: 0,
            fetch_fail_reason: None,
            last_fetched: None,
            git_info_fetched_at: None,
        }
    }

    fn project(name: &str, path: &str, worktrees: Vec<WorktreeInfo>) -> Project {
        Project {
            name: name.into(),
            path: PathBuf::from(path),
            default_branch: "main".into(),
            last_agent_active_unix_ms: None,
            last_terminal_active_unix_ms: None,
            worktrees,
            routines: Vec::new(),
            routine_revision: 0,
            routines_expanded: false,
            config: None,
            expanded: false,
            missing: false,
        }
    }

    #[test]
    fn creation_prefers_explicit_scope_then_cwd_then_only_worktree() {
        let workspace = WorkspaceState {
            projects: vec![
                project(
                    "api",
                    "/repos/api",
                    vec![worktree("main", "/repos/api", None)],
                ),
                project(
                    "web",
                    "/repos/web",
                    vec![
                        worktree("main", "/repos/web", None),
                        worktree("feature", "/repos/web-feature", Some("review")),
                    ],
                ),
            ],
        };
        let explicit = SessionScope {
            project: Some("web".into()),
            worktree: Some("review".into()),
        };
        let (resolved_project, resolved_worktree) =
            resolve_creation_worktree(&workspace, &explicit, Some(Path::new("/repos/api")))
                .unwrap();
        assert_eq!(resolved_project.name, "web");
        assert_eq!(resolved_worktree.branch, "feature");

        let inferred = SessionScope::default();
        let (resolved_project, resolved_worktree) = resolve_creation_worktree(
            &workspace,
            &inferred,
            Some(Path::new("/repos/web-feature/src")),
        )
        .unwrap();
        assert_eq!(resolved_project.name, "web");
        assert_eq!(resolved_worktree.branch, "feature");

        let only_project = WorkspaceState {
            projects: vec![project(
                "api",
                "/repos/api",
                vec![worktree("main", "/repos/api", None)],
            )],
        };
        let (_, resolved_worktree) =
            resolve_creation_worktree(&only_project, &inferred, None).unwrap();
        assert_eq!(resolved_worktree.branch, "main");
    }

    #[test]
    fn creation_rejects_ambiguous_project_and_worktree_targets() {
        let ambiguous_projects = WorkspaceState {
            projects: vec![
                project(
                    "api",
                    "/repos/api",
                    vec![worktree("main", "/repos/api", None)],
                ),
                project(
                    "web",
                    "/repos/web",
                    vec![worktree("main", "/repos/web", None)],
                ),
            ],
        };
        assert!(
            resolve_creation_worktree(&ambiguous_projects, &SessionScope::default(), None)
                .unwrap_err()
                .to_string()
                .contains("multiple projects")
        );

        let ambiguous_worktrees = WorkspaceState {
            projects: vec![project(
                "api",
                "/repos/api",
                vec![
                    worktree("main", "/repos/api", None),
                    worktree("feature", "/repos/api-feature", None),
                ],
            )],
        };
        assert!(
            resolve_creation_worktree(&ambiguous_worktrees, &SessionScope::default(), None)
                .unwrap_err()
                .to_string()
                .contains("multiple worktrees")
        );
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            protocol: 15,
            epoch: 1,
            revision: 1,
            projects: vec![
                RuntimeProject {
                    id: ProjectId(1),
                    path: "/repos/api".into(),
                    name: "api".into(),
                    revision: 1,
                    last_agent_active_unix_ms: None,
                    last_terminal_active_unix_ms: None,
                },
                RuntimeProject {
                    id: ProjectId(10),
                    path: "/repos/web".into(),
                    name: "web".into(),
                    revision: 1,
                    last_agent_active_unix_ms: None,
                    last_terminal_active_unix_ms: None,
                },
            ],
            worktrees: vec![
                Worktree {
                    id: WorktreeId(2),
                    project_id: ProjectId(1),
                    path: "/repos/api-feature".into(),
                    branch: "feature".into(),
                    revision: 1,
                },
                Worktree {
                    id: WorktreeId(11),
                    project_id: ProjectId(10),
                    path: "/repos/web-feature".into(),
                    branch: "feature".into(),
                    revision: 1,
                },
            ],
            sessions: vec![
                Session {
                    id: SessionId(3),
                    worktree_id: WorktreeId(2),
                    label: "worker".into(),
                    primary_pane: PaneId(4),
                    focused_pane: PaneId(4),
                    panes: vec![PaneId(4)],
                    layout: PaneLayout::Leaf { pane_id: PaneId(4) },
                    revision: 1,
                },
                Session {
                    id: SessionId(12),
                    worktree_id: WorktreeId(11),
                    label: "worker".into(),
                    primary_pane: PaneId(13),
                    focused_pane: PaneId(13),
                    panes: vec![PaneId(13)],
                    layout: PaneLayout::Leaf {
                        pane_id: PaneId(13),
                    },
                    revision: 1,
                },
            ],
            panes: vec![
                Pane {
                    id: PaneId(4),
                    terminal_id: TerminalId(5),
                    session_id: SessionId(3),
                    label: "terminal".into(),
                    agent: None,
                    exited: false,
                    revision: 1,
                },
                Pane {
                    id: PaneId(13),
                    terminal_id: TerminalId(14),
                    session_id: SessionId(12),
                    label: "terminal".into(),
                    agent: None,
                    exited: false,
                    revision: 1,
                },
            ],
            listening_ports: Vec::new(),
            pane_activity: Vec::new(),
            plugin_sidecars: Vec::new(),
            capabilities: Capabilities::default(),
        }
    }

    #[test]
    fn scoped_labels_resolve_once_and_exact_ids_reject_scope_mismatch() {
        let snapshot = snapshot();
        let mut config = GlobalConfig::default();
        config.projects.push(ProjectEntry {
            name: "api".into(),
            path: "/repos/api".into(),
            groups: Vec::new(),
            aliases: [("feature".into(), "review".into())].into(),
            worktree_order: Vec::new(),
        });
        let requested = SessionScope {
            project: Some("api".into()),
            worktree: Some("review".into()),
        };
        let scope = runtime_session_scope(&snapshot, &requested, Some(&config)).unwrap();
        let resolved = resolve_session_in_snapshot(&snapshot, "worker", scope).unwrap();
        assert_eq!(resolved.session_id, SessionId(3));
        assert_eq!(resolved.pane_id, PaneId(4));

        let unscoped = RuntimeSessionScope {
            project_id: None,
            worktree_id: None,
        };
        assert!(resolve_session_in_snapshot(&snapshot, "worker", unscoped)
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(resolve_session_in_snapshot(&snapshot, "12", scope)
            .unwrap_err()
            .to_string()
            .contains("outside the requested scope"));
    }

    #[test]
    fn exact_pane_id_preserves_the_selected_pane() {
        let snapshot = snapshot();
        let resolved = resolve_session_in_snapshot(
            &snapshot,
            "13",
            RuntimeSessionScope {
                project_id: None,
                worktree_id: None,
            },
        )
        .unwrap();
        assert_eq!(resolved.session_id, SessionId(12));
        assert_eq!(resolved.pane_id, PaneId(13));
        assert_eq!(resolved.label, "worker");
    }
}

fn send_pane_bytes(pane_id: runtime::PaneId, bytes: Vec<u8>) -> Result<()> {
    let client = runtime::Client::local();
    let client_id = runtime::new_client_id();
    match client.call(&runtime::Request::TerminalAcquire {
        pane_id,
        client_id,
        takeover: false,
    })? {
        runtime::Response::Ack { .. } => {}
        runtime::Response::Error(error) => bail!("{}: {}", error.code, error.message),
        _ => bail!("wsxd returned an unexpected lease response"),
    }
    let result = match client.call(&runtime::Request::TerminalInput {
        pane_id,
        client_id,
        bytes,
    })? {
        runtime::Response::Ack { .. } => Ok(()),
        runtime::Response::Error(error) => {
            Err(anyhow::anyhow!("{}: {}", error.code, error.message))
        }
        _ => Err(anyhow::anyhow!(
            "wsxd returned an unexpected input response"
        )),
    };
    let _ = client.call(&runtime::Request::TerminalRelease { pane_id, client_id });
    result
}

fn cmd_session_create(
    scope: SessionScope,
    name: Option<String>,
    command: Option<String>,
    json: bool,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let cwd = std::env::current_dir().ok();
    let (project, worktree) = resolve_creation_worktree(&workspace, &scope, cwd.as_deref())?;
    let (session_id, label) = ops::create_session(
        &config,
        &project.name,
        &worktree.session_slug(&project.name),
        &worktree.path,
        name,
        command,
    )?;
    if json {
        let (pane_id, agent) = created_session_pane(session_id);
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "session_id": session_id,
                "pane_id": pane_id,
                "agent": agent,
                "label": label,
                "project": project.name,
                "worktree": worktree.path,
                "branch": worktree.branch,
            }))?
        );
    } else {
        println!("{}", created_session_line(session_id));
        println!("label:    {label}");
        println!("worktree: {}", worktree.path.display());
    }
    Ok(())
}

fn cmd_session_current(json: bool) -> Result<()> {
    let pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .filter(|id| !id.is_empty())
        .context("not inside a wsx-managed pane: WSX_PANE_ID is not set")?
        .parse::<runtime::PaneId>()
        .map_err(|_| anyhow::anyhow!("WSX_PANE_ID is not a valid pane ID"))?;
    let snapshot = existing_daemon_snapshot()?;
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.id == pane_id)
        .ok_or_else(|| anyhow::anyhow!("managed pane {pane_id} is not present in wsxd"))?;
    let session = snapshot
        .sessions
        .iter()
        .find(|session| session.id == pane.session_id)
        .context("pane session missing")?;
    let worktree = snapshot
        .worktrees
        .iter()
        .find(|worktree| worktree.id == session.worktree_id)
        .context("pane worktree missing")?;
    let project = snapshot
        .projects
        .iter()
        .find(|project| project.id == worktree.project_id)
        .context("pane project missing")?;
    let agent = pane.agent.as_ref().filter(|agent| agent.attached);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pane_id": pane.id,
                "session_id": session.id,
                "label": session.label,
                "project": project.name,
                "worktree": worktree.path,
                "branch": worktree.branch,
                "agent": agent.map(|agent| &agent.provider),
                "agent_status": agent.map(|agent| agent.state),
            }))?
        );
    } else {
        println!("pane:     {}", pane.id);
        println!("{}", created_session_line(session.id));
        println!("label:    {}", session.label);
        println!("project:  {}", project.name);
        println!("worktree: {}", worktree.path.display());
        println!("branch:   {}", worktree.branch);
        if let Some(agent) = agent {
            println!(
                "agent:    {} ({})",
                agent.provider,
                format!("{:?}", agent.state).to_lowercase()
            );
        }
    }
    Ok(())
}

fn cmd_session_delete(selector: &str, scope: &SessionScope, json: bool) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    ops::kill_session(session.session_id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "deleted": true,
                "session_id": session.session_id,
                "label": session.label,
            }))?
        );
    } else {
        println!(
            "deleted session: {} ({})",
            session.session_id, session.label
        );
    }
    Ok(())
}

fn cmd_session_restart(selector: &str, json: bool, scope: &SessionScope) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    let revision = ops::restart_session(session.pane_id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "restarted": true,
                "session_id": session.session_id,
                "pane_id": session.pane_id,
                "label": session.label,
                "revision": revision,
            }))?
        );
    } else {
        println!(
            "restarted session: {} ({})",
            session.session_id, session.label
        );
    }
    Ok(())
}

fn cmd_session_send_text(
    selector: &str,
    text: &str,
    no_enter: bool,
    scope: &SessionScope,
) -> Result<()> {
    let pane_id = resolve_pane(selector, scope)?;
    let mut bytes = text.as_bytes().to_vec();
    if !no_enter {
        bytes.push(b'\r');
    }
    send_pane_bytes(pane_id, bytes)
}

fn cmd_session_prompt(selector: &str, prompt: PromptSource, scope: &SessionScope) -> Result<()> {
    // ^ wsxd accepts 64 KiB of terminal input, and Enter is one byte of it.
    let prompt = prompt.read(runtime::MAX_AGENT_EXCHANGE_PROMPT_BYTES - 1)?;
    cmd_session_send_text(selector, &prompt, false, scope)
}

// ^ [[Session Peek]] Reads are bounded projections of authoritative semantic frames.
fn cmd_session_peek(
    selector: &str,
    lines: Option<u32>,
    trim: bool,
    agent: bool,
    scope: &SessionScope,
) -> Result<()> {
    let frame = pane_frame(resolve_pane(selector, scope)?)?;
    let mut rendered = frame
        .cells
        .chunks(usize::from(frame.cols))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>();
    let requested = lines.unwrap_or(u32::from(frame.rows));
    let keep_end = rendered.len();
    let keep_start = keep_end.saturating_sub(requested as usize);
    rendered = rendered[keep_start..keep_end].to_vec();
    let mut output = rendered.join("\n");
    if trim || agent {
        output = output.trim_end().to_string();
    }
    print!("{}", output);
    Ok(())
}

fn cmd_session_rename(selector: &str, new_label: &str, scope: &SessionScope) -> Result<()> {
    let session = resolve_session(selector, scope)?;
    ops::rename_session(session.session_id, new_label)?;
    println!("renamed session {} to '{}'", session.session_id, new_label);
    Ok(())
}
fn cmd_group_ls() -> Result<()> {
    let config = load_config()?;
    for group in &config.groups {
        println!("{}", group);
    }
    Ok(())
}

fn validate_new_group(config: &GlobalConfig, name: &str) -> Result<()> {
    GroupKey::named(name.to_string()).map_err(anyhow::Error::msg)?;
    if config.named_group_exists(name) {
        bail!("group '{}' already exists", name);
    }
    Ok(())
}

fn cmd_group_create(name: &str) -> Result<()> {
    let mut config = load_config()?;
    validate_new_group(&config, name)?;
    config.groups.push(name.to_string());
    config.save()?;
    println!("created group: {}", name);
    Ok(())
}

fn cmd_group_rename(old: &str, new_name: &str) -> Result<()> {
    let mut config = load_config()?;
    let index = config
        .groups
        .iter()
        .position(|group| group == old)
        .ok_or_else(|| anyhow::anyhow!("group '{}' not found", old))?;
    validate_new_group(&config, new_name)?;
    config.groups[index] = new_name.to_string();
    for entry in &mut config.projects {
        for group in &mut entry.groups {
            if group == old {
                *group = new_name.to_string();
            }
        }
    }
    config.save()?;
    println!("renamed group: {} → {}", old, new_name);
    Ok(())
}

fn project_path(config: &GlobalConfig, project: &str) -> Result<PathBuf> {
    config
        .projects
        .iter()
        .find(|entry| entry.name == project)
        .map(|entry| entry.path.clone())
        .ok_or_else(|| anyhow::anyhow!("project '{}' not found", project))
}

fn cmd_group_add(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if config
        .project_groups(&path)
        .iter()
        .any(|name| name == group)
    {
        println!("project '{}' is already in group '{}'", project, group);
        return Ok(());
    }
    config.add_project_to_group(&path, group);
    config.save()?;
    println!("added '{}' to group '{}'", project, group);
    Ok(())
}

fn cmd_group_remove(group: &str, project: &str) -> Result<()> {
    let mut config = load_config()?;
    GroupKey::named(group.to_string()).map_err(anyhow::Error::msg)?;
    if !config.named_group_exists(group) {
        bail!("group '{}' not found", group);
    }
    let path = project_path(&config, project)?;
    if !config.remove_project_from_group(&path, group) {
        println!("project '{}' is not in group '{}'", project, group);
        return Ok(());
    }
    config.save()?;
    println!("removed '{}' from group '{}'", project, group);
    Ok(())
}

fn cmd_session_list(
    project_name: Option<&str>,
    json: bool,
    format: Format,
    group: Option<&GroupKey>,
) -> Result<()> {
    let (config, workspace) = load_full_workspace()?;
    let projects = resolve_projects(&config, &workspace, project_name, group)?;
    if json {
        let mut sessions = Vec::new();
        for project in &projects {
            for worktree in &project.worktrees {
                for session in &worktree.sessions {
                    // ^ Added keys only: a row says where it is without a second call.
                    let mut row = serde_json::to_value(session)?;
                    row["project"] = serde_json::json!(project.name);
                    row["branch"] = serde_json::json!(worktree.branch);
                    row["worktree"] = serde_json::json!(worktree.path);
                    sessions.push(row);
                }
            }
        }
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    if let Format::Compact = format {
        let headers = &["project", "branch", "sessions"];
        let rows: Vec<Vec<String>> = projects
            .iter()
            .flat_map(|p| {
                p.worktrees
                    .iter()
                    .map(move |wt| vec![p.name.clone(), wt.branch.clone(), sessions_inline(wt)])
            })
            .collect();
        print_table(headers, &rows);
        return Ok(());
    }
    for p in projects {
        for wt in &p.worktrees {
            for s in &wt.sessions {
                println!(
                    "{}/{} — {} ({})",
                    p.name,
                    wt.branch,
                    s.pane_id,
                    activity_label(s)
                );
            }
        }
    }
    Ok(())
}
