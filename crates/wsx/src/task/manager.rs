//! `wsx task chat-host <chat>`: one chat's host in a hidden session in the chat's repo's main
//! checkout. It edits no project file but a commit of the owner's own changes on the owner's
//! yes; it answers from the ledger, acts through `wsx task` commands and keeps notes in the
//! chat's folder. It only talks: the repo's service (`repo.rs`) hands it the events of its
//! tasks, and it leaves `CHAT_LINGER` after its queue is empty, started again by the next
//! message. The hidden `manager-host <repo> [--chat <name>]` of the release before runs as this
//! host and starts the service. ref: docs/tasks.md "Manager", "Chats"

use super::agent::{
    self, new_session_id, Agent, AgentLine, Ask, Cause, Launch, Next, TurnResult, PROMPT_CACHE_MS,
};
use super::boundary::Boundary;
use super::claude::{account_env, config_root, skill_names, skills_plugin, SKILLS_PLUGIN_NAME};
use super::instructions::Skills;
use super::{broker, claude, floor, hosting, limit, notes};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::manager_rotate_limit;
use wsx_core::routine::RunRecord;
use wsx_core::runtime;
use wsx_core::task::chat;
use wsx_core::task::checks::{CheckState, ItemReply};
use wsx_core::task::manager::{
    answering, asks_owner, instructions_digest, pending_approvals, quiet_reply, quoted_id,
    resolve_quote, says_nothing, split_quote, Answers, ApprovalLine, QUOTE_REF,
};
use wsx_core::task::manager::{
    Author, Level, LimitHold, ManagerAgent, ManagerEntry, ManagerEvent, ManagerRequest,
    ManagerResponse, ManagerState, Party, Seen, Short,
};
use wsx_core::task::policy;
use wsx_core::task::replace::Binary;
use wsx_core::task::stall;
use wsx_core::task::status;
use wsx_core::task::store;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::thread;
use wsx_core::task::view::{self, clip, project_tasks};
use wsx_core::task::{chats, owner, Actor};
use wsx_core::task::{checks, ledger, now_unix_ms, TaskRecord, TaskState, MAX_MESSAGE_BYTES};

use wsx_core::task::owner::ACTOR_ENV;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
const START_RETRY: Duration = Duration::from_secs(30);
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
/// A chat's host leaves after this long with no message and no turn; `say` starts it again.
const CHAT_LINGER: Duration = Duration::from_secs(20);
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
/// The ledger summary at the end of a turn's message: at most this many task lines.
const SUMMARY_ROWS: usize = 12;
const SUMMARY_TITLE_CHARS: usize = 60;

pub(super) enum Event {
    Request(ManagerRequest, Sender<ManagerResponse>),
    Agent(u64, Option<AgentLine>),
    Tick,
    /// The agent's `say --from-chat` reached a chat of this project or another project's
    /// first chat (`to` is the `Party` key).
    Sent {
        to: String,
        text: String,
        reply: bool,
    },
}

/// Answers this manager owes other projects' managers: one per message from one of them
/// that a turn delivered. A `say --from-chat --reply` spends one, so a reply is never
/// answered and a message is answered once. ref: docs/tasks.md "Managers talk"
type Owed = std::sync::Arc<std::sync::Mutex<BTreeMap<String, u32>>>;

/// `url.<base>.pushInsteadOf` with an empty prefix rewrites every push URL to `<base><url>`:
/// `git push` then answers "'remote-wsx-task-push' is not a git command".
const NO_PUSH_KEY: &str = "url.wsx-task-push::.pushInsteadOf";

struct Manager {
    dir: ManagerDir,
    project: String,
    main: PathBuf,
    /// `tasks.manager.adapter`: the headless adapter the manager runs on.
    adapter: String,
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
    queue: VecDeque<(u64, Author, String, bool)>,
    /// The ids of the messages in the running turn.
    turn: Option<Vec<u64>>,
    /// The running turn holds only events: its reply is quiet.
    turn_events: bool,
    pane: Option<u64>,
    started: Instant,
    /// When a message was logged, a turn ran or the host last looked for a recap.
    last_activity: Instant,
    /// The running turn's record (`stall::Turn`, `turn.json`): past its deadline with no line
    /// from the agent, the turn ends (`end_hung_turn`).
    turn_record: Option<stall::Turn>,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<Binary>,
    reexec: bool,
    owed: Owed,
    /// The policy of the agent that runs now, which the broker checks commands against.
    policy: broker::SharedPolicy,
    /// When the agent last failed to start a turn; the next try waits `START_RETRY`.
    start_failed: Option<Instant>,
    /// The approval lines there were after the last reply: a request after them is new.
    asks_seen: usize,
    /// A chat host that is closed or idle: it leaves.
    leaving: bool,
    /// Like `overview_asked`, for the request to write an answer that sat in thinking.
    thought_asked: bool,
    /// The home chat asked for a shorter overview since the last owner message or in-cap result.
    overview_asked: bool,
    /// A hand edit of `decisions.md` or `context.md` was said once; no second notice until the
    /// owner writes again or the views are whole.
    drift_said: bool,
}

/// The manager's lean project context in its notes folder. ref: docs/tasks.md "Memo"
pub(super) const CONTEXT_FILE: &str = "context.md";
/// The owner's decisions, one line each, appended by the manager as they are made.
pub(super) const DECISIONS_FILE: &str = "decisions.md";
/// A conversation idle past `PROMPT_CACHE_MS` lost Claude's prompt cache: past this context, a
/// fresh conversation costs less than the cold resume.
const IDLE_ROTATE_TOKENS: u64 = 60_000;
/// Changed instructions go to a warm conversation under this context as a message.
const INSTRUCTION_MESSAGE_TOKENS: u64 = 100_000;
// ^ The agent writes the notes: a link or a huge file must not reach the host's read.
const MAX_NOTES_READ: u64 = 256 * 1024;

/// A chat's instructions open with its name and goal, then the manager's.
/// ref: docs/tasks.md "Chats"
fn chat_preamble(chat: &str, project: &str, goal: &str) -> String {
    format!("You are {chat}, a chat of the project {project}. The owner's goal:\n{goal}\n\n")
}

/// The manager's boundary profile as text. ref: scripts/test-manager-boundary.py
pub fn print_profile(project: &str, main: &Path, untrusted: bool) -> Result<()> {
    let project_id = store::project_id(project, main);
    let dir = ManagerDir::open_home(&project_id)?;
    let overview = store::ensure_overview(&project_id, project)?;
    let (trust, adapter) = match untrusted {
        true => (policy::Trust::Untrusted, "pi"),
        false => (policy::Trust::Trusted, "claude"),
    };
    let policy = policy::Policy::of(trust, policy::Role::Manager, None);
    print!(
        "{}",
        Boundary::manager(main, &dir.path, &overview, policy)?
            .adapter(adapter, None)
            .broker(&dir.broker_path())
            .profile()
    );
    Ok(())
}

/// The hidden `manager-host <repo> [--chat <name>]` of the release before: the chat's host runs
/// as `chat-host`, in this session, and the repo's service starts in its own. A session that
/// wsxd restores from its stored argv, or a host that follows a replaced wsx, lands here.
pub fn run_old(project_id: &str, chat: Option<&str>) -> Result<()> {
    let (project, main) = super::project_by_id(project_id)?;
    let dir = match chat {
        None => ManagerDir::open_home(project_id)?,
        Some(chat) => chats::open_chat(chat)?,
    };
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    if let Err(error) = owner::ensure_repo(&project, &main, &exe, super::launch_manager(&main)) {
        eprintln!("cannot start the repo's service: {error:#}");
    }
    run(dir.name())
}

