//! `wsx task manager-host`: one manager agent per project in a hidden session in the
//! main checkout. It edits no project file but a commit of the owner's own changes on the
//! owner's yes; it answers from the ledger, acts through
//! `wsx task` commands and keeps notes in its own folder. With `--role <name>` the same host
//! runs a role: its brief, its own folder and only its tasks' events, and it leaves once idle.
//! ref: docs/tasks.md "Manager", "Roles"

use super::agent::{
    self, new_session_id, Agent, AgentLine, Launch, TurnResult, KEEP_WARM_PINGS, KEEP_WARM_TEXT,
    PROMPT_CACHE_MS,
};
use super::boundary::Boundary;
use super::claude::{account_env, config_root, skills_plugin, SKILLS_PLUGIN_NAME};
use super::{broker, hosting, limit, notes};
use crate::cli::existing_daemon_snapshot;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::{manager_rotate_limit, TaskWorkflow};
use wsx_core::routine::{store as routines, RunRecord, RunStatus, Trigger};
use wsx_core::runtime;
use wsx_core::task::manager::{
    asks_owner, decided_request, event_wakes, instructions_digest, pending_approvals, quiet_reply,
    resolve_quote, says_nothing, ApprovalLine, QUOTE_REF,
};
use wsx_core::task::manager::{
    Author, Level, LimitHold, ManagerAgent, ManagerEntry, ManagerEvent, ManagerRequest,
    ManagerResponse, ManagerState, Seen, Short,
};
use wsx_core::task::session_news;
use wsx_core::task::stall;
use wsx_core::task::store;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{self, clip, project_tasks, read_result};
use wsx_core::task::{checks, ledger, now_unix_ms, TaskRecord, TaskState, MAX_MESSAGE_BYTES};
use wsx_core::task::{owner, roles, usage};

use wsx_core::task::owner::ACTOR_ENV;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
/// Events within this long of each other, or during a turn, become one message.
const START_RETRY: Duration = Duration::from_secs(30);
const EVENT_QUIET: Duration = Duration::from_secs(3);
/// How often the host removes the captures of long-decided "To check" items.
const CAPTURE_SWEEP: Duration = Duration::from_secs(60 * 60);
/// How often the host looks at wsxd for sessions that need the owner.
const SESSION_LOOK: Duration = Duration::from_secs(2);
/// What wsx asks after a quiet period. The reply is the recap the owner reads.
const RECAP_REQUEST: &str = "The project has been quiet. Write the owner a short recap, at most 8 lines: \
what landed and is live, what runs, and what waits for the owner and why. Read the ledger for it. Start no \
task and change nothing. Name a task by its number and what it does.";
/// The follow-up when a turn thought far more than it said. A turn about events may hold
/// nothing for the owner: it answers `-`. ref: docs/tasks.md "Reply text"
const THOUGHT_ASKED: &str = "Your answer to the owner was only in your thinking. Write it now as \
reply text, or reply - if it held nothing for the owner.";
/// ref: docs/tasks.md "Project overview"
fn overview_too_long(lines: usize, chars: usize) -> String {
    format!(
        "The project overview is {lines} lines / {chars} characters; the cap is {} / {},{:03}. Cut it now: \
keep the goal, open tasks as a mermaid graph, live plans in short and open questions. Finished work \
and old decisions go out; decisions are already in decisions.md.",
        store::MAX_OVERVIEW_LINES,
        store::MAX_OVERVIEW_CHARS / 1000,
        store::MAX_OVERVIEW_CHARS % 1000
    )
}
const EVENT_TITLE_CHARS: usize = 80;
/// The turn after a usage limit: the messages of the turn it ended got no answer yet.
const LIMIT_CONTINUE: &str = "The usage limit has reset. Answer the messages before this one \
that got no reply yet, then any below.";
/// The first turn on another agent: the limit ended the earlier conversation's turn.
const SWITCH_CONTINUE: &str = "The earlier conversation's agent stopped at a usage limit or an \
error before it answered these messages; you continue in a new conversation. Answer them, then \
any below.";
/// What a stand-in is told. ref: docs/tasks.md "Agents"
const STAND_IN: &str = "You stand in for a project's manager for a short while, because its agent \
is at its usage limit. You have no tools: you cannot read files, start tasks or change anything. \
Answer the owner from the notes, the last exchanges and the ledger below, in a few short lines. \
Say what you do not know. Promise nothing. The manager's agent reads your reply and answers the \
owner itself when it returns; say so only if the owner asked for work.";
/// A stand-in that has not answered by then has failed; a failed one is left alone this long.
const STAND_IN_TIMEOUT: Duration = Duration::from_secs(180);
const STAND_IN_RETRY_MS: u64 = 5 * 60_000;
const STAND_IN_LOOK: Duration = Duration::from_secs(10);
/// Agent errors in a row after which the conversation itself may be the cause.
const ERROR_ROTATE_ATTEMPTS: u32 = 3;
/// An idle task runs at most this often.
const IDLE_EVERY_MS: u64 = 24 * 3_600_000;
/// The title of the task `tasks.workflow.idle` starts.
const IDLE_TITLE: &str = "Consolidate project knowledge";
/// A role's host leaves after this long with no message and no turn; `say` starts it again.
const ROLE_LINGER: Duration = Duration::from_secs(20);
/// How often the manager host closes roles idle past `tasks.roles.closeDays`.
const ROLE_SWEEP: Duration = Duration::from_secs(10);
/// A message in a handover: one line, clipped, so a quote never reads as a new message.
fn one_line(text: &str) -> String {
    clip(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        HANDOVER_CHARS,
    )
}

/// The handover of a fresh conversation: this many last messages and replies, each clipped.
const HANDOVER_ENTRIES: usize = 4;
/// A quote by id reads this much of the log's end first, more until it reaches the id.
const QUOTE_READ_BYTES: u64 = 256 * 1024;
/// How much of the log's end a notes search reads for the earlier conversation's exchanges.
const EARLIER_TAIL_BYTES: u64 = 1024 * 1024;
const HANDOVER_CHARS: usize = 600;
const EVENT_SUMMARY_CHARS: usize = 160;
/// A failed deploy's event carries this many last lines of its output.
const DEPLOY_EVENT_LINES: usize = 10;
/// The ledger summary at the end of a turn's message: at most this many task lines.
const SUMMARY_ROWS: usize = 12;
const SUMMARY_TITLE_CHARS: usize = 60;

pub(super) enum Event {
    Request(ManagerRequest, Sender<ManagerResponse>),
    Agent(u64, Option<AgentLine>),
    Tick,
    /// `wsx task new` with `tasks.workflow.idle` ended: when it began, and the task number or
    /// the error.
    IdleStarted(u64, Result<u32, String>),
    /// The agent's `say --from-manager` reached another project's manager, or its
    /// `say --from-role` a role of this project (`to` is `role:<name>`).
    Sent {
        to: String,
        text: String,
        reply: bool,
    },
    /// The events of a role's tasks that its host did not take: the manager hears them.
    Undelivered {
        role: String,
        lines: Vec<(String, Short)>,
    },
}

/// Answers this manager owes other projects' managers: one per message from one of them
/// that a turn delivered. A `say --from-manager --reply` spends one, so a reply is never
/// answered and a message is answered once. ref: docs/tasks.md "Managers talk"
type Owed = std::sync::Arc<std::sync::Mutex<BTreeMap<String, u32>>>;

struct Manager {
    dir: ManagerDir,
    project: String,
    main: PathBuf,
    /// `tasks.manager.adapter`: the headless adapter the manager runs on.
    adapter: String,
    /// `tasks.deploy.auto`. ref: `event_wakes`
    deploy_auto: bool,
    /// A stand-in's call runs: which agent and when it began.
    stand_in: Option<(agent::Choice, Instant)>,
    /// A hold runs for hours: looking for a stand-in reads config and log, so not every tick.
    stand_in_look: Option<Instant>,
    model: String,
    /// `tasks.manager.eventModel`: the model of a turn with only events and no review due.
    event_model: Option<String>,
    /// The model the running agent process uses now.
    agent_model: String,
    /// `tasks.manager.rotateTokens` when set; `rotate_limit` is the size that counts.
    rotate_tokens: Option<u64>,
    /// `tasks.manager.effort`, passed to every launch.
    effort: String,
    state: ManagerState,
    events: Sender<Event>,
    agent: Option<Agent>,
    generation: u64,
    /// Logged messages not yet delivered, oldest first.
    queue: VecDeque<(u64, Author, String)>,
    /// The ids of the messages in the running turn.
    turn: Option<Vec<u64>>,
    /// The running turn holds only events: its reply is quiet.
    turn_events: bool,
    pane: Option<u64>,
    started: Instant,
    /// Task states as last seen, including ones not yet reported.
    observed: BTreeMap<u32, Seen>,
    /// Approval decisions seen, including ones not yet reported.
    observed_decisions: u64,
    /// The `unix_ms` of the newest deploy record seen.
    observed_deploys_ms: u64,
    /// The `finished_epoch` of the newest routine run seen.
    observed_routines: i64,
    /// The stall reported for each open task, by marker, including ones not yet logged.
    observed_stalls: BTreeMap<u32, String>,
    /// The stall each task shows now, and since when: it is reported after `stall::GRACE`.
    stall_suspects: BTreeMap<u32, (String, Instant)>,
    stalls_swept: Option<Instant>,
    sessions_looked: Option<Instant>,
    /// Event lines not yet logged as a message, each with the chat's short form of it, and
    /// when the last one came.
    pending_events: Vec<(String, Short)>,
    last_event: Option<Instant>,
    /// `tasks.manager.recapMinutes` as a duration; `None` is off.
    recap_quiet: Option<Duration>,
    /// When a message was logged, a turn ran or the host last looked for a recap.
    last_activity: Instant,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<hosting::Binary>,
    reexec: bool,
    /// When the host last removed old captures; `None` sweeps on the first tick.
    captures_swept: Option<Instant>,
    owed: Owed,
    /// When the agent last failed to start a turn; the next try waits `START_RETRY`.
    start_failed: Option<Instant>,
    /// Event lines of tasks a role created, by role, not yet sent to it.
    role_events: BTreeMap<String, Vec<(String, Short)>>,
    roles_swept: Option<Instant>,
    /// A role host that is closed or idle: it leaves.
    leaving: bool,
    /// The host asked for a shorter overview since the last owner message or in-cap result.
    overview_asked: bool,
    /// Like `overview_asked`, for the request to compact `context.md`.
    context_asked: bool,
    /// Like `overview_asked`, for the request to write an answer that sat in thinking.
    thought_asked: bool,
    /// Ping turns since the last real turn, and whether the running turn is one.
    /// ref: docs/tasks.md "Manager"
    pings: u32,
    pinging: bool,
}

/// The plugin that lends the manager the owner's skills, `manager/<this>/`.
/// The manager's lean project context in its notes folder. ref: docs/tasks.md "Memo"
pub(super) const CONTEXT_FILE: &str = "context.md";
/// The owner's decisions, one line each, appended by the manager as they are made.
pub(super) const DECISIONS_FILE: &str = "decisions.md";
/// A conversation idle this long lost Claude's prompt cache (1 hour): past `IDLE_ROTATE_TOKENS`
/// of context, a fresh conversation costs less than the cold resume.
const IDLE_ROTATE_MS: u64 = 60 * 60_000;
const IDLE_ROTATE_TOKENS: u64 = 60_000;
/// Changed instructions go to a warm conversation under this context as a message.
const INSTRUCTION_MESSAGE_TOKENS: u64 = 100_000;
// ^ The agent writes the notes: a link or a huge file must not reach the host's read.
const MAX_NOTES_READ: u64 = 256 * 1024;

