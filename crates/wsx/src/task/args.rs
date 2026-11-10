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
    /// The manager: the owner's message of this turn that asked for it (its id), which
    /// approves it without a new request
    #[arg(long, value_name = "MESSAGE")]
    pub(super) owner_said: Option<u64>,
}

/// The detached run that a `deploy` or `release` by hand starts: it holds `deploy.lock` as
/// this descriptor, and its checks ran before it started (docs/tasks.md "Deploy and release").
#[derive(clap::Args)]
pub struct RunLocked {
    #[arg(long, hide = true, value_name = "FD")]
    pub(super) run_locked: Option<i32>,
}

#[derive(Subcommand)]
pub enum TaskCmd {
    /// Start a worker on a new branch in its own working copy
    New(NewArgs),
    /// List a project's tasks (closed ones with --all)
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one task with its latest result
    Show {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message into the running turn
    Steer {
        task: String,
        #[command(flatten)]
        text: PromptSource,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Send a message after the current turn
    Add {
        task: String,
        #[command(flatten)]
        text: PromptSource,
        /// Add a glob of the paths it expects to change (repeatable)
        #[arg(long, value_name = "GLOB")]
        files: Vec<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Interrupt the worker; the next add resumes the same conversation
    Stop {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Bring a ready task onto the default branch; several land as one batch, checked once
    Land {
        #[arg(required = true, num_args = 1..)]
        task: Vec<String>,
        /// The owner passed its trial: its Human Verify items are not added to To check
        #[arg(long)]
        tried: bool,
        #[command(flatten)]
        said: OwnerSaid,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
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
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Hold a task for the owner to try before it lands, or release it with --off
    Hold {
        task: String,
        /// Why the owner should try it first
        #[arg(required_unless_present = "off", conflicts_with = "off")]
        reason: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put a ready task's revision on the live site to try before it lands, or end the trial
    Trial {
        task: String,
        /// End the trial and deploy the default branch again
        #[arg(long)]
        end: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove the working copy, session and branch
    Close {
        task: String,
        /// Close although the branch has commits that are not on the default branch
        #[arg(long)]
        discard: bool,
        #[command(flatten)]
        said: OwnerSaid,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Work on the task by hand in its working copy, with the worker's conversation (owner only)
    Hand {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Give a task worked by hand back: its commits are checked as the next revision (owner only)
    Handback {
        task: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The owner dismisses an open question of the manager by its reply id: it asks no more (owner)
    Dismiss {
        id: u64,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List what the owner has to check of the landed tasks, or pass, skip or fail items (owner; the manager passes with --owner-said)
    Check {
        #[command(subcommand)]
        action: Option<CheckAction>,
        #[arg(short, long, global = true)]
        project: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Run tasks.deploy.command in a temporary copy of one default-branch commit and record it
    Deploy(DeployArgs),
    /// Run tasks.release.command the same way and record it
    Release(ReleaseArgs),
    /// Push the default branch, or one commit, to origin's default branch and move the local one there (the manager asks the owner first)
    Push(PushArgs),
    /// Print the bounded project state a manager reads first (JSON, at most 16 KiB)
    Ledger {
        #[arg(short, long)]
        project: Option<String>,
        /// Accepted like on every task command; the ledger is always JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the last deployed and released commits and how far each is behind
    Deployed {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Post what the project's tasks did since the last digest to its chat, with no manager
    /// turn, and print nothing; a project gets one with a routine (docs/tasks.md "Routines")
    Digest {
        #[arg(short, long)]
        project: Option<String>,
        /// Start here (unix seconds) instead of at the last digest, else 24 h back
        #[arg(long, value_name = "SECONDS")]
        since: Option<u64>,
    },
    /// Wake the manager once, `--in` a time or `--at` a local time, with `--why` as its event;
    /// a new check-in replaces the last one (docs/tasks.md "Routines")
    Checkin {
        #[arg(short, long)]
        project: Option<String>,
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
    /// Post stdin to the project's chat as wsx's message, with no manager turn; nothing when
    /// it is empty. A routine pipes its report here (docs/tasks.md "Routines")
    Post {
        #[arg(short, long)]
        project: Option<String>,
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
    /// The project's scheduled routines: `wsx routine` (docs/tasks.md "Routines")
    Routine {
        #[command(subcommand)]
        action: RoutineCmd,
        /// The manager: the owner's message of this turn that asked to add, edit or delete it
        #[arg(long, value_name = "MESSAGE", global = true)]
        owner_said: Option<u64>,
    },
    /// Start the project's manager agent, or report the running one
    Manager {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The project's roles: named chats like the manager's (docs/tasks.md "Roles")
    Role {
        #[command(subcommand)]
        action: RoleCmd,
    },
    /// Add a project to wsx: clone it when the folder is missing (the manager asks the owner first)
    Project {
        #[command(subcommand)]
        action: ProjectCmd,
    },
    /// Send a message to the project's manager, or a role; returns at once with its id
    Say {
        #[command(flatten)]
        text: PromptSource,
        /// To this role instead of the manager
        #[arg(long, value_name = "NAME")]
        role: Option<String>,
        /// From a project's manager to the manager of the project named with -p
        #[arg(long, conflicts_with = "from_role")]
        from_manager: bool,
        /// From the manager or a role to a role (--role) or the manager of its own project
        #[arg(long)]
        from_role: bool,
        /// With --from-manager or --from-role: this answers a message it sent
        #[arg(long)]
        reply: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read the manager conversation, or a role's, after a message id or from the oldest message with some words
    Chat {
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        /// From the oldest message that shows these words, case-insensitive
        #[arg(long, value_name = "WORDS", conflicts_with_all = ["after", "around"])]
        find: Option<String>,
        /// The part of the conversation around message ID, at any age the log keeps
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        around: Option<u64>,
        #[arg(long, value_name = "NAME")]
        role: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Find earlier messages, decisions and task history, ranked: the manager and role chats, the notes and the tasks
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
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Approve a request the manager made, and run its action (owner only)
    Approve {
        /// The request id, such as a3
        id: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Deny a request the manager made (owner only)
    Deny {
        id: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// From a task's worker: run the project's checks as after its result (all, or by number
    /// or scenario name). `--part` reruns one part of one scenario, such as
    /// `check-run web --part tasks`: the scenario's command gets `WSX_SCENARIO_PART`, and a
    /// part's pass is no full pass (nothing is kept, no evidence is recorded); the full run
    /// still gates the task
    CheckRun {
        names: Vec<String>,
        /// One part of one scenario (the project's scenario must know its parts)
        #[arg(long)]
        part: Option<String>,
    },
    /// Serve the task for a person to try on its own tailnet or local port (tasks.preview)
    Preview {
        task: String,
        /// Stop the running preview
        #[arg(long)]
        stop: bool,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    PreviewHost { project_id: String, task: String },
    #[command(hide = true)]
    LandBatch {
        tasks: Vec<String>,
        #[arg(short, long)]
        project: Option<String>,
    },
    #[command(hide = true)]
    Host { project_id: String, task: String },
    /// Prints the manager's Seatbelt profile; scripts/test-manager-boundary.py starts a second
    /// sandbox with it.
    #[command(hide = true)]
    BoundaryProfile {
        #[arg(short, long)]
        project: Option<String>,
        /// The profile of an untrusted agent (Pi, or Claude on another endpoint)
        #[arg(long)]
        untrusted: bool,
    },
    #[command(hide = true)]
    ManagerHost {
        project_id: String,
        #[arg(long)]
        role: Option<String>,
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
    /// The first turn writes a plan and stops without editing; an `add` continues it
    #[arg(long)]
    pub(crate) plan: bool,
    /// Leave out this `tasks.workflow.steps` step (repeatable), for a prompt that already
    /// carries the context and the plan
    #[arg(long, value_name = "STEP")]
    pub(crate) skip: Vec<String>,
    /// One turn without `tasks.workflow.steps`: the manager's idle task
    #[arg(long, hide = true)]
    pub(crate) no_steps: bool,
    #[arg(short, long)]
    pub(crate) project: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub struct DeployArgs {
    /// Commit, tag or branch to deploy; default: the default branch tip
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
    /// Only when the default branch tip is not deployed yet, after a running deploy ends
    #[arg(long, conflicts_with = "commit")]
    pub(crate) if_new: bool,
    /// Deploy the revision in this task's trial slot (the task host starts it)
    #[arg(long, hide = true, value_name = "TASK", conflicts_with_all = ["commit", "if_new"])]
    pub(crate) trial: Option<u32>,
    #[command(flatten)]
    pub(crate) run: RunLocked,
    #[command(flatten)]
    pub(crate) said: OwnerSaid,
    #[arg(short, long)]
    pub(crate) project: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(clap::Args)]
pub struct ReleaseArgs {
    /// Commit, tag or branch to release; default: the default branch tip
    #[arg(long, value_name = "REF")]
    pub(crate) commit: Option<String>,
    #[command(flatten)]
    pub(crate) run: RunLocked,
    #[command(flatten)]
    pub(crate) said: OwnerSaid,
    #[arg(short, long)]
    pub(crate) project: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
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
    pub(crate) said: OwnerSaid,
    #[arg(short, long)]
    pub(crate) project: Option<String>,
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Register a folder inside the home folder as a project; with --clone, git clones it first
    Add {
        path: String,
        /// The ssh or https git URL to clone into a folder that does not exist yet
        #[arg(long, value_name = "URL")]
        clone: Option<String>,
        #[command(flatten)]
        said: OwnerSaid,
        /// The manager's project that asks (the owner needs none)
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum RoleCmd {
    /// Open a role (the manager asks the owner first; at most tasks.roles.max)
    New {
        name: String,
        /// The role's brief, a file or - for standard input
        #[arg(long, value_name = "FILE")]
        brief: PathBuf,
        #[command(flatten)]
        said: OwnerSaid,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The roles: open, asked for and closed
    List {
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Close a role; its notes stay
    Close {
        name: String,
        #[arg(short, long)]
        project: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

impl TaskCmd {
    /// The command an agent's argv names, `None` when it parses to no `wsx task` command.
    pub fn parse_argv(argv: &[String]) -> Option<Self> {
        use clap::Parser;
        let args = crate::cli::Args::try_parse_from(
            std::iter::once("wsx").chain(argv.iter().map(String::as_str)),
        )
        .ok()?;
        match args.command {
            Some(crate::cli::Command::Task { subcommand }) => Some(subcommand),
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
            Self::List { .. }
            | Self::Show { .. }
            | Self::Ledger { .. }
            | Self::Deployed { .. }
            | Self::Chat { .. }
            | Self::Search { .. }
            | Self::Check { .. }
            | Self::Manager { .. } => &[Read],
            Self::Post { .. } | Self::Digest { .. } | Self::Say { .. } => &[Notify],
            Self::CheckRun { .. } => &[Result],
            Self::After { .. } | Self::Hold { .. } => &[Backlog],
            Self::Steer { .. } | Self::Add { .. } | Self::Stop { .. } | Self::Preview { .. } => {
                &[Direct]
            }
            Self::New(_) => &[CreateTask],
            Self::Close { discard: true, .. } => &[Discard],
            Self::Close { .. } => &[Close],
            Self::Land { .. } => &[Land, LandChild],
            Self::Trial { .. }
            | Self::Deploy(_)
            | Self::Release(_)
            | Self::Push(_)
            | Self::Project { .. } => &[Deploy],
            Self::Checkin { .. } => &[Routine],
            Self::Routine { action, .. } => match action {
                RoutineCmd::List { .. } | RoutineCmd::Show { .. } | RoutineCmd::Logs { .. } => {
                    &[Read]
                }
                _ => &[Routine],
            },
            Self::Role { action } => match action {
                RoleCmd::List { .. } => &[Read],
                RoleCmd::New { .. } | RoleCmd::Close { .. } => &[RoleAdmin],
            },
            Self::Hand { .. }
            | Self::Handback { .. }
            | Self::Dismiss { .. }
            | Self::Approve { .. }
            | Self::Deny { .. }
            | Self::PreviewHost { .. }
            | Self::LandBatch { .. }
            | Self::Host { .. }
            | Self::BoundaryProfile { .. }
            | Self::ManagerHost { .. } => &[Approve],
        }
    }

    pub fn json_output(&self) -> bool {
        match self {
            Self::New(NewArgs { json, .. })
            | Self::Deploy(DeployArgs { json, .. })
            | Self::Release(ReleaseArgs { json, .. })
            | Self::Push(PushArgs { json, .. })
            | Self::List { json, .. }
            | Self::Show { json, .. }
            | Self::Steer { json, .. }
            | Self::Add { json, .. }
            | Self::Stop { json, .. }
            | Self::Land { json, .. }
            | Self::Hold { json, .. }
            | Self::After { json, .. }
            | Self::Trial { json, .. }
            | Self::Hand { json, .. }
            | Self::Handback { json, .. }
            | Self::Check { json, .. }
            | Self::Close { json, .. }
            | Self::Deployed { json, .. }
            | Self::Preview { json, .. } => *json,
            Self::Role { action } => match action {
                RoleCmd::New { json, .. }
                | RoleCmd::List { json, .. }
                | RoleCmd::Close { json, .. } => *json,
            },
            Self::Project { action } => match action {
                ProjectCmd::Add { json, .. } => *json,
            },
            Self::Manager { json, .. }
            | Self::Say { json, .. }
            | Self::Chat { json, .. }
            | Self::Search { json, .. }
            | Self::Dismiss { json, .. }
            | Self::Approve { json, .. }
            | Self::Deny { json, .. } => *json,
            Self::Ledger { .. } => true,
            Self::Routine { action, .. } => action.json(),
            Self::CheckRun { .. }
            | Self::Digest { .. }
            | Self::Checkin { .. }
            | Self::Post { .. }
            | Self::Host { .. }
            | Self::ManagerHost { .. }
            | Self::BoundaryProfile { .. }
            | Self::PreviewHost { .. }
            | Self::LandBatch { .. } => false,
        }
    }

    /// Commands that read files or a host socket and never need wsxd.
    pub fn without_daemon(&self) -> bool {
        !matches!(
            self,
            Self::New(_)
                | Self::Check {
                    action: Some(CheckAction::Fail { .. }),
                    ..
                }
                | Self::Close { .. }
                | Self::Hand { .. }
                | Self::Handback { .. }
                | Self::Manager { .. }
                | Self::Say { .. }
                | Self::Approve { .. }
        )
    }
}

#[derive(Subcommand)]
pub enum CheckAction {
    /// The named items passed, or every open item with --all (the manager: named items with --owner-said)
    Pass {
        #[command(flatten)]
        chosen: Chosen,
        #[command(flatten)]
        said: OwnerSaid,
    },
    /// Close the named items, or every open item with --all, without a verdict: no fix task
    Skip(Chosen),
    /// The item failed: a new task fixes it, linked to the item
    Fail {
        id: String,
        /// What is wrong; the new task gets it
        #[arg(long)]
        note: String,
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
