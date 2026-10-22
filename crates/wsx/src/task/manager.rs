//! `wsx task manager-host`: one manager agent per project in a hidden session in the
//! main checkout. It edits no project file; it answers from the ledger, acts through
//! `wsx task` commands and keeps notes in its own folder. ref: docs/tasks.md "Manager"

use super::claude::{
    kept_from_edits, new_session_id, skills_plugin, AgentLine, Boundary, ClaudeAgent, Launch,
    TurnResult, SKILLS_PLUGIN_NAME,
};
use super::{broker, hosting};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskWorkflow;
use wsx_core::runtime;
use wsx_core::task::manager::{instructions_digest, pending_approvals, ApprovalLine};
use wsx_core::task::manager::{
    Author, ManagerEvent, ManagerRequest, ManagerResponse, ManagerState, Seen,
};
use wsx_core::task::store;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{self, clip, project_tasks, read_result};
use wsx_core::task::{checks, ledger, TaskRecord, TaskState, WaitOn, MAX_MESSAGE_BYTES};

use wsx_core::task::owner::ACTOR_ENV;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
/// Events within this long of each other, or during a turn, become one message.
const EVENT_QUIET: Duration = Duration::from_secs(3);
const EVENT_TITLE_CHARS: usize = 80;
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
/// The ledger summary at the end of a turn's message: at most this many task lines.
const SUMMARY_ROWS: usize = 12;
const SUMMARY_TITLE_CHARS: usize = 60;

/// Task states the manager hears about: not a task the owner works on by hand.
fn is_event(record: &TaskRecord) -> bool {
    match record.state {
        TaskState::Waiting => !record.waits_on(WaitOn::Hand),
        TaskState::Ready | TaskState::Failed | TaskState::Landed => true,
        _ => false,
    }
}