/// ref: docs/tasks.md "Manager", "Memo"
fn instructions(
    project: &str,
    notes: &Path,
    overview: &Path,
    skills: bool,
    workflow: &TaskWorkflow,
) -> String {
    // ^ No skill is named: the owner's skills are whatever their folder holds.
    let skills = match skills {
        true => format!(
            "The owner's skills are yours with the Skill tool as `{SKILLS_PLUGIN_NAME}:<name>`. "
        ),
        false => String::new(),
    };
    format!(
        "You are the manager of the wsx project {project}. You never edit the project's files or run builds or tests; \
         workers do that in tasks. Keep your notes in {notes}. \
         Messages headed 'Owner' come from the owner; 'Events' come from \
         wsx. Each message ends with wsx's ledger summary. The ledger is the truth, not \
         your memory: run `wsx task ledger -p {project}` for more. Every `wsx task` command takes `-p {project} --json`, also in a pipe or \
         with a heredoc; no other `wsx` command runs. Name tasks as #<n>. \
         Talk like a person in a chat, not a report: answer each Owner message first, every \
         question in it, in a few short lines. Define each name the owner has not used; \
         when an answer seems to contradict an earlier one, say how both hold. {presentation} \
         A turn of only Events is your work. Act on it, then \
         reply with one short line of what you did, or with exactly `-` when nothing \
         changed for the owner; when it finishes what you said you would report, report \
         it. A routine line marked 'for the owner' is answered with a short message, \
         never `-`. Write more only when the owner must decide now or a talk goes on, and \
         end the reply with one plain question. Never hand the owner chores (commands, files, settings, tasks to \
         verify): do such work through a task, or drop it. \
         Judge from the owner's words whether the owner wants to talk it over (a question, \
         'let's discuss', 'one by one') or wants work ('fix', 'go', 'agreed, do it'). To talk, \
         stay in chat: read the code yourself, answer the points in turn (one per reply when \
         asked one by one) with your view and the question that moves it on; no task, `add` or \
         `steer` for them until the owner agrees, then one `wsx task new` or `add` with all. \
         A request for work starts at once, with the plan in a few lines in the same reply \
         (goal, tasks, choices, what you leave out); ask first only when a choice the owner \
         has not made changes what the owner gets, or the work is large or costly to undo \
         (then `--plan` or `--hold`). A longer plan goes on the project overview with a mermaid \
         diagram, and the reply links it. Before `wsx task new`, compare the request with \
         the open tasks (titles and files): similar work or the same area goes to that task \
         with `wsx task add`; a new task only for separate work, run at once. Start one \
         with `wsx task new --title <title> --files <glob> \"<prompt>\"`: the title is a few plain words, \
         never starting with 'Owner'; pass one `--files` per path or \
         glob it will change, in every task new (`wsx task add <n> --files <glob> \
         \"<why>\"` adds more). The prompt stands alone, since a worker cannot ask: the goal, \
         the owner's decisions, the scope (a fix covers every project and agent adapter its \
         cause reaches, not one owner's config or one routine), the files and how it \
         proves the result. A change you \
         can state (what changes, where, and how it proves itself) runs with `--size small`, \
         also when its prompt only names the area; only an unknown cause, a change to a \
         module's design or work that is costly to undo runs without it. A broad hunt (find all bugs) is first one read-only audit task per area, then one \
         task per fix. With \
         `--plan` the worker writes a short plan and the task waits on `plan`; read it with \
         `wsx task show <n> --json` and continue with `wsx task add <n> \"go\"` or what to \
         change. Never ask the owner to wait. Never restate a plan, promise or wait: open with \
         `> message <id>`, then only what is new. `wsx task add` or \
         `wsx task steer` changes a running task. \
         A ready task waits for your review when you created it, or the owner did while the \
         ledger's `auto_land` is true and checks ran. Read its diff (`git -C <working copy> \
         diff <base>..HEAD`; `wsx task show <n> --json` names both and its `log`) against \
         the owner's words, the task's prompt and each later add (the log's `message` lines), \
         and for lean code: no copy of a helper that exists, no dead code, nothing the \
         request did not ask for. Then `wsx task land <n>`, or `wsx task add <n> \
         \"<what to change>\"` naming each ask it missed. A land rebases and runs the checks \
         again by itself: never send a task back to rebase. Report checks from the ledger's \
         `checks` and trust them over the worker's words; pass on a worker's cause, count \
         or cost only after you checked it (its log, a command), or call it unchecked. \
         {review}When a worker leaves for a \
         person what a command, a test or a capture could prove, tell that worker to prove \
         it. What only a person can check goes to the To check list (the ledger's \
         `to_check`); never skip or fail items. Pass one with `wsx task check pass <id> \
         --owner-said <message>` only when the owner's message of this turn says the thing \
         looks good and the item is clearly about that same thing; when in doubt, leave it \
         open and ask. Name each item (no `--all`) and tell the owner in one line which items \
         you passed. A release refuses while items are \
         open, unless the owner's message asks for it (`--owner-said`): then the items stay \
         to check on the release. `deploy` and `release` answer at once; the \
         result comes as an event. `wsx task routine` lists and runs the project's \
         routines (results as events and in the ledger's `routines`); add, edit and delete \
         need `--owner-said`. When you wait for something no event reports (an outside \
         review, a time of day), set `wsx task checkin --in 40m --why \"<what to \
         look at>\"` (or `--at HH:MM`): its why comes back as an event then, and a new \
         check-in replaces the last. When the owner wants to be sure of a change or to try it, start \
         it with `wsx task new --hold \"<why>\"` (`wsx task hold <n> \"<why>\"` holds a \
         running one, `--off` releases it). \
         Long multi-step work goes into one task: each step is `wsx task new --into <n>`, \
         which holds #<n>; land a step when an event names it ready, and #<n> once the owner \
         says so. Many tasks of one area (a parent with 3 or more \
         children) may get a role, a second manager with its own chat: ask the owner once, \
         then `wsx task role new <name> --brief <file>`. \
         Say in one line how to try it: Try live (`wsx task trial <n>`), or a preview \
         (`wsx task preview <n>`) when the live site cannot roll back from it (a \
         daemon, a migration). \
         When the owner's message in this turn asks for the action itself (\"land #5\", \
         \"close #42\"), pass `--owner-said <id>`, the id in its 'Owner (message \
         <id>)' heading, to `land`, `close --discard`, `deploy` or `release`; never name a \
         message that did not ask for that action. When a command says the owner was asked, \
         say so. An event 'approval <id> (...) dropped': \
         tell the owner why in one line. A land needs no approval, and the project deploys \
         by itself when the ledger's `deploys` say so; call a change live only when \
         `deploys` shows its commit. \
         A task `waiting` on `owner` asked or stopped: answer with `wsx task add` when you \
         can, else ask the owner in plain words; on `disk` or `limit` it resumes by \
         itself. An event '#<n> stalled' is wsx's \
         finding, with the evidence, that the task is stuck: act with `wsx task` (a silent \
         worker: `stop`, then `add` what to do), start a task that fixes wsx when the evidence \
         shows its bug, else tell the owner in one line what is stuck. On \
         `hand`, the owner works in its copy: never add, steer, land or close it; only the \
         owner runs `wsx task hand` and `handback`. \
         Nothing asks you for permission inside wsx's boundary. Read the \
         project and the copies with the file and web tools and read-only git in Bash. The \
         one git command that writes: when a land refuses because the main checkout has the \
         owner's uncommitted changes, name those files and ask; only after the owner says \
         yes, commit exactly those files in the main checkout (`git commit -m \"<message \
         that names them>\" -- <files>`) and land again. {skills}Use the skills and subagents that fit; check rather than guess. \
         When the owner decides a rule, a preference or a choice, append one line \
         `- <YYYY-MM-DD> <decision>` to {decisions} at once. Every new conversation of yours \
         starts with that file and {context}. wsx adds each \
         finished task's memo to {context}; fix or merge a line there only when you see it is \
         wrong. Only events that need you start a turn; a landing, a close, a wait that ends \
         by itself and an automatic deploy come along with your next turn. The project overview {overview} is the owner's one-screen picture, at most \
         60 lines and 4,000 characters: the goal, open tasks as a mermaid graph, live plans in short, \
         open questions. \
         Update a section in place; never create another overview. \
         You may read another project's `wsx task ledger -p \
         <project>` and message its manager with `wsx task say -p <project> --from-manager \
         \"<text>\"`; never create or change its tasks. A message headed 'Manager <project>' \
         asks you something: answer it once with `wsx task say -p <project> --from-manager \
         --reply \"<answer>\"`. 'Manager <project> replies' answers yours: use it and do not \
         answer it. '@<project>' in an owner message asks you to consult that manager the \
         same way and use its reply. Roles talk the same way inside the project with \
         `--role <name> --from-role`; answer a message headed 'Role <name>' once with `--reply`.",
        review = match workflow.review.is_empty() {
            true => String::new(),
            false => format!("{} ", workflow.review),
        },
        presentation = wsx_core::integration::PRESENTATION.trim_end(),
        context = notes.join(CONTEXT_FILE).display(),
        decisions = notes.join(DECISIONS_FILE).display(),
        notes = notes.display(),
        overview = overview.display()
    )
}

/// A role's instructions open with this, its brief, then the manager's.
/// ref: docs/tasks.md "Roles"
fn role_preamble(role: &str, project: &str, brief: &str) -> String {
    format!(
        "You are {role}, a role in the project {project}: a second manager for one area, \
         with the owner's brief below. The manager's instructions after it are yours, with these \
         differences. You hear the events only of the tasks you create, and act only on them; \
         you land only a child into its parent task. The manager keeps every other task, the \
         recap, approvals, deploys, releases, routines and the project overview. You never create a \
         role or change the project's config. Message the manager with `wsx task say \
         --from-role \"<text>\"`; a message headed 'Manager' or 'Role <name>' asks you \
         something: answer it once with `wsx task say --role <name> --from-role --reply \
         \"<answer>\"` (`--role manager` for the manager).\n\nThe owner's brief:\n{brief}\n\n\
         The manager's instructions:\n"
    )
}

/// The manager's boundary profile as text. ref: scripts/test-manager-boundary.py
pub fn print_profile(project: &str, main: &Path) -> Result<()> {
    let project_id = store::project_id(project, main);
    let dir = ManagerDir::open(&project_id)?;
    let overview = store::ensure_overview(&project_id, project)?;
    print!(
        "{}",
        Boundary::manager(main, &dir.path, &overview)?.profile()
    );
    Ok(())
}

