//! The clap types of `wsx task`: the subcommand enums and the argument groups they share.
//! ref: docs/tasks.md

use super::cmd::new::Size;
use super::cmd::routine::post_key;
use crate::cli::PromptSource;
use crate::cli::RoutineCmd;
use clap::Subcommand;
use std::path::PathBuf;
use wsx_core::task::policy::Capability;
use wsx_core::task::AfterKind;

/// `--owner-said <id>` on an action that needs the owner's approval (docs/tasks.md "Approvals").
#[derive(clap::Args)]
pub struct OwnerSaid {
    /// The lead: the owner's message of this turn that asked for it (its id), which
    /// approves it without a new request
    #[arg(long, value_name = "MESSAGE")]
    pub(super) owner_said: Option<u64>,
}

/// `--owner-said <id>` and `--after <n>` on `land`, `close --discard`, `deploy`, `release` and
/// `push`: the owner's go of this turn for an action that can run only once #n has landed.
#[derive(clap::Args)]
pub struct OwnerSaidAfter {
    #[command(flatten)]
    pub(super) said: OwnerSaid,
    /// The lead: record the approved action to run when this task lands, not now
    #[arg(long, value_name = "TASK", requires = "owner_said")]
    pub(super) after: Option<u32>,
}

/// The detached run that a `deploy` or `release` by hand starts: it holds `deploy.lock` as
/// this descriptor, and its checks ran before it started (docs/tasks.md "Deploy and release").
#[derive(clap::Args)]
pub struct RunLocked {
    #[arg(long, hide = true, value_name = "FD")]
    pub(super) run_locked: Option<i32>,
    /// A release the owner's message approved (`--after`): open To check items stay on it
    #[arg(long, hide = true)]
    pub(super) checks_waived: bool,
    /// Run from an approval (now or after a land): wait for a running deploy or release
    #[arg(long, hide = true)]
    pub(super) approved: bool,
}

/// `-p` and `--json` of every `wsx chat`, `wsx task` and `wsx project` command (`digest`, `checkin`, `post` and the hidden host
/// commands accept `--json` and ignore it; `ledger` always prints JSON).
#[derive(clap::Args)]
pub struct Common {
    /// The project (default: the current folder's, or the only one)
    #[arg(short, long, global = true)]
    pub project: Option<String>,
    /// Print the answer as JSON
    #[arg(long, global = true)]
    pub json: bool,
}

