use super::*;

#[derive(Clone, Default, ValueEnum)]
pub enum Format {
    #[default]
    Normal,
    Compact,
}

/// `--json`, `--format` and `--group` of the commands that print the workspace.
#[derive(clap::Args)]
pub struct ListOutput {
    #[arg(long)]
    pub(super) json: bool,
    #[arg(short = 'f', long, value_enum, default_value = "normal")]
    pub(super) format: Format,
    #[arg(short = 'g', long = "group", value_parser = parse_group_key)]
    pub(super) group: Option<GroupKey>,
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
        #[command(flatten)]
        output: ListOutput,
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
    /// Headless worker tasks and the project manager (docs/tasks.md)
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
    pub(super) name: Option<String>,
    /// Same as SITE
    #[arg(long = "site", value_name = "SITE", conflicts_with = "name")]
    pub(super) flag: Option<String>,
}

impl WebSite {
    pub(super) fn named(&self) -> Option<&str> {
        self.flag.as_deref().or(self.name.as_deref())
    }
}

impl Command {
    /// True when the caller asked for JSON, so stderr must stay free of notices.
    pub fn json_output(&self) -> bool {
        match self {
            Command::Status { output, .. } => output.json,
            Command::Worktree { subcommand } => matches!(
                subcommand,
                WorktreeCmd::Create { json: true, .. }
                    | WorktreeCmd::List {
                        output: ListOutput { json: true, .. },
                        ..
                    }
            ),
            Command::Session { subcommand } => matches!(
                subcommand,
                SessionCmd::Create { json: true, .. }
                    | SessionCmd::Delete { json: true, .. }
                    | SessionCmd::Restart { json: true, .. }
                    | SessionCmd::List {
                        output: ListOutput { json: true, .. },
                        ..
                    }
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
            Command::Routine { subcommand } => subcommand.json(),
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
    pub(super) branch: String,
    #[command(flatten)]
    pub(super) prompt: PromptSource,
    #[arg(short, long)]
    pub(super) project: Option<String>,
    /// Start a new branch from this commit, tag, or branch instead of the default branch
    #[arg(long)]
    pub(super) from: Option<String>,
    /// Command that starts the agent in the new session
    /// (default: the project's worktree.defaultSession.command)
    #[arg(long, value_name = "COMMAND")]
    pub(super) agent: Option<String>,
    /// Session name; an existing session of this name in the worktree is used
    /// (default: the first word of the agent command)
    #[arg(long)]
    pub(super) name: Option<String>,
    #[arg(long, default_value_t = 600)]
    pub(super) timeout: u64,
    /// Seconds to wait until the agent can take a prompt
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub(super) ready_timeout: u64,
    #[arg(long)]
    pub(super) writer: bool,
    #[arg(long = "write-claim", requires = "writer")]
    pub(super) write_claims: Vec<PathBuf>,
    /// Deliver once per key and pane: a repeat returns the existing exchange
    #[arg(long, value_name = "TEXT")]
    pub(super) key: Option<String>,
    /// Answer yes when the agent asks whether to trust the worktree folder. The agent
    /// saves that decision and then runs the folder's own settings and hooks
    #[arg(long)]
    pub(super) accept_folder_trust: bool,
    #[arg(long)]
    pub(super) json: bool,
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
        #[arg(long)]
        json: bool,
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
        #[arg(long)]
        json: bool,
    },
    Delete {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Enable cron scheduling for a routine
    Enable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Disable cron scheduling without cancelling an active run
    Disable {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        revision: Option<u64>,
        #[arg(long)]
        json: bool,
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
        #[arg(long)]
        json: bool,
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

impl RoutineCmd {
    pub fn json(&self) -> bool {
        match self {
            Self::List { json, .. }
            | Self::Show { json, .. }
            | Self::Add { json, .. }
            | Self::Edit { json, .. }
            | Self::Delete { json, .. }
            | Self::Enable { json, .. }
            | Self::Disable { json, .. }
            | Self::Run { json, .. }
            | Self::Cancel { json, .. }
            | Self::Logs { json, .. }
            | Self::Fire { json, .. } => *json,
        }
    }
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
        #[command(flatten)]
        output: ListOutput,
    },
}

/// Exactly one prompt source: the argument, `-` for standard input, or a file.
#[derive(clap::Args)]
pub struct PromptSource {
    /// Prompt text, or `-` to read the prompt from standard input
    pub(super) prompt: Option<String>,
    /// Read the prompt from this file
    #[arg(long, value_name = "PATH")]
    pub(super) prompt_file: Option<PathBuf>,
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
    pub(super) project: Option<String>,
    /// Select or restrict a session target to one worktree branch, alias, or path
    #[arg(short = 'w', long)]
    pub(super) worktree: Option<String>,
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
        #[command(flatten)]
        output: ListOutput,
    },
}
