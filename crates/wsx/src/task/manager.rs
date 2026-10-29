//! `wsx task manager-host`: one manager agent per project in a hidden session in the
//! main checkout. It edits no project file but a commit of the owner's own changes on the
//! owner's yes; it answers from the ledger, acts through
//! `wsx task` commands and keeps notes in its own folder. With `--role <name>` the same host
//! runs a role: its brief, its own folder and only its tasks' events, and it leaves once idle.
//! ref: docs/tasks.md "Manager", "Roles"

use super::boundary::Boundary;
use super::claude::{
    account_env, config_root, new_session_id, skills_plugin, AgentLine, ClaudeAgent, Launch,
    TurnResult, SKILLS_PLUGIN_NAME,
};
use super::{broker, hosting, limit, stall};
use anyhow::{Context, Result};
use asched_core::routine::{store::RoutineStore, RunRecord, RunStatus};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskWorkflow;
use wsx_core::runtime;
use wsx_core::task::manager::{
    decided_request, instructions_digest, pending_approvals, quiet_reply, ApprovalLine,
};
use wsx_core::task::manager::{
    Author, LimitHold, ManagerEntry, ManagerEvent, ManagerRequest, ManagerResponse, ManagerState,
    Seen,
};
use wsx_core::task::store;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{self, clip, project_tasks, read_result};
use wsx_core::task::{
    checks, ledger, now_unix_ms, LogEvent, TaskRecord, TaskState, WaitOn, MAX_MESSAGE_BYTES,
};
use wsx_core::task::{owner, roles};

use wsx_core::task::owner::ACTOR_ENV;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
/// Events within this long of each other, or during a turn, become one message.
const EVENT_QUIET: Duration = Duration::from_secs(3);
/// How often the host removes the captures of long-decided "To check" items.
const CAPTURE_SWEEP: Duration = Duration::from_secs(60 * 60);
/// What wsx asks after a quiet period. The reply is the recap the owner reads.
const RECAP_REQUEST: &str = "The project has been quiet. Write the owner a short recap, at most 8 lines: \
what landed and is live, what runs, and what waits for the owner and why. Read the ledger for it. Start no \
task and change nothing. Name a task by its number and what it does.";
const EVENT_TITLE_CHARS: usize = 80;
/// The turn after a usage limit: the messages of the turn it ended got no answer yet.
const LIMIT_CONTINUE: &str = "The session limit has reset. Answer the messages before this one \
that got no reply yet, then any below.";
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
const HANDOVER_CHARS: usize = 600;
const EVENT_SUMMARY_CHARS: usize = 160;
/// A failed deploy's event carries this many last lines of its output.
const DEPLOY_EVENT_LINES: usize = 10;
/// The ledger summary at the end of a turn's message: at most this many task lines.
const SUMMARY_ROWS: usize = 12;
const SUMMARY_TITLE_CHARS: usize = 60;

/// Task states the manager hears about: not a task the owner works on by hand.
fn is_event(record: &TaskRecord) -> bool {
    match record.state {
        TaskState::Waiting => !record.waits_on(WaitOn::Hand),
        TaskState::Ready | TaskState::Failed | TaskState::Landed | TaskState::Closed => true,
        _ => false,
    }
}

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
        text: String,
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
    model: String,
    /// `tasks.manager.eventModel`: the model of a turn with only events and no review due.
    event_model: Option<String>,
    /// The model the running agent process uses now.
    agent_model: String,
    rotate_tokens: u64,
    state: ManagerState,
    events: Sender<Event>,
    agent: Option<ClaudeAgent>,
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
    /// Event lines not yet logged as a message, and when the last one came.
    pending_events: Vec<String>,
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
    /// Event lines of tasks a role created, by role, not yet sent to it.
    role_events: BTreeMap<String, Vec<String>>,
    roles_swept: Option<Instant>,
    /// A role host that is closed or idle: it leaves.
    leaving: bool,
}