/// `wsx task`: one task's life, from `new` to `land` or `close` (docs/tasks.md).
#[derive(Subcommand)]
pub enum TaskCmd {
    /// Start a worker on a new branch in its own working copy
    New(NewArgs),
    /// List a project's tasks (closed ones with --all)
    List {
        #[arg(long)]
        all: bool,
    },
    /// Show one task with its latest result; --json names its working copy, base and log
    Show { task: String },
    /// Why a task is where it is: its state, what it waits on, its last turn and its log's end
    Why { task: String },
    /// A task's prompt and adds with its commits and diff against its base, for review
    Review { task: String },
    /// Send a message after the current turn; --now sends it into the running turn
    Add {
        task: String,
        #[command(flatten)]
        text: PromptSource,
        /// Add a glob of the paths it expects to change (repeatable)
        #[arg(long, value_name = "GLOB", conflicts_with = "now")]
        files: Vec<String>,
        /// Rename the task to what it now carries (at most 60 characters)
        #[arg(long, value_name = "WORDS", conflicts_with = "now")]
        title: Option<String>,
        /// Type it into the running turn at once; with no turn running it is a plain add
        #[arg(long)]
        now: bool,
    },
    /// Interrupt the worker; the next add resumes the same conversation
    Stop { task: String },
    /// Bring a ready task onto the default branch
    Land {
        task: String,
        #[command(flatten)]
        said: OwnerSaidAfter,
    },
    /// Give tasks to another chat of the repo: its events, waits and To check items go there
    Move {
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        /// The open chat that owns them from now on
        #[arg(long, value_name = "NAME")]
        chat: String,
    },
    /// Park a task until another lands, then add text to it; --off ends the wait
    After {
        task: String,
        /// The task whose land ends the wait
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        on: Option<String>,
        /// What the task does then; it comes as a message
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        then: Option<String>,
        /// Why it waits: needs (builds on #m's change), files (changes the same files, lands
        /// after it), check (its checks fail on main until #m lands)
        #[arg(
            long,
            value_name = "needs|files|check",
            value_parser = AfterKind::parse,
            required_unless_present = "off",
            conflicts_with = "off"
        )]
        because: Option<AfterKind>,
        #[arg(long)]
        off: bool,
    },
    /// Hold a task for the owner to try before it lands, or release it with --off
    Hold {
        task: String,
        /// Why the owner should try it first
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        reason: Option<String>,
        #[arg(long)]
        off: bool,
    },
    /// Remove the working copy, session and branch
    Close {
        task: String,
        /// Close although the branch has commits that are not on the default branch
        #[arg(long)]
        discard: bool,
        #[command(flatten)]
        said: OwnerSaidAfter,
    },
    /// Work on the task by hand in its working copy, with the worker's conversation, or give it
    /// back with --back: its commits are checked as the next revision (owner only)
    Hand {
        task: String,
        /// Give the task back
        #[arg(long)]
        back: bool,
    },
    #[command(hide = true)]
    /// Post what the project's tasks did since the last digest to its chat, with no lead
    /// turn, and print nothing; a project gets one with a routine (docs/tasks.md "Routines")
    Digest {
        /// Start here (unix seconds) instead of at the last digest, else 24 h back
        #[arg(long, value_name = "SECONDS")]
        since: Option<u64>,
    },
    #[command(hide = true)]
    /// Post stdin to the project's chat as wsx's message, with no lead turn; nothing when
    /// it is empty. A routine pipes its report here (docs/tasks.md "Routines")
    Post {
        /// The chat's line above the text
        #[arg(long, default_value = "routine")]
        title: String,
        /// `warn` or `bad` shows the level in the chat and pushes to the owner's phone at once
        #[arg(long, default_value = "info", value_parser = ["info", "warn", "bad"])]
        level: String,
        /// Drop a post with this key (1-64 of A-Z a-z 0-9 . _ -) when one was kept in the last
        /// 10 minutes; prints `deduped: <key>`
        #[arg(long, value_parser = post_key)]
        key: Option<String>,
    },
    /// Runs an approved run's command on this terminal and records its end (the pane's command)
    #[command(hide = true)]
    Ran { id: String },
    #[command(hide = true)]
    /// From a task's worker: run the project's checks as after its result (all, or by number
    /// or scenario name). `--part` reruns one part of one scenario, such as
    /// `check-run web --part tasks`: the scenario's command gets `WSX_SCENARIO_PART`, and a
    /// part's pass is no full pass (nothing is kept, no evidence is recorded)
    CheckRun {
        names: Vec<String>,
        /// One part of one scenario (the project's scenario must know its parts)
        #[arg(long)]
        part: Option<String>,
    },
    #[command(hide = true)]
    Host { project_id: String, task: String },
    /// Runs a command in the machine's scenario lane, as a task's scenario does: the deploy's
    /// smoke test and the release's live handoff wait here for the Mac's load (docs/design/scheduler.md).
    #[command(hide = true)]
    Slot {
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Prints `name<TAB>command` for each `tasks.scenarios` entry with paths that the change from
    /// `--since` to HEAD of the current folder touches; with no `--since`, or one git does not
    /// know, every such scenario. The deploy gate runs them (scripts/deploy-web-live.sh).
    #[command(hide = true)]
    TouchedScenarios {
        #[arg(long)]
        since: Option<String>,
    },
    /// Stops every process whose arguments or environment name a path under the folder, with
    /// its descendants: a scenario part's end, so no helper outlives its run (docs/tasks.md
    /// "Scenarios").
    #[command(hide = true)]
    Reap { folder: PathBuf },
    /// Prints the lead's Seatbelt profile; scripts/test-lead-boundary.py starts a second
    /// sandbox with it.
    #[command(hide = true)]
    BoundaryProfile {
        /// The profile of an untrusted agent (Pi, or Claude on another endpoint)
        #[arg(long)]
        untrusted: bool,
    },
    /// One chat's host (docs/tasks.md "Chats"): it only talks, and leaves when idle.
    #[command(hide = true)]
    ChatHost { chat: String },
    /// The repo's service (docs/tasks.md "Chats"): events, stalls, deploys, routines, recap.
    #[command(hide = true)]
    RepoHost { project_id: String },
    /// The host of the release before; it runs as `chat-host` and starts `repo-host`.
    #[command(hide = true)]
    ManagerHost {
        project_id: String,
        /// `role`: the argv wsxd stored for a session before chats starts the same host
        #[arg(long, alias = "role")]
        chat: Option<String>,
    },
}