pub fn run(chat: &str) -> Result<()> {
    let dir = chats::open_chat(chat)?;
    let project_id = dir.project_id.clone();
    let (project, main) = super::project_by_id(&project_id)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let listener = hosting::bind(&dir.socket_path())?;
    let brokered = hosting::bind(&dir.broker_path())?;
    let state = dir.read_state()?;
    // ^ A chat with its own agent runs it; the event model names one of the manager's models.
    let (adapter, model, event_model) = match chats::agent(&dir) {
        Some(own) => (own.adapter, own.model, None),
        None => (
            settings.manager_adapter,
            settings.manager_model,
            settings.manager_event_model,
        ),
    };
    let (events, receiver) = channel();
    let mut manager = Manager {
        dir,
        project,
        main,
        adapter,
        stand_in: None,
        stand_in_look: None,
        model,
        effort: settings.manager_effort,
        event_model,
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
        last_activity: Instant::now(),
        turn_record: None,
        binary: Binary::capture(),
        reexec: false,
        owed: Owed::default(),
        policy: broker::SharedPolicy::default(),
        start_failed: None,
        asks_seen: store::read_approvals(&project_id).map_or(0, |lines| lines.len()),
        leaving: false,
        thought_asked: false,
        overview_asked: false,
        drift_said: false,
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
    let (own, project_id, owed, sent, actor) = (
        manager.project.clone(),
        manager.dir.project_id.clone(),
        manager.owed.clone(),
        events.clone(),
        manager.actor(),
    );
    let policy = manager.policy.clone();
    std::thread::spawn(move || {
        broker::serve_manager(
            brokered,
            broker::ManagerBroker {
                exe,
                fallback_cwd: folder,
                own,
                project_id,
                actor: actor.into(),
                owed,
                policy,
            },
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

/// The messages of one manager turn, taken off the queue by `take_batch`.
struct Batch {
    /// The author the turn answers.
    first: Author,
    ids: Vec<u64>,
    text: String,
    /// What left the queue, to put back when the agent does not start.
    taken: Vec<(u64, Author, String, bool)>,
    /// A routine event for the owner is among them.
    for_owner: bool,
}

/// The heading of a message in the turn's text.
fn author_heading(author: &Author) -> String {
    match author {
        Author::Owner => "Owner".to_string(),
        Author::Events => "Events".to_string(),
        Author::Recap => "Recap request from wsx".to_string(),
        // ^ A chat's, this project's manager's or another project's message is answered once
        // (docs/tasks.md "Chats", "Managers talk").
        Author::Peer { from, reply } => {
            let who = match from {
                Party::Chat(name) => format!("Chat {name}"),
            };
            match reply {
                true => format!("{who} replies"),
                false => who,
            }
        }
    }
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
                Event::Tick
                    if self
                        .stand_in
                        .as_ref()
                        .is_some_and(|(_, began)| began.elapsed() >= STAND_IN_TIMEOUT) =>
                {
                    self.stand_in_done(None)
                }
                Event::Tick => {
                    self.end_hung_turn();
                    self.leave_when_done()
                }
                Event::Sent { to, text, reply } => self.log(ManagerEvent::Sent { to, text, reply }),
            }
            if self.turn.is_none() {
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
                binary: self.binary.clone(),
                message: format!("{} message(s) waiting", self.queue.len()),
            },
            ManagerRequest::Say {
                text,
                from,
                short,
                thread,
            } => match self.accept(from.unwrap_or(Author::Owner), text, short, thread) {
                Ok(id) => ManagerResponse {
                    ok: true,
                    id: Some(id),
                    busy,
                    binary: None,
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
                    binary: None,
                    message: format!("{error:#}"),
                },
            },
            ManagerRequest::Post { event, text, level } => {
                let refusal = match (busy, text.trim().is_empty(), text.len() > MAX_MESSAGE_BYTES) {
                    (true, ..) => Some("a manager turn runs"),
                    (_, true, _) => Some("the text is empty"),
                    (.., true) => Some("the text is larger than the limit"),
                    _ => None,
                };
                if refusal.is_none() {
                    self.post(event, text, level, None);
                }
                ManagerResponse {
                    ok: refusal.is_none(),
                    id: None,
                    busy,
                    binary: None,
                    message: refusal.unwrap_or("posted to the chat").into(),
                }
            }
            // ^ A check-in belongs to the chat that set it and the repo's service fires it.
            ManagerRequest::Checkin { .. } => ManagerResponse {
                ok: false,
                id: None,
                busy,
                binary: None,
                message: "a check-in goes to the repo's service".into(),
            },
        }
    }

    /// Write to the owner with no turn: a delivered events message, so that no restart hands it
    /// to the agent, and the reply to it, which the web pushes like the manager's own. The reply
    /// has no usage and is marked `post`, which the chats read.
    fn post(
        &mut self,
        event: String,
        text: String,
        level: Option<Level>,
        approval: Option<String>,
    ) {
        let id = self.next_id();
        self.log(ManagerEvent::Message {
            id,
            from: Author::Events,
            text: event,
            short: Vec::new(),
            quote: None,
            check: None,
            thread: None,
        });
        self.log(ManagerEvent::Delivered { ids: vec![id] });
        let reply = self.next_id();
        self.log(ManagerEvent::Reply {
            id: reply,
            to: vec![id],
            threads: Vec::new(),
            text,
            usage: None,
            quiet: false,
            asks: false,
            provisional: None,
            level,
            post: true,
            quote: None,
            approval,
        });
    }

    /// The first failed turn of a hold (an agent error, a 403, a hung turn, a failed sign-in) or
    /// a turn whose agent ended reaches the owner at once: a post at level bad, which the web
    /// pushes, with the reason, the retry and the open tasks. A usage limit and a move to a
    /// fallback post none: a stand-in or the next agent answers. ref: docs/tasks.md "Manager"
    fn alert_failed(&mut self, reason: &str, until_ms: Option<u64>) {
        let records: Vec<TaskRecord> = project_tasks(&self.dir.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, record)| record.ok())
            .collect();
        let (count, states) = open_counts(&records);
        let open = match count {
            0 => "No task is open.".to_string(),
            1 => format!("1 open task: {states}."),
            n => format!("{n} open tasks: {states}."),
        };
        let retry = until_ms.map_or(String::new(), |until| {
            format!(
                " It tries again at {}.",
                status::resume_time(until, now_unix_ms())
            )
        });
        let text = format!("{}.{retry} {open}", reason.trim_end_matches('.'));
        self.post("turn failed".into(), text, Some(Level::Bad), None);
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
        for entry in log {
            if let ManagerEvent::Message {
                id,
                from,
                text,
                short,
                ..
            } = entry.event
            {
                if !delivered.contains(&id) {
                    self.enqueue(id, from, text, Short::any_wakes(&short));
                }
            }
        }
        Ok(())
    }

    fn approval_lines(&self) -> Vec<ApprovalLine> {
        store::read_approvals(&self.dir.project_id).unwrap_or_default()
    }

    /// A chat's host leaves when its chat is closed, or after `CHAT_LINGER` with no message
    /// and no turn: an idle chat costs no agent and no pane.
    fn leave_when_done(&mut self) {
        let closed = !chats::is_open(&self.dir);
        let idle = self.turn.is_none()
            && !self.work_waiting()
            && self.state.limit.is_none()
            && self.last_activity.elapsed() >= CHAT_LINGER;
        if closed || idle {
            self.say(match closed {
                true => "the chat is closed: its host leaves",
                false => "idle: the chat's host leaves until the next message",
            });
            self.leaving = true;
        }
    }

    /// Who the agent's commands act as (`WSX_TASK_ACTOR`): the manager, or the chat.
    fn actor(&self) -> Actor {
        Actor::Chat(self.dir.name().to_string())
    }

    /// Log a message and queue it; owner messages go before events.
    fn accept(
        &mut self,
        from: Author,
        mut text: String,
        short: Vec<Short>,
        window: Option<String>,
    ) -> Result<u64> {
        // ^ A reply to a To check item: "x" skips it here and starts no turn; other words go on
        // with the item's check as their quote and its id on the message. The item keeps the
        // message's id, so it is taken first. ref: `checks::item_reply`
        let mut reserved = None;
        let mut check = None;
        if from == Author::Owner && text.trim_start().starts_with(checks::QUOTE_REF) {
            let message = self.next_id();
            reserved = Some(message);
            match checks::item_reply(&self.dir.project_id, &text, message) {
                Some(ItemReply::Skip {
                    text,
                    item,
                    check,
                    was,
                }) => {
                    return Ok(self.skipped(message, text, item, &check, was));
                }
                Some(ItemReply::Words { text: words, item }) => (text, check) = (words, item),
                None => {}
            }
        }
        // ^ The owner's Reply sends `> message <id>`, as a manager's reply does.
        let (text, quote) = match from {
            Author::Owner => self.quoted(text, &[], true),
            _ => (text, None),
        };
        if text.trim().is_empty() {
            anyhow::bail!("the message is empty");
        }
        if text.len() > MAX_MESSAGE_BYTES {
            anyhow::bail!("the message is larger than the limit of {MAX_MESSAGE_BYTES} bytes");
        }
        // ^ The owner's message is tagged by rule, no model: its quote, its window, its item.
        let thread = match from {
            Author::Owner => self.thread_of(quote, window.as_deref(), check.as_deref())?,
            _ => None,
        };
        let id = reserved.unwrap_or_else(|| self.next_id());
        let wakes = Short::any_wakes(&short);
        self.dir.append(ManagerEvent::Message {
            id,
            from: from.clone(),
            text: text.clone(),
            short,
            quote,
            check,
            thread,
        })?;
        self.last_activity = Instant::now();
        self.overview_asked &= from != Author::Owner;
        self.drift_said &= from != Author::Owner;
        self.thought_asked &= from != Author::Owner;
        // ^ The owner may have just signed in again: a sign-in hold tries at once.
        if from == Author::Owner {
            if let Some(hold) = self.state.limit.as_mut().filter(|hold| hold.auth) {
                hold.until_ms = hold.until_ms.min(now_unix_ms());
                self.save();
            }
        }
        self.enqueue(id, from, text, wakes);
        Ok(id)
    }

    /// The owner's "x" on a To check item, logged as delivered, and the muted line that says what
    /// became of the item: no turn reads either. Returns the owner message's id.
    fn skipped(
        &mut self,
        id: u64,
        text: String,
        item: String,
        check: &str,
        was: Option<CheckState>,
    ) -> u64 {
        self.log(ManagerEvent::Message {
            id,
            from: Author::Owner,
            text,
            short: Vec::new(),
            quote: None,
            check: Some(item),
            thread: None,
        });
        self.log(ManagerEvent::Delivered { ids: vec![id] });
        let check = clip(check, 60);
        let said = match was {
            None => format!("Skipped: {check}"),
            Some(state) => format!("Already {}: {check}", checks::state_name(state)),
        };
        // ^ A quiet post answers it, so a restart does not hand the message to the agent.
        let reply = self.next_id();
        self.log(ManagerEvent::Reply {
            id: reply,
            to: vec![id],
            threads: Vec::new(),
            text: said,
            usage: None,
            quiet: true,
            asks: false,
            provisional: None,
            level: None,
            post: true,
            quote: None,
            approval: None,
        });
        self.last_activity = Instant::now();
        id
    }

    /// Owner messages go first, then other managers', then events.
    fn enqueue(&mut self, id: u64, from: Author, text: String, wakes: bool) {
        let rank = |author: &Author| match author {
            Author::Owner => 0,
            Author::Peer { .. } => 1,
            Author::Events => 2,
            Author::Recap => 3,
        };
        let at = self
            .queue
            .iter()
            .position(|(_, author, ..)| rank(author) > rank(&from))
            .unwrap_or(self.queue.len());
        self.queue.insert(at, (id, from, text, wakes));
    }

    /// A message of a manager or a chat is answered once (docs/tasks.md "Chats").
    fn owe(&self, author: &Author) {
        let Author::Peer { from, reply: false } = author else {
            return;
        };
        *self
            .owed
            .lock()
            .expect("owed lock")
            .entry(from.key())
            .or_default() += 1;
    }

    fn next_id(&mut self) -> u64 {
        self.state.next_id += 1;
        self.save();
        self.state.next_id
    }

    /// The messages of the next turn, off the queue: those of the first author that wakes the
    /// manager, then the events that need no judgment, which go along after them.
    fn take_batch(&mut self) -> Batch {
        let first = self
            .queue
            .iter()
            .find(|(_, author, _, wakes)| *author != Author::Events || *wakes)
            .or(self.queue.front())
            .map_or(Author::Owner, |(_, author, ..)| author.clone());
        let mut ids = Vec::new();
        let mut text = String::new();
        let mut said = String::new();
        // ^ Events that need no judgment go along with a turn that runs anyway, after its
        // own messages.
        let mut along = Vec::new();
        // ^ What left the queue, to put back when the agent does not start.
        let mut taken = Vec::new();
        let mut for_owner = false;
        // ^ The last `#<n> <state> r<k>` line of each task in the turn's events.
        let mut heads: BTreeMap<u32, (String, u32)> = BTreeMap::new();
        if first != Author::Events {
            while let Some(at) = self
                .queue
                .iter()
                .position(|(_, author, _, wakes)| *author == Author::Events && !wakes)
            {
                along.push(self.queue.remove(at).expect("position is in the queue"));
            }
        }
        while self
            .queue
            .front()
            .is_some_and(|(_, author, ..)| *author == first)
        {
            let (id, author, body, wakes) = self.queue.pop_front().expect("checked above");
            let heading = author_heading(&author);
            for_owner |= author == Author::Events && body.lines().any(routine_for_owner);
            if author == Author::Events {
                heads.extend(body.lines().filter_map(event_head));
            }
            text.push_str(&format!("{heading} (message {id}):\n{body}\n\n"));
            if author == Author::Owner {
                said.push_str(&body);
                said.push('\n');
            }
            ids.push(id);
            taken.push((id, author, body, wakes));
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
        for (id, author, body, wakes) in along {
            heads.extend(body.lines().filter_map(event_head));
            text.push_str(&format!("Events (message {id}):\n{body}\n\n"));
            ids.push(id);
            taken.push((id, author, body, wakes));
        }
        if let Some(since) = self.since_then(&heads) {
            text.push_str(&format!("{since}\n\n"));
        }
        Batch {
            first,
            ids,
            text,
            taken,
            for_owner,
        }
    }

    /// "since then: #426 working r1, #427 landed r1": the tasks of the turn's events that are
    /// in another state or revision now than their last `#<n> <state> r<k>` line says. A turn
    /// reads events minutes or hours after wsx wrote them (on 11-13 three limit waits were
    /// read 25 min after the limit ended and relayed as current): the line that says what
    /// holds now is wsx's, read at the turn's start. None when nothing changed.
    /// ref: docs/tasks.md "Manager" (Events)
    fn since_then(&self, heads: &BTreeMap<u32, (String, u32)>) -> Option<String> {
        if heads.is_empty() {
            return None;
        }
        let tasks = project_tasks(&self.dir.project_id).unwrap_or_default();
        let changed: Vec<String> = heads
            .iter()
            .filter_map(|(id, (word, revision))| {
                let record = tasks
                    .iter()
                    .filter_map(|(_, record)| record.as_ref().ok())
                    .find(|record| record.id == *id)?;
                let now = wsx_core::task::status::state_word(record);
                if (now, record.revision) == (word.as_str(), *revision) {
                    return None;
                }
                let on = match (&record.waiting, record.state) {
                    (Some(waiting), TaskState::Waiting) => format!(" on {}", waiting.on.as_str()),
                    _ => String::new(),
                };
                Some(format!("#{id} {now} r{}{on}", record.revision))
            })
            .collect();
        match changed.is_empty() {
            true => None,
            false => Some(format!("since then: {}", changed.join(", "))),
        }
    }

    fn next_turn(&mut self) {
        // ^ A start that failed waits: its messages are back in the queue.
        if self
            .start_failed
            .is_some_and(|failed| failed.elapsed() < START_RETRY)
        {
            return;
        }
        // ^ A replaced binary: exec it before this turn, also while a hold waits, so a fix
        // to the wait reaches a held host. The messages stay logged and not delivered, so
        // the new host queues them again (`restore`); a hold stays in the state.
        if (self.state.limit.is_some() || self.work_waiting())
            && self.binary.as_ref().is_some_and(Binary::replaced)
        {
            self.say("the wsx binary was replaced: the manager restarts with it");
            self.reexec = true;
            return;
        }
        if self.choose_agent() {
            return;
        }
        let held = self.state.limit.clone();
        if held.is_none() && !self.work_waiting() {
            return;
        }
        let Batch {
            first,
            ids,
            mut text,
            taken,
            for_owner,
        } = self.take_batch();
        // ^ The limit preface is no owner message: a held turn is events-only when its
        // held and its new messages all are. Another manager's or a chat's reply is used, not
        // answered: like events. ref: docs/tasks.md "Manager"
        let events_only = held.as_ref().is_none_or(|hold| hold.events)
            && if ids.is_empty() {
                held.is_some()
            } else {
                matches!(first, Author::Events | Author::Peer { reply: true, .. })
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
        text.push_str(&self.open_questions());
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
        for (_, author, ..) in &taken {
            self.owe(author);
        }
        // ^ A sign-in hold logged no pause, so it logs no resume: the chat reads a `limit`
        // without a time as the end of a hold (`waiting_ids`), and each retry would say it.
        if held
            .as_ref()
            .is_some_and(|hold| !hold.switched && !hold.auth)
        {
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
                .any(|(dir, record)| {
                    record.is_ok_and(|record| {
                        status::waits_on(&dir, &record, land_auto)
                            .is_some_and(|on| on.kind == status::Holder::Manager)
                    })
                })
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

    fn work_waiting(&self) -> bool {
        self.queue
            .iter()
            .any(|(_, author, _, wakes)| *author != Author::Events || *wakes)
    }

    /// Before every turn, the one rule of where the manager goes (`agent::next`): back on its
    /// own agent as soon as that is free, in a fresh conversation that opens with the handover
    /// and the held messages in full; down to a fallback for a failed sign-in, or a limit past
    /// `maxWaitMinutes`; else a hold on the own account's record, which ends when the record
    /// does. Only with a turn to run. True while the turn waits: a hold on an error's
    /// backoff waits on its time, one on the account on its record, and the stand-in answers
    /// the owner meanwhile. ref: docs/tasks.md "Agents"
    fn choose_agent(&mut self) -> bool {
        let now = now_unix_ms();
        // ^ The own account's probe, whatever agent runs now.
        if self.adapter == "claude" {
            claude::probe_limit();
        }
        let held = self.state.limit.clone();
        if held.is_none() && !self.work_waiting() {
            return false;
        }
        let hold = held.as_ref().filter(|hold| now < hold.until_ms);
        if hold.is_some_and(|hold| !hold.account) {
            self.stand_in();
            return true;
        }
        let (ids, events) = held.map_or((Vec::new(), true), |hold| (hold.ids, hold.events));
        match self.next(Ask::Look, now) {
            Next::Stay => {
                if let Some(hold) = self.state.limit.as_mut().filter(|hold| now < hold.until_ms) {
                    hold.until_ms = now;
                    self.say("the usage limit has lifted: the manager continues");
                    self.save();
                }
                false
            }
            Next::Home => {
                self.go_home(ids, events, None);
                false
            }
            Next::Move(next, cause) => {
                let reason = self.move_words(&next, cause, now);
                self.move_to(next, reason, ids, events);
                false
            }
            Next::Wait(until_ms) => {
                let announce = self.state.limit.is_none();
                self.hold_on_account(until_ms, 0, ids, events, false, announce, now);
                self.stand_in();
                true
            }
        }
    }

    /// Where the manager goes (`agent::next`): its own agent is `tasks.manager.adapter` and
    /// `model`, the current one `state.agent` when set.
    fn next(&self, ask: Ask, now: u64) -> Next {
        let settings = load_task_config(&self.main).unwrap_or_default();
        let current = self.current_agent();
        agent::next(
            (&self.adapter, &self.model),
            (&current.0, &current.1),
            &settings.manager_fallback,
            settings.max_wait_secs,
            policy::Role::Manager,
            ask,
            now,
        )
    }

    /// The (adapter, model) that runs now: a fallback's, else the own.
    fn current_agent(&self) -> (String, String) {
        match &self.state.agent {
            Some(agent) => (agent.adapter.clone(), agent.model.clone()),
            None => (self.adapter.clone(), self.model.clone()),
        }
    }

    /// The one chat line of a move: "switched to <name>: claude usage limit until 03:20", or
    /// "claude moved to <name>: sign-in expired".
    fn move_words(&self, next: &agent::Choice, cause: Cause, now: u64) -> String {
        let adapter = self.current_adapter();
        match cause {
            Cause::Limit(_) => format!("switched to {}: {}", next.name, cause.words(&adapter, now)),
            Cause::SignIn => format!(
                "{adapter} moved to {}: {}",
                next.name,
                cause.words(&adapter, now)
            ),
        }
    }

    /// The current account refused the turn (a usage limit or a failed sign-in, on record
    /// until `until_ms`, the `attempt`-th in a row), with `why` its words; the turn's `ids`
    /// are held. `agent::next` says where: home when the own agent is free, to a free
    /// fallback, else a hold on the own account, which a fallback leaves for.
    /// ref: docs/tasks.md "Agents"
    fn refused(&mut self, until_ms: u64, attempt: u32, ids: Vec<u64>, why: &str, now: u64) {
        let events = self.turn_events;
        let until_ms = match self.next(Ask::Refused(until_ms), now) {
            Next::Home => return self.go_home(ids, events, Some(why)),
            Next::Move(next, cause) => {
                let reason = self.move_words(&next, cause, now);
                return self.move_to(next, reason, ids, events);
            }
            Next::Wait(until_ms) => until_ms,
            Next::Stay => until_ms,
        };
        let away = self.state.agent.is_some();
        if away {
            let reason = format!(
                "back on {}: {} was refused: {why}; waits for it",
                self.adapter,
                self.current_adapter()
            );
            self.say(&reason);
            self.rotate(0, Some(reason));
            self.state.agent = None;
        } else {
            self.say(&limit::wait_reason(
                &self.adapter,
                &self.current_account(),
                until_ms,
                now,
            ));
        }
        self.hold_on_account(until_ms, attempt, ids, events, away, true, now);
    }

    /// Hold the turn `ids` on the own account until its record ends (`until_ms` as it reads
    /// now); `switched` when a new conversation must see the messages in full, as when the
    /// hold before was; `announce` logs the hold's `limit` entry. ref: docs/tasks.md "Agents"
    #[allow(clippy::too_many_arguments)]
    fn hold_on_account(
        &mut self,
        until_ms: u64,
        attempt: u32,
        ids: Vec<u64>,
        events: bool,
        switched: bool,
        announce: bool,
        now: u64,
    ) {
        let auth = store::sign_in_failed_until(&self.current_account(), now).is_some();
        let before = self.state.limit.as_ref();
        self.state.limit = Some(LimitHold {
            until_ms,
            attempt,
            ids,
            events,
            switched: switched || before.is_some_and(|hold| hold.switched),
            auth,
            account: true,
        });
        self.save();
        // ^ A `limit` entry names a reset the owner waits for. A sign-in has none: its `stopped`
        // entry says to sign in again, and a `limit` would show a pause with a time. It follows
        // the saved hold: whoever sees the entry finds the hold in the state.
        if announce && !auth {
            self.log(ManagerEvent::Limit {
                until: Some(self.held_until(until_ms, now)),
            });
        }
    }

    /// Back on the own agent in a fresh conversation that opens with the handover and the
    /// held `ids` in full; `refused` names what the fallback answered, when it was refused.
    fn go_home(&mut self, ids: Vec<u64>, events: bool, refused: Option<&str>) {
        let reason = format!("back on {}", self.adapter);
        self.say(&match refused {
            Some(why) => format!("{reason}: {} was refused: {why}", self.current_adapter()),
            None => reason.clone(),
        });
        self.rotate(0, Some(reason));
        self.state.agent = None;
        self.state.limit = Some(LimitHold {
            until_ms: now_unix_ms(),
            attempt: 0,
            ids,
            events,
            switched: true,
            auth: false,
            account: false,
        });
        self.save();
    }

    /// Continue the held turn `ids` in a new conversation on `next`, which opens with the
    /// handover; `reason` is the one chat line.
    fn move_to(&mut self, next: agent::Choice, reason: String, ids: Vec<u64>, events: bool) {
        self.say(&reason);
        self.state.limit = Some(LimitHold {
            until_ms: now_unix_ms(),
            attempt: 0,
            ids,
            events,
            switched: true,
            auth: false,
            account: false,
        });
        self.rotate(0, Some(reason));
        self.state.agent = Some(ManagerAgent {
            name: next.name,
            adapter: next.adapter,
            model: next.model,
        });
        self.save();
    }

    /// A turn with no line from the agent past its record's deadline ends: the agent is
    /// stopped and the turn is its error, which holds and retries; on a fallback it counts as
    /// a refusal with no reset, so the manager moves on (`refused`).
    /// ref: docs/tasks.md "Agents"
    fn end_hung_turn(&mut self) {
        if self.turn.is_none() || self.stand_in.is_some() {
            return;
        }
        let Some(quiet) = self
            .turn_record
            .as_ref()
            .filter(|turn| turn.left().is_zero())
            .map(stall::Turn::quiet)
        else {
            return;
        };
        let turn = agent::hung_turn(quiet, self.state.agent.is_some());
        self.say(&turn.text);
        // ^ A late line of the stopped process is not a turn of the manager's.
        self.generation += 1;
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.on_result(turn);
    }

    /// The hold's end for the chat's line, and why no stand-in answers when none does.
    fn held_until(&self, until_ms: u64, now: u64) -> String {
        let time = status::resume_time(until_ms, now);
        let settings = load_task_config(&self.main).unwrap_or_default();
        match agent::stand_in(&settings.manager_fallback, now) {
            Some(_) => time,
            None => format!("{time} ({})", floor::down_note()),
        }
    }

    /// During a hold, an owner message with no answer yet gets one from an agent that fills the organizer role, one
    /// call with no tools. The message stays due: the own agent answers it after the hold and
    /// is told of this reply. ref: docs/tasks.md "Agents"
    fn stand_in(&mut self) {
        let done = self.state.provisional_to;
        let queued: Vec<u64> = self
            .queue
            .iter()
            .filter(|(id, author, ..)| *author == Author::Owner && *id > done)
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
        let Some(agent) = agent::stand_in(&settings.manager_fallback, now) else {
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
        let away = match store::limited_until(&self.current_account(), now) {
            Some(until_ms) => format!(
                "{} is limited until {}: say so in your reply.\n\n",
                self.adapter,
                status::resume_time(until_ms, now)
            ),
            None => String::new(),
        };
        let prompt = format!(
            "{away}{}The last exchanges:\n{}\n\n{ledger}\n\n{text}",
            self.notes_preamble(),
            recent.join("\n")
        );
        // ^ The held conversation stays resumable: its process has nothing to do now.
        if let Some(agent) = self.agent.take() {
            agent.stop();
        }
        self.generation += 1;
        let generation = self.generation;
        let started = self
            .launch_for(&agent.adapter, &agent.model)
            .and_then(|launch| {
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
                let _ = store::mark_limited(&account, now + STAND_IN_RETRY_MS, now, false, None);
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
                self.spend(
                    &stand_in.adapter,
                    &stand_in.model,
                    result.usage.cost_micro_usd,
                    now,
                );
                let id = self.next_id();
                self.say(&format!("{name} stood in: reply {id}"));
                let answered = to.iter().copied().max().unwrap_or(0);
                let (text, quote) = self.quoted(result.text, &to, false);
                let threads = self.reply_threads(&to);
                self.log(ManagerEvent::Reply {
                    id,
                    to,
                    threads,
                    text,
                    usage: Some(result.usage),
                    quiet: false,
                    asks: false,
                    provisional: Some(name),
                    level: None,
                    post: false,
                    quote,
                    approval: None,
                });
                self.state.provisional_to = answered;
                self.state.provisional = true;
                self.save();
            }
            None => {
                self.say(&format!("{name} could not stand in"));
                let account = agent::account(&stand_in.adapter, &stand_in.model);
                let _ = store::mark_limited(&account, now + STAND_IN_RETRY_MS, now, false, None);
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
                    one_line(split_quote(text).1)
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

    /// The replies of this chat that still ask the owner (`chat::asking`, the one rule), one
    /// line each, so the agent asks none of them again; empty when none is open.
    /// ref: docs/tasks.md "Questions"
    fn open_questions(&self) -> String {
        let asks = chat::asking(&self.dir, now_unix_ms()).unwrap_or_default();
        if asks.is_empty() {
            return String::new();
        }
        let lines: String = asks
            .iter()
            .map(|ask| format!("- {}: {}\n", ask.id, ask.quote))
            .collect();
        format!(
            "Open questions (the owner has them; do not ask again; unanswered: do not act on one until the owner answers):\n{lines}\n"
        )
    }

    /// A few lines of the ledger for the end of a turn's message: the open tasks by state,
    /// the tasks changed since the last turn (every open one in a fresh conversation), the
    /// To check and approval counts and the deploy. The manager reads the full ledger only
    /// when it needs more. ref: docs/tasks.md "Manager"
    fn ledger_summary(&mut self) -> String {
        // ^ The chat's own tasks, To check items and approvals (`chats::Viewer`), as its page lists them.
        let viewer = chats::Viewer::new(&self.dir.project_id, Some(&self.dir.chat));
        let tasks: Vec<(store::TaskDir, wsx_core::task::TaskRecord)> =
            project_tasks(&self.dir.project_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(dir, record)| record.ok().map(|record| (dir, record)))
                .filter(|(_, record)| viewer.owns(record.created_by.chat()))
                .collect();
        let open = match open_counts(tasks.iter().map(|(_, record)| record)) {
            (0, _) => "none".to_string(),
            (_, states) => states,
        };
        let to_check = chats::checks_of(
            &self.dir.project_id,
            checks::open_items(&self.dir.project_id).unwrap_or_default(),
            &viewer,
        )
        .len();
        let approvals =
            pending_approvals(&chats::approvals_of(self.approval_lines(), &viewer)).len();
        let baseline = self.state.turn_seen.take();
        let seen: BTreeMap<u32, Seen> = tasks
            .iter()
            .map(|(dir, record)| (record.id, Seen::of(dir, record)))
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
                    wsx_core::task::status::state_word(record),
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
        // ^ Claude's prompt cache lives 1 hour: a large conversation resumed later writes its
        // whole context again, so it starts afresh with the handover.
        let idle = now_unix_ms().saturating_sub(self.state.last_reply_ms);
        // ^ A usage limit pauses the conversation, it does not end it: the turn after it
        // resumes the same session, however long the limit was.
        if self.state.agent_session.is_some()
            && self.state.limit.is_none()
            && self.state.last_reply_ms > 0
            && idle > PROMPT_CACHE_MS
            && self.state.context_tokens > IDLE_ROTATE_TOKENS
        {
            let tokens = self.state.context_tokens;
            self.rotate(
                tokens,
                Some(format!("idle over 60 min with {}k context", tokens / 1000)),
            );
        }
        // ^ No turn runs here: a program updated since the agent started is replaced by one
        // that resumes the same conversation, through the start below.
        if self.state.agent_session.is_some() {
            if let Some(change) = self.agent.as_ref().and_then(Agent::replaced) {
                self.say(&format!("agent {} {change}", self.current_adapter()));
                self.generation += 1;
                self.agent = None;
            }
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
                    && now_unix_ms().saturating_sub(self.state.last_reply_ms) < PROMPT_CACHE_MS;
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
                    let handover = self.handover();
                    self.remember_handover(&handover);
                    text = handover + &text;
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
        agent.send(text.trim_end())?;
        self.start_turn_record();
        Ok(())
    }

    /// Start the record of the turn just sent (`stall::Turn`).
    fn start_turn_record(&mut self) {
        let (adapter, _) = self.current_agent();
        let model = &self.agent_model;
        let settings = load_task_config(&self.main).unwrap_or_default();
        let quiet_ms = stall::turn_quiet_ms(settings.check_timeout_secs);
        self.turn_record = Some(stall::Turn::start(
            &self.dir.path,
            format!("{adapter}/{model}"),
            quiet_ms,
        ));
    }

    /// End the running turn's record with `reason`.
    fn end_turn_record(&mut self, reason: &str) {
        if let Some(turn) = self.turn_record.take() {
            turn.end(&self.dir.path, reason);
        }
    }

    /// The launch of the agent that runs now; its policy is the broker's from here on.
    fn launch(&self) -> Result<Launch> {
        let (adapter, model) = match &self.state.agent {
            Some(agent) => (agent.adapter.clone(), agent.model.clone()),
            None => (self.adapter.clone(), self.model.clone()),
        };
        let launch = self.launch_for(&adapter, &model)?;
        self.policy.set(launch.boundary.policy());
        Ok(launch)
    }

    /// The manager runs inside its boundary and prompts for nothing. ref: docs/tasks.md "Permissions"
    fn launch_for(&self, adapter: &str, model: &str) -> Result<Launch> {
        let exe = std::env::current_exe().context("cannot find the wsx binary")?;
        let notes = self.dir.notes()?;
        let broker_socket = self.dir.broker_path();
        let overview = store::ensure_overview(&self.dir.project_id, &self.project)?;
        let root = config_root(&self.main)?;
        let env: Vec<(String, std::ffi::OsString)> = [
            (
                broker::BROKER_ENV.into(),
                broker_socket.clone().into_os_string(),
            ),
            (ACTOR_ENV.into(), String::from(self.actor()).into()),
            // ^ Its git reads must not take the index lock of the owner's checkout.
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            // ^ Every push URL becomes one no helper serves, so `git push` fails with its name;
            // the profile also denies the programs a push runs (boundary.rs).
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), NO_PUSH_KEY.into()),
            ("GIT_CONFIG_VALUE_0".into(), "".into()),
        ]
        .into_iter()
        .chain(account_env(root.clone()))
        .collect();
        let policy = agent::policy(policy::Role::Manager, adapter, model, &env);
        let boundary = Boundary::manager(&self.main, &self.dir.path, &overview, policy)?
            .adapter(adapter, root.as_deref())
            .reach(adapter, model, &env)
            .broker(&broker_socket);
        let skills = skills_plugin(&self.dir.path, root.as_deref(), &[])?;
        let workflow = load_task_config(&self.main)
            .map(|settings| settings.workflow)
            .unwrap_or_default();
        let names = skills.as_deref().map(skill_names).unwrap_or_default();
        let folder = skills.as_deref().map(|plugin| plugin.join("skills"));
        let listed = match (&folder, adapter) {
            (None, _) => Skills::None,
            (Some(folder), "claude") => Skills::Folder(folder, &names),
            (Some(_), _) => Skills::Tool,
        };
        let mut text =
            super::instructions::instructions(&self.project, &notes, &overview, listed, &workflow);
        if let Some(chat) = self.dir.named() {
            text = chat_preamble(chat, &self.project, &chats::goal(&self.dir)) + &text;
        }
        // ^ Other adapters list lent skills themselves (pi.rs `--skill`).
        if adapter != "claude" {
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
        launch.env = env;
        launch.path_prefix = exe.parent().map(Path::to_path_buf);
        Ok(launch)
    }

    fn on_agent(&mut self, line: Option<AgentLine>) {
        match line {
            // ^ The manager's size is checked after each reply: a turn grew at most 53k tokens
            // in 7 days to 2026-11-15, never past `rotateTokens`. ref: docs/tasks.md "Manager"
            Some(AgentLine::Context(_)) => {}
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
            // ^ A sign-in failure again, during a hold for one: the chat already says so.
            Some(AgentLine::Progress { text, .. })
                if agent::auth_failed(&text)
                    && self.state.limit.as_ref().is_some_and(|hold| hold.auth) =>
            {
                self.say(&text)
            }
            Some(AgentLine::Progress { text, .. }) => {
                if let Some(turn) = &mut self.turn_record {
                    turn.output(&self.dir.path);
                }
                self.say(&text);
                self.log(ManagerEvent::Agent { text });
            }
            Some(AgentLine::Result(result)) => self.on_result(*result),
            None => {
                self.agent = None;
                if self.turn.take().is_some() {
                    let reason = "the manager agent ended during a turn";
                    self.end_turn_record(reason);
                    self.log(ManagerEvent::Stopped {
                        reason: reason.into(),
                    });
                    if self.state.limit.is_none() {
                        self.alert_failed(reason, None);
                    }
                    self.report(runtime::AgentState::Error);
                }
            }
        }
    }

    fn on_result(&mut self, result: TurnResult) {
        if result.answers_nothing() {
            return;
        }
        self.end_turn_record(&result.ended());
        let to = self.turn.take().unwrap_or_default();
        let (adapter, model) = match &self.state.agent {
            Some(agent) => (agent.adapter.clone(), agent.model.clone()),
            None => (self.adapter.clone(), self.model.clone()),
        };
        let now = now_unix_ms();
        self.spend(&adapter, &model, result.usage.cost_micro_usd, now);
        // ^ A usage limit is no reply: its messages are answered once it resets.
        // ref: docs/tasks.md "Manager"
        if let Some(reached) = &result.limit {
            let attempt = self.state.limit.as_ref().map_or(0, |hold| hold.attempt) + 1;
            let (until_ms, problem) = agent::record_limit(&adapter, &model, reached, attempt, now);
            if let Some(message) = problem {
                self.say(&message);
            }
            let why = clip(result.text.lines().next().unwrap_or_default(), 200);
            self.refused(until_ms, attempt, to, &why, now);
            self.save();
            self.report(runtime::AgentState::Idle);
            return;
        }
        // ^ An agent's error is no reply either: the owner sees why, and the turn tries again
        // with a backoff. A conversation that keeps failing may be the cause: a fresh one
        // follows. ref: docs/tasks.md "Manager"
        if result.is_error && result.subtype != "aborted" {
            let attempt = self.state.limit.as_ref().map_or(0, |hold| hold.attempt) + 1;
            let reason = clip(result.text.lines().next().unwrap_or_default(), 200);
            self.say(&format!("agent error, attempt {attempt}: {reason}"));
            // ^ A failed sign-in waits for the owner's new one: no doubling backoff and no new
            // conversation, but a new process each try, since the running one keeps its old
            // credentials. Logged once, not on each try.
            // ^ A failed sign-in: every host skips the account, and the running process keeps
            // its old credentials, so the next try is a new one. The account's record decides
            // where (`refused`), as for a limit; the chat hears "Sign in again" once per hold.
            if agent::auth_failed(&result.text) {
                let (until_ms, problem) = agent::record_sign_in_failed(&adapter, &model, now);
                if let Some(message) = problem {
                    self.say(&message);
                }
                if let Some(agent) = self.agent.take() {
                    agent.stop();
                }
                let was_auth = self.state.limit.as_ref().is_some_and(|hold| hold.auth);
                self.refused(until_ms, attempt, to, &reason, now);
                // ^ A move to a fallback answers the owner; a hold for the sign-in does not.
                if self.state.limit.as_ref().is_some_and(|hold| hold.auth) && !was_auth {
                    self.log(ManagerEvent::Stopped {
                        reason: format!(
                            "{reason}. Sign in again (`claude /login`); the manager retries \
                             every {} min.",
                            store::sign_in_retry_ms().div_ceil(60_000)
                        ),
                    });
                    let until = self.state.limit.as_ref().map(|hold| hold.until_ms);
                    self.alert_failed(&reason, until);
                }
                self.save();
                self.report(runtime::AgentState::Error);
                return;
            }
            let until_ms = limit::resume_at(None, attempt, now);
            let rotate = attempt == ERROR_ROTATE_ATTEMPTS;
            // ^ The hold is saved before the entries and the alert that announce it: whoever
            // sees the `stopped` entry finds the hold in the state.
            self.state.limit = Some(LimitHold {
                until_ms,
                attempt,
                ids: to,
                events: self.turn_events,
                switched: rotate || self.state.limit.as_ref().is_some_and(|hold| hold.switched),
                auth: false,
                account: false,
            });
            self.save();
            self.log(ManagerEvent::Stopped {
                reason: reason.clone(),
            });
            self.log(ManagerEvent::Limit {
                until: Some(status::resume_time(until_ms, now)),
            });
            if attempt == 1 {
                self.alert_failed(&reason, Some(until_ms));
            }
            if rotate {
                self.rotate(0, Some(reason));
            }
            self.report(runtime::AgentState::Error);
            return;
        }
        self.state.limit = None;
        store::clear_sign_in_failed(&agent::account(&adapter, &model));
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
        let (text, quote) = self.quoted(result.text, &to, false);
        let asks = asks_owner(&text);
        let quiet = self.turn_events && !asks && quiet_reply(&text);
        if !quiet && !says_nothing(&text) {
            self.state.last_reply = Some(id);
        }
        self.save();
        let threads = self.reply_threads(&to);
        self.log(ManagerEvent::Reply {
            id,
            to,
            threads,
            text,
            usage: Some(result.usage),
            quiet,
            asks,
            provisional: None,
            level: None,
            post: false,
            quote,
            approval: None,
        });
        if context > self.rotate_limit() {
            self.rotate(context, None);
        }
        self.report(runtime::AgentState::Done);
        self.post_asks();
        self.check_overview();
        self.check_views();
        self.check_thought(in_thinking);
    }

    /// The ask line of each approval this host's agent asked of the owner in the turn, so the
    /// reply needs no words for it. ref: docs/tasks.md "Approvals"
    fn post_asks(&mut self) {
        let lines = self.approval_lines();
        let asked: Vec<(String, String)> = lines
            .iter()
            .skip(self.asks_seen)
            .filter(|line| match line {
                ApprovalLine::Request { said, chat, .. } => {
                    said.is_none() && chat.as_deref() == Some(self.dir.name())
                }
                ApprovalLine::Decision { .. } => false,
            })
            .map(|line| {
                let ask = format!("{} ({})", owner::approval_label(line), line.id());
                (ask, line.id().to_string())
            })
            .collect();
        self.asks_seen = lines.len();
        // ^ The request owns its push item; the line only shows the ask in the chat.
        for (ask, id) in asked {
            self.post(
                format!("asked the owner: {ask}"),
                format!("Asked you: {ask}."),
                None,
                Some(id),
            );
        }
    }

    /// A turn whose words are far less than its thinking gets one follow-up, and no second one
    /// until the owner writes again: it ends the loop when the follow-up thinks just as much.
    /// ref: docs/tasks.md "Reply text"
    fn check_thought(&mut self, in_thinking: bool) {
        if in_thinking && !self.thought_asked {
            self.thought_asked = true;
            if let Err(error) = self.accept(Author::Recap, THOUGHT_ASKED.into(), Vec::new(), None) {
                self.say(&format!("cannot ask for the answer in thinking: {error:#}"));
            }
        }
    }

    /// The thread of an owner's message (`thread::tag`): its quote's, its window's, its To check
    /// item's task's; the trunk when none. A closed thread opens again with the message, a merged
    /// one reads as the thread it went into (`thread::join`).
    fn thread_of(
        &self,
        quote: Option<u64>,
        window: Option<&str>,
        item: Option<&str>,
    ) -> Result<Option<String>> {
        let log = match quote {
            Some(id) => self
                .dir
                .read_around(QUOTE_READ_BYTES, |log| {
                    log.iter()
                        .any(|entry| entry.id().is_some_and(|at| at <= id))
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let item = item
            .and_then(|item| thread::of_item(&self.dir.project_id, item))
            .and_then(|key| thread::slug_in(self.dir.name(), &key).map(str::to_string));
        thread::tag(&log, quote, window, item.as_deref())
            .map(|slug| thread::join(&self.dir, &slug))
            .transpose()
    }

    /// The threads of a reply to messages `to`: those of the messages. A turn of only events has none.
    fn reply_threads(&self, to: &[u64]) -> Vec<String> {
        let Some(oldest) = to.iter().min().filter(|_| !self.turn_events) else {
            return Vec::new();
        };
        let log = self
            .dir
            .read_around(QUOTE_READ_BYTES, |log| {
                log.iter()
                    .any(|entry| entry.id().is_some_and(|at| at <= *oldest))
            })
            .unwrap_or_default();
        thread::reply_threads(&log, to)
    }

    /// A reply that opens with `> message <id>` quotes that message, when it is the owner's or
    /// another chat's (`resolve_quote`; the owner's Reply may name a manager's reply too). One that opens with no quote and says something quotes the
    /// owner message it answers when it will not sit under it (`chat::quoted_message`); an owner's
    /// own message (`to` empty) only its explicit quote. ref: docs/tasks.md "A quote"
    fn quoted(&self, text: String, to: &[u64], by_owner: bool) -> (String, Option<u64>) {
        let explicit = text.trim_start().starts_with(QUOTE_REF);
        if !explicit && (to.is_empty() || says_nothing(&text)) {
            return (text, None);
        }
        // ^ The log back to the oldest id wanted, not all of it: ids grow (docs/tasks.md
        // "Log window").
        let oldest = match explicit {
            true => quoted_id(&text),
            false => to.iter().min().copied(),
        };
        let log = match oldest {
            Some(id) => self
                .dir
                .read_around(QUOTE_READ_BYTES, |log| {
                    log.iter()
                        .any(|entry| entry.id().is_some_and(|at| at <= id))
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let text = match explicit {
            true => text,
            false => match chat::quoted_message(&log, to) {
                Some(id) => answering(&text, Some(&Answers::Message(id))),
                None => return (text, None),
            },
        };
        let (text, quote, note) = resolve_quote(&text, &log, by_owner);
        if let Some(note) = note.filter(|_| explicit) {
            self.say(&note);
        }
        (text, quote)
    }

    /// The manager writes the overview with its own tools: after a turn, a file over the cap
    /// gets one follow-up turn, and no second one until the owner writes again.
    /// ref: docs/tasks.md "Project overview"
    fn check_overview(&mut self) {
        if !self.dir.is_home() {
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
            if let Err(error) = self.accept(
                Author::Recap,
                overview_too_long(lines, chars),
                Vec::new(),
                None,
            ) {
                self.say(&format!("cannot ask for a shorter overview: {error:#}"));
            }
        }
    }

    /// A hand edit of `decisions.md` or `context.md` (the records do not hold it) is said once;
    /// the next write of a record keeps the edited file as `<file>.drift` and renders again.
    /// ref: docs/tasks.md "Notes budget"
    fn check_views(&mut self) {
        let drift = super::repo::memory_store(&self.dir.project_id, &self.project, &self.main)
            .ok()
            .and_then(|store| wsx_core::memory::manage::drift(&store).ok())
            .unwrap_or_default();
        if drift.is_empty() {
            self.drift_said = false;
        } else if !self.drift_said {
            self.drift_said = true;
            for d in &drift {
                self.say(&d.notice(false));
            }
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
        // ^ The fresh conversation has no context yet: a turn of it that never answers (a limit,
        // a failed sign-in) must not count as the old size, or the idle rule rotates it again.
        self.state.context_tokens = 0;
        self.save();
        self.log(ManagerEvent::Rotated { tokens, reason });
    }

    /// The owner's decisions and the project context, within 10 KiB, with what is left out
    /// named: every fresh conversation starts knowing them. ref: docs/tasks.md "Notes budget"
    fn notes_preamble(&self) -> String {
        let own = self
            .dir
            .notes()
            .map_or_else(|_| String::new(), |notes| notes::preamble(&notes));
        // ^ A new chat has no notes yet: it starts from the repo's, the home chat's.
        match (own.is_empty(), self.dir.is_home()) {
            (true, false) => ManagerDir::home(&self.dir.project_id)
                .ok()
                .flatten()
                .map_or(own, |home| notes::preamble(&home.path.join("notes"))),
            _ => own,
        }
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
        let routines = handover_routines(super::repo::finished_routine_runs(&self.main), replied);
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

    /// The handover as an episode of the project's memory, in the turn that carries it: no turn
    /// of its own. A chat's notes stay files. ref: docs/memory.md "Capture"
    fn remember_handover(&self, handover: &str) {
        if !self.dir.is_home() {
            return;
        }
        let done = super::repo::memory_store(&self.dir.project_id, &self.project, &self.main)
            .and_then(|store| {
                Ok(wsx_core::memory::manage::add_episode(
                    &store.without_rules(),
                    "handover",
                    &format!("path:chats/{}/log.jsonl", self.dir.name()),
                    handover,
                    &self.dir.path.join(wsx_core::task::store::LOG_FILE),
                )?)
            });
        if let Err(error) = done {
            self.say(&format!("cannot remember the handover: {error:#}"));
        }
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

    /// Count a turn's cost on its account and say what that did (a budget reached, a failure).
    fn spend(&self, adapter: &str, model: &str, micro_usd: u64, now_ms: u64) {
        if let Some(message) = agent::note_spend(adapter, model, micro_usd, now_ms) {
            self.say(&message);
        }
    }

    fn report(&self, state: runtime::AgentState) {
        if let Some(pane) = self.pane {
            if let Some(problem) = hosting::report_pane(pane, "manager", state, true) {
                self.say(&problem);
            }
        }
    }
}

/// Whether an events line is a routine run's event for the owner (`routine_event`): the manager
/// answers its turn with a message, not a quiet line. An output line never matches, since
/// `routine_event` holds the output in one line.
fn routine_for_owner(line: &str) -> bool {
    line.starts_with("routine ") && line.split("; ").nth(1) == Some(super::repo::FOR_OWNER)
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
            _ if failed == 0 => newest.result_line(),
            Some(at) => format!(
                "{}; {failed} failure{plural} since its last success at {}",
                newest.result_line(),
                clock(mine[at].finished_epoch.unwrap_or_default())
            ),
            None => format!(
                "{}; {failed} failure{plural}, never succeeded",
                newest.result_line()
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
    wsx_core::routine::local::local_stamp(secs.max(0)).unwrap_or_else(|| secs.to_string())
}

/// An event line the chat shows as its first clause (`Short::plain`).
pub(super) fn plain_event(line: String) -> (String, Short) {
    let short = Short::plain(&line);
    (line, short)
}

/// `(n, state, k)` of a `#<n> <state> r<k>: ...` line `watch_tasks` wrote; none for a stall
/// line (`stalled` is no state) or another project's line (headed by its name).
fn event_head(line: &str) -> Option<(u32, (String, u32))> {
    let (head, _) = line.strip_prefix('#')?.split_once(':')?;
    let mut parts = head.split(' ');
    let id = parts.next()?.parse().ok()?;
    let word = parts.next()?;
    let revision = parts.next()?.strip_prefix('r')?.parse().ok()?;
    (parts.next().is_none() && word != "stalled").then(|| (id, (word.to_string(), revision)))
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

/// The open tasks of `records`: how many, and "2 working, 1 ready" by state word.
fn open_counts<'a>(records: impl IntoIterator<Item = &'a TaskRecord>) -> (usize, String) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for record in records.into_iter().filter(|record| record.state.is_open()) {
        *counts.entry(status::state_word(record)).or_default() += 1;
    }
    let states = counts
        .iter()
        .map(|(state, count)| format!("{count} {state}"))
        .collect::<Vec<_>>()
        .join(", ");
    (counts.values().sum(), states)
}