pub(super) enum Event {
    Request(ManagerRequest, Sender<ManagerResponse>),
    Agent(u64, Option<AgentLine>),
    Tick,
    /// The agent's `say --from-manager` reached another project's manager.
    Sent {
        to: String,
        text: String,
        reply: bool,
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
    /// `tasks.manager.tools`, added to the read-only ones.
    tools: Vec<String>,
    state: ManagerState,
    events: Sender<Event>,
    agent: Option<ClaudeAgent>,
    generation: u64,
    /// Logged messages not yet delivered, oldest first.
    queue: VecDeque<(u64, Author, String)>,
    /// The ids of the messages in the running turn.
    turn: Option<Vec<u64>>,
    pane: Option<u64>,
    started: Instant,
    /// Task states as last seen, including ones not yet reported.
    observed: BTreeMap<u32, Seen>,
    /// Approval decisions seen, including ones not yet reported.
    observed_decisions: u64,
    /// Event lines not yet logged as a message, and when the last one came.
    pending_events: Vec<String>,
    last_event: Option<Instant>,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<hosting::Binary>,
    reexec: bool,
    owed: Owed,
}

/// Tools the manager has beyond `Read Glob Grep`, so it loads context as a worker does and
/// beyond: the owner's skills and research subagents. ref: docs/tasks.md "Manager"
const MANAGER_TOOLS: [&str; 4] = ["WebFetch", "WebSearch", "Skill", "Agent"];

/// Denied by name: its writes go through sandboxed Bash alone, also in a subagent or a skill
/// whose `allowed-tools` names them. ref: docs/tasks.md "Permissions"
const MANAGER_DENIED_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

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
        "You are the manager of the wsx project {project}. The owner talks to you as to a \
         colleague. You never edit the project's files and never run builds or tests; \
         workers do that in their own tasks. You may keep notes in {notes}, the only folder \
         you can write besides the project page. \
         Messages headed 'Owner' come from the owner; messages headed 'Events' are written \
         by wsx. Each message ends with a ledger summary from wsx: the open tasks by state \
         and what changed since your last turn. The ledger is the source of truth, not your \
         memory: run `wsx task ledger -p {project}` when you need more, such as a task's \
         checks, asks, overlaps or memo, or the deploys. Every `wsx task` command takes \
         `-p {project} --json`. When a command says the owner was asked to approve, tell \
         the owner and do not retry it. Keep turns short. Name tasks as #<n>. \
         Present every reply to the owner by this rule. {presentation} \
         Talk before work. When the owner asks for work, first talk it through in the chat: \
         the goal in one line, its size (how many tasks; which can run at once because they \
         change different files, and which must wait for another to land), the choices that \
         change the work with your recommendation for each, and what you would leave out. \
         Then ask whether to go. Start tasks only after the owner agrees, or at once when \
         the owner's message says to just do it. A plan longer than a few lines goes on the \
         project page as its own section with a mermaid diagram; the reply says so in a few \
         lines and links it. \
         Start a task with `wsx task new --title <title> \"<prompt>\"`: the title is a few \
         plain words of at most 60 characters, never starting with 'Owner'. The prompt is \
         complete on its own, since a worker cannot ask: the goal, the owner's decisions, \
         what is in and out of scope, the files you expect it to change and how it proves \
         the result. Run tasks at once only when they change different files; the ledger's \
         `overlaps` names open tasks that changed the same paths. Give overlapping work to \
         one task, or start the second after the first lands. Pick the worker's model with \
         `--model`: `sonnet` may do when the prompt is already thorough; keep the default \
         (opus) for a critical fix or heavy, complex work. To change a running task use \
         `wsx task add` or `wsx task steer`. \
         The ledger's `checks` (and 'checks n of m passed' in an event) are what wsx ran on \
         the task's latest revision: report each check as passed or failed from them, and \
         trust them over the worker's words about what it could not run or verify. \
         A task the owner created lands by itself when its checks pass and the ledger's \
         `auto_land` is true. A ready task you created waits for your review: read its diff \
         (`git -C <working copy> diff <base>..HEAD`; `wsx task show <n> --json` names both) \
         against the owner's words and for lean code: no copy of a helper that exists, no \
         dead code, nothing the request did not ask for. Then `wsx task land <n>`, or \
         `wsx task add <n> \"<what to change>\"`. Never send a task back to rebase: a land \
         rebases onto the newest {project} main and runs the checks again by itself. \
         {review}\
         When the last task of a request lands and no open task changes the same files, \
         start one housekeeping task over the files those tasks changed, with this prompt \
         and the task numbers: {batch} \
         A land needs no approval, and after it the project deploys by itself when the \
         ledger's `deploys` say so; call a change live only when `deploys` shows its commit. \
         Never ask the owner to verify a task. What only a person can check goes to the \
         project's To check list (`wsx task check`, the ledger's `to_check`), which the \
         owner works through at their own time; never pass or fail items yourself. When a \
         worker leaves for a person what a command, a test or a capture could prove, tell \
         that worker to prove it. A release refuses while items are open: tell the owner \
         how many are open. \
         A task waits when the ledger shows `waiting`: on `permission`, the sandbox stopped \
         a host or folder the task asks for; when the task plainly needs it, grant it with \
         `wsx task allow <n> --host <host>` or `--path <path>`, which resumes it, else add \
         to the worker. A grant for every task (`--project-wide`) or a tool rule asks the \
         owner's approval; never tell the owner to edit settings files. On `owner`, the \
         worker asked or stopped: answer with `wsx task add` when you can, else put the \
         question to the owner in plain words. On `disk`, it resumes by itself once there \
         is space. On `hand`, the owner works in its copy: never add, steer, land or close \
         it. Only the owner runs `wsx task hand <n>` and `wsx task handback <n>`; you may \
         name them when work needs the owner's own hands. \
         Your Bash runs in a sandbox that writes only your notes folder and the project \
         page; `wsx task` commands reach the tasks from there, also in a pipe or with a \
         heredoc, and no other `wsx` command runs. Load context as a worker would, without \
         editing or building: Read, Glob and Grep the project and the tasks' working \
         copies, WebFetch and WebSearch, and read-only git in Bash (`git log`, `git show`, \
         `git diff`, `git status`, also `git -C <working copy>`); never a git command that \
         writes. {skills}At the start of a turn, look at the skills and subagents you have. \
         Before you answer about a prior decision, a term you do not know, the code or a \
         research question, use any skill or subagent whose description fits lookup, \
         context, memory or research; check rather than guess. When none fits, use Read, \
         Grep, WebSearch and the Explore subagent; use the docs tools the project settings \
         allow. Think about the owner's goal and the likely next step before you answer, \
         and say what you checked in one short line. Subagents and skills keep your \
         boundary: Edit, Write and NotebookEdit are denied to them as to you, and their \
         Bash writes only where yours does. When the owner asks for status, read the \
         ledger and answer in a few plain lines: what landed and is live, what runs, what \
         waits for the owner and why; never give the owner commands to run. \
         When the owner decides something (a rule, a preference, a choice), append one line \
         `- <YYYY-MM-DD> <decision>` to {decisions} at once. Every new conversation of yours \
         starts with that file and {context}. \
         Other registered projects have managers too. For another project you may read \
         `wsx task ledger -p <project>` and message its manager with `wsx task say -p \
         <project> --from-manager \"<text>\"`; you never create or change its tasks. A \
         message headed 'Manager <project>' asks you something: answer it once with \
         `wsx task say -p <project> --from-manager --reply \"<answer>\"`. A message headed \
         'Manager <project> replies' answers yours: use it and do not answer it. \
         The project page {page} is the owner's one-page picture of the project: the goal, \
         the tasks and how they relate as a mermaid graph, the plans, the decisions and the \
         open questions. When one of them changes, update that section in place with Bash, \
         such as a short python3 script that reads the page, replaces that section and \
         writes the same file back (only that file is writable: a temporary file beside it \
         fails); never rewrite the whole file, and never create another page. Workers do \
         not write it. When a reply points to it, call it the project page. \
         {context} is your lean project context; read it when a conversation starts. \
         Workers leave a `memo` of durable project knowledge on the ledger's recent tasks. \
         When a message opens with 'Handover' (a new conversation after a rotation), \
         first fold those memos into {context} with Bash: keep it lean, merge duplicates, \
         drop what is stale, at most 60 lines.",
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

pub fn run(project_id: &str) -> Result<()> {
    let (project, main) = super::project_by_id(project_id)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let dir = ManagerDir::open(project_id)?;
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
        tools: settings.manager_tools,
        state,
        events: events.clone(),
        agent: None,
        generation: 0,
        queue: VecDeque::new(),
        turn: None,
        pane: hosting::own_pane(),
        started: Instant::now(),
        observed: BTreeMap::new(),
        observed_decisions: 0,
        pending_events: Vec::new(),
        last_event: None,
        binary: hosting::Binary::capture(),
        reexec: false,
        owed: Owed::default(),
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
    let (own, owed, sent) = (
        manager.project.clone(),
        manager.owed.clone(),
        events.clone(),
    );
    std::thread::spawn(move || {
        broker::serve_manager(brokered, exe, folder, own, owed, move |to, text, reply| {
            let _ = sent.send(Event::Sent { to, text, reply });
        })
    });
    std::thread::spawn(move || {
        while events.send(Event::Tick).is_ok() {
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    manager.say(&format!(
        "manager of {} in {}",
        manager.project,
        manager.main.display()
    ));
    manager.report(runtime::AgentState::Idle);
    manager.run(receiver)?;
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
                Event::Tick => self.watch_tasks(),
                Event::Sent { to, text, reply } => self.log(ManagerEvent::Sent { to, text, reply }),
            }
            if self.turn.is_none() {
                self.flush_events();
                self.next_turn();
            }
            if self.reexec {
                std::thread::sleep(Duration::from_millis(200));
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
            }
            None => {
                // ^ First start: what already happened is the baseline, not news.
                self.observed = self.current_tasks().into_iter().collect();
                self.observed_decisions = self.approval_lines().len() as u64;
                self.state.seen = Some(self.observed.clone());
                self.state.decisions_seen = self.observed_decisions;
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

    /// Turn new approval decisions into event lines.
    fn watch_approvals(&mut self) {
        let lines = self.approval_lines();
        let decisions: Vec<&ApprovalLine> = lines
            .iter()
            .filter(|line| matches!(line, ApprovalLine::Decision { .. }))
            .collect();
        for decision in decisions.iter().skip(self.observed_decisions as usize) {
            let ApprovalLine::Decision {
                id,
                approved,
                note,
                outcome,
                ..
            } = decision
            else {
                continue;
            };
            let asked = lines.iter().find_map(|line| match line {
                ApprovalLine::Request {
                    id: requested,
                    action,
                    task,
                    ..
                } if requested == id => Some(format!(
                    "{action:?}{}",
                    task.map_or(String::new(), |task| format!(" #{task}"))
                )),
                _ => None,
            });
            let verdict = if *approved {
                format!("approved; {outcome}")
            } else {
                format!("denied; {}", note.as_deref().unwrap_or("no note"))
            };
            self.pending_events.push(format!(
                "approval {id} ({}) {}",
                asked.unwrap_or_default().to_ascii_lowercase(),
                clip(&verdict, EVENT_SUMMARY_CHARS * 2)
            ));
            self.last_event = Some(Instant::now());
        }
        self.observed_decisions = decisions.len() as u64;
    }

    /// Turn task state changes into event lines.
    fn watch_tasks(&mut self) {
        self.watch_approvals();
        let Ok(tasks) = project_tasks(&self.dir.project_id) else {
            return;
        };
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
            self.pending_events.push(format!(
                "#{} {} r{}: {}; {}",
                record.id,
                record.state.as_str(),
                record.revision,
                clip(&title, EVENT_TITLE_CHARS),
                summary,
            ));
            self.last_event = Some(Instant::now());
        }
    }

    /// Log the waiting event lines as one message once they have been quiet for a while.
    fn flush_events(&mut self) {
        let quiet = self
            .last_event
            .is_some_and(|last| last.elapsed() >= EVENT_QUIET);
        if self.pending_events.is_empty() || !quiet {
            return;
        }
        let text = self.pending_events.join("\n");
        match self.accept(Author::Events, text) {
            Ok(_) => {
                self.pending_events.clear();
                self.state.seen = Some(self.observed.clone());
                self.state.decisions_seen = self.observed_decisions;
                self.save();
            }
            Err(error) => self.say(&format!("cannot log the events: {error:#}")),
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
        self.enqueue(id, from, text);
        Ok(id)
    }

    /// Owner messages go first, then other managers', then events.
    fn enqueue(&mut self, id: u64, from: Author, text: String) {
        let rank = |author: &Author| match author {
            Author::Owner => 0,
            Author::Manager { .. } => 1,
            Author::Events => 2,
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
        let Some((_, first, _)) = self.queue.front() else {
            return;
        };
        // ^ A replaced binary: exec it before this turn. The messages stay logged and not
        // delivered, so the new host queues them again (`restore`).
        if self.binary.as_ref().is_some_and(hosting::Binary::replaced) {
            self.say("the wsx binary was replaced: the manager restarts with it");
            self.reexec = true;
            return;
        }
        let first = first.clone();
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
            };
            text.push_str(&format!("{heading} (message {id}):\n{body}\n\n"));
            ids.push(id);
        }
        if let Err(error) = self.send_turn(text.trim_end(), first == Author::Events) {
            self.say(&format!("cannot start the manager agent: {error:#}"));
            self.log(ManagerEvent::Stopped {
                reason: format!("cannot start the manager agent: {error:#}"),
            });
            return;
        }
        self.log(ManagerEvent::Delivered { ids: ids.clone() });
        self.turn = Some(ids);
        self.report(runtime::AgentState::Working);
    }

    /// The model of a turn: `eventModel` for one with only events while no ready task the
    /// manager created waits for its review, else the manager's. ref: docs/tasks.md "Manager"
    fn turn_model(&self, events_only: bool) -> String {
        let review_due = || {
            project_tasks(&self.dir.project_id)
                .unwrap_or_default()
                .into_iter()
                .any(|(_, record)| record.is_ok_and(|record| record.manager_reviews()))
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
        Some(match deploy["commit"].as_str() {
            Some(commit) => format!(
                "deploy: {} live, {} commit(s) behind{failed}",
                &commit[..commit.len().min(7)],
                deploy["behind"]
                    .as_u64()
                    .map_or("?".into(), |n| n.to_string())
            ),
            None => format!("deploy: none yet{failed}"),
        })
    }

    fn send_turn(&mut self, text: &str, events_only: bool) -> Result<()> {
        let mut text = text.to_string();
        let model = self.turn_model(events_only);
        if self.agent.is_none() {
            let launch = self.launch()?;
            let digest = instructions_digest(&launch.instructions);
            // ^ Claude keeps the system prompt a conversation began with, also on resume:
            // new instructions need a new conversation (docs/tasks.md "Manager").
            if self.state.agent_session.is_some()
                && self.state.instructions.as_deref() != Some(&digest)
            {
                self.rotate(0, Some("the manager's instructions changed".into()));
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

    /// The manager's Bash writes only its notes folder; its `wsx task` commands leave the
    /// sandbox. ref: docs/tasks.md "Permissions"
    fn launch(&self) -> Result<Launch> {
        let exe = std::env::current_exe().context("cannot find the wsx binary")?;
        let notes = self.dir.notes()?;
        // ^ The folder exists: the host bound the socket before any launch.
        let broker_socket = hosting::real_socket_path(&self.dir.broker_path());
        let page = store::ensure_page(&self.dir.project_id, &self.project)?;
        // ^ Claude resolves symlinks such as /tmp → /private/tmp before it matches a rule.
        let page = page.canonicalize().unwrap_or(page);
        let boundary = Boundary {
            cwd: self.main.clone(),
            // ^ The page is one file: Bash may rewrite it in place, never beside it.
            writes: vec![notes.display().to_string(), page.display().to_string()],
            deny_writes: vec![self.main.display().to_string()],
            domains: Vec::new(),
            unix_sockets: vec![broker_socket.display().to_string()],
            deny_edits: kept_from_edits(&self.main, None),
            deny_tools: MANAGER_DENIED_TOOLS.map(String::from).to_vec(),
        };
        let skills = skills_plugin(&self.dir.path)?;
        let workflow = load_task_config(&self.main)
            .map(|settings| settings.workflow)
            .unwrap_or_default();
        let mut launch = Launch::new(
            &boundary,
            &self.model,
            MANAGER_TOOLS
                .map(String::from)
                .into_iter()
                .chain(self.tools.iter().cloned())
                .collect(),
            instructions(&self.project, &notes, &page, skills.is_some(), &workflow),
        );
        launch.plugin_dirs = skills.into_iter().collect();
        // ^ The agent's `wsx task` commands go to this host (broker.rs), which runs them as
        // the manager actor outside the sandbox; the actor mark stays for the approval rules.
        launch.env = vec![
            (broker::BROKER_ENV.into(), broker_socket.into_os_string()),
            (ACTOR_ENV.into(), "manager".into()),
            // ^ Its git reads must not try to refresh the index of a checkout it cannot write.
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ];
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
            // ^ The manager's commands ask nothing; its asks are the tasks'.
            Some(AgentLine::Stopped(_) | AgentLine::Reached(_)) => {}
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
        let to = self.turn.take().unwrap_or_default();
        let id = self.next_id();
        // ^ The last model call's context: the summed usage counts it once per call.
        let context = result.usage.context_tokens;
        self.state.session_cost_micro_usd = result.usage.session_cost_micro_usd;
        self.save();
        self.say(&format!(
            "reply {id}: {}",
            result.text.lines().next().unwrap_or_default()
        ));
        self.log(ManagerEvent::Reply {
            id,
            to,
            text: result.text,
            usage: Some(result.usage),
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
        // ^ Always sent after a rotation: it is the manager's cue to fold the memos.
        format!(
            "Handover (this is a new conversation; fold the ledger's memos into your context \
             first; the last exchanges before it, read the ledger for the state):\n{}\n\n",
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