#[derive(clap::Args)]
pub struct NewArgs {
    #[command(flatten)]
    pub(crate) prompt: PromptSource,
    /// Start from this branch, tag or commit instead of the default branch
    #[arg(long, value_name = "REF")]
    pub(crate) from: Option<String>,
    /// Start from this open task's branch and land into it, not the default branch; the
    /// task is held so it lands only on the owner's word (docs/tasks.md "Parent tasks")
    #[arg(long, value_name = "TASK", conflicts_with = "from")]
    pub(crate) into: Option<u32>,
    /// Start only when this open task has landed: the worker waits with no turn, then starts
    /// on the default branch with it (docs/tasks.md "After another task")
    #[arg(
        long,
        value_name = "TASK",
        requires = "because",
        conflicts_with_all = ["from", "into"]
    )]
    pub(crate) after: Option<String>,
    /// Why it waits for --after: needs (builds on its change), files (changes the same files,
    /// lands after it), check (its checks fail on main until it lands)
    #[arg(
        long,
        value_name = "needs|files|check",
        value_parser = AfterKind::parse,
        requires = "after"
    )]
    pub(crate) because: Option<AfterKind>,
    /// A short title in plain words (at most 60 characters); default: the prompt's first line
    #[arg(long)]
    pub(crate) title: Option<String>,
    /// The worker's model; default: tasks.agent.model (opus), or tasks.agent.smallModel
    /// (sonnet) with --size small
    #[arg(long, value_name = "NAME")]
    pub(crate) model: Option<String>,
    /// small: a change you can state (what, where, how it proves itself) runs only the
    /// build step on tasks.agent.smallModel; full (default): every step
    #[arg(long, value_enum, default_value_t = Size::Full, conflicts_with = "skip")]
    pub(crate) size: Size,
    /// Hold the task for the owner to try before it lands, and say why
    #[arg(long, value_name = "WHY")]
    pub(crate) hold: Option<String>,
    /// A glob of the paths it expects to change, relative to the checkout (repeatable)
    #[arg(long, value_name = "GLOB")]
    pub(crate) files: Vec<String>,
    /// The owner's skills its worker gets, comma-separated, in place of tasks.skillsOnly
    #[arg(long, value_name = "NAMES", value_delimiter = ',')]
    pub(crate) skills: Option<Vec<String>>,
    /// The first turn writes a plan and stops without editing; an `add` continues it
    #[arg(long)]
    pub(crate) plan: bool,
    /// Leave out this `tasks.workflow.steps` step (repeatable), for a prompt that already
    /// carries the context and the plan
    #[arg(long, value_name = "STEP")]
    pub(crate) skip: Vec<String>,
    /// One turn without `tasks.workflow.steps`: the lead's idle task
    #[arg(long, hide = true)]
    pub(crate) no_steps: bool,
}