/// The plugin that lends the manager the owner's skills, `manager/<this>/`.
/// The manager's lean project context in its notes folder. ref: docs/tasks.md "Memo"
const CONTEXT_FILE: &str = "context.md";
/// The owner's decisions, one line each, appended by the manager as they are made.
const DECISIONS_FILE: &str = "decisions.md";
/// The most of both files a fresh conversation's first message carries.
const MAX_NOTES_BYTES: usize = 8 * 1024;
// ^ The agent writes the notes: a link or a huge file must not reach the host's read.
const MAX_NOTES_READ: u64 = 256 * 1024;

/// ref: docs/tasks.md "Manager", "Memo"
fn instructions(
    project: &str,
    notes: &Path,
    page: &Path,
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
         Talk like a person in a chat, not a report: answer what the owner asked in a few \
         short lines. {presentation} \
         A turn of only Events is your work, not a message for the owner. Act on it, then \
         reply with one short line of what you did, or with exactly `-` when nothing \
         changed for the owner. Write more only when the owner must decide now, and then \
         ask one plain question. Never hand the owner chores: no commands, files, settings or \
         tasks to verify. Do such work through a task, or drop \
         it. \
         Start a request for work at once and say the plan in a few lines in the same reply \
         (goal, tasks, choices, what you leave out); ask first only when a choice the owner \
         has not made changes what the owner gets, or the work is large or costly to undo \
         (then `--plan` or `--hold`). A longer plan goes on the project page with a mermaid \
         diagram, and the reply links it. Before `wsx task new`, compare the request with \
         the open tasks (titles and files): similar work or the same area goes to that task \
         with `wsx task add`; a new task only for separate work, run at once. Start one \
         with `wsx task new --title <title> --files <glob> \"<prompt>\"`: the title is a few plain words of at \
         most 60 characters, never starting with 'Owner'; pass one `--files` per path or \
         glob it will change, in every task new (`wsx task add <n> --files <glob> \
         \"<why>\"` adds more). The prompt stands alone, since a worker cannot ask: the goal, \
         the owner's decisions, the scope, the files and how it proves the result. When you have read the code and can state the goal exactly, \
         and the prompt carries the files, what they do now, the change and its proof, run \
         it with `--model sonnet` and `--skip` for each step of `tasks.workflow.steps` your \
         prompt already did (context, plan). Work that needs deep exploration, an unknown \
         cause or a change to a module's design keeps the default model and every step. A \
         broad hunt (find all bugs) is first one read-only audit task per area, then one \
         task per fix. With \
         `--plan` the worker writes a short plan and the task waits on `plan`; read it with \
         `wsx task show <n> --json` and continue with `wsx task add <n> \"go\"` or what to \
         change. Never ask the owner to wait. `wsx task add` or \
         `wsx task steer` changes a running task. \
         A ready task waits for your review when you created it, or the owner did while the \
         ledger's `auto_land` is true and checks ran. Read its diff (`git -C <working copy> \
         diff <base>..HEAD`; `wsx task show <n> --json` names both) against the owner's \
         words and for lean code: no copy of a helper that exists, no dead code, nothing \
         the request did not ask for. Then `wsx task land <n>`, or `wsx task add <n> \
         \"<what to change>\"`. A land rebases and runs the checks \
         again by itself: never send a task back to rebase. Report checks from the ledger's \
         `checks` and trust them over the worker's words. {review}When a worker leaves for a \
         person what a command, a test or a capture could prove, tell that worker to prove \
         it. What only a person can check goes to the To check list (the ledger's \
         `to_check`); never pass, fail or skip items yourself. A release refuses while items are \
         open, unless the owner's message asks for it (`--owner-said`): then the items stay \
         to check on the release. `deploy` and `release` answer at once; the \
         result comes as an event. `wsx task routine` lists and runs the project's \
         routines (results as events and in the ledger's `routines`); add, edit and delete \
         need `--owner-said`. When the last task of a request \
         lands and no open task \
         changes the same files, start one housekeeping task over the files those tasks changed, with this \
         prompt and the task numbers: {batch} \
         When the owner wants to be sure of a change or to try it, start \
         it with `wsx task new --hold \"<why>\"` (`wsx task hold <n> \"<why>\"` holds a \
         running one, `--off` releases it). \
         Long multi-step work goes into one task: each step is `wsx task new --into <n>`, \
         which holds #<n>; land a step when an event names it ready, and #<n> once the owner \
         says so. Work across many tasks of one area, such as a parent with 3 or more \
         children, may get a role, a second manager with its own chat: ask the owner once, \
         then `wsx task role new <name> --brief <file>`. \
         Say in one line how to try it: Try live (`wsx task trial <n>`), or a preview \
         (`wsx task preview <n>`) when the live site cannot roll back from it, such as a \
         daemon or a migration. \
         When the owner's message in this turn asks for the action itself (\"land #5\", \
         \"close #42\"), pass `--owner-said <id>`, the id in its 'Owner (message \
         <id>)' heading, to `land`, `close --discard`, `deploy` or `release`; never name a \
         message that did not ask for that action. When a command says the owner was asked, \
         say so. An event 'approval <id> (...) dropped': \
         tell the owner why in one line. A land needs no approval, and the project deploys \
         by itself when the ledger's `deploys` say so; call a change live only when \
         `deploys` shows its commit. \
         A task waits when the ledger shows `waiting`. On `owner`, the worker asked or \
         stopped: answer with `wsx task add` when you can, else ask the owner in plain \
         words. On `disk` or `limit` it resumes by itself. An event '#<n> stalled' is wsx's \
         finding that the task is stuck with the evidence: act on it with `wsx task` (a silent \
         worker: `stop`, then `add` what to do), start a task that fixes wsx when the evidence \
         shows its bug, else tell the owner in one line what is stuck and why. On \
         `hand`, the owner works in its copy: never add, steer, land or close it; only the \
         owner runs `wsx task hand` and `handback`. \
         Nothing asks you for permission inside wsx's boundary. Read the \
         project and the copies with the file and web tools and read-only git in Bash. The \
         one git command that writes: when a land refuses because the main checkout has the \
         owner's uncommitted changes, name those files and ask; only after the owner says \
         yes, commit exactly those files in the main checkout (`git commit -m \"<message \
         that names them>\" -- <files>`) and land again. {skills}Use the skills and \
         subagents that fit; check rather than guess. \
         When the owner decides a rule, a preference or a choice, append one line \
         `- <YYYY-MM-DD> <decision>` to {decisions} at once. Every new conversation of yours \
         starts with that file and {context}, your lean project context. The event of a \
         task that lands or closes carries its worker's 'memo:': fold it into {context} \
         (merge duplicates, drop the stale, at most 60 lines). When a message opens with \
         'Handover' (a new conversation), first fold the memos of the ledger's recent tasks \
         the same way. The project page {page} is the owner's one-page picture: the goal, \
         the tasks as a mermaid graph, the plans, the decisions and the open questions. \
         Update a section in place when it changes; never rewrite the file or create \
         another page. \
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
        batch = workflow.batch,
        presentation = wsx_core::integration::PRESENTATION.trim_end(),
        context = notes.join(CONTEXT_FILE).display(),
        decisions = notes.join(DECISIONS_FILE).display(),
        notes = notes.display(),
        page = page.display()
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
         recap, approvals, deploys, releases, routines and the project page. You never create a \
         role or change the project's config. Message the manager with `wsx task say \
         --from-role \"<text>\"`; a message headed 'Manager' or 'Role <name>' asks you \
         something: answer it once with `wsx task say --role <name> --from-role --reply \
         \"<answer>\"` (`--role manager` for the manager).\n\nThe owner's brief:\n{brief}\n\n\
         The manager's instructions:\n"
    )
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
        model: settings.manager_model,
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
        pending_events: Vec::new(),
        last_event: None,
        recap_quiet: (settings.manager_recap_secs > 0)
            .then(|| Duration::from_secs(settings.manager_recap_secs)),
        last_activity: Instant::now(),
        binary: hosting::Binary::capture(),
        reexec: false,
        captures_swept: None,
        owed: Owed::default(),
        role_events: BTreeMap::new(),
        roles_swept: None,
        leaving: false,
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
        if let Some(agent) = manager.agent.take() {
            agent.stop();
        }
        let _ = std::fs::remove_file(manager.dir.socket_path());
        let _ = std::fs::remove_file(manager.dir.broker_path());
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
                Event::Tick if self.dir.role.is_some() => self.leave_when_done(),
                Event::Tick => {
                    self.watch_tasks();
                    self.watch_stalls();
                    self.ask_recap();
                    self.sweep_captures();
                    self.close_idle_roles();
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
                Event::Undelivered { role, text } => {
                    self.pending_events.push(format!(
                        "role {role} did not take the events of its tasks, so they are yours:\n{text}"
                    ));
                    self.last_event = Some(Instant::now());
                }
            }
            if self.turn.is_none() {
                self.flush_events();
                self.next_turn();
            } else {
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
            ManagerRequest::Say { text, from } => {
                match self.accept(from.unwrap_or(Author::Owner), text) {
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
        }
    }

    /// Re-queue messages logged but never delivered, and pick up what was last reported.
    /// ^ No event is lost across a restart: a change reported in no logged message is
    /// found again because `seen` is saved only when its message is logged.
    fn restore(&mut self) -> Result<()> {
        let log = self.dir.read_log()?;
        let delivered: std::collections::HashSet<u64> = log
            .iter()
            .flat_map(|entry| match &entry.event {
                ManagerEvent::Delivered { ids } => ids.clone(),
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
            if let ManagerEvent::Message { id, from, text } = entry.event {
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
            let verdict = match (approved, outcome.starts_with(owner::DROPPED)) {
                (true, _) => format!("approved; {outcome}"),
                (false, true) => outcome.clone(),
                (false, false) => format!("denied; {}", note.as_deref().unwrap_or("no note")),
            };
            self.pending_events.push(format!(
                "approval {id} ({asked}) {}",
                clip(&verdict, EVENT_SUMMARY_CHARS * 2)
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
            if record.exit == Some(0) {
                self.pending_events.push(format!(
                    "{} of {:.12} succeeded after {} s",
                    ledger::kind_name(record.kind),
                    record.commit,
                    record.duration_ms / 1000
                ));
                self.last_event = Some(Instant::now());
                continue;
            }
            self.pending_events.push(format!(
                "{} of {:.12} failed ({}){}",
                ledger::kind_name(record.kind),
                record.commit,
                exit_words(record.exit),
                output_ends(record.output.as_deref().unwrap_or_default())
            ));
            self.last_event = Some(Instant::now());
        }
    }

    /// The project's routine runs that ended, as wsxd's scheduler recorded them.
    fn finished_routine_runs(&self) -> Vec<RunRecord> {
        let Ok(root) = asched_core::RegistryStore::default_root() else {
            return Vec::new();
        };
        RoutineStore::new(root, &self.main)
            .and_then(|store| store.load_runtime())
            .map(|state| state.runs.into_values().flatten().collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
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

    /// Turn each routine run that ended into an event line, a failure with the end of its
    /// output. ref: docs/tasks.md "Routines"
    fn watch_routines(&mut self) {
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
        for run in runs {
            self.observed_routines = run.finished_epoch.unwrap_or_default();
            self.pending_events.push(routine_line(&run));
            self.last_event = Some(Instant::now());
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
            if !is_event(&record) {
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
                let mut summary = match checks.len() {
                    0 => worker,
                    total => format!("checks {passed} of {total} passed; {worker}"),
                };
                if let Some(waiting) = &record.waiting {
                    summary = format!(
                        "waits on {}: {}; {summary}",
                        waiting.on.as_str(),
                        waiting.reason
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
                        summary.push_str(&format!("; memo: {}", clip(&memo, ledger::MEMO_CHARS)));
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
            self.task_event(&record, &open_roles, line);
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
    fn task_event(&mut self, record: &TaskRecord, open_roles: &[String], line: String) {
        match record
            .created_by
            .role()
            .filter(|role| open_roles.iter().any(|open| open == role))
        {
            Some(role) => self
                .role_events
                .entry(role.to_string())
                .or_default()
                .push(line),
            None => self.pending_events.push(line),
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
            self.task_event(&record, &open_roles, line);
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
            self.pending_events.push(format!(
                "role {role} closed by wsx: no turn for tasks.roles.closeDays; its notes are kept"
            ));
            self.last_event = Some(Instant::now());
        }
    }

    /// A role's host leaves when its role is closed, or after `ROLE_LINGER` with no message
    /// and no turn: an idle role costs no agent and no pane.
    fn leave_when_done(&mut self) {
        let closed = !roles::is_open(&self.dir);
        let idle = self.turn.is_none()
            && self.queue.is_empty()
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
                let text = lines.join("\n");
                let sent = owner::say(
                    &project,
                    &main,
                    &exe,
                    Some(&role),
                    super::launch_manager(&main),
                    text.clone(),
                    Some(Author::Events),
                );
                if !sent.is_ok_and(|response| response.ok) {
                    let _ = events.send(Event::Undelivered { role, text });
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
        let text = self.pending_events.join("\n");
        match self.accept(Author::Events, text) {
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
    fn accept(&mut self, from: Author, text: String) -> Result<u64> {
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
        })?;
        self.last_activity = Instant::now();
        self.enqueue(id, from, text);
        Ok(id)
    }

    /// After `recapMinutes` with no message and no turn: start the idle task when it is due,
    /// and ask the manager for a recap when something changed since the last one, with the
    /// token cost since. A quiet project gets one recap. ref: docs/tasks.md "Recap"
    fn ask_recap(&mut self) {
        let due = self.recap_quiet.is_some_and(|quiet| {
            self.turn.is_none()
                && self.queue.is_empty()
                && self.pending_events.is_empty()
                && self.last_activity.elapsed() >= quiet
        });
        if !due {
            return;
        }
        // ^ Once per quiet period: the next look waits a whole period.
        self.last_activity = Instant::now();
        self.start_idle();
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
        if let Err(error) = self.accept(Author::Recap, text) {
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
        let manager: Vec<u64> = log
            .iter()
            .filter_map(|entry| match &entry.event {
                ManagerEvent::Reply { usage, .. } => {
                    Some(usage.as_ref().map_or(0, |usage| usage.cost_micro_usd))
                }
                _ => None,
            })
            .collect();
        let mut tasks: Vec<(u64, u32, usize)> = project_tasks(&self.dir.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(dir, record)| {
                let costs: Vec<u64> = dir
                    .read_log()
                    .ok()?
                    .into_iter()
                    .filter(|entry| entry.unix_ms > since)
                    .filter_map(|entry| match entry.event {
                        LogEvent::Result { usage, .. } => {
                            Some(usage.map_or(0, |usage| usage.cost_micro_usd))
                        }
                        _ => None,
                    })
                    .collect();
                let id = record.ok()?.id;
                (!costs.is_empty()).then(|| (costs.iter().sum(), id, costs.len()))
            })
            .collect();
        tasks.sort_by(|left, right| right.cmp(left));
        let usd = |micro: u64| format!("{:.2} USD", micro as f64 / 1e6);
        let workers: u64 = tasks.iter().map(|(cost, ..)| cost).sum();
        let turns: usize = tasks.iter().map(|(.., turns)| turns).sum();
        let top: Vec<String> = tasks
            .iter()
            .take(3)
            .map(|(cost, id, _)| format!("#{id} {}", usd(*cost)))
            .collect();
        format!(
            "Token cost since the last recap: {} in all; you {} in {} turn(s), workers {} in {turns} turn(s){}.",
            usd(manager.iter().sum::<u64>() + workers),
            usd(manager.iter().sum()),
            manager.len(),
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
        if prompt.is_empty() || open || notes.is_none_or(|ms| ms <= self.state.idle_ms.unwrap_or(0))
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

    fn next_id(&mut self) -> u64 {
        self.state.next_id += 1;
        self.save();
        self.state.next_id
    }

    /// Start a turn with every waiting owner message, or else with the waiting events.
    fn next_turn(&mut self) {
        // ^ During a usage limit the messages wait; after it they go with the held ones.
        let held = self.state.limit.clone();
        if held
            .as_ref()
            .is_some_and(|hold| now_unix_ms() < hold.until_ms)
            || (held.is_none() && self.queue.is_empty())
        {
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
            .front()
            .map_or(Author::Owner, |(_, author, _)| author.clone());
        let mut ids = Vec::new();
        let mut text = String::new();
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
                } => {
                    *self
                        .owed
                        .lock()
                        .expect("owed lock")
                        .entry(project.clone())
                        .or_default() += 1;
                    format!("Manager {project}")
                }
                Author::Manager {
                    project,
                    reply: true,
                } => format!("Manager {project} replies"),
                // ^ A role's or this project's manager's message is answered once, as a
                // manager's (docs/tasks.md "Roles").
                Author::Role { name, reply } => {
                    if !reply {
                        *self
                            .owed
                            .lock()
                            .expect("owed lock")
                            .entry(format!("role:{name}"))
                            .or_default() += 1;
                    }
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
            text.push_str(&format!("{heading} (message {id}):\n{body}\n\n"));
            ids.push(id);
        }
        // ^ The limit preface is no owner message: a held turn is events-only when its
        // held and its new messages all are.
        let events_only = held.as_ref().map_or(true, |hold| hold.events)
            && if ids.is_empty() {
                held.is_some()
            } else {
                first == Author::Events
            };
        if held.is_some() {
            text = format!("{LIMIT_CONTINUE}\n\n{text}");
        }
        if let Err(error) = self.send_turn(text.trim_end(), events_only) {
            self.say(&format!("cannot start the manager agent: {error:#}"));
            self.log(ManagerEvent::Stopped {
                reason: format!("cannot start the manager agent: {error:#}"),
            });
            return;
        }
        if held.is_some() {
            self.log(ManagerEvent::Limit { until: None });
        }
        if !ids.is_empty() {
            self.log(ManagerEvent::Delivered { ids: ids.clone() });
        }
        let mut turn = held.map(|hold| hold.ids).unwrap_or_default();
        turn.extend(ids);
        self.turn = Some(turn);
        self.turn_events = events_only;
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
        match &self.event_model {
            Some(model) if events_only && !review_due() => model.clone(),
            _ => self.model.clone(),
        }
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

    fn send_turn(&mut self, text: &str, events_only: bool) -> Result<()> {
        let mut text = text.to_string();
        let model = self.turn_model(events_only);
        if self.agent.is_none() {
            let mut launch = self.launch()?;
            let digest = instructions_digest(&launch.instructions);
            // ^ Claude keeps the system prompt a conversation began with, also on resume:
            // new instructions need a new conversation (docs/tasks.md "Manager").
            if self.state.agent_session.is_some()
                && self.state.instructions.as_deref() != Some(&digest)
            {
                self.rotate(0, Some("the manager's instructions changed".into()));
                launch = self.launch()?;
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
            self.agent = Some(ClaudeAgent::spawn(
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
        let page = store::ensure_page(&self.dir.project_id, &self.project)?;
        let boundary = Boundary::manager(&self.main, &self.dir.path, &page)?;
        let root = config_root(&self.main)?;
        let skills = skills_plugin(&self.dir.path, root.as_deref())?;
        let workflow = load_task_config(&self.main)
            .map(|settings| settings.workflow)
            .unwrap_or_default();
        let mut text = instructions(&self.project, &notes, &page, skills.is_some(), &workflow);
        if let Some(role) = &self.dir.role {
            text = role_preamble(role, &self.project, &roles::brief(&self.dir)) + &text;
        }
        let mut launch = Launch::new(&self.main, boundary, &self.model, text);
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
            Some(AgentLine::Init(session)) => {
                if self.state.agent_session.as_deref() != Some(&session) {
                    self.state.agent_session = Some(session);
                    self.save();
                }
            }
            Some(AgentLine::Progress(text)) => {
                self.say(&text);
                self.log(ManagerEvent::Agent { text });
            }
            Some(AgentLine::Result(result)) => self.on_result(result),
            None => {
                self.agent = None;
                if self.turn.take().is_some() {
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
        let to = self.turn.take().unwrap_or_default();
        // ^ A usage limit is no reply: its messages are answered once it resets.
        // ref: docs/tasks.md "Manager"
        if let Some(reached) = &result.limit {
            let attempt = self.state.limit.as_ref().map_or(0, |hold| hold.attempt) + 1;
            let now = now_unix_ms();
            let until_ms = limit::resume_at(reached.resets_at, attempt, now);
            self.say(&limit::reason(until_ms, now));
            self.log(ManagerEvent::Limit {
                until: Some(limit::resume_time(until_ms, now)),
            });
            self.state.limit = Some(LimitHold {
                until_ms,
                attempt,
                ids: to,
                events: self.turn_events,
            });
            self.save();
            self.report(runtime::AgentState::Idle);
            return;
        }
        self.state.limit = None;
        let id = self.next_id();
        // ^ The last model call's context: the summed usage counts it once per call.
        let context = result.usage.context_tokens;
        self.state.session_cost_micro_usd = result.usage.session_cost_micro_usd;
        self.save();
        self.say(&format!(
            "reply {id}: {}",
            result.text.lines().next().unwrap_or_default()
        ));
        let quiet = self.turn_events && quiet_reply(&result.text);
        self.log(ManagerEvent::Reply {
            id,
            to,
            text: result.text,
            usage: Some(result.usage),
            quiet,
        });
        if context > self.rotate_tokens {
            self.rotate(context, None);
        }
        self.report(runtime::AgentState::Done);
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

    /// The owner's decisions, newest first, then the project context, at most 8 KiB
    /// together: every fresh conversation starts knowing them. ref: docs/tasks.md "Memo"
    fn notes_preamble(&self) -> String {
        let Ok(notes) = self.dir.notes() else {
            return String::new();
        };
        let read = |name: &str| read_note(&notes.join(name));
        let decisions: Vec<String> = read(DECISIONS_FILE)
            .lines()
            .rev()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        let context = read(CONTEXT_FILE);
        let mut out = String::new();
        let mut budget = MAX_NOTES_BYTES;
        let mut take = |line: &str, out: &mut String| {
            if line.len() + 1 > budget {
                return false;
            }
            budget -= line.len() + 1;
            out.push_str(line);
            out.push('\n');
            true
        };
        if !decisions.is_empty() {
            out.push_str("The owner's decisions (decisions.md, newest first):\n");
            for line in &decisions {
                if !take(line, &mut out) {
                    break;
                }
            }
            out.push('\n');
        }
        if !context.trim().is_empty() {
            out.push_str("Your project context (context.md):\n");
            for line in context.lines() {
                if !take(line, &mut out) {
                    break;
                }
            }
            out.push('\n');
        }
        out
    }

    /// The last exchanges of the log, so a fresh conversation knows what was just said.
    fn handover(&self) -> String {
        let log = self.dir.read_log().unwrap_or_default();
        let mut lines: Vec<String> = log
            .iter()
            .rev()
            .filter_map(|entry| match &entry.event {
                ManagerEvent::Message {
                    from: Author::Owner,
                    text,
                    ..
                } => Some(format!("Owner: {}", one_line(text))),
                ManagerEvent::Reply { text, .. } => Some(format!("You: {}", one_line(text))),
                _ => None,
            })
            .take(HANDOVER_ENTRIES)
            .collect();
        lines.reverse();
        // ^ A routine's result may not have been acted on before the rotation.
        let replied = log
            .iter()
            .rev()
            .find(|entry| matches!(entry.event, ManagerEvent::Reply { .. }))
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
        // ^ Always sent after a rotation: it is the manager's cue to fold the memos.
        format!(
            "Handover (this is a new conversation; fold the ledger's memos into your context \
             first; the last exchanges before it, read the ledger for the state):\n{}\n{routines}\n",
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

/// A routine run that ended as an event line, a failure with the end of its output.
fn routine_line(run: &RunRecord) -> String {
    match run.status {
        RunStatus::Succeeded => routine_result(run),
        _ => format!("{}{}", routine_result(run), output_ends(&run.final_output)),
    }
}

/// `routine <name> succeeded after 4 s`, `routine <name> failed (exit 3)`.
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
        let success = mine
            .iter()
            .rposition(|run| run.status == RunStatus::Succeeded);
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
            let failed = after
                .iter()
                .filter(|run| run.status != RunStatus::Succeeded)
                .count();
            lines.push(format!(
                "routine {name} ran {} times since your last reply, {failed} of them without success",
                after.len()
            ));
        }
    }
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
fn read_note(path: &Path) -> String {
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
