//! `wsx task manager-host`: one manager agent per project in a hidden session in the
//! main checkout. It edits no project file; it answers from the ledger, acts through
//! `wsx task` commands and keeps notes in its own folder. ref: docs/tasks.md "Manager"

use super::claude::{
    kept_from_edits, new_session_id, AgentLine, Boundary, ClaudeAgent, Launch, TurnResult,
};
use super::{broker, hosting};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::runtime;
use wsx_core::task::manager::{instructions_digest, ApprovalLine};
use wsx_core::task::manager::{
    Author, ManagerEvent, ManagerRequest, ManagerResponse, ManagerState, Seen,
};
use wsx_core::task::store;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::{self, clip, project_tasks, read_result};
use wsx_core::task::{brief, TaskState, MAX_MESSAGE_BYTES};

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

/// Task states the manager hears about.
fn is_event_state(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Ready
            | TaskState::Briefed
            | TaskState::Blocked
            | TaskState::Failed
            | TaskState::Conflict
            | TaskState::InReview
            | TaskState::Landed
    )
}

pub(super) enum Event {
    Request(ManagerRequest, Sender<ManagerResponse>),
    Agent(u64, Option<AgentLine>),
    Tick,
}

struct Manager {
    dir: ManagerDir,
    project: String,
    main: PathBuf,
    model: String,
    rotate_tokens: u64,
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
}