#[derive(clap::Args)]
pub struct DeployArgs {
    /// Commit, tag or branch to deploy; default: the default branch tip
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
    /// Only when the default branch tip is not deployed yet, after a running deploy ends
    #[arg(long, conflicts_with = "commit")]
    pub(crate) if_new: bool,
    /// After a land: wait until the default branch has stayed unchanged this many seconds, else
    /// leave the deploy to the land that moved it
    #[arg(long, hide = true, value_name = "SECS", requires = "if_new")]
    pub(crate) quiet: Option<u64>,
    #[command(flatten)]
    pub(crate) run: RunLocked,
    #[command(flatten)]
    pub(crate) said: OwnerSaidAfter,
}

#[derive(clap::Args)]
pub struct ReleaseArgs {
    /// Commit, tag or branch to release; default: the default branch tip
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
    #[command(flatten)]
    pub(crate) run: RunLocked,
    #[command(flatten)]
    pub(crate) said: OwnerSaidAfter,
}

#[derive(clap::Args)]
pub struct PushArgs {
    /// Commit, tag or branch to push; default: the default branch tip
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
    /// Rewrite origin's default branch (a squash): --force-with-lease on the tip seen now
    #[arg(long)]
    pub(crate) force: bool,
    /// The origin tip an approved request saw; the push refuses when origin moved since
    #[arg(long, hide = true, requires = "force", value_name = "COMMIT")]
    pub(crate) lease: Option<String>,
    #[command(flatten)]
    pub(crate) said: OwnerSaidAfter,
}

/// `wsx chat`: talk with a project's chats and decide what they ask (docs/tasks.md "Chats").
#[derive(Subcommand)]
pub enum ChatGroup {
    /// Send a message to the project's lead, or another chat; returns at once with its id
    Say {
        #[command(flatten)]
        text: PromptSource,
        /// To this chat instead of the first
        #[arg(long, value_name = "NAME")]
        to: Option<String>,
        /// From a chat (the first is `manager`) to another chat of this project (--to, the
        /// first when none), or with -p to another project's first chat
        #[arg(long)]
        from_chat: bool,
        /// With --from-chat: this answers a message it sent
        #[arg(long)]
        reply: bool,
        /// To the main chat of every registered project whose lockfile or manifest names this
        /// dependency, instead of one project (docs/tasks.md "Mail")
        #[arg(long, value_name = "DEPENDENCY", conflicts_with_all = ["to", "reply"])]
        uses: Option<String>,
    },
    /// Read a chat's conversation, the main chat's when no --chat, after a message id or around
    /// one (docs/tasks.md "Chats")
    Read {
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        /// The part of the conversation around message ID, at any age the log keeps
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        around: Option<u64>,
        #[arg(long, value_name = "NAME")]
        chat: Option<String>,
    },
    #[command(flatten)]
    Manage(ChatCmd),
    /// Find earlier messages, decisions and task history, ranked: the chats, the notes and the
    /// tasks; a hit's `quote` line quotes it in a reply
    Search {
        /// The words to find, all of them or some; the best matches come first
        #[arg(required_unless_present = "task", value_name = "WORDS")]
        words: Vec<String>,
        /// One task, open or closed: its dated history, or only its hits for the words
        #[arg(long, value_name = "N")]
        task: Option<String>,
        /// Every registered project, not only -p
        #[arg(long)]
        all: bool,
        /// Most hits shown (10), or lines of a task's history (40)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Build the index of the searched projects again from their logs and tasks
        #[arg(long)]
        reindex: bool,
    },
    /// Approve a request the lead made, and run its action (owner only)
    Approve {
        /// The request id, such as a3
        id: String,
    },
    /// Deny a request the lead made (owner only)
    Deny {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// The owner dismisses an open question of the lead by its reply id: it asks no more (owner)
    Dismiss { id: u64 },
    /// Wake the lead once, `--in` a time or `--at` a local time, with `--why` as its event;
    /// a new check-in replaces the last one. It is no routine (docs/tasks.md "Check-in")
    Checkin {
        /// `40m`, `2h` or `1d`
        #[arg(
            long = "in",
            value_name = "DURATION",
            conflicts_with = "at",
            required_unless_present = "at"
        )]
        after: Option<String>,
        /// `HH:MM` or `YYYY-MM-DD HH:MM` local time
        #[arg(long)]
        at: Option<String>,
        /// What to look at then; the event's text
        #[arg(long)]
        why: String,
    },
    /// Start the project's lead (its main chat's agent), or report the running one
    Lead,
    /// Print the bounded project state a chat reads first (JSON, at most 16 KiB)
    Ledger,
}