pub fn run(project_id: &str, role: Option<&str>) -> Result<()> {
    let (project, main) = super::project_by_id(project_id)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let dir = match role {
        None => ManagerDir::open(project_id)?,
        Some(role) => roles::open_role(project_id, role)?,
    };
    let listener = hosting::bind(&dir.socket_path())?;
    let brokered = hosting::bind(&dir.broker_path())?;
    let state = dir.read_state()?;
    let (events, receiver) = channel();
    let mut manager = Manager {
        dir,
        project,
        main,
        adapter: settings.manager_adapter,
        deploy_auto: settings.deploy_auto,
        stand_in: None,
        stand_in_look: None,
        model: settings.manager_model,
        effort: settings.manager_effort,
        event_model: settings.manager_event_model,
        agent_model: String::new(),
        rotate_tokens: settings.manager_rotate_tokens,
        state,
        events: events.clone(),
        agent: None,
        generation: 0,
        queue: VecDeque::new(),
        turn: None,
        turn_events: false,
        pane: hosting::own_pane(),
        started: Instant::now(),
        observed: BTreeMap::new(),
        observed_decisions: 0,
        observed_deploys_ms: 0,
        observed_routines: 0,
        observed_stalls: BTreeMap::new(),
        stall_suspects: BTreeMap::new(),
        stalls_swept: None,
        sessions_looked: None,
        pending_events: Vec::new(),
        last_event: None,
        recap_quiet: (settings.manager_recap_secs > 0)
            .then(|| Duration::from_secs(settings.manager_recap_secs)),
        last_activity: Instant::now(),
        binary: hosting::Binary::capture(),
        reexec: false,
        captures_swept: None,
        owed: Owed::default(),
        start_failed: None,
        role_events: BTreeMap::new(),
        roles_swept: None,
        leaving: false,
        overview_asked: false,
        context_asked: false,
        thought_asked: false,
        pings: 0,
        pinging: false,
    };
    manager.restore()?;
    let requests = events.clone();
    std::thread::spawn(move || {
        hosting::serve(
            listener,
            requests,
            Event::Request,
            MAX_REQUEST_BYTES,
            Duration::from_secs(15),
        )
    });
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let folder = manager.main.clone();
    let (own, owed, sent, actor) = (
        manager.project.clone(),
        manager.owed.clone(),
        events.clone(),
        manager.actor(),
    );
    std::thread::spawn(move || {
        broker::serve_manager(
            brokered,
            exe,
            folder,
            own,
            actor,
            owed,
            move |to, text, reply| {
                let _ = sent.send(Event::Sent { to, text, reply });
            },
        )
    });
    std::thread::spawn(move || {
        while events.send(Event::Tick).is_ok() {
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    manager.say(&format!(
        "{} of {} in {}",
        manager.dir.name(),
        manager.project,
        manager.main.display()
    ));
    manager.report(runtime::AgentState::Idle);
    manager.run(receiver)?;
    if manager.leaving {
        // ^ The sockets go first: a `say` during the agent's stop (up to 5 s) starts the next
        // host, which refuses to bind while this one still accepts on the same path.
        let _ = std::fs::remove_file(manager.dir.socket_path());
        let _ = std::fs::remove_file(manager.dir.broker_path());
        if let Some(agent) = manager.agent.take() {
            agent.stop();
        }
        return Ok(());
    }
    if let (true, Some(binary)) = (manager.reexec, &manager.binary) {
        if let Some(agent) = manager.agent.take() {
            agent.stop();
        }
        let _ = std::fs::remove_file(manager.dir.socket_path());
        let _ = std::fs::remove_file(manager.dir.broker_path());
        let error = binary.exec();
        anyhow::bail!("cannot start the replaced wsx binary: {error}");
    }
    Ok(())
}

impl Manager {
    fn run(&mut self, receiver: Receiver<Event>) -> Result<()> {
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Request(request, reply) => {
                    let _ = reply.send(self.handle(request));
                }
                Event::Agent(generation, line) if generation == self.generation => {
                    self.on_agent(line)
                }
                Event::Agent(..) => {}
                // ^ A role hears only what the manager host sends it.
                Event::Tick
                    if self
                        .stand_in
                        .as_ref()
                        .is_some_and(|(_, began)| began.elapsed() >= STAND_IN_TIMEOUT) =>
                {
                    self.stand_in_done(None)
                }
                Event::Tick if self.dir.role.is_some() => self.leave_when_done(),
                Event::Tick => {
                    self.watch_tasks();
                    self.watch_stalls();
                    self.watch_sessions();
                    self.ask_recap();
                    self.sweep_captures();
                    self.close_idle_roles();
                    self.ping();
                }
                Event::IdleStarted(began, Ok(id)) => {
                    self.say(&format!("started idle task #{id}"));
                    self.state.idle_task = Some(id);
                    self.state.idle_ms = Some(began);
                    self.save();
                }
                // ^ The next idle period tries again.
                Event::IdleStarted(_, Err(error)) => {
                    self.say(&format!("cannot start the idle task: {error}"))
                }
                Event::Sent { to, text, reply } => self.log(ManagerEvent::Sent { to, text, reply }),
                Event::Undelivered { role, lines } => {
                    let head = format!(
                        "role {role} did not take the events of its tasks, so they are yours:"
                    );
                    self.pending_events.push(plain_event(head));
                    self.pending_events.extend(lines);
                    self.last_event = Some(Instant::now());
                }
            }
            if self.turn.is_none() {
                self.flush_events();
                self.next_turn();
            } else if !self.pinging {
                self.last_activity = Instant::now();
            }
            if self.reexec {
                std::thread::sleep(Duration::from_millis(200));
                return Ok(());
            }
            if self.leaving {
                return Ok(());
            }
        }
        Ok(())
    }

    fn handle(&mut self, request: ManagerRequest) -> ManagerResponse {
        let busy = self.turn.is_some();
        match request {
            ManagerRequest::Status => ManagerResponse {
                ok: true,
                id: None,
                busy,
                message: format!("{} message(s) waiting", self.queue.len()),
            },
            ManagerRequest::Say { text, from, short } => {
                match self.accept(from.unwrap_or(Author::Owner), text, short) {
                    Ok(id) => ManagerResponse {
                        ok: true,
                        id: Some(id),
                        busy,
                        message: if busy {
                            "queued until the manager's turn ends".into()
                        } else {
                            "sent to the manager".into()
                        },
                    },
                    Err(error) => ManagerResponse {
                        ok: false,
                        id: None,
                        busy,
                        message: format!("{error:#}"),
                    },
                }
            }
            ManagerRequest::Post { event, text, level } => {
                let refusal = match (busy, text.trim().is_empty(), text.len() > MAX_MESSAGE_BYTES) {
                    (true, ..) => Some("a manager turn runs"),
                    (_, true, _) => Some("the text is empty"),
                    (.., true) => Some("the text is larger than the limit"),
                    _ => None,
                };
                if refusal.is_none() {
                    self.post(event, text, level);
                }
                ManagerResponse {
                    ok: refusal.is_none(),
                    id: None,
                    busy,
                    message: refusal.unwrap_or("posted to the chat").into(),
                }
            }
        }
    }

    /// Write to the owner with no turn: a delivered events message, so that no restart hands it
    /// to the agent, and the reply to it, which the web pushes like the manager's own. The reply
    /// has no usage and is marked `post`, which the chats read.
    fn post(&mut self, event: String, text: String, level: Option<Level>) {
        let id = self.next_id();
        self.log(ManagerEvent::Message {
            id,
            from: Author::Events,
            text: event,
            short: Vec::new(),
        });
        self.log(ManagerEvent::Delivered { ids: vec![id] });
        let reply = self.next_id();
        self.log(ManagerEvent::Reply {
            id: reply,
            to: vec![id],
            text,
            usage: None,
            quiet: false,
            asks: false,
            provisional: None,
            level,
            post: true,
        });
    }

    /// Re-queue messages logged but never delivered, and those delivered to a turn that got no
    /// reply, and pick up what was last reported.
    /// ^ No event is lost across a restart: a change reported in no logged message is
    /// found again because `seen` is saved only when its message is logged. A turn ends with
    /// a reply naming its messages in `to`: the host killed or the agent lost in between leaves
    /// them unanswered. Only deliveries after the last agent reply count (older unanswered ones
    /// ended in `stopped` long ago), and a limit hold keeps its own ids, which the turn after
    /// the limit delivers.
    fn restore(&mut self) -> Result<()> {
        let log = self.dir.read_log()?;
        let after = log
            .iter()
            .rposition(|entry| matches!(&entry.event, ManagerEvent::Reply { usage: Some(_), .. }))
            .map_or(0, |at| at + 1);
        let answered: std::collections::HashSet<u64> = log
            .iter()
            .flat_map(|entry| match &entry.event {
                ManagerEvent::Reply { to, .. } => to.clone(),
                _ => Vec::new(),
            })
            .chain(self.state.limit.iter().flat_map(|hold| hold.ids.clone()))
            .collect();
        let delivered: std::collections::HashSet<u64> = log
            .iter()
            .enumerate()
            .flat_map(|(at, entry)| match &entry.event {
                ManagerEvent::Delivered { ids } if at < after => ids.clone(),
                ManagerEvent::Delivered { ids } => ids
                    .iter()
                    .copied()
                    .filter(|id| answered.contains(id))
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        // ^ A state from before `recap_ms`: the last recap request, else now.
        if self.state.recap_ms.is_none() {
            self.state.recap_ms = Some(
                log.iter()
                    .rev()
                    .find(|entry| {
                        matches!(&entry.event, ManagerEvent::Message { from, .. } if *from == Author::Recap)
                    })
                    .map_or_else(now_unix_ms, |entry| entry.unix_ms),
            );
            self.save();
        }
        for entry in log {
            if let ManagerEvent::Message { id, from, text, .. } = entry.event {
                if !delivered.contains(&id) {
                    self.enqueue(id, from, text);
                }
            }
        }
        match self.state.seen.clone() {
            Some(seen) => {
                self.observed = seen;
                self.observed_decisions = self.state.decisions_seen;
                self.observed_stalls = self.state.stalls.clone();
                self.observed_deploys_ms = self
                    .state
                    .deploys_seen_ms
                    .unwrap_or_else(|| self.newest_deploy_ms());
                self.observed_routines = self
                    .state
                    .routines_seen
                    .unwrap_or_else(|| self.newest_routine_run());
            }
            None => {
                // ^ First start: what already happened is the baseline, not news.
                self.observed = self.current_tasks().into_iter().collect();
                self.observed_decisions = self.approval_lines().len() as u64;
                self.observed_deploys_ms = self.newest_deploy_ms();
                self.observed_routines = self.newest_routine_run();
                self.state.seen = Some(self.observed.clone());
                self.state.decisions_seen = self.observed_decisions;
                self.state.deploys_seen_ms = Some(self.observed_deploys_ms);
                self.state.routines_seen = Some(self.observed_routines);
                self.save();
            }
        }
        Ok(())
    }

    fn current_tasks(&self) -> Vec<(u32, Seen)> {
        project_tasks(&self.dir.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, record)| record.ok())
            .map(|record| (record.id, Seen::of(&record)))
            .collect()
    }

    fn approval_lines(&self) -> Vec<ApprovalLine> {
        store::read_approvals(&self.dir.project_id).unwrap_or_default()
    }

    /// Drop pending requests whose task left or moved on, then turn new decisions into event
    /// lines, except those the owner's chat message approved: the manager ran them itself.
    fn watch_approvals(&mut self) {
        let project_id = self.dir.project_id.clone();
        let mut lines = self.approval_lines();
        let stale: Vec<(String, String)> = pending_approvals(&lines)
            .into_iter()
            .filter_map(|request| {
                let ApprovalLine::Request {
                    task: Some(task), ..
                } = request
                else {
                    return None;
                };
                let record = store::TaskDir::open(&project_id, *task)
                    .and_then(|dir| dir.read_record())
                    .ok()?;
                owner::stale_reason(request, &record).map(|why| (request.id().to_string(), why))
            })
            .collect();
        if !stale.is_empty() {
            for (id, why) in stale {
                if let Err(error) = owner::drop_request(&project_id, &id, &why) {
                    self.say(&format!("cannot drop approval {id}: {error:#}"));
                }
            }
            lines = self.approval_lines();
        }
        let decisions: Vec<usize> = (0..lines.len())
            .filter(|at| matches!(lines[*at], ApprovalLine::Decision { .. }))
            .collect();
        for at in decisions.iter().skip(self.observed_decisions as usize) {
            let ApprovalLine::Decision {
                id,
                approved,
                note,
                outcome,
                owner_said: None,
                ..
            } = &lines[*at]
            else {
                continue;
            };
            let asked = match decided_request(&lines, *at) {
                Some(ApprovalLine::Request {
                    action, task, role, ..
                }) => format!(
                    "{}{}",
                    format!("{action:?}").to_ascii_lowercase(),
                    match (task, role) {
                        (Some(task), _) => format!(" #{task}"),
                        (None, Some(role)) => format!(" {role}"),
                        (None, None) => String::new(),
                    }
                ),
                _ => String::new(),
            };
            let (verdict, short) = match (approved, outcome.strip_prefix(owner::DROPPED)) {
                (true, _) => (format!("approved; {outcome}"), format!("Approved {asked}")),
                (false, Some(why)) => (outcome.clone(), format!("Dropped {asked}: {why}")),
                (false, None) => (
                    format!("denied; {}", note.as_deref().unwrap_or("no note")),
                    format!("Denied {asked}"),
                ),
            };
            self.pending_events.push((
                format!(
                    "approval {id} ({asked}) {}",
                    clip(&verdict, EVENT_SUMMARY_CHARS * 2)
                ),
                Short {
                    text: clip(&short, EVENT_SUMMARY_CHARS * 2),
                    task: None,
                },
            ));
            self.last_event = Some(Instant::now());
        }
        self.observed_decisions = decisions.len() as u64;
    }

    fn newest_deploy_ms(&self) -> u64 {
        store::read_deploys(&self.dir.project_id)
            .ok()
            .and_then(|records| records.iter().map(|record| record.unix_ms).max())
            .unwrap_or(0)
    }

    /// Turn each new failed deploy or release into an event line with the end of its output, and
    /// each new successful one into one line.
    /// ref: docs/tasks.md "Manager"
    fn watch_deploys(&mut self) {
        let Ok(records) = store::read_deploys(&self.dir.project_id) else {
            return;
        };
        for record in &records {
            if record.unix_ms <= self.observed_deploys_ms {
                continue;
            }
            self.observed_deploys_ms = record.unix_ms;
            // ^ Deploys and releases run detached for minutes: a success is news too, or a
            // manager that waits for one never hears that it may go on.
            let outcome = match record.exit == Some(0) {
                true => "succeeded",
                false => "failed",
            };
            self.fire(
                &format!("{}.{outcome}", ledger::kind_name(record.kind)),
                format!("{}-{}", ledger::kind_name(record.kind), record.unix_ms),
                json!({ "commit": record.commit, "exit": record.exit }),
            );
            if record.exit == Some(0) {
                let line = format!(
                    "{} of {:.12} succeeded after {} s",
                    ledger::kind_name(record.kind),
                    record.commit,
                    record.duration_ms / 1000
                );
                self.pending_events.push(plain_event(line));
                self.last_event = Some(Instant::now());
                continue;
            }
            let failed = format!(
                "{} of {:.12} failed ({})",
                ledger::kind_name(record.kind),
                record.commit,
                exit_words(record.exit),
            );
            let line = format!(
                "{failed}{}",
                output_ends(record.output.as_deref().unwrap_or_default())
            );
            self.pending_events.push((line, Short::plain(&failed)));
            self.last_event = Some(Instant::now());
        }
    }

    /// The project's routine runs that ended, as wsxd's scheduler recorded them.
    fn finished_routine_runs(&self) -> Vec<RunRecord> {
        self.stored_routines()
            .runs
            .into_values()
            .flatten()
            .filter(|run| run.finished_epoch.is_some())
            .collect()
    }

    fn newest_routine_run(&self) -> i64 {
        self.finished_routine_runs()
            .iter()
            .filter_map(|run| run.finished_epoch)
            .max()
            .unwrap_or(0)
    }

    /// Make an event of each routine run that ended: for a routine that posts, one the manager
    /// answers to the owner in a turn of its own ([`routine_event`]); for one that does not, the
    /// end of its output for the manager's work. A success that printed nothing to stdout is no
    /// event and costs no turn. ref: docs/tasks.md "Routines"
    fn watch_routines(&mut self) {
        // ^ An event inside a running turn would read as its end: the runs wait for the turn.
        if self.turn.is_some() {
            return;
        }
        // ^ A run that ends in this second may be recorded after this read: wait the second out.
        let settled = (now_unix_ms() / 1000) as i64 - 1;
        let mut runs: Vec<RunRecord> = self
            .finished_routine_runs()
            .into_iter()
            .filter(|run| {
                run.finished_epoch
                    .is_some_and(|end| end > self.observed_routines && end < settled)
            })
            .collect();
        runs.sort_by_key(|run| run.finished_epoch);
        let quiet: std::collections::HashSet<String> = match runs.is_empty() {
            true => Default::default(),
            false => self
                .stored_routines()
                .routines
                .into_iter()
                .filter(|routine| !routine.post)
                .map(|routine| routine.name)
                .collect(),
        };
        for run in runs {
            self.observed_routines = run.finished_epoch.unwrap_or_default();
            if run.status == RunStatus::Succeeded && stdout_empty(&run) {
                continue;
            }
            let line = match quiet.contains(&run.routine) {
                true => routine_line(&run),
                false => routine_event(&run, &self.main),
            };
            self.pending_events.push(plain_event(line));
            self.last_event = Some(Instant::now());
        }
    }

    /// Post a project agent session that entered `exited`, `error` or `blocked`, once per
    /// entry, with no turn. The first look is the baseline, not news. A turn that runs holds
    /// the post: nothing is marked seen, so the next free look posts it.
    /// ref: docs/tasks.md "Session news"
    fn watch_sessions(&mut self) {
        if self.turn.is_some()
            || self
                .sessions_looked
                .is_some_and(|at| at.elapsed() < SESSION_LOOK)
        {
            return;
        }
        self.sessions_looked = Some(Instant::now());
        // ^ No wsxd, no change: a sleeping daemon must not read as every session gone.
        let Ok(snapshot) = existing_daemon_snapshot() else {
            return;
        };
        let found = session_news::attention(&snapshot, &self.main);
        let (fresh, keep) = session_news::unposted(found, self.state.sessions_seen.as_ref());
        for news in fresh {
            self.post(news.event, news.text, None);
        }
        if self.state.sessions_seen.as_ref() != Some(&keep) {
            self.state.sessions_seen = Some(keep);
            self.save();
        }
    }

    /// Remove the capture files of items decided more than 7 days ago, at the start and hourly.
    /// ref: docs/tasks.md "To check"
    fn sweep_captures(&mut self) {
        if self
            .captures_swept
            .is_some_and(|at| at.elapsed() < CAPTURE_SWEEP)
        {
            return;
        }
        self.captures_swept = Some(Instant::now());
        match checks::remove_old_captures(&self.dir.project_id) {
            Ok(0) => {}
            Ok(removed) => self.say(&format!(
                "removed {removed} capture file(s) of decided items"
            )),
            Err(error) => self.say(&format!("cannot remove old captures: {error:#}")),
        }
    }

    /// Turn task state changes into event lines.
    fn watch_tasks(&mut self) {
        self.watch_approvals();
        self.watch_deploys();
        self.watch_routines();
        let Ok(tasks) = project_tasks(&self.dir.project_id) else {
            return;
        };
        let open_roles = self.open_role_names();
        // ^ A child's land waits for its parent at rest: the parent's event names them.
        // ref: docs/tasks.md "Parent tasks"
        let ready_children: Vec<(u32, u32)> = tasks
            .iter()
            .filter_map(|(_, record)| record.as_ref().ok())
            .filter(|record| record.state == TaskState::Ready)
            .filter_map(|record| record.parent.map(|parent| (parent, record.id)))
            .collect();
        for (dir, record) in tasks {
            let Ok(record) = record else { continue };
            let now = Seen::of(&record);
            if self.observed.get(&record.id) == Some(&now) {
                continue;
            }
            self.observed.insert(record.id, now);
            if !view::is_event(&record) {
                continue;
            }
            let title = view::title(&dir, &record).unwrap_or_default();
            let summary = {
                // ^ The checks the host ran come before the worker's own words.
                let result = read_result(&dir).unwrap_or_default();
                let checks = view::check_outcomes(&result);
                let passed = checks
                    .iter()
                    .filter(|(_, outcome)| *outcome == "passed")
                    .count();
                let words = result["result"]["summary"].as_str().unwrap_or_default();
                let worker = format!("worker says: {}", clip(words, EVENT_SUMMARY_CHARS));
                // ^ A check-run that passed after the worker's result is in the evidence, not in
                // its words, which may still say the run is pending.
                let recorded: String = result["result"]["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry["check"].as_str())
                    .filter(|check| check.ends_with(super::RECORDED_BY_HOST))
                    .filter_map(|check| check.strip_prefix("wsx task "))
                    .map(|check| format!("{check}; "))
                    .collect();
                let mut summary = match checks.len() {
                    0 => format!("{recorded}{worker}"),
                    total => format!("checks {passed} of {total} passed; {recorded}{worker}"),
                };
                // ^ `waits on <on>` is the record's key, which `event_wakes` tests; the
                // sentence is the words every view shows (docs/tasks.md "Status").
                if let Some(waiting) = &record.waiting {
                    let status = wsx_core::task::status::status(
                        &dir,
                        &record,
                        &Default::default(),
                        wsx_core::task::now_unix_ms(),
                    );
                    summary = format!(
                        "waits on {}: {}; {summary}",
                        waiting.on.as_str(),
                        status.sentence
                    );
                }
                let children: Vec<u32> = ready_children
                    .iter()
                    .filter(|(parent, _)| *parent == record.id)
                    .map(|(_, child)| *child)
                    .collect();
                if record.at_rest() && !children.is_empty() {
                    summary.push_str(&format!(
                        "; children ready to land: {}",
                        view::task_list(children)
                    ));
                }
                if let Some(hold) = record.hold.as_ref().filter(|_| record.state.is_open()) {
                    summary.push_str(&format!(
                        "; held for the owner: {}",
                        clip(hold, EVENT_SUMMARY_CHARS)
                    ));
                }
                // ^ The worker's memo reaches the manager when its task leaves, never later:
                // the ledger lists only the last few. ref: docs/tasks.md "Memo"
                if !record.state.is_open() {
                    let memo = result["result"]["memo"].as_str().unwrap_or_default();
                    let memo = memo.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !memo.is_empty() {
                        let memo = clip(&memo, ledger::MEMO_CHARS);
                        self.fold_memo(record.id, &memo);
                        summary.push_str(&format!("; memo: {memo}"));
                    }
                }
                // ^ A landing adds its Human Verify items to the project's To check list.
                if record.state == TaskState::Landed {
                    let open = checks::open_items(&self.dir.project_id).map_or(0, |items| {
                        items.iter().filter(|item| item.task == record.id).count()
                    });
                    if open > 0 {
                        summary.push_str(&format!("; {open} to check"));
                    }
                }
                summary
            };
            let line = format!(
                "#{} {} r{}: {}; {}",
                record.id,
                record.state.as_str(),
                record.revision,
                clip(&title, EVENT_TITLE_CHARS),
                summary,
            );
            // ^ The chat says `done` for a landing (docs/web-client-tasks.md "Project screen").
            let words = match record.state {
                TaskState::Landed => "done",
                state => state.as_str(),
            };
            self.task_event(&record, &open_roles, line, Short::task(record.id, words));
            let state = record.state.as_str();
            self.fire(
                &format!("task.{state}"),
                format!("{}-r{}-{state}", record.id, record.revision),
                json!({ "task": record.id, "title": title, "revision": record.revision, "state": state }),
            );
        }
    }

    /// Start the project's routines whose trigger is this event kind (docs/tasks.md
    /// "Routines"). Only a project with such a routine asks the scheduler, so the others send
    /// no request; a scheduler that does not answer drops the event, as a busy routine does.
    fn fire(&self, kind: &str, event_id: String, payload: serde_json::Value) {
        let wanted = self.stored_routines().routines.iter().any(|routine| {
            routine.enabled
                && matches!(&routine.trigger, Trigger::Event { kind: wanted } if wanted == kind)
        });
        if wanted {
            let action = wsx_core::routine::Action::Fire {
                kind: kind.into(),
                payload,
                event_id,
            };
            let client = wsx_core::runtime::Client::local();
            let _ = wsx_core::routine::request(&client, &self.main, action);
        }
    }

    /// The project's routines and runs as stored, without asking the scheduler; none when
    /// unreadable.
    fn stored_routines(&self) -> routines::ProjectRoutines {
        routines::canonical_project(&self.main)
            .and_then(|project| routines::load(&routines::root(), &project))
            .unwrap_or_default()
    }

    /// Add a finished task's memo to `notes/context.md`. ref: docs/tasks.md "Memo"
    fn fold_memo(&self, task: u32, memo: &str) {
        let Ok(notes) = self.dir.notes() else {
            return;
        };
        if let Err(error) = notes::fold_memo(&notes, task, memo) {
            self.say(&format!("cannot add the memo: {error:#}"));
        }
    }

    fn open_role_names(&self) -> Vec<String> {
        roles::open_roles(&self.dir.project_id)
            .iter()
            .map(|dir| dir.name().to_string())
            .collect()
    }

    /// Queue a task's event line. Only a task's creator hears of it: an open role, else the
    /// manager. ref: docs/tasks.md "Roles"
    fn task_event(
        &mut self,
        record: &TaskRecord,
        open_roles: &[String],
        line: String,
        short: Short,
    ) {
        match record
            .created_by
            .role()
            .filter(|role| open_roles.iter().any(|open| open == role))
        {
            Some(role) => self
                .role_events
                .entry(role.to_string())
                .or_default()
                .push((line, short)),
            None => self.pending_events.push((line, short)),
        }
        self.last_event = Some(Instant::now());
    }

    /// Turn each new stall of an open task into one event line, after it lasted
    /// `stall::GRACE`. ref: docs/tasks.md "Manager" (Stalls)
    fn watch_stalls(&mut self) {
        if self
            .stalls_swept
            .is_some_and(|at| at.elapsed() < stall::SWEEP)
        {
            return;
        }
        self.stalls_swept = Some(Instant::now());
        let Ok(tasks) = project_tasks(&self.dir.project_id) else {
            return;
        };
        let open_roles = self.open_role_names();
        let now = now_unix_ms();
        let mut open = Vec::new();
        let mut suspects = BTreeMap::new();
        for (dir, record) in tasks {
            let Ok(record) = record else { continue };
            if !record.state.is_open() {
                continue;
            }
            open.push(record.id);
            let Some(found) = stall::find(&dir, &record, now) else {
                continue;
            };
            let since = match self.stall_suspects.get(&record.id) {
                Some((marker, since)) if *marker == found.marker => *since,
                _ => Instant::now(),
            };
            suspects.insert(record.id, (found.marker.clone(), since));
            if since.elapsed() < stall::GRACE
                || self.observed_stalls.get(&record.id) == Some(&found.marker)
            {
                continue;
            }
            let title = view::title(&dir, &record).unwrap_or_default();
            let line = format!(
                "#{} stalled r{}: {}; {}",
                record.id,
                record.revision,
                clip(&title, EVENT_TITLE_CHARS),
                found.words
            );
            self.observed_stalls.insert(record.id, found.marker);
            self.task_event(
                &record,
                &open_roles,
                line,
                Short::task(record.id, "stalled"),
            );
        }
        self.stall_suspects = suspects;
        // ^ Saved with the next event's `seen`.
        self.observed_stalls.retain(|id, _| open.contains(id));
    }

    /// Close the roles with no turn for `tasks.roles.closeDays`, and tell the manager.
    /// ref: docs/tasks.md "Roles"
    fn close_idle_roles(&mut self) {
        if self.roles_swept.is_some_and(|at| at.elapsed() < ROLE_SWEEP) {
            return;
        }
        self.roles_swept = Some(Instant::now());
        let Ok(settings) = load_task_config(&self.main) else {
            return;
        };
        for role in roles::close_idle(&self.dir.project_id, settings.roles_close_secs) {
            let line = format!(
                "role {role} closed by wsx: no turn for tasks.roles.closeDays; its notes are kept"
            );
            self.pending_events.push(plain_event(line));
            self.last_event = Some(Instant::now());
        }
    }

    /// A role's host leaves when its role is closed, or after `ROLE_LINGER` with no message
    /// and no turn: an idle role costs no agent and no pane.
    fn leave_when_done(&mut self) {
        let closed = !roles::is_open(&self.dir);
        let idle = self.turn.is_none()
            && !self.work_waiting()
            && self.state.limit.is_none()
            && self.last_activity.elapsed() >= ROLE_LINGER;
        if closed || idle {
            self.say(match closed {
                true => "the role is closed: its host leaves",
                false => "idle: the role's host leaves until the next message",
            });
            self.leaving = true;
        }
    }

    /// Send the role's own events through `say`, which starts its host when it is not
    /// running; the events of a role whose host does not take them go to the manager.
    fn send_role_events(&mut self) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        for (role, lines) in std::mem::take(&mut self.role_events) {
            let (project, main, exe, events) = (
                self.project.clone(),
                self.main.clone(),
                exe.clone(),
                self.events.clone(),
            );
            std::thread::spawn(move || {
                let text = lines
                    .iter()
                    .map(|(line, _)| line.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let short = lines.iter().map(|(_, short)| short.clone()).collect();
                let sent = owner::say(
                    &project,
                    &main,
                    &exe,
                    Some(&role),
                    super::launch_manager(&main),
                    text,
                    Some(Author::Events),
                    short,
                );
                if !sent.is_ok_and(|response| response.ok) {
                    let _ = events.send(Event::Undelivered { role, lines });
                }
            });
        }
    }

    /// Log the waiting event lines as one message once they have been quiet for a while.
    fn flush_events(&mut self) {
        let quiet = self
            .last_event
            .is_some_and(|last| last.elapsed() >= EVENT_QUIET);
        if (self.pending_events.is_empty() && self.role_events.is_empty()) || !quiet {
            return;
        }
        self.send_role_events();
        if self.pending_events.is_empty() {
            self.save_seen();
            return;
        }
        let text = self
            .pending_events
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let short = self
            .pending_events
            .iter()
            .map(|(_, short)| short.clone())
            .collect();
        match self.accept(Author::Events, text, short) {
            Ok(_) => {
                self.pending_events.clear();
                self.save_seen();
            }
            Err(error) => self.say(&format!("cannot log the events: {error:#}")),
        }
    }

    fn save_seen(&mut self) {
        self.state.seen = Some(self.observed.clone());
        self.state.decisions_seen = self.observed_decisions;
        self.state.deploys_seen_ms = Some(self.observed_deploys_ms);
        self.state.routines_seen = Some(self.observed_routines);
        self.state.stalls = self.observed_stalls.clone();
        self.save();
    }

    /// `WSX_TASK_ACTOR` of the agent's commands: `manager` or `role:<name>`.
    fn actor(&self) -> String {
        match &self.dir.role {
            Some(role) => wsx_core::task::Actor::Role(role.clone()).into(),
            None => "manager".into(),
        }
    }

    /// Log a message and queue it; owner messages go before events.
    fn accept(&mut self, from: Author, text: String, short: Vec<Short>) -> Result<u64> {
        if text.trim().is_empty() {
            anyhow::bail!("the message is empty");
        }
        if text.len() > MAX_MESSAGE_BYTES {
            anyhow::bail!("the message is larger than the limit of {MAX_MESSAGE_BYTES} bytes");
        }
        let id = self.next_id();
        self.dir.append(ManagerEvent::Message {
            id,
            from: from.clone(),
            text: text.clone(),
            short,
        })?;
        self.last_activity = Instant::now();
        self.overview_asked &= from != Author::Owner;
        self.context_asked &= from != Author::Owner;
        self.thought_asked &= from != Author::Owner;
        self.enqueue(id, from, text);
        Ok(id)
    }

    /// After `recapMinutes` with no message and no turn: start the idle task when it is due,
    /// and ask the manager for a recap when something changed since the last one, with the
    /// token cost since. A quiet project gets one recap. ref: docs/tasks.md "Recap"
    fn ask_recap(&mut self) {
        let due = self.recap_quiet.is_some_and(|quiet| {
            self.turn.is_none()
                && !self.work_waiting()
                && self.pending_events.is_empty()
                && self.last_activity.elapsed() >= quiet
        });
        if !due {
            return;
        }
        // ^ Once per quiet period: the next look waits a whole period.
        self.last_activity = Instant::now();
        self.start_idle();
        let now = now_unix_ms();
        self.fire(
            "project.idle",
            format!("idle-{now}"),
            json!({ "unix_ms": now }),
        );
        let since = self.state.recap_ms.unwrap_or(0);
        let log: Vec<_> = self
            .dir
            .read_log()
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.unix_ms > since)
            .collect();
        if !self.changed_since(&log, since) {
            return;
        }
        let text = format!("{RECAP_REQUEST} {}", self.cost_since(&log, since));
        self.state.recap_ms = Some(now_unix_ms());
        self.save();
        if let Err(error) = self.accept(Author::Recap, text, Vec::new()) {
            self.say(&format!("cannot ask for a recap: {error:#}"));
        }
    }

    /// A land, a close, a new wait or an approval decision the manager heard of in `log` (the
    /// entries after `since`), a new approval request or a To check item after `since`.
    fn changed_since(&self, log: &[ManagerEntry], since: u64) -> bool {
        let heard = log.iter().any(|entry| match &entry.event {
            ManagerEvent::Message {
                from: Author::Events,
                text,
                ..
            } => text.lines().any(|line| {
                line.starts_with("approval ")
                    || matches!(
                        line.split(' ').nth(1),
                        Some("landed" | "closed" | "waiting" | "stalled")
                    )
            }),
            _ => false,
        });
        let asked = self
            .approval_lines()
            .iter()
            .any(|line| matches!(line, ApprovalLine::Request { unix_ms, .. } if *unix_ms > since));
        let to_check = checks::read(&self.dir.project_id)
            .unwrap_or_default()
            .iter()
            .any(|item| item.unix_ms > since);
        heard || asked || to_check
    }

    /// The spend since `since` from the logs' `cost_micro_usd`: the manager's replies and the
    /// workers' results, the costliest tasks named. ref: docs/tasks.md "Recap"
    fn cost_since(&self, log: &[ManagerEntry], since: u64) -> String {
        let manager = usage::Usage::since(&usage::manager_log_turns(log), 0);
        let mut tasks: Vec<(u64, u32, u64)> = project_tasks(&self.dir.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(dir, record)| {
                let spent = usage::Usage::since(&usage::task_turns(&dir), since + 1);
                let id = record.ok()?.id;
                (spent.turns > 0).then_some((spent.cost_micro_usd, id, spent.turns))
            })
            .collect();
        tasks.sort_by(|left, right| right.cmp(left));
        let usd = |micro: u64| format!("{:.2} USD", micro as f64 / 1e6);
        let workers: u64 = tasks.iter().map(|(cost, ..)| cost).sum();
        let turns: u64 = tasks.iter().map(|(.., turns)| turns).sum();
        let top: Vec<String> = tasks
            .iter()
            .take(3)
            .map(|(cost, id, _)| format!("#{id} {}", usd(*cost)))
            .collect();
        format!(
            "Token cost since the last recap: {} in all; you {} in {} turn(s), workers {} in {turns} turn(s){}.",
            usd(manager.cost_micro_usd + workers),
            usd(manager.cost_micro_usd),
            manager.turns,
            usd(workers),
            match top.is_empty() {
                true => String::new(),
                false => format!(" ({})", top.join(", ")),
            }
        )
    }

    /// When session notes in the main checkout's `knowledge/sessions/<project>/` are newer
    /// than the last idle task's start and that task is not open, start one task with
    /// `tasks.workflow.idle` as the manager would, so it waits for the manager's review.
    /// Otherwise nothing, not even a line. ref: docs/tasks.md "Workflow"
    fn start_idle(&mut self) {
        let prompt = load_task_config(&self.main)
            .map(|settings| settings.workflow.idle)
            .unwrap_or_default();
        let open = self.state.idle_task.is_some_and(|id| {
            store::TaskDir::open(&self.dir.project_id, id)
                .and_then(|dir| dir.read_record())
                .is_ok_and(|record| record.state.is_open())
        });
        let notes = newest_ms(&self.main.join("knowledge/sessions").join(&self.project));
        let recent = self
            .state
            .idle_ms
            .is_some_and(|ms| now_unix_ms().saturating_sub(ms) < IDLE_EVERY_MS);
        if recent
            || prompt.is_empty()
            || open
            || notes.is_none_or(|ms| ms <= self.state.idle_ms.unwrap_or(0))
        {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return self.say("cannot find the wsx binary for the idle task");
        };
        let (project, events, began) = (self.project.clone(), self.events.clone(), now_unix_ms());
        std::thread::spawn(move || {
            let _ = events.send(Event::IdleStarted(began, new_task(&exe, &project, &prompt)));
        });
    }

    /// Owner messages go first, then other managers', then events.
    fn enqueue(&mut self, id: u64, from: Author, text: String) {
        let rank = |author: &Author| match author {
            Author::Owner => 0,
            Author::Manager { .. } | Author::Role { .. } => 1,
            Author::Events => 2,
            Author::Recap => 3,
        };
        let at = self
            .queue
            .iter()
            .position(|(_, author, _)| rank(author) > rank(&from))
            .unwrap_or(self.queue.len());
        self.queue.insert(at, (id, from, text));
    }

    /// A message of a manager or a role is answered once (docs/tasks.md "Roles").
    fn owe(&self, author: &Author) {
        let who = match author {
            Author::Manager {
                project,
                reply: false,
            } => project.clone(),
            Author::Role { name, reply: false } => format!("role:{name}"),
            _ => return,
        };
        *self.owed.lock().expect("owed lock").entry(who).or_default() += 1;
    }

    fn next_id(&mut self) -> u64 {
        self.state.next_id += 1;
        self.save();
        self.state.next_id
    }

    /// Start a turn with every waiting owner message, or else with the waiting events.
    fn next_turn(&mut self) {
        // ^ A start that failed waits: its messages are back in the queue.
        if self
            .start_failed
            .is_some_and(|failed| failed.elapsed() < START_RETRY)
        {
            return;
        }
        self.choose_agent();
        // ^ During a limit the messages wait, also when another host met it or a budget is
        // spent; after it they go with the held ones.
        let now = now_unix_ms();
        let waits_until = self
            .state
            .limit
            .as_ref()
            .map(|hold| hold.until_ms)
            .filter(|until| now < *until)
            .or_else(|| store::limited_until(&self.current_account(), now));
        if let Some(until_ms) = waits_until {
            if self.state.limit.is_none() && self.work_waiting() {
                self.log(ManagerEvent::Limit {
                    until: Some(limit::resume_time(until_ms, now)),
                });
                self.state.limit = Some(LimitHold {
                    until_ms,
                    attempt: 0,
                    ids: Vec::new(),
                    events: true,
                    switched: false,
                });
                self.save();
            }
            self.stand_in();
            return;
        }
        let held = self.state.limit.clone();
        if held.is_none() && !self.work_waiting() {
            return;
        }
        // ^ A replaced binary: exec it before this turn. The messages stay logged and not
        // delivered, so the new host queues them again (`restore`).
        if self.binary.as_ref().is_some_and(hosting::Binary::replaced) {
            self.say("the wsx binary was replaced: the manager restarts with it");
            self.reexec = true;
            return;
        }
        let first = self
            .queue
            .iter()
            .find(|(_, author, text)| self.wakes(author, text))
            .or(self.queue.front())
            .map_or(Author::Owner, |(_, author, _)| author.clone());
        let mut ids = Vec::new();
        let mut text = String::new();
        let mut said = String::new();
        // ^ Events that need no judgment go along with a turn that runs anyway, after its
        // own messages.
        let mut along = Vec::new();
        // ^ What left the queue, to put back when the agent does not start.
        let mut taken = Vec::new();
        let mut for_owner = false;
        if first != Author::Events {
            while let Some(at) = self.queue.iter().position(|(_, author, text)| {
                *author == Author::Events && !self.wakes(author, text)
            }) {
                along.push(self.queue.remove(at).expect("position is in the queue"));
            }
        }
        while self
            .queue
            .front()
            .is_some_and(|(_, author, _)| *author == first)
        {
            let (id, author, body) = self.queue.pop_front().expect("checked above");
            let heading = match &author {
                Author::Owner => "Owner".to_string(),
                Author::Events => "Events".to_string(),
                Author::Recap => "Recap request from wsx".to_string(),
                Author::Manager {
                    project,
                    reply: false,
                } => format!("Manager {project}"),
                Author::Manager {
                    project,
                    reply: true,
                } => format!("Manager {project} replies"),
                // ^ A role's or this project's manager's message is answered once, as a
                // manager's (docs/tasks.md "Roles").
                Author::Role { name, reply } => {
                    let who = match name.as_str() {
                        "manager" => "Manager".to_string(),
                        name => format!("Role {name}"),
                    };
                    match reply {
                        true => format!("{who} replies"),
                        false => who,
                    }
                }
            };
            for_owner |= author == Author::Events && body.lines().any(routine_for_owner);
            text.push_str(&format!("{heading} (message {id}):\n{body}\n\n"));
            if author == Author::Owner {
                said.push_str(&body);
                said.push('\n');
            }
            ids.push(id);
            taken.push((id, author, body));
        }
        if !said.is_empty() {
            text.push_str(&self.notes_matching(&said));
        }
        // ^ Events that go along must not take the owner's answer: a fresh turn of 2284 with
        // two events answered only the events (docs/design/manager-reliability.md "A").
        if first == Author::Owner && !along.is_empty() {
            let owner = ids.iter().map(u64::to_string).collect::<Vec<_>>();
            text.push_str(&format!(
                "Answer message{} {} first.\n\n",
                if owner.len() > 1 { "s" } else { "" },
                owner.join(", ")
            ));
        }
        for (id, author, body) in along {
            text.push_str(&format!("Events (message {id}):\n{body}\n\n"));
            ids.push(id);
            taken.push((id, author, body));
        }
        // ^ The limit preface is no owner message: a held turn is events-only when its
        // held and its new messages all are. Another manager's or a role's reply is used, not
        // answered: like events. ref: docs/tasks.md "Manager"
        let events_only = held.as_ref().is_none_or(|hold| hold.events)
            && if ids.is_empty() {
                held.is_some()
            } else {
                matches!(
                    first,
                    Author::Events
                        | Author::Manager { reply: true, .. }
                        | Author::Role { reply: true, .. }
                )
            };
        match &held {
            // ^ A wait no turn met: no message waits for an answer.
            Some(hold) if hold.ids.is_empty() => {}
            Some(hold) if hold.switched => {
                text = format!(
                    "{SWITCH_CONTINUE}\n\n{}{text}",
                    self.held_messages(&hold.ids)
                )
            }
            Some(_) => text = format!("{LIMIT_CONTINUE}\n\n{text}"),
            None => {}
        }
        // ^ The id of its last reply: a quote names a message by id, and the agent never sees
        // the ids of its own replies. Older ones: `wsx task chat`.
        if let Some(id) = self.state.last_reply.filter(|_| !events_only) {
            text.push_str(&format!("Your last reply: {id}.\n\n"));
        }
        if std::mem::take(&mut self.state.provisional) {
            text = self.provisional_note() + &text;
        }
        if let Err(error) = self.send_turn(text.trim_end(), events_only) {
            self.say(&format!("cannot start the manager agent: {error:#}"));
            // ^ The messages go back to the front, in order, for the retry: they are not
            // logged as delivered. One `stopped` entry for a run of failures.
            for item in taken.into_iter().rev() {
                self.queue.push_front(item);
            }
            if self.start_failed.replace(Instant::now()).is_none() {
                self.log(ManagerEvent::Stopped {
                    reason: format!("cannot start the manager agent: {error:#}"),
                });
            }
            return;
        }
        self.start_failed = None;
        for (_, author, _) in &taken {
            self.owe(author);
        }
        if held.as_ref().is_some_and(|hold| !hold.switched) {
            self.log(ManagerEvent::Limit { until: None });
        }
        if !ids.is_empty() {
            self.log(ManagerEvent::Delivered { ids: ids.clone() });
        }
        let mut turn = held.map(|hold| hold.ids).unwrap_or_default();
        turn.extend(ids);
        self.turn = Some(turn);
        // ^ A routine event for the owner gets a normal message, not a quiet line; the model
        // choice stays with the events.
        self.turn_events = events_only && !for_owner;
        self.report(runtime::AgentState::Working);
    }

    /// The model of a turn: `eventModel` for one with only events while no ready task the
    /// manager created waits for its review, else the manager's. ref: docs/tasks.md "Manager"
    fn turn_model(&self, events_only: bool) -> String {
        let review_due = || {
            let land_auto = load_task_config(&self.main).is_ok_and(|settings| settings.land_auto);
            project_tasks(&self.dir.project_id)
                .unwrap_or_default()
                .into_iter()
                .any(|(_, record)| record.is_ok_and(|record| record.manager_reviews(land_auto)))
        };
        // ^ A fallback runs its own model: the event model names one of the own agent's.
        if let Some(agent) = &self.state.agent {
            return agent.model.clone();
        }
        match &self.event_model {
            Some(model) if events_only && !review_due() => model.clone(),
            _ => self.model.clone(),
        }
    }

    /// Whether a queued message starts a turn. ref: `event_wakes`
    fn wakes(&self, author: &Author, text: &str) -> bool {
        *author != Author::Events || text.lines().any(|line| event_wakes(line, self.deploy_auto))
    }

    fn work_waiting(&self) -> bool {
        self.queue
            .iter()
            .any(|(_, author, text)| self.wakes(author, text))
    }

    /// Before every turn, the best agent that can take it: back on the own agent as soon as
    /// its limit is over, in a fresh conversation; down to a fallback only past
    /// `maxWaitMinutes` and only when a turn waits. ref: docs/tasks.md "Agents"
    fn choose_agent(&mut self) {
        let now = now_unix_ms();
        let own = agent::account(&self.adapter, &self.model);
        if self.state.agent.is_some() && store::limited_until(&own, now).is_none() {
            let reason = format!("back on {}", self.adapter);
            self.say(&reason);
            let held = self.state.limit.take();
            self.rotate(0, Some(reason));
            self.state.agent = None;
            self.state.limit = held.map(|hold| LimitHold {
                until_ms: now,
                attempt: 0,
                switched: true,
                ..hold
            });
            self.save();
            return;
        }
        let held = self.state.limit.clone();
        if held.is_none() && !self.work_waiting() {
            return;
        }
        if let Some(until_ms) = store::limited_until(&self.current_account(), now) {
            let (ids, events) = held.map_or((Vec::new(), true), |hold| (hold.ids, hold.events));
            if self.switch_agent(until_ms, &ids, events) {
                self.save();
            }
        }
    }

    /// A limit until `until_ms` past `tasks.agent.maxWaitMinutes` moves the manager to the
    /// first available `tasks.manager.fallback` in a new conversation, whose first turn
    /// answers the held messages `ids`. A short limit is waited out. Returns whether it moved.
    /// ref: docs/tasks.md "Agents"
    fn switch_agent(&mut self, until_ms: u64, ids: &[u64], events: bool) -> bool {
        let now = now_unix_ms();
        let settings = load_task_config(&self.main).unwrap_or_default();
        let Some(next) = agent::fallback_past_wait(
            &settings.manager_fallback,
            settings.max_wait_secs,
            &self.current_account(),
            until_ms,
            now,
        ) else {
            return false;
        };
        let reason = format!(
            "switched to {}: {} usage limit until {}",
            next.name,
            self.current_adapter(),
            limit::resume_time(until_ms, now)
        );
        self.say(&reason);
        self.state.limit = Some(LimitHold {
            until_ms: now,
            attempt: 0,
            ids: ids.to_vec(),
            events,
            switched: true,
        });
        self.rotate(0, Some(reason));
        self.state.agent = Some(ManagerAgent {
            name: next.name,
            adapter: next.adapter,
            model: next.model,
        });
        true
    }

    /// During a hold, an owner message with no answer yet gets one from a `minimal` agent, one
    /// call with no tools. The message stays due: the own agent answers it after the hold and
    /// is told of this reply. ref: docs/tasks.md "Agents"
    fn stand_in(&mut self) {
        let done = self.state.provisional_to;
        let queued: Vec<u64> = self
            .queue
            .iter()
            .filter(|(id, author, _)| *author == Author::Owner && *id > done)
            .map(|(id, ..)| *id)
            .collect();
        // ^ A hold of only events owes the owner nothing, and this runs on every tick.
        let held: Vec<u64> = self
            .state
            .limit
            .as_ref()
            .filter(|hold| !hold.events)
            .map_or(Vec::new(), |hold| {
                hold.ids.iter().copied().filter(|id| *id > done).collect()
            });
        if queued.is_empty() && held.is_empty() {
            return;
        }
        if self
            .stand_in_look
            .is_some_and(|look| look.elapsed() < STAND_IN_LOOK)
        {
            return;
        }
        self.stand_in_look = Some(Instant::now());
        let now = now_unix_ms();
        let settings = load_task_config(&self.main).unwrap_or_default();
        let Some(agent) = agent::fallback(&settings.manager_fallback, now, true) else {
            return;
        };
        let account = agent::account(&agent.adapter, &agent.model);
        let log = self.dir.read_log().unwrap_or_default();
        let (mut asked, mut text) = (Vec::new(), String::new());
        for entry in &log {
            if let ManagerEvent::Message {
                id,
                from: Author::Owner,
                text: said,
                ..
            } = &entry.event
            {
                if queued.contains(id) || held.contains(id) {
                    asked.push(*id);
                    text.push_str(&format!("Owner:\n{said}\n\n"));
                }
            }
        }
        if asked.is_empty() {
            return;
        }
        let recent = recent_exchanges(&log, HANDOVER_ENTRIES);
        // ^ The manager's own next summary must still name what changed since its last turn.
        let seen = self.state.turn_seen.clone();
        let ledger = self.ledger_summary();
        self.state.turn_seen = seen;
        let prompt = format!(
            "{}The last exchanges:\n{}\n\n{ledger}\n\n{text}",
            self.notes_preamble(),
            recent.join("\n")
        );
        // ^ The held conversation stays resumable: its process has nothing to do now.
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.generation += 1;
        let generation = self.generation;
        let started = self.launch().and_then(|launch| {
            let launch = Launch {
                model: agent.model.clone(),
                instructions: STAND_IN.into(),
                plugin_dirs: Vec::new(),
                minimal: true,
                ..launch
            };
            let mut spawned = Agent::spawn(
                &agent.adapter,
                &launch,
                &new_session_id()?,
                false,
                0,
                self.events.clone(),
                move |line| Event::Agent(generation, line),
            )?;
            spawned.send(prompt.trim_end())?;
            Ok(spawned)
        });
        match started {
            Ok(spawned) => {
                self.agent = Some(spawned);
                self.stand_in = Some((agent, Instant::now()));
                self.turn = Some(asked);
                self.turn_events = false;
            }
            Err(error) => {
                self.say(&format!("cannot start {}: {error:#}", agent.name));
                let _ = store::mark_limited(&account, now + STAND_IN_RETRY_MS, now, false);
            }
        }
    }

    /// A stand-in's call ended, or `result` is none: it died or took too long.
    fn stand_in_done(&mut self, result: Option<TurnResult>) {
        let Some((stand_in, _)) = self.stand_in.take() else {
            return;
        };
        // ^ A late line of the stopped process is not a turn of the manager's.
        self.generation += 1;
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        let to = self.turn.take().unwrap_or_default();
        let now = now_unix_ms();
        let name = stand_in.name;
        match result.filter(|result| !result.is_error && !result.text.trim().is_empty()) {
            Some(result) => {
                if let Some(message) = agent::note_spend(
                    &stand_in.adapter,
                    &stand_in.model,
                    result.usage.cost_micro_usd,
                    now,
                ) {
                    self.say(&message);
                }
                let id = self.next_id();
                self.say(&format!("{name} stood in: reply {id}"));
                let answered = to.iter().copied().max().unwrap_or(0);
                self.log(ManagerEvent::Reply {
                    id,
                    to,
                    text: result.text,
                    usage: Some(result.usage),
                    quiet: false,
                    asks: false,
                    provisional: Some(name),
                    level: None,
                    post: false,
                });
                self.state.provisional_to = answered;
                self.state.provisional = true;
                self.save();
            }
            None => {
                self.say(&format!("{name} could not stand in"));
                let account = agent::account(&stand_in.adapter, &stand_in.model);
                let _ = store::mark_limited(&account, now + STAND_IN_RETRY_MS, now, false);
            }
        }
    }

    /// What the own agent must know of the replies a stand-in gave since its last turn.
    fn provisional_note(&self) -> String {
        let log = self.dir.read_log().unwrap_or_default();
        let mut replies: Vec<String> = log
            .iter()
            .rev()
            .filter_map(|entry| match &entry.event {
                ManagerEvent::Reply {
                    to,
                    text,
                    provisional,
                    ..
                } => Some((to, text, provisional)),
                _ => None,
            })
            .map_while(|(to, text, provisional)| {
                let to: Vec<String> = to.iter().map(u64::to_string).collect();
                Some(format!(
                    "{} to message {}: {}",
                    provisional.as_ref()?,
                    to.join(", "),
                    one_line(text)
                ))
            })
            .collect();
        replies.reverse();
        format!(
            "While you were away, a stand-in model with no tools answered these. Its words are \
             not yours and may be wrong: check them, say so in your reply where they were wrong, \
             and do not repeat what was right.\n{}\n\n",
            replies.join("\n")
        )
    }

    /// The account the manager's turns run on now.
    fn current_account(&self) -> String {
        match &self.state.agent {
            Some(agent) => agent::account(&agent.adapter, &agent.model),
            None => agent::account(&self.adapter, &self.model),
        }
    }

    /// The adapter the manager runs on now: a fallback's, else `tasks.manager.adapter`.
    fn current_adapter(&self) -> String {
        self.state
            .agent
            .as_ref()
            .map_or_else(|| self.adapter.clone(), |agent| agent.adapter.clone())
    }

    /// The context size past which the next turn starts a fresh conversation, for the adapter
    /// that runs now (a fallback's, not only `tasks.manager.adapter`).
    fn rotate_limit(&self) -> u64 {
        manager_rotate_limit(self.rotate_tokens, &self.current_adapter())
    }

    /// The held turn of a switch in full: a new conversation never saw its messages.
    fn held_messages(&self, ids: &[u64]) -> String {
        let log = self.dir.read_log().unwrap_or_default();
        log.iter()
            .filter_map(|entry| match &entry.event {
                ManagerEvent::Message { id, text, .. } if ids.contains(id) => {
                    Some(format!("Message {id}:\n{text}\n\n"))
                }
                _ => None,
            })
            .collect()
    }

    /// A few lines of the ledger for the end of a turn's message: the open tasks by state,
    /// the tasks changed since the last turn (every open one in a fresh conversation), the
    /// To check and approval counts and the deploy. The manager reads the full ledger only
    /// when it needs more. ref: docs/tasks.md "Manager"
    fn ledger_summary(&mut self) -> String {
        let tasks: Vec<(store::TaskDir, wsx_core::task::TaskRecord)> =
            project_tasks(&self.dir.project_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(dir, record)| record.ok().map(|record| (dir, record)))
                .collect();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, record) in tasks.iter().filter(|(_, record)| record.state.is_open()) {
            *counts.entry(record.state.as_str()).or_default() += 1;
        }
        let open = match counts.is_empty() {
            true => "none".to_string(),
            false => counts
                .iter()
                .map(|(state, count)| format!("{count} {state}"))
                .collect::<Vec<_>>()
                .join(", "),
        };
        let to_check = checks::open_items(&self.dir.project_id).map_or(0, |items| items.len());
        let approvals = pending_approvals(&self.approval_lines()).len();
        let baseline = self.state.turn_seen.take();
        let seen: BTreeMap<u32, Seen> = tasks
            .iter()
            .map(|(_, record)| (record.id, Seen::of(record)))
            .collect();
        let rows: Vec<String> = tasks
            .iter()
            .filter(|(_, record)| match &baseline {
                Some(old) => old.get(&record.id) != seen.get(&record.id),
                None => record.state.is_open(),
            })
            .map(|(dir, record)| {
                format!(
                    "#{} {} r{}: {}",
                    record.id,
                    record.state.as_str(),
                    record.revision,
                    clip(
                        &view::title(dir, record).unwrap_or_default(),
                        SUMMARY_TITLE_CHARS
                    )
                )
            })
            .collect();
        self.state.turn_seen = Some(seen);
        let mut out = format!(
            "Ledger summary from wsx (`wsx task ledger -p {}` has the details):\n\
             - open: {open}; to check: {to_check}; approvals waiting: {approvals}\n",
            self.project
        );
        let heading = match baseline {
            Some(_) => "changed since your last turn",
            None => "open tasks",
        };
        if !rows.is_empty() {
            out.push_str(&format!("- {heading}:\n"));
            for row in rows.iter().take(SUMMARY_ROWS) {
                out.push_str(&format!("  {row}\n"));
            }
            if rows.len() > SUMMARY_ROWS {
                out.push_str(&format!("  and {} more\n", rows.len() - SUMMARY_ROWS));
            }
        }
        if let Some(deploy) = self.deploy_line() {
            out.push_str(&format!("- {deploy}\n"));
        }
        out
    }

    /// The last deploy and how far it is behind, when the project configures one.
    fn deploy_line(&self) -> Option<String> {
        let settings = load_task_config(&self.main).ok()?;
        let status = ledger::deploy_status(&self.project, &self.main, &settings).ok()?;
        let deploy = &status["deploy"];
        if deploy["configured"] != true {
            return None;
        }
        let failed = match deploy["failed_since"].as_u64().unwrap_or(0) {
            0 => String::new(),
            failed => format!(", {failed} failed since"),
        };
        // ^ A trial holds the live site; the default branch deploys when it ends.
        let trial = &status["trial"];
        let trial = match trial["task"].as_u64() {
            Some(task) => format!(
                "; the live site runs the trial of #{task} ({})",
                trial["stage"].as_str().unwrap_or("?")
            ),
            None => String::new(),
        };
        Some(match deploy["commit"].as_str() {
            Some(commit) => format!(
                "deploy: {} live, {} commit(s) behind{failed}{trial}",
                &commit[..commit.len().min(7)],
                deploy["behind"]
                    .as_u64()
                    .map_or("?".into(), |n| n.to_string())
            ),
            None => format!("deploy: none yet{failed}{trial}"),
        })
    }

    /// One no-op turn that keeps Claude's prompt cache warm while a task works or checks: its
    /// event wakes the manager later. Only the own Claude agent, idle for `keepWarmSeconds`
    /// and under the cache's hour, at most `KEEP_WARM_PINGS` since the last real turn, never
    /// in a usage limit. ref: docs/tasks.md "Manager"
    fn ping(&mut self) {
        let now = now_unix_ms();
        let idle = now.saturating_sub(self.state.last_reply_ms);
        if self.dir.role.is_some()
            || self.turn.is_some()
            || self.agent.is_none()
            || self.stand_in.is_some()
            || self.state.agent.is_some()
            || self.adapter != "claude"
            || self.state.limit.is_some()
            || self.pings >= KEEP_WARM_PINGS
            || self.state.agent_session.is_none()
            || self.state.last_reply_ms == 0
            || idle >= PROMPT_CACHE_MS
            || self.state.context_tokens > self.rotate_limit()
        {
            return;
        }
        let seconds = load_task_config(&self.main)
            .unwrap_or_default()
            .keep_warm_seconds;
        if seconds == 0 || idle < seconds * 1000 {
            return;
        }
        let working = project_tasks(&self.dir.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, record)| record.ok())
            .any(|record| matches!(record.state, TaskState::Working | TaskState::Checking));
        if !working || store::limited_until(&self.current_account(), now).is_some() {
            return;
        }
        // ^ Counted before the send: a failed send never loops.
        self.pings += 1;
        let sent = self
            .agent
            .as_mut()
            .map_or(Ok(()), |agent| agent.send(KEEP_WARM_TEXT));
        match sent {
            Ok(()) => {
                self.turn = Some(Vec::new());
                self.pinging = true;
                self.say(&format!(
                    "ping {} of {KEEP_WARM_PINGS}: keeps the prompt cache warm",
                    self.pings
                ));
            }
            Err(error) => self.say(&format!("cannot ping the agent: {error:#}")),
        }
    }

    /// A ping's answer: logged as `ping`, never as a reply, so the chat does not show it.
    fn on_ping(&mut self, result: TurnResult) {
        self.turn = None;
        let now = now_unix_ms();
        // ^ A limit is no answer; the next real turn meets it and waits as usual.
        if result.limit.is_none() {
            // ^ The cache is warm again: the idle rotation counts from here.
            self.state.last_reply_ms = now;
        }
        if result.usage.context_tokens > 0 {
            self.state.context_tokens = result.usage.context_tokens;
        }
        self.state.session_cost_micro_usd = result.usage.session_cost_micro_usd;
        if let Some(message) =
            agent::note_spend(&self.adapter, &self.model, result.usage.cost_micro_usd, now)
        {
            self.say(&message);
        }
        self.save();
        self.log(ManagerEvent::Ping {
            usage: Some(result.usage),
        });
    }

    fn send_turn(&mut self, text: &str, events_only: bool) -> Result<()> {
        self.pings = 0;
        let mut text = text.to_string();
        let model = self.turn_model(events_only);
        // ^ Claude's prompt cache lives 1 hour: a large conversation resumed later writes its
        // whole context again, so it starts afresh with the handover.
        let idle = now_unix_ms().saturating_sub(self.state.last_reply_ms);
        if self.state.agent_session.is_some()
            && self.state.last_reply_ms > 0
            && idle > IDLE_ROTATE_MS
            && self.state.context_tokens > IDLE_ROTATE_TOKENS
        {
            let tokens = self.state.context_tokens;
            self.rotate(
                tokens,
                Some(format!("idle over 60 min with {}k context", tokens / 1000)),
            );
        }
        if self.agent.is_none() {
            let mut launch = self.launch()?;
            let digest = instructions_digest(&launch.instructions);
            // ^ Claude keeps the system prompt a conversation began with, also on resume:
            // new instructions need a new conversation (docs/tasks.md "Manager").
            if self.state.agent_session.is_some()
                && self.state.instructions.as_deref() != Some(&digest)
            {
                // ^ A warm small conversation takes the new instructions as a message: a
                // rotation costs a cold start. A state with no digest cannot tell what changed.
                let warm = self.state.instructions.is_some()
                    && self.state.context_tokens < INSTRUCTION_MESSAGE_TOKENS
                    && now_unix_ms().saturating_sub(self.state.last_reply_ms) < IDLE_ROTATE_MS;
                if warm {
                    text = format!(
                        "Your instructions changed. These replace the ones this conversation \
                         began with:\n\n{}\n\n{text}",
                        launch.instructions
                    );
                } else {
                    self.rotate(0, Some("the manager's instructions changed".into()));
                    launch = self.launch()?;
                }
            }
            let (session, resume) = match &self.state.agent_session {
                Some(session) => (session.clone(), true),
                None => (new_session_id()?, false),
            };
            if !resume {
                self.state.turn_seen = None;
                self.state.session_cost_micro_usd = 0;
                if self.state.handover {
                    text = self.handover() + &text;
                    self.state.handover = false;
                }
                text = self.notes_preamble() + &text;
            }
            self.state.instructions = Some(digest);
            self.save();
            self.generation += 1;
            let generation = self.generation;
            let launch = Launch {
                model: model.clone(),
                ..launch
            };
            self.agent_model = model.clone();
            self.agent = Some(Agent::spawn(
                &self.current_adapter(),
                &launch,
                &session,
                resume,
                self.state.session_cost_micro_usd,
                self.events.clone(),
                move |line| Event::Agent(generation, line),
            )?);
        }
        // ^ At the end, so a message still opens with its heading or the handover.
        text = format!("{text}\n\n{}", self.ledger_summary());
        self.save();
        let agent = self
            .agent
            .as_mut()
            .context("the manager agent is not running")?;
        if self.agent_model != model {
            agent.set_model(&model)?;
            self.agent_model = model;
        }
        agent.send(text.trim_end())
    }

    /// The manager runs inside its boundary and prompts for nothing. ref: docs/tasks.md "Permissions"
    fn launch(&self) -> Result<Launch> {
        let exe = std::env::current_exe().context("cannot find the wsx binary")?;
        let notes = self.dir.notes()?;
        let broker_socket = self.dir.broker_path();
        let overview = store::ensure_overview(&self.dir.project_id, &self.project)?;
        let boundary = Boundary::manager(&self.main, &self.dir.path, &overview)?;
        let root = config_root(&self.main)?;
        let skills = skills_plugin(&self.dir.path, root.as_deref(), &[])?;
        let workflow = load_task_config(&self.main)
            .map(|settings| settings.workflow)
            .unwrap_or_default();
        let mut text = instructions(
            &self.project,
            &notes,
            &overview,
            skills.is_some(),
            &workflow,
        );
        if let Some(role) = &self.dir.role {
            text = role_preamble(role, &self.project, &roles::brief(&self.dir)) + &text;
        }
        // ^ Other adapters list lent skills themselves (pi.rs `--skill`).
        if self.current_adapter() != "claude" {
            text = text.replace(
                &format!("with the Skill tool as `{SKILLS_PLUGIN_NAME}:<name>`"),
                "in your skills list",
            );
        }
        let mut launch = Launch::new(&self.main, boundary, &self.model, text);
        launch.effort = self.effort.clone();
        launch.plugin_dirs = skills.into_iter().collect();
        // ^ The agent's `wsx task` commands go to this host (broker.rs), which runs them as
        // the manager actor; the actor mark stays for the approval rules.
        launch.env = vec![
            (broker::BROKER_ENV.into(), broker_socket.into_os_string()),
            (ACTOR_ENV.into(), self.actor().into()),
            // ^ Its git reads must not take the index lock of the owner's checkout.
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ];
        launch.env.extend(account_env(root));
        launch.path_prefix = exe.parent().map(Path::to_path_buf);
        Ok(launch)
    }

    fn on_agent(&mut self, line: Option<AgentLine>) {
        match line {
            // ^ A stand-in's call is no conversation of the manager's.
            Some(AgentLine::Init(_)) if self.stand_in.is_some() => {}
            Some(AgentLine::Progress { .. }) if self.stand_in.is_some() => {}
            Some(AgentLine::Result(result)) if self.stand_in.is_some() => {
                self.stand_in_done(Some(*result))
            }
            None if self.stand_in.is_some() => self.stand_in_done(None),
            Some(AgentLine::Init(session)) => {
                if self.state.agent_session.as_deref() != Some(&session) {
                    self.state.agent_session = Some(session);
                    self.save();
                }
            }
            Some(AgentLine::Progress { .. }) if self.pinging => {}
            Some(AgentLine::Progress { text, .. }) => {
                self.say(&text);
                self.log(ManagerEvent::Agent { text });
            }
            Some(AgentLine::Result(result)) => self.on_result(*result),
            None => {
                self.agent = None;
                // ^ A lost ping is no stopped turn: the next turn resumes the conversation.
                if std::mem::take(&mut self.pinging) {
                    self.turn = None;
                } else if self.turn.take().is_some() {
                    self.log(ManagerEvent::Stopped {
                        reason: "the manager agent ended during a turn".into(),
                    });
                    self.report(runtime::AgentState::Error);
                }
            }
        }
    }

    fn on_result(&mut self, result: TurnResult) {
        if result.answers_nothing() {
            return;
        }
        if std::mem::take(&mut self.pinging) {
            return self.on_ping(result);
        }
        let to = self.turn.take().unwrap_or_default();
        let (adapter, model) = match &self.state.agent {
            Some(agent) => (agent.adapter.clone(), agent.model.clone()),
            None => (self.adapter.clone(), self.model.clone()),
        };
        let now = now_unix_ms();
        if let Some(message) = agent::note_spend(&adapter, &model, result.usage.cost_micro_usd, now)
        {
            self.say(&message);
        }
        // ^ A usage limit is no reply: its messages are answered once it resets.
        // ref: docs/tasks.md "Manager"
        if let Some(reached) = &result.limit {
            let attempt = self.state.limit.as_ref().map_or(0, |hold| hold.attempt) + 1;
            let until_ms = limit::resume_at(reached.resets_at, attempt, now);
            let backoff = limit::known_reset(reached.resets_at, now).is_none();
            let account = agent::account(&adapter, &model);
            if let Err(error) = store::mark_limited(&account, until_ms, now, backoff) {
                self.say(&format!("cannot record the usage limit: {error:#}"));
            }
            if self.switch_agent(until_ms, &to, self.turn_events) {
                self.save();
                self.report(runtime::AgentState::Idle);
                return;
            }
            self.say(&limit::reason(&adapter, until_ms, now));
            self.log(ManagerEvent::Limit {
                until: Some(limit::resume_time(until_ms, now)),
            });
            self.state.limit = Some(LimitHold {
                until_ms,
                attempt,
                ids: to,
                events: self.turn_events,
                switched: false,
            });
            self.save();
            self.report(runtime::AgentState::Idle);
            return;
        }
        // ^ An agent's error is no reply either: the owner sees why, and the turn tries again
        // with a backoff. A conversation that keeps failing may be the cause: a fresh one
        // follows. ref: docs/tasks.md "Manager"
        if result.is_error && result.subtype != "aborted" {
            let attempt = self.state.limit.as_ref().map_or(0, |hold| hold.attempt) + 1;
            let until_ms = limit::resume_at(None, attempt, now);
            let reason = clip(result.text.lines().next().unwrap_or_default(), 200);
            self.say(&format!("agent error, attempt {attempt}: {reason}"));
            self.log(ManagerEvent::Stopped {
                reason: reason.clone(),
            });
            self.log(ManagerEvent::Limit {
                until: Some(limit::resume_time(until_ms, now)),
            });
            let rotate = attempt == ERROR_ROTATE_ATTEMPTS;
            if rotate {
                self.rotate(0, Some(reason));
            }
            self.state.limit = Some(LimitHold {
                until_ms,
                attempt,
                ids: to,
                events: self.turn_events,
                switched: rotate || self.state.limit.as_ref().is_some_and(|hold| hold.switched),
            });
            self.save();
            self.report(runtime::AgentState::Error);
            return;
        }
        self.state.limit = None;
        let id = self.next_id();
        // ^ The last model call's context: the summed usage counts it once per call.
        let context = result.usage.context_tokens;
        self.state.session_cost_micro_usd = result.usage.session_cost_micro_usd;
        self.state.context_tokens = context;
        self.state.last_reply_ms = now_unix_ms();
        self.say(&format!(
            "reply {id}: {}",
            result.text.lines().next().unwrap_or_default()
        ));
        let in_thinking = result.answered_in_thinking();
        let text = self.quoted(result.text);
        let asks = asks_owner(&text);
        let quiet = self.turn_events && !asks && quiet_reply(&text);
        if !quiet && !says_nothing(&text) {
            self.state.last_reply = Some(id);
        }
        self.save();
        self.log(ManagerEvent::Reply {
            id,
            to,
            text,
            usage: Some(result.usage),
            quiet,
            asks,
            provisional: None,
            level: None,
            post: false,
        });
        if context > self.rotate_limit() {
            self.rotate(context, None);
        }
        self.report(runtime::AgentState::Done);
        self.check_overview();
        self.check_context();
        self.check_thought(in_thinking);
    }

    /// A turn whose words are far less than its thinking gets one follow-up, and no second one
    /// until the owner writes again: it ends the loop when the follow-up thinks just as much.
    /// ref: docs/tasks.md "Reply text"
    fn check_thought(&mut self, in_thinking: bool) {
        if in_thinking && !self.thought_asked {
            self.thought_asked = true;
            if let Err(error) = self.accept(Author::Recap, THOUGHT_ASKED.into(), Vec::new()) {
                self.say(&format!("cannot ask for the answer in thinking: {error:#}"));
            }
        }
    }

    /// A reply that opens with `> message <id>` quotes that message. ref: `resolve_quote`
    fn quoted(&self, text: String) -> String {
        if !text.trim_start().starts_with(QUOTE_REF) {
            return text;
        }
        // ^ The log back to the quoted id, not all of it: ids grow (docs/tasks.md "Log window").
        let quoted = text.trim_start()[QUOTE_REF.len()..]
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<u64>().ok());
        let log = match quoted {
            Some(id) => self
                .dir
                .read_back(QUOTE_READ_BYTES, |log| {
                    log.iter()
                        .any(|entry| entry.id().is_some_and(|at| at <= id))
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let (text, note) = resolve_quote(&text, &log);
        if let Some(note) = note {
            self.say(&note);
        }
        text
    }

    /// The manager writes the overview with its own tools: after a turn, a file over the cap
    /// gets one follow-up turn, and no second one until the owner writes again.
    /// ref: docs/tasks.md "Project overview"
    fn check_overview(&mut self) {
        if self.dir.role.is_some() {
            return;
        }
        let Ok(Some((text, _))) = store::read_overview(&self.dir.project_id) else {
            return;
        };
        let (lines, chars) = store::overview_size(&text);
        let over = lines > store::MAX_OVERVIEW_LINES || chars > store::MAX_OVERVIEW_CHARS;
        if !over {
            self.overview_asked = false;
        } else if !self.overview_asked {
            self.overview_asked = true;
            if let Err(error) =
                self.accept(Author::Recap, overview_too_long(lines, chars), Vec::new())
            {
                self.say(&format!("cannot ask for a shorter overview: {error:#}"));
            }
        }
    }

    /// `context.md` over its cap gets one request to compact it, like the overview; no second
    /// one until the owner writes again. ref: docs/tasks.md "Notes budget"
    fn check_context(&mut self) {
        if self.dir.role.is_some() {
            return;
        }
        let Ok(notes) = self.dir.notes() else {
            return;
        };
        match notes::over_cap(&notes) {
            None => self.context_asked = false,
            Some((lines, bytes)) if !self.context_asked => {
                self.context_asked = true;
                if let Err(error) = self.accept(
                    Author::Recap,
                    notes::too_long(&notes, bytes, lines),
                    Vec::new(),
                ) {
                    self.say(&format!("cannot ask for a shorter context.md: {error:#}"));
                }
            }
            Some(_) => {}
        }
    }

    /// The notes lines and the earlier conversations' exchanges that match an owner message.
    fn notes_matching(&self, said: &str) -> String {
        let Ok(notes) = self.dir.notes() else {
            return String::new();
        };
        notes::matching(&notes, said, || {
            // ^ Before the last rotation, and not the last few: the handover shows those.
            let tail = self.dir.read_tail(EARLIER_TAIL_BYTES).unwrap_or_default();
            let Some(rotated) = tail
                .iter()
                .rposition(|entry| matches!(entry.event, ManagerEvent::Rotated { .. }))
            else {
                return Vec::new();
            };
            let mut lines = recent_exchanges(&tail[..rotated], usize::MAX);
            lines.truncate(lines.len().saturating_sub(HANDOVER_ENTRIES));
            lines
        })
    }

    /// End the conversation; the next turn starts a fresh one with a short handover.
    /// ^ The ledger carries the state; a fresh conversation loses nothing it needs.
    fn rotate(&mut self, tokens: u64, reason: Option<String>) {
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.state.agent_session = None;
        self.state.instructions = None;
        self.state.turn_seen = None;
        self.state.handover = true;
        self.save();
        self.log(ManagerEvent::Rotated { tokens, reason });
    }

    /// The owner's decisions and the project context, within 10 KiB, with what is left out
    /// named: every fresh conversation starts knowing them. ref: docs/tasks.md "Notes budget"
    fn notes_preamble(&self) -> String {
        self.dir
            .notes()
            .map_or_else(|_| String::new(), |notes| notes::preamble(&notes))
    }

    /// The last exchanges of the log, so a fresh conversation knows what was just said.
    fn handover(&self) -> String {
        let log = self.dir.read_log().unwrap_or_default();
        let lines = recent_exchanges(&log, HANDOVER_ENTRIES);
        // ^ A routine's result may not have been acted on before the rotation.
        let replied = log
            .iter()
            .rev()
            .find(|entry| matches!(entry.event, ManagerEvent::Reply { usage: Some(_), .. }))
            .map_or(0, |entry| (entry.unix_ms / 1000) as i64);
        let routines = handover_routines(self.finished_routine_runs(), replied);
        let routines = match routines.is_empty() {
            true => String::new(),
            false => format!(
                "\nRoutine results (each routine's newest with its failures since its last \
                 success, and its runs since your last reply):\n{}\n",
                routines.join("\n")
            ),
        };
        format!(
            "Handover (this is a new conversation; the last exchanges before it, read the \
             ledger for the state):\n{}\n{routines}\n",
            lines.join("\n")
        )
    }

    fn save(&mut self) {
        if let Err(error) = self.dir.write_state(&self.state) {
            self.say(&format!("cannot write the manager state: {error:#}"));
        }
    }

    fn log(&self, event: ManagerEvent) {
        if let Err(error) = self.dir.append(event) {
            self.say(&format!("cannot append to the manager log: {error:#}"));
        }
    }

    fn say(&self, text: &str) {
        hosting::progress(self.started, text);
    }

    fn report(&self, state: runtime::AgentState) {
        if let Some(pane) = self.pane {
            if let Some(problem) = hosting::report_pane(pane, "manager", state, true) {
                self.say(&problem);
            }
        }
    }
}

/// The newest modification time of the files in `folder`, or none.
fn newest_ms(folder: &Path) -> Option<u64> {
    std::fs::read_dir(folder)
        .ok()?
        .filter_map(|entry| entry.ok()?.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .filter_map(|meta| {
            meta.modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
        })
        .map(|since| since.as_millis() as u64)
        .max()
}

/// Whether a run wrote nothing to stdout; an unreadable file counts as output, so that the
/// manager still hears of the run.
fn stdout_empty(run: &RunRecord) -> bool {
    std::fs::metadata(&run.stdout_path).is_ok_and(|file| file.len() == 0)
}

/// A routine run that ended as an event line with the end of its output: what a check-in
/// or any routine that prints wants the manager to read. ref: docs/tasks.md "Routines"
fn routine_line(run: &RunRecord) -> String {
    format!("{}{}", routine_result(run), output_ends(&run.final_output))
}

/// The most of a run's output the owner event holds: its end.
const EVENT_OUTPUT_CHARS: usize = 1500;
/// The most files an owner event links.
const EVENT_LINKS: usize = 3;
/// The second clause of an owner event line (`routine_event`), which `routine_for_owner` reads.
const FOR_OWNER: &str = "for the owner";

/// A routine run's event for the owner, on one line, since the chat shows an event line only to
/// its first `; `: `routine nightly succeeded after 4 s; for the owner; output ends: <the last
/// 1,500 characters, newlines as ⏎>; files: [board.html](files/<path>) …`, a link to each file of
/// the main checkout the output names, which the web opens in its files page
/// (docs/web-client.md "Files"). ref: docs/tasks.md "Routines"
fn routine_event(run: &RunRecord, main: &Path) -> String {
    let mut line = format!("{}; {FOR_OWNER}", routine_result(run));
    let output = run.final_output.trim();
    if !output.is_empty() {
        let skip = output.chars().count().saturating_sub(EVENT_OUTPUT_CHARS);
        let cut = if skip > 0 { "…" } else { "" };
        let end: String = output.chars().skip(skip).collect();
        line.push_str(&format!(
            "; output ends: {cut}{}",
            end.lines().collect::<Vec<_>>().join(" ⏎ ")
        ));
    }
    let links: Vec<String> = named_files(output, main)
        .iter()
        .map(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            format!("[{name}](files/{})", link_path(path))
        })
        .collect();
    if !links.is_empty() {
        line.push_str(&format!("; files: {}", links.join(" ")));
    }
    line
}

/// Whether an events line is a routine run's event for the owner (`routine_event`): the manager
/// answers its turn with a message, not a quiet line. An output line never matches, since
/// `routine_event` holds the output in one line.
fn routine_for_owner(line: &str) -> bool {
    line.starts_with("routine ") && line.split("; ").nth(1) == Some(FOR_OWNER)
}

/// The files of the main checkout that `output` names, as paths relative to it: a word with a
/// `/` or a `.`, relative to the checkout or absolute inside it, never under `.git`.
fn named_files(output: &str, main: &Path) -> Vec<String> {
    let root = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
    let mut found: Vec<String> = Vec::new();
    let words = output.split(|c: char| c.is_whitespace() || "\"'`()[]<>,;=".contains(c));
    for word in words {
        let word = word.trim_end_matches(['.', ':']);
        if word.len() > 512 || !word.contains(['/', '.']) {
            continue;
        }
        let path = Path::new(word);
        let relative = match path.is_absolute() {
            true => match path
                .strip_prefix(&root)
                .or_else(|_| path.strip_prefix(main))
            {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => continue,
            },
            false => path.to_path_buf(),
        };
        let normal = relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"));
        let Some(text) = relative.to_str().filter(|_| normal) else {
            continue;
        };
        if found.iter().any(|known| known == text)
            || !std::fs::symlink_metadata(root.join(&relative)).is_ok_and(|meta| meta.is_file())
        {
            continue;
        }
        found.push(text.to_string());
        if found.len() == EVENT_LINKS {
            break;
        }
    }
    found
}

/// A relative path as a link's address: each byte outside `A-Z a-z 0-9 - . _ ~ /` as `%XX`.
fn link_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// `routine <name> succeeded after 4 s`, `routine <name> failed (exit 3)`, `routine <name> found
/// something (exit 1)`.
fn routine_result(run: &RunRecord) -> String {
    let end = run.finished_epoch.unwrap_or_default();
    match &run.status {
        RunStatus::Succeeded => format!(
            "routine {} succeeded after {} s",
            run.routine,
            end - run.started_epoch
        ),
        status => format!(
            "routine {} {} ({})",
            run.routine,
            match status {
                RunStatus::Findings => "found something",
                RunStatus::SpawnFailed => "could not start",
                RunStatus::Cancelled => "was cancelled",
                RunStatus::Interrupted => "was interrupted",
                _ => "failed",
            },
            exit_words(run.exit_code),
        ),
    }
}

/// What a fresh conversation needs of the routines, at most two lines per routine, the
/// routines in the order of their newest run: its standing (the newest run, or the newest
/// failure since its last success with how many there were), then its runs after the previous
/// conversation's last reply (`replied`, epoch seconds) when they are more than that newest run.
/// ref: docs/tasks.md "Manager"
fn handover_routines(mut runs: Vec<RunRecord>, replied: i64) -> Vec<String> {
    runs.sort_by_key(|run| run.finished_epoch);
    let mut names: Vec<&str> = Vec::new();
    for run in runs.iter().rev() {
        if !names.contains(&run.routine.as_str()) {
            names.push(&run.routine);
        }
    }
    let mut lines = Vec::new();
    for name in names.into_iter().rev() {
        let mine: Vec<&RunRecord> = runs.iter().filter(|run| run.routine == name).collect();
        let Some(newest) = mine.last() else { continue };
        let success = mine.iter().rposition(|run| run.status.completed());
        let failed = mine.len() - success.map_or(0, |at| at + 1);
        let plural = if failed == 1 { "" } else { "s" };
        lines.push(match success {
            _ if failed == 0 => routine_result(newest),
            Some(at) => format!(
                "{}; {failed} failure{plural} since its last success at {}",
                routine_result(newest),
                clock(mine[at].finished_epoch.unwrap_or_default())
            ),
            None => format!(
                "{}; {failed} failure{plural}, never succeeded",
                routine_result(newest)
            ),
        });
        let after: Vec<&&RunRecord> = mine
            .iter()
            .filter(|run| run.finished_epoch.is_some_and(|end| end > replied))
            .collect();
        if after.len() > 1 {
            let failed = after.iter().filter(|run| !run.status.completed()).count();
            lines.push(format!(
                "routine {name} ran {} times since your last reply, {failed} of them without success",
                after.len()
            ));
        }
    }
    lines
}

/// The last `limit` owner messages and replies, oldest first, each on one line.
fn recent_exchanges(log: &[ManagerEntry], limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = log
        .iter()
        .rev()
        .filter_map(|entry| match &entry.event {
            ManagerEvent::Message {
                id,
                from: Author::Owner,
                text,
                ..
            } => Some(format!("Owner (message {id}): {}", one_line(text))),
            ManagerEvent::Reply {
                id,
                text,
                provisional: Some(name),
                ..
            } => Some(format!("Stand-in {name} (reply {id}): {}", one_line(text))),
            // ^ A post (`wsx task post`, the digest, the session news): the owner saw it, the
            // manager did not write it.
            ManagerEvent::Reply {
                id,
                text,
                post: true,
                ..
            } => Some(format!("Posted (reply {id}): {}", one_line(text))),
            ManagerEvent::Reply { id, text, .. } => {
                Some(format!("You (reply {id}): {}", one_line(text)))
            }
            _ => None,
        })
        .take(limit)
        .collect();
    lines.reverse();
    lines
}

/// `2026-10-28 03:00` in this machine's zone.
fn clock(secs: i64) -> String {
    limit::local_time(secs.max(0) as u64).map_or_else(
        || secs.to_string(),
        |tm| {
            format!(
                "{}-{:02}-{:02} {:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            )
        },
    )
}

/// An event line the chat shows as its first clause (`Short::plain`).
fn plain_event(line: String) -> (String, Short) {
    let short = Short::plain(&line);
    (line, short)
}

fn exit_words(exit: Option<i32>) -> String {
    exit.map_or("no exit code".into(), |code| format!("exit {code}"))
}

/// "; its output ends:" and the last lines of `output`, indented, or nothing without output.
fn output_ends(output: &str) -> String {
    let lines: Vec<String> = ledger::tail_lines(output, DEPLOY_EVENT_LINES)
        .lines()
        .map(|line| format!("    {}", clip(line, EVENT_SUMMARY_CHARS)))
        .collect();
    match lines.is_empty() {
        true => String::new(),
        false => format!("; its output ends:\n{}", lines.join("\n")),
    }
}

/// `wsx task new -p <project> --title <IDLE_TITLE> -` as the manager; the new task's number.
fn new_task(exe: &Path, project: &str, prompt: &str) -> Result<u32, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args([
            "task",
            "new",
            "-p",
            project,
            "--title",
            IDLE_TITLE,
            "--no-steps",
            "--json",
            "-",
        ])
        .env(ACTOR_ENV, "manager")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run wsx task new: {error}"))?;
    // ^ Dropped after the write, so the child reads the end of its input.
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(prompt.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot run wsx task new: {error}"))?;
    if !output.status.success() || !matches!(written, Some(Ok(()))) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .ok()
        .and_then(|value| value["id"].as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .ok_or_else(|| "wsx task new printed no task number".into())
}

/// The end of a notes file, at most `MAX_NOTES_READ` bytes: the newest lines are last.
/// A link, a missing file or text that is not UTF-8 reads as empty.
pub(super) fn read_note(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return String::new();
    };
    let size = file.metadata().map_or(0, |meta| meta.len());
    let mut bytes = Vec::new();
    if file
        .seek(SeekFrom::Start(size.saturating_sub(MAX_NOTES_READ)))
        .is_err()
        || file.take(MAX_NOTES_READ).read_to_end(&mut bytes).is_err()
    {
        return String::new();
    }
    // ^ A cut at the start may split a character and leaves a partial line: drop that line.
    let start = match size > MAX_NOTES_READ {
        true => bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |at| at + 1),
        false => 0,
    };
    String::from_utf8(bytes.split_off(start)).unwrap_or_default()
}