/// ref: docs/tasks.md "Manager", "Brief"
fn instructions(project: &str, notes: &Path, page: &Path) -> String {
    format!(
        "You are the manager of the wsx project {project}. You never edit the project's \
         files and never run builds or tests; workers do that in their own tasks. You may \
         keep notes in {notes}, the only folder you can write. Start every turn with \
         `wsx task ledger -p {project}`: it is the source of truth, not your memory. \
         Messages headed 'Owner' come from the owner; messages headed 'Events' are written \
         by wsx. For each owner message choose one: answer it, `wsx task add`, \
         `wsx task steer`, `wsx task new`, `wsx task verify` or `wsx task land`, always with \
         `-p {project} --json`. When a command says the owner was asked to approve, tell the \
         owner and do not retry it. Keep turns short. Name tasks as #<n>. \
         Work the owner asks for starts with `wsx task new --title <title>`: the title is \
         a few plain words of at most 60 characters that name the work, never starting \
         with 'Owner'. Its worker first writes a brief and the task waits as briefed. Tell the owner the brief is coming. Add \
         `--no-brief` only when the owner's message asks to skip the brief, such as \
         'just do it' or 'no brief'. Never start a build on your own: when the owner says \
         'start #<n>', run `wsx task start <n>`, which asks the owner's approval. A reply \
         about a brief goes to its task with `wsx task add`. In status answers, name the \
         briefs that wait for the owner. \
         The ledger's `checks` (and 'checks n of m passed' in an event) are what wsx \
         ran on the task's latest revision. When you report a task, give each check as \
         passed or failed from them, and trust them over the worker's own words about \
         what it could not run or verify; quote the worker only for what no check covers. \
         Your Bash runs in a sandbox that writes only your notes folder; `wsx task` \
         commands reach the tasks from there, also in a pipe or with a heredoc, and no other \
         `wsx` command runs. Workers run in a sandbox too. When the ledger shows a blocked \
         task with asks, fix it yourself: when the task plainly needs that host or folder, \
         grant it with `wsx task allow <n> --host <host>` or `--path <path>`, which resumes \
         it; otherwise steer or add to the worker. Never tell the owner to edit settings \
         files. A grant for every task (`--project-wide`) or a tool rule asks the owner's \
         approval; tell the owner what you asked for. \
         The project page {page} is the owner's one-page picture of the project: the goal, \
         the tasks and how they relate as a mermaid graph, the decisions and the open \
         questions. When one of them changes, update that section in place with the Edit \
         tool; never rewrite the whole file, and never create another page. Workers do not \
         write it. When a reply points to it, call it the project page.",
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
        rotate_tokens: settings.manager_rotate_tokens,
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
    std::thread::spawn(move || broker::serve(brokered, exe, folder));
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
            ManagerRequest::Say { text } => match self.accept(Author::Owner, text) {
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
            },
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
            .map(|record| {
                (
                    record.id,
                    Seen {
                        state: record.state,
                        revision: record.revision,
                    },
                )
            })
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
            let now = Seen {
                state: record.state,
                revision: record.revision,
            };
            if self.observed.get(&record.id) == Some(&now) {
                continue;
            }
            self.observed.insert(record.id, now);
            if !is_event_state(record.state) {
                continue;
            }
            let title = view::title(&dir, &record).unwrap_or_default();
            let summary = if record.state == TaskState::Briefed {
                brief::read(&dir)
                    .map(|file| format!("brief v{}: {}", file.version, file.brief.rewritten_prompt))
                    .unwrap_or_default()
            } else {
                // ^ The checks the host ran come before the worker's own words.
                let result = read_result(&dir).unwrap_or_default();
                let checks = view::check_outcomes(&result);
                let passed = checks
                    .iter()
                    .filter(|(_, outcome)| *outcome == "passed")
                    .count();
                let words = result["result"]["summary"].as_str().unwrap_or_default();
                let worker = format!("worker says: {}", clip(words, EVENT_SUMMARY_CHARS));
                match checks.len() {
                    0 => worker,
                    total => format!("checks {passed} of {total} passed; {worker}"),
                }
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
            from,
            text: text.clone(),
        })?;
        self.enqueue(id, from, text);
        Ok(id)
    }

    fn enqueue(&mut self, id: u64, from: Author, text: String) {
        let at = match from {
            Author::Owner => self
                .queue
                .iter()
                .position(|(_, author, _)| *author == Author::Events)
                .unwrap_or(self.queue.len()),
            Author::Events => self.queue.len(),
        };
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
        let first = *first;
        let mut ids = Vec::new();
        let mut text = String::new();
        while self
            .queue
            .front()
            .is_some_and(|(_, author, _)| *author == first)
        {
            let (id, author, body) = self.queue.pop_front().expect("checked above");
            let heading = match author {
                Author::Owner => "Owner",
                Author::Events => "Events",
            };
            text.push_str(&format!("{heading} (message {id}):\n{body}\n\n"));
            ids.push(id);
        }
        if let Err(error) = self.send_turn(text.trim_end()) {
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

    fn send_turn(&mut self, text: &str) -> Result<()> {
        let mut text = text.to_string();
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
            if !resume && self.state.handover {
                text = self.handover() + &text;
                self.state.handover = false;
            }
            self.state.instructions = Some(digest);
            self.save();
            self.generation += 1;
            let generation = self.generation;
            self.agent = Some(ClaudeAgent::spawn(
                &launch,
                &session,
                resume,
                self.events.clone(),
                move |line| Event::Agent(generation, line),
            )?);
        }
        self.agent
            .as_mut()
            .context("the manager agent is not running")?
            .send(&text)
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
            writes: vec![notes.display().to_string()],
            deny_writes: vec![self.main.display().to_string()],
            domains: Vec::new(),
            unix_sockets: vec![broker_socket.display().to_string()],
            deny_edits: kept_from_edits(&self.main, None),
        };
        let mut launch = Launch::new(
            &boundary,
            &self.model,
            // ^ `//` starts an absolute path in a rule; one file, never a folder.
            vec![
                format!("Edit(/{})", page.display()),
                format!("Write(/{})", page.display()),
            ],
            instructions(&self.project, &notes, &page),
        );
        // ^ The agent's `wsx task` commands go to this host (broker.rs), which runs them as
        // the manager actor outside the sandbox; the actor mark stays for the approval rules.
        launch.env = vec![
            (broker::BROKER_ENV.into(), broker_socket.into_os_string()),
            (ACTOR_ENV.into(), "manager".into()),
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
        let context = result.usage.input_tokens
            + result.usage.cache_read_tokens
            + result.usage.cache_creation_tokens;
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
        self.state.handover = true;
        self.save();
        self.log(ManagerEvent::Rotated { tokens, reason });
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
        if lines.is_empty() {
            return String::new();
        }
        lines.reverse();
        format!(
            "Handover (this is a new conversation; the last exchanges before it, read the \
             ledger for the state):\n{}\n\n",
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