/// `wsx project`: the project's code side, its default branch, deploys and routines (docs/tasks.md).
#[derive(Subcommand)]
pub enum ProjectGroup {
    #[command(flatten)]
    Manage(ProjectCmd),
    /// Push the default branch, or one commit, to origin's default branch and move the local one there (the lead asks the owner first)
    Push(PushArgs),
    /// Fetch origin and move the local default branch to its tip when no work is lost (a branch that is behind, or whose commits origin has already after a rebase or squash merge); else refuse and name the commits or files
    Sync,
    /// Commit files of the main checkout on the default branch once its light checks pass on
    /// that commit (the lead: with the owner's message)
    Commit {
        /// Paths in the main checkout, as they are on disk
        #[arg(required = true, num_args = 1..)]
        files: Vec<String>,
        #[arg(short, long)]
        message: String,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Run tasks.deploy.command in a temporary copy of one default-branch commit and record it
    Deploy(DeployArgs),
    /// Run tasks.release.command the same way and record it
    Release(ReleaseArgs),
    /// Show the last deployed and released commits and how far each is behind
    Deployed,
    /// List what the owner has to check of the landed tasks, or pass, skip or fail items (owner; the lead passes with --owner-said)
    Check {
        #[command(subcommand)]
        action: Option<CheckAction>,
    },
    /// Ask the owner to run one command outside the boundary, in a session pane the owner can
    /// watch and type into (sudo); its exit and last lines come back (docs/tasks.md "Approved runs")
    Run {
        /// The folder it runs in (default: where this command runs)
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Why: shown to the owner with the command
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
        /// The command and its arguments, run as given (no shell): `wsx project run -- killall X`
        #[arg(required = true, last = true, value_name = "COMMAND")]
        argv: Vec<String>,
    },
    /// The project's scheduled routines: `wsx routine` (docs/tasks.md "Routines")
    Routine {
        #[command(subcommand)]
        action: RoutineCmd,
        /// The lead: the owner's message of this turn that asked to add, edit or delete it
        #[arg(long, value_name = "MESSAGE", global = true)]
        owner_said: Option<u64>,
    },
}

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Register a folder inside the home folder as a project; with --clone, git clones it first.
    /// A git URL alone clones into <root>/<repo>: the folder of the current project, else the one
    /// that holds the most projects, else ~/ws
    Add {
        /// A folder inside the home folder, or an ssh or https git URL
        path: String,
        /// The ssh or https git URL to clone into a folder that does not exist yet
        #[arg(long, value_name = "URL")]
        clone: Option<String>,
        #[command(flatten)]
        said: OwnerSaid,
    },
}

#[derive(Subcommand)]
pub enum ChatCmd {
    /// Open a chat: a chat with a name and a goal, in the same project (the owner's, or a
    /// chat's on the owner's message of its turn)
    New {
        name: String,
        /// The chat's goal, a file or - for standard input
        #[arg(long, value_name = "FILE")]
        goal: Option<PathBuf>,
        /// The `[[agents]]` entry it runs on; default: the lead's (tasks.chats)
        #[arg(long, value_name = "ENTRY")]
        agent: Option<String>,
        /// The lead (the main chat with no goal) forks the owner's message --owner-said names:
        /// the new chat starts on it, its goal is the message's first line, and the lead notes
        /// the fork. The name must be new
        #[arg(long, requires = "owner_said", conflicts_with = "goal")]
        brief: bool,
        /// Open it at one of `tasks.roles`: it owns that role's paths and runs on its agent; a
        /// --goal follows the role's purpose (docs/tasks.md "Roles")
        #[arg(long, value_name = "ROLE", conflicts_with = "brief")]
        role: Option<String>,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// The chats: open and closed
    List,
    /// Close a chat; its log and notes stay, its open tasks go to the project's main chat
    Close {
        name: String,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Make an open chat of the project its main chat: it hears what no chat owns and what the
    /// project's routines and other projects' chats address to it. Setting is the owner's, or a
    /// chat's agent on the owner's message of its turn
    Main {
        name: String,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Show a chat's purpose, or set it from a file or - for standard input (an empty one
    /// clears it); the chat's agent reads it at its next turn. Setting is the owner's, or a
    /// chat's agent on the owner's message of its turn
    Goal {
        name: String,
        #[arg(long, value_name = "FILE")]
        goal: Option<PathBuf>,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Set the name the owner reads for a chat, up to 40 characters; an empty title clears it.
    /// The chat's id stays what `say --to` and `chat close` take
    Rename { name: String, title: String },
}

/// One parsed command of the three groups that act on a project: `wsx chat`, `wsx task` and
/// `wsx project` (docs/design/orchestration.md, the CLI split).
pub enum Invocation {
    Chat(ChatGroup),
    Task(TaskCmd),
    Project(ProjectGroup),
}

impl Invocation {
    /// The command an agent's argv names, `None` when it parses to no command of the three groups.
    pub fn parse_argv(argv: &[String]) -> Option<Self> {
        use clap::Parser;
        let args = crate::cli::Args::try_parse_from(
            std::iter::once("wsx").chain(argv.iter().map(String::as_str)),
        )
        .ok()?;
        match args.command? {
            crate::cli::Command::Chat { subcommand, .. } => Some(Self::Chat(subcommand)),
            crate::cli::Command::Task { subcommand, .. } => Some(Self::Task(subcommand)),
            crate::cli::Command::Project { subcommand, .. } => Some(Self::Project(subcommand)),
            _ => None,
        }
    }

    /// What this command needs, any one of: the broker lets an agent run it only when its
    /// policy grants one (`wsx_core::task::policy`). One owner of the table: a new subcommand
    /// does not compile until it says. A command the CLI judges finer (an owner's word, a
    /// task's creator) is checked again there.
    pub fn needs(&self) -> &'static [Capability] {
        use Capability::*;
        match self {
            Self::Chat(command) => match command {
                ChatGroup::Read { .. }
                | ChatGroup::Search { .. }
                | ChatGroup::Lead
                | ChatGroup::Ledger => &[Read],
                ChatGroup::Say { .. } => &[Notify],
                ChatGroup::Checkin { .. } => &[Routine],
                ChatGroup::Manage(_)
                | ChatGroup::Dismiss { .. }
                | ChatGroup::Approve { .. }
                | ChatGroup::Deny { .. } => &[Approve],
            },
            Self::Project(command) => match command {
                ProjectGroup::Deployed | ProjectGroup::Check { .. } => &[Read],
                ProjectGroup::Commit { .. } => &[Land],
                ProjectGroup::Deploy(_)
                | ProjectGroup::Release(_)
                | ProjectGroup::Push(_)
                | ProjectGroup::Sync
                | ProjectGroup::Manage(_) => &[Deploy],
                ProjectGroup::Run { .. } => &[Ask],
                ProjectGroup::Routine { action, .. } => match action {
                    RoutineCmd::List { .. } | RoutineCmd::Show { .. } | RoutineCmd::Logs { .. } => {
                        &[Read]
                    }
                    _ => &[Routine],
                },
            },
            Self::Task(command) => match command {
                TaskCmd::List { .. }
                | TaskCmd::Show { .. }
                | TaskCmd::Why { .. }
                | TaskCmd::Review { .. }
                | TaskCmd::TouchedScenarios { .. } => &[Read],
                TaskCmd::Post { .. } | TaskCmd::Digest { .. } => &[Notify],
                TaskCmd::CheckRun { .. } => &[Result],
                TaskCmd::After { .. } | TaskCmd::Hold { .. } => &[Backlog],
                TaskCmd::Add { .. } | TaskCmd::Stop { .. } | TaskCmd::Move { .. } => &[Direct],
                TaskCmd::New(_) => &[CreateTask],
                TaskCmd::Close { discard: true, .. } => &[Discard],
                TaskCmd::Close { .. } => &[Close],
                TaskCmd::Land { .. } => &[Land],
                TaskCmd::Hand { .. }
                | TaskCmd::Host { .. }
                | TaskCmd::BoundaryProfile { .. }
                | TaskCmd::Slot { .. }
                | TaskCmd::Reap { .. }
                | TaskCmd::Ran { .. }
                | TaskCmd::ChatHost { .. }
                | TaskCmd::RepoHost { .. }
                | TaskCmd::ManagerHost { .. } => &[Approve],
            },
        }
    }
}

// ^ Whether a command prints JSON whatever `--json` says (the ledger always does; a routine
// command has its own `--json`, `RoutineCmd`), and whether it reads files or a host socket and
// never needs wsxd: `main` and `Args::json` ask each group.
impl ChatGroup {
    pub fn prints_json(&self) -> bool {
        matches!(self, Self::Ledger)
    }

    pub fn without_daemon(&self) -> bool {
        !matches!(self, Self::Lead | Self::Say { .. } | Self::Approve { .. })
    }
}

impl ProjectGroup {
    pub fn prints_json(&self) -> bool {
        matches!(self, Self::Routine { action, .. } if action.json())
    }

    pub fn without_daemon(&self) -> bool {
        !matches!(
            self,
            Self::Check {
                action: Some(CheckAction::Fail { note: Some(_), .. }),
                ..
            }
        )
    }
}

impl TaskCmd {
    pub fn without_daemon(&self) -> bool {
        !matches!(
            self,
            Self::New(_) | Self::Close { .. } | Self::Hand { .. } | Self::Move { .. }
        )
    }
}

/// The new form of a `wsx task` command that moved to `wsx chat` or `wsx project`, for the
/// refusal of the old one (docs/design/orchestration.md, the CLI split).
pub fn moved(argv: &[String]) -> Option<String> {
    let (group, after) = argv.split_first()?;
    if group != "task" {
        return None;
    }
    // ^ `Common`'s flags may come before the command: `wsx task -p demo say ...`.
    let mut words = after.iter().map(String::as_str);
    let command = loop {
        match words.next()? {
            "-p" | "--project" => {
                words.next()?;
            }
            flag if flag == "--json" || flag.starts_with("--project=") => {}
            command => break command,
        }
    };
    let rest: Vec<&str> = words.collect();
    const CHAT_ACTIONS: [&str; 6] = ["new", "list", "close", "main", "goal", "rename"];
    let new = match command {
        "say" | "search" | "approve" | "deny" | "dismiss" | "checkin" | "ledger" => {
            format!("wsx chat {command}")
        }
        "manager" => "wsx chat lead".into(),
        "chat" => match rest.iter().find(|word| !word.starts_with('-')) {
            Some(action) if CHAT_ACTIONS.contains(action) => format!("wsx chat {action}"),
            _ => "wsx chat read".into(),
        },
        "project" => "wsx project add".into(),
        "push" | "sync" | "commit" | "deploy" | "release" | "deployed" | "check" | "run"
        | "routine" => format!("wsx project {command}"),
        _ => return None,
    };
    Some(format!("`wsx task {command}` moved: use `{new}`"))
}

#[derive(Subcommand)]
pub enum CheckAction {
    /// The named items passed, or every open item with --all (the lead: named items with --owner-said)
    Pass {
        #[command(flatten)]
        chosen: Chosen,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Close the named items, or every open item with --all, without a verdict: no fix task
    Skip(Chosen),
    /// The item failed. With --note a new task fixes it, linked to the item; without, the item
    /// only closes as failed (the lead starts the fix from the owner's words)
    Fail {
        id: String,
        /// What is wrong; the new task gets it
        #[arg(long)]
        note: Option<String>,
    },
}

/// The items a pass or a skip closes.
#[derive(clap::Args)]
pub struct Chosen {
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub(super) ids: Vec<String>,
    #[arg(long)]
    pub(super) all: bool,
}
