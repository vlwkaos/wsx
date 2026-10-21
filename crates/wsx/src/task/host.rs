//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

use super::broker;
use super::claude::{new_session_id, AgentLine, ClaudeAgent, Launch, TurnResult};
use super::hosting;
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::git::{git_cmd, output_with_timeout_limit};
use wsx_core::model::workspace::LandPolicy;
use wsx_core::runtime;
use wsx_core::task::{
    brief::{self, Brief, BriefFile},
    checks,
    copy::{self, Rebase},
    now_unix_ms, report,
    store::{self, TaskDir, BRIEF_FILE, BRIEF_TEXT_FILE, REPORT_FILE, RESULT_FILE},
    view, Access, Actor, CheckResult, HostRequest, HostResponse, LogEvent, MessageKind, Phase,
    TaskRecord, TaskState, WorkerResult, MAX_MESSAGE_BYTES,
};

const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
const CHECK_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
/// The turn a host hands to its replacement: queued messages joined, so a few messages.
const HANDOFF_FILE: &str = "handoff.txt";
const MAX_HANDOFF_BYTES: u64 = 4 * 1024 * 1024;
/// Turns a failed check starts by itself before the task fails; an owner's message resets it.
const MAX_CHECK_RETRIES: u32 = 2;
/// The end of each failed check's output that goes back to the worker.
const CHECK_TAIL_BYTES: usize = 3 * 1024;

enum Event {
    Request(HostRequest, Sender<HostResponse>),
    /// `None` when the agent process of that generation ended.
    Agent(u64, Option<AgentLine>),
    ChecksDone(Vec<CheckResult>),
    /// The worker's `wsx task check-run` through the broker, and its results to log.
    CheckRun(Vec<String>, Sender<broker::Response>),
    CheckRunDone(Vec<CheckResult>),
}

/// What waits for free disk, resumed once `tasks.minFreeGb` is free again.
enum DiskWait {
    Turn(String),
    Checks,
}

/// How often a task that waits for disk looks again.
const DISK_POLL: Duration = Duration::from_secs(30);
/// What a check prints when the disk is full: the check then waits instead of failing.
const NO_SPACE: &str = "No space left on device";

struct Host {
    dir: TaskDir,
    record: TaskRecord,
    events: Sender<Event>,
    agent: Option<ClaudeAgent>,
    generation: u64,
    turn_active: bool,
    /// Adds sent during a turn or a check, delivered when it ends.
    pending: VecDeque<String>,
    /// A grant, a revoke or a phase change takes effect when the agent next starts.
    restart_needed: bool,
    stop_requested: bool,
    /// The result of the latest turn, until its checks end.
    last_result: Option<WorkerResult>,
    /// Tool calls denied in that turn.
    last_denials: Vec<String>,
    /// What the sandbox stopped in the running turn, and the hosts its commands reached.
    stops: Vec<Access>,
    reached: std::collections::BTreeSet<String>,
    /// What the latest blocked result asks for.
    last_asks: Vec<Access>,
    /// Turns started for failed checks since the last message from a person.
    check_retries: u32,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<hosting::Binary>,
    reexec: bool,
    /// A `land` waits for the checks of its rebased revision.
    landing: bool,
    disk_wait: Option<DiskWait>,
    /// The Human Verify items of the revision whose landing conflicted: the turn that
    /// resolves it rarely repeats them, and they still need a person.
    carried_verify: Vec<wsx_core::task::HumanVerify>,
    started: Instant,
}

pub fn run(project_id: &str, id: u32) -> Result<()> {
    let dir = TaskDir::open(project_id, id)?;
    let mut record = dir.read_record()?;
    if record.state == TaskState::Closed {
        bail!("task t{id} is closed");
    }
    let socket = dir.socket_path();
    let listener = hosting::bind(&socket)?;
    let broker_socket = dir.broker_path();
    let brokered = hosting::bind(&broker_socket)?;
    record.pane_id = hosting::own_pane();
    // ^ A restarted host still knows what the block asks for.
    let last_asks = view::read_result(&dir)
        .and_then(|result| serde_json::from_value(result["asks"].clone()).ok())
        .unwrap_or_default();
    let (events, receiver) = channel();
    let mut host = Host {
        dir,
        record,
        events: events.clone(),
        agent: None,
        generation: 0,
        turn_active: false,
        pending: VecDeque::new(),
        restart_needed: false,
        stop_requested: false,
        last_result: None,
        last_denials: Vec::new(),
        stops: Vec::new(),
        reached: Default::default(),
        last_asks,
        check_retries: 0,
        binary: hosting::Binary::capture(),
        reexec: false,
        landing: false,
        disk_wait: None,
        carried_verify: Vec::new(),
        started: Instant::now(),
    };
    let to_host = events.clone();
    std::thread::spawn(move || {
        broker::serve(brokered, move |request| {
            let (reply, answer) = channel();
            if to_host.send(Event::CheckRun(request.argv, reply)).is_err() {
                return broker::refused("the task host is closing".into());
            }
            answer
                .recv()
                .unwrap_or_else(|_| broker::refused("the task host is closing".into()))
        })
    });
    // ^ A pr landing pushes and calls gh, each bounded by 120 s.
    std::thread::spawn(move || {
        hosting::serve(
            listener,
            events,
            Event::Request,
            MAX_REQUEST_BYTES,
            Duration::from_secs(330),
        )
    });
    let result = host.run(receiver);
    if let Some(agent) = host.agent.take() {
        agent.stop();
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&broker_socket);
    if let (true, Some(binary)) = (host.reexec, &host.binary) {
        let error = binary.exec();
        host.say(&format!("cannot start the new wsx: {error}"));
        host.log(LogEvent::Stopped {
            reason: format!("cannot start the replaced wsx binary: {error}"),
        });
        host.set_state(TaskState::Stopped);
    }
    host.report_agent(false);
    result
}

impl Host {
    fn run(&mut self, receiver: Receiver<Event>) -> Result<()> {
        self.say(&format!(
            "task t{} on {} in {}",
            self.record.id,
            self.record.branch,
            self.record.working_copy.display()
        ));
        // ^ A host that exec'd its replaced binary starts the turn the old one handed over.
        if let Some(text) = self.take_handoff() {
            self.say("restarted with the replaced wsx binary");
            self.start_turn(&text);
        } else {
            self.resume_state()?;
        }
        let mut disk_checked = Instant::now();
        loop {
            // ^ On a deadline, not after a quiet spell: status calls arrive all the time.
            let event = match self.disk_wait {
                None => receiver.recv().ok(),
                Some(_) => {
                    let due = DISK_POLL.saturating_sub(disk_checked.elapsed());
                    match receiver.recv_timeout(due) {
                        Ok(event) => Some(event),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            disk_checked = Instant::now();
                            self.resume_after_disk();
                            continue;
                        }
                        Err(_) => None,
                    }
                }
            };
            let Some(event) = event else { break };
            match event {
                Event::Request(request, reply) => {
                    let close = matches!(request, HostRequest::Close { .. });
                    let response = self.handle(request);
                    let _ = reply.send(response);
                    if close {
                        // ^ Give the socket thread time to write the answer.
                        std::thread::sleep(Duration::from_millis(200));
                        return Ok(());
                    }
                }
                Event::Agent(generation, line) if generation == self.generation => {
                    self.on_agent(line)
                }
                Event::Agent(..) => {}
                Event::ChecksDone(results) => self.on_checks(results),
                Event::CheckRun(argv, reply) => self.check_run(&argv, reply),
                Event::CheckRunDone(results) => self.log(LogEvent::CheckRun { results }),
            }
            if self.reexec {
                std::thread::sleep(Duration::from_millis(200));
                return Ok(());
            }
        }
        Ok(())
    }

    fn take_handoff(&self) -> Option<String> {
        let bytes = self
            .dir
            .read_file(HANDOFF_FILE, MAX_HANDOFF_BYTES)
            .ok()
            .flatten()?;
        let _ = std::fs::remove_file(self.dir.file(HANDOFF_FILE));
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn resume_state(&mut self) -> Result<()> {
        match self.record.state {
            TaskState::Starting => {
                let prompt = self.dir.prompt()?.context("the task log has no prompt")?;
                match self.record.phase {
                    Phase::Brief => self.start_turn(&brief_request(&prompt)),
                    Phase::Build => self.start_turn(&prompt),
                }
            }
            TaskState::Working | TaskState::Checking => {
                // ^ A restarted pane cannot rejoin a turn of the process it lost.
                self.log(LogEvent::Stopped {
                    reason: "the host restarted during a turn".into(),
                });
                self.set_state(TaskState::Stopped);
            }
            state => {
                self.set_state(state);
            }
        }
        Ok(())
    }

    fn handle(&mut self, request: HostRequest) -> HostResponse {
        // ^ While the owner works by hand nothing else writes the copy or its conversation.
        let held = self.record.state == TaskState::ByHand
            && matches!(
                request,
                HostRequest::Steer { .. }
                    | HostRequest::Add { .. }
                    | HostRequest::Start
                    | HostRequest::Land {}
            );
        let message = match request {
            _ if held => Err(self.by_hand_refusal()),
            HostRequest::Status => Ok("".to_string()),
            HostRequest::Steer { text } => self.steer(text),
            HostRequest::Add { text } => self.add(text),
            HostRequest::Grant {
                access,
                project,
                by,
            } => self.grant(access, project, by),
            HostRequest::Revoke {
                access,
                project,
                by,
            } => self.revoke(access, project, by),
            HostRequest::Stop => self.stop(),
            HostRequest::Start => self.start(false),
            HostRequest::Land {} => self.land(false),
            HostRequest::Hand => self.hand(),
            HostRequest::Handback => self.handback(),
            HostRequest::Close { discard } => {
                if let Some(agent) = self.agent.take() {
                    agent.stop();
                }
                self.turn_active = false;
                self.log(LogEvent::Closed { discarded: discard });
                // ^ A landed task keeps `landed`: its work is on the default branch.
                if self.record.state != TaskState::Landed || discard {
                    self.set_state(TaskState::Closed);
                }
                Ok("closed".into())
            }
        };
        let (ok, message) = match message {
            Ok(message) => (true, message),
            Err(error) => (false, format!("{error:#}")),
        };
        HostResponse {
            ok,
            state: self.record.state,
            message,
        }
    }

    fn steer(&mut self, text: String) -> Result<String> {
        check_message(&text)?;
        if !self.turn_active {
            self.log(LogEvent::Message {
                mode: MessageKind::Add,
                text: text.clone(),
            });
            self.deliver_or_queue(text);
            return Ok("no turn was running, so this was sent as an add".into());
        }
        let agent = self.agent.as_mut().context("the agent is not running")?;
        agent.send(&text)?;
        self.say(&format!("steer: {}", first_line(&text)));
        self.log(LogEvent::Message {
            mode: MessageKind::Steer,
            text,
        });
        Ok("steered the running turn".into())
    }

    fn add(&mut self, text: String) -> Result<String> {
        check_message(&text)?;
        self.log(LogEvent::Message {
            mode: MessageKind::Add,
            text: text.clone(),
        });
        Ok(self.deliver_or_queue(text))
    }

    fn deliver_or_queue(&mut self, text: String) -> String {
        self.check_retries = 0;
        // ^ Before the build, an add is a talk-through: the worker answers and may revise the brief.
        let text = match self.record.phase {
            Phase::Brief => talk_request(&text),
            Phase::Build => text,
        };
        if self.turn_active || self.record.state == TaskState::Checking {
            self.pending.push_back(text);
            "queued until the current turn ends".into()
        } else {
            self.start_turn(&text);
            "started a new turn".into()
        }
    }

    /// Add a grant; a blocked task resumes once every ask of its block is granted. A
    /// project grant is already in the project's file (`owner::prepare_request`), so only
    /// the log names it here.
    fn grant(&mut self, access: Access, project: bool, by: Actor) -> Result<String> {
        access.check().map_err(anyhow::Error::msg)?;
        if !project && !self.record.grants.contains(&access) {
            self.record.grants.push(access.clone());
        }
        self.log(LogEvent::Granted {
            access: access.clone(),
            project,
            by,
        });
        self.restart_needed = true;
        let what = access.describe();
        let project_grants = store::read_grants(&self.record.project_id).unwrap_or_default();
        let open: Vec<String> = self
            .last_asks
            .iter()
            .filter(|ask| !self.record.grants.contains(ask) && !project_grants.contains(ask))
            .map(Access::describe)
            .collect();
        let blocked = self.record.state == TaskState::Blocked && !self.turn_active;
        if blocked && !open.is_empty() {
            self.save();
            return Ok(format!("may now {what}; still asks to {}", open.join(", ")));
        }
        if blocked {
            self.check_retries = 0;
            let who = match by {
                Actor::Owner => "The owner",
                Actor::Manager => "The manager",
            };
            self.start_turn(&format!(
                "{who} allowed you to {what}. Retry what was stopped and continue the task."
            ));
            return Ok(format!("may now {what}; resumed the task"));
        }
        self.save();
        Ok(format!("may now {what} from the next turn"))
    }

    fn revoke(&mut self, access: Access, project: bool, by: Actor) -> Result<String> {
        self.record.grants.retain(|kept| *kept != access);
        self.log(LogEvent::Revoked {
            access: access.clone(),
            project,
            by,
        });
        self.restart_needed = true;
        self.save();
        Ok(format!(
            "may no longer {} from the next turn",
            access.describe()
        ))
    }

    /// A refused write as the folder to grant: `~/.cargo` for a hidden folder in home,
    /// otherwise the parent folder, or the file itself when its parent is home or the root.
    /// None for the working copy and the main repository's git folder, which are never granted.
    fn write_ask(&self, printed: &str) -> Option<Access> {
        let path = normalize(&self.record.working_copy.join(printed));
        if path.starts_with(&self.record.working_copy)
            || path.starts_with(self.record.project_path.join(".git"))
        {
            return None;
        }
        let home = dirs::home_dir();
        let shown = match home
            .as_deref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rest) => {
                let first = rest
                    .components()
                    .next()?
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned();
                match rest.parent() {
                    _ if first.starts_with('.') => format!("~/{first}"),
                    Some(parent) if parent.as_os_str().is_empty() => {
                        format!("~/{}", rest.display())
                    }
                    Some(parent) => format!("~/{}", parent.display()),
                    None => return None,
                }
            }
            None => match path.parent() {
                Some(parent) if parent.parent().is_some() => parent.display().to_string(),
                _ => path.display().to_string(),
            },
        };
        let access = Access::Write { path: shown };
        access.check().is_ok().then_some(access)
    }

    /// What the turn's stops ask for: hosts no command reached and write folders, once each.
    fn turn_asks(&self, denied_writes: &[String]) -> Vec<Access> {
        let mut asks: Vec<Access> = Vec::new();
        let written = denied_writes.iter().filter_map(|path| self.write_ask(path));
        for ask in self.stops.iter().cloned().chain(written) {
            let reached = matches!(&ask, Access::Network { host } if self.reached.contains(host));
            if !reached && !asks.contains(&ask) {
                asks.push(ask);
            }
        }
        asks
    }

    fn stop(&mut self) -> Result<String> {
        let Some(agent) = self.agent.take() else {
            bail!("the agent is not running");
        };
        self.stop_requested = true;
        agent.stop();
        self.turn_active = false;
        let dropped = self.pending.len();
        self.pending.clear();
        self.log(LogEvent::Stopped {
            reason: "stopped by request".into(),
        });
        self.set_state(TaskState::Stopped);
        Ok(if dropped == 0 {
            "stopped; the next add resumes the same conversation".into()
        } else {
            format!("stopped and dropped {dropped} queued add(s); the next add resumes")
        })
    }

    fn by_hand_refusal(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "the owner works on task t{} by hand; run `wsx task handback {}` first",
            self.record.id,
            self.record.id
        )
    }

    /// The owner takes the task over in its own working copy: the running turn stops, queued
    /// messages are dropped, and no turn, check or landing starts until `handback`.
    /// ref: docs/tasks.md "By hand"
    fn hand(&mut self) -> Result<String> {
        let state = self.record.state;
        if self.record.phase != Phase::Build
            || !state.is_open()
            || matches!(
                state,
                TaskState::Starting | TaskState::Briefed | TaskState::Checking | TaskState::ByHand
            )
        {
            bail!(
                "task t{} is {}{}; a build that is not starting or checking is worked by hand",
                self.record.id,
                state.as_str(),
                if self.record.phase == Phase::Brief {
                    " in its brief"
                } else {
                    ""
                }
            );
        }
        if let Some(agent) = self.agent.take() {
            self.stop_requested = true;
            agent.stop();
        }
        self.turn_active = false;
        let dropped = self.pending.len() as u32;
        self.pending.clear();
        self.disk_wait = None;
        self.log(LogEvent::Hand {
            by: Actor::Owner,
            dropped,
        });
        self.set_waiting(TaskState::ByHand, None);
        Ok(match dropped {
            0 => "the owner works on it by hand".into(),
            n => format!("the owner works on it by hand; dropped {n} queued message(s)"),
        })
    }

    /// The owner's commits become the next revision: its checks run and the task becomes
    /// ready or failed as after a worker result, so auto land and the close after it apply.
    fn handback(&mut self) -> Result<String> {
        if self.record.state != TaskState::ByHand {
            bail!(
                "task t{} is {}; only a task worked by hand is handed back",
                self.record.id,
                self.record.state.as_str()
            );
        }
        let changes = copy::tracked_changes(&self.record.working_copy)?;
        if !changes.is_empty() {
            bail!(
                "the working copy has uncommitted changes: {}; commit them, then run `wsx task handback {}` again",
                changes.join(", "),
                self.record.id
            );
        }
        let commits = self.commits_since_revision()?;
        self.log(LogEvent::Handback {
            by: Actor::Owner,
            commits,
        });
        // ^ The worker's Human Verify items and memo still describe the work.
        let previous = self.current_result();
        let result = WorkerResult {
            status: "done".into(),
            summary: format!(
                "The owner worked on the task by hand: {commits} commit(s) after r{}.",
                self.record.revision
            ),
            human_verify: previous.human_verify,
            memo: previous.memo,
            ..WorkerResult::default()
        };
        self.last_denials.clear();
        self.last_asks.clear();
        self.write_result(&result, &[]);
        self.last_result = Some(result);
        // ^ A failed check of the owner's work goes back to the owner, not to the worker.
        self.check_retries = MAX_CHECK_RETRIES;
        self.record.state = TaskState::Working;
        self.start_checks();
        Ok(format!(
            "handed back with {commits} commit(s); the checks run on the next revision"
        ))
    }

    /// Commits on the branch after the last revision (after the base before the first).
    fn commits_since_revision(&self) -> Result<u32> {
        let since = match self.record.revision {
            0 => self.record.base.clone(),
            revision => format!("refs/wsx/task/{}/r{revision}", self.record.id),
        };
        let output = git_cmd(&self.record.working_copy)
            .args(["rev-list", "--count", &format!("{since}..HEAD")])
            .output()
            .context("cannot run git rev-list")?;
        if !output.status.success() {
            bail!("git rev-list {since}..HEAD failed");
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0))
    }

    /// Build the latest brief's rewritten prompt. The agent restarts with the build's
    /// tools and result schema and resumes the conversation, so it keeps what it read.
    /// `auto`: the brief has no open questions, so the host starts it (docs/tasks.md "Brief").
    fn start(&mut self, auto: bool) -> Result<String> {
        if self.record.phase != Phase::Brief || self.record.state != TaskState::Briefed {
            bail!(
                "task t{} is {}; only a briefed task starts",
                self.record.id,
                self.record.state.as_str()
            );
        }
        let file = brief::read(&self.dir).context("the task has no readable brief")?;
        let prompt = file.brief.rewritten_prompt.trim().to_string();
        self.check_retries = 0;
        self.record.phase = Phase::Build;
        self.restart_needed = true;
        self.log(LogEvent::Started {
            version: file.version,
            prompt: prompt.clone(),
            auto,
        });
        self.say(&format!("build of brief v{} started", file.version));
        self.start_turn(&build_request(file.version, &prompt));
        Ok(format!("started the build of brief v{}", file.version))
    }

    fn start_turn(&mut self, text: &str) {
        // ^ Every path that starts a turn ends here: none runs while the owner works by hand.
        if self.record.state == TaskState::ByHand {
            self.say("by hand: no agent turn starts until handback");
            return;
        }
        if let Err(error) = self.try_start_turn(text) {
            self.say(&format!("cannot start the agent: {error:#}"));
            self.log(LogEvent::Stopped {
                reason: format!("cannot start the agent: {error:#}"),
            });
            self.set_state(TaskState::Failed);
        }
    }

    fn try_start_turn(&mut self, text: &str) -> Result<()> {
        if self.binary.as_ref().is_some_and(hosting::Binary::replaced) {
            // ^ Every turn gets the replaced binary's launch: this host hands the turn over
            // and execs it once the caller has its answer; the new host resumes the agent.
            self.dir.write_file(HANDOFF_FILE, text.as_bytes())?;
            self.reexec = true;
            if self.record.state != TaskState::Conflict {
                self.set_state(TaskState::Working);
            }
            self.say("the wsx binary was replaced: the host restarts with it");
            return Ok(());
        }
        if self.wait_for_disk(DiskWait::Turn(text.to_string())) {
            return Ok(());
        }
        if self.restart_needed {
            if let Some(agent) = self.agent.take() {
                agent.stop();
            }
            self.restart_needed = false;
        }
        if self.agent.is_none() {
            let (session, resume) = match &self.record.agent_session {
                Some(session) => (session.clone(), true),
                None => (new_session_id()?, false),
            };
            self.generation += 1;
            let generation = self.generation;
            self.stop_requested = false;
            let project_grants = store::read_grants(&self.record.project_id)?;
            self.agent = Some(ClaudeAgent::spawn(
                &Launch::worker(
                    &self.record,
                    &project_grants,
                    &hosting::real_socket_path(&self.dir.broker_path()),
                )?,
                &session,
                resume,
                self.events.clone(),
                move |line| Event::Agent(generation, line),
            )?);
        }
        self.agent
            .as_mut()
            .context("the agent is not running")?
            .send(text)?;
        self.turn_active = true;
        self.stops.clear();
        self.reached.clear();
        // ^ A conflict turn shows as `conflict` until its result, so lists say why it runs.
        if self.record.state != TaskState::Conflict {
            self.set_state(TaskState::Working);
        }
        Ok(())
    }

    fn on_agent(&mut self, line: Option<AgentLine>) {
        match line {
            Some(AgentLine::Init(session)) => {
                if self.record.agent_session.as_deref() != Some(&session) {
                    self.record.agent_session = Some(session);
                    self.save();
                }
            }
            Some(AgentLine::Progress(text)) => {
                self.say(&text);
                self.log(LogEvent::Agent { text });
            }
            Some(AgentLine::Result(result)) => self.on_result(result),
            Some(AgentLine::Stopped(access)) => {
                let access = match access {
                    Access::Write { path } => self.write_ask(&path),
                    other => Some(other),
                };
                if let Some(access) = access.filter(|access| !self.stops.contains(access)) {
                    self.say(&format!("sandbox stopped: {}", access.describe()));
                    self.stops.push(access);
                }
            }
            Some(AgentLine::Reached(hosts)) => self.reached.extend(hosts),
            None => {
                self.agent = None;
                if self.turn_active && !self.stop_requested {
                    self.turn_active = false;
                    self.log(LogEvent::Stopped {
                        reason: "the agent process ended during a turn".into(),
                    });
                    self.set_state(TaskState::Failed);
                }
            }
        }
    }

    fn on_result(&mut self, turn: TurnResult) {
        self.turn_active = false;
        if self.record.phase == Phase::Brief {
            self.on_brief(turn);
            return;
        }
        let mut result = turn
            .structured
            .and_then(|value| serde_json::from_value::<WorkerResult>(value).ok())
            .unwrap_or_else(|| WorkerResult {
                status: "failed".into(),
                summary: if turn.text.is_empty() {
                    format!("the agent ended with {} and no result", turn.subtype)
                } else {
                    turn.text.clone()
                },
                ..WorkerResult::default()
            });
        if turn.is_error && result.status == "done" {
            result.status = "failed".into();
        }
        self.log(LogEvent::Result {
            status: result.status.clone(),
            summary: result.summary.clone(),
            usage: Some(turn.usage.clone()),
            denials: turn.denials.clone(),
        });
        self.say(&format!(
            "result {}: {}",
            result.status,
            first_line(&result.summary)
        ));
        self.last_denials = turn.denials.clone();
        self.last_asks = if result.status == "blocked" {
            self.turn_asks(&turn.denied_writes)
        } else {
            Vec::new()
        };
        self.write_result(&result, &[]);
        self.last_result = Some(result.clone());
        // ^ Only the worker's own word blocks a task. Denials it worked around are evidence.
        if result.status == "blocked" {
            self.log(LogEvent::Blocked {
                denials: turn.denials,
                asks: self.last_asks.clone(),
            });
            self.set_state(TaskState::Blocked);
            // ^ A worker stopped by a full disk continues once there is space again.
            self.wait_for_disk(DiskWait::Turn(
                "The disk was full; there is space again. Continue the task where it stopped."
                    .into(),
            ));
            return;
        }
        self.start_checks();
    }

    /// A brief turn ended: save the brief and wait for the owner, or fail with the reason.
    /// Edits the worker tried are denied and logged with the brief.
    fn on_brief(&mut self, turn: TurnResult) {
        let saved = match turn.structured {
            Some(value) => serde_json::from_value::<Brief>(value)
                .map_err(|error| format!("the brief does not parse: {error}"))
                .and_then(|brief| brief.check().map(|()| brief))
                .and_then(|brief| self.save_brief(brief).map_err(|error| format!("{error:#}"))),
            None if turn.text.is_empty() => Err(format!(
                "the agent ended with {} and no brief",
                turn.subtype
            )),
            None => Err(turn.text),
        };
        match saved {
            Ok((version, answer, questions)) => {
                self.say(&format!("brief v{version} ready"));
                self.log(LogEvent::Brief {
                    version,
                    answer,
                    usage: Some(turn.usage),
                    denials: turn.denials,
                });
                self.set_state(TaskState::Briefed);
                // ^ Nothing to ask: build at once. A message that waits is a talk-through
                // the worker answers first.
                if questions == 0 && self.pending.is_empty() {
                    if let Err(error) = self.start(true) {
                        self.say(&format!("cannot start the build: {error:#}"));
                    }
                }
            }
            Err(reason) => {
                self.say(&format!("no brief: {}", first_line(&reason)));
                self.log(LogEvent::Result {
                    status: "failed".into(),
                    summary: reason,
                    usage: Some(turn.usage),
                    denials: turn.denials,
                });
                self.set_state(TaskState::Failed);
            }
        }
        self.deliver_pending();
    }

    /// Write `brief.json` and `brief.md` as the next version; returns it, the answer and
    /// the number of open questions.
    fn save_brief(&self, brief: Brief) -> Result<(u32, String, usize)> {
        let version = brief::read(&self.dir).map_or(1, |file| file.version + 1);
        let answer = brief.answer.trim().to_string();
        let questions = brief
            .open_questions
            .iter()
            .filter(|question| !question.trim().is_empty())
            .count();
        let file = BriefFile { version, brief };
        let bytes = serde_json::to_vec_pretty(&file)?;
        if bytes.len() > brief::MAX_BRIEF_BYTES {
            bail!("the brief is larger than {} bytes", brief::MAX_BRIEF_BYTES);
        }
        let title = view::title(&self.dir, &self.record).unwrap_or_default();
        self.dir.write_file(BRIEF_FILE, &bytes)?;
        self.dir.write_file(
            BRIEF_TEXT_FILE,
            brief::render(self.record.id, &title, &file).as_bytes(),
        )?;
        Ok((version, answer, questions))
    }

    /// Hold `what` while less than `tasks.minFreeGb` is free: the task shows the reason and
    /// resumes by itself, without spending a check retry. True when it waits.
    /// ref: docs/tasks.md "Checks, revisions and report"
    fn wait_for_disk(&mut self, what: DiskWait) -> bool {
        let Some((free_mb, min_gb)) = self.low_disk() else {
            return false;
        };
        self.disk_wait = Some(match (self.disk_wait.take(), what) {
            (Some(DiskWait::Turn(waiting)), DiskWait::Turn(text)) => {
                DiskWait::Turn(format!("{waiting}\n\n{text}"))
            }
            (Some(DiskWait::Turn(waiting)), DiskWait::Checks) => DiskWait::Turn(waiting),
            (_, what) => what,
        });
        self.log(LogEvent::DiskWait { free_mb, min_gb });
        let reason = disk_full(free_mb, min_gb);
        self.say(&reason);
        self.set_waiting(TaskState::Blocked, Some(reason));
        true
    }

    fn resume_after_disk(&mut self) {
        if self.low_disk().is_some() {
            return;
        }
        let Some(what) = self.disk_wait.take() else {
            return;
        };
        self.set_waiting(self.record.state, None);
        match what {
            DiskWait::Turn(text) => self.start_turn(&text),
            DiskWait::Checks => self.start_checks(),
        }
    }

    /// Free MiB and the GB a task needs, when less than that is free.
    fn low_disk(&self) -> Option<(u64, u64)> {
        let min_gb =
            load_task_config(&self.record.project_path).map_or(5, |settings| settings.min_free_gb);
        hosting::free_mb(&self.record.working_copy)
            .filter(|free_mb| *free_mb < min_gb * 1024)
            .map(|free_mb| (free_mb, min_gb))
    }

    fn set_waiting(&mut self, state: TaskState, waiting: Option<String>) {
        self.record.waiting = waiting;
        self.set_state(state);
    }

    /// `wsx task check-run [<n>...]` from the worker: the declared checks (all, or by number)
    /// run as after a result, in a thread, and answer the worker; no revision is recorded.
    fn check_run(&mut self, argv: &[String], reply: Sender<broker::Response>) {
        let refuse = |message: String| {
            let _ = reply.send(broker::refused(message));
        };
        let numbers = match argv {
            [task, sub, numbers @ ..] if task == "task" && sub == "check-run" => numbers,
            _ => return refuse("a worker may run only `wsx task check-run [<n>...]`".into()),
        };
        if self.record.phase != Phase::Build {
            return refuse("checks run once the build has started".into());
        }
        if self.record.state == TaskState::ByHand {
            return refuse(format!("{:#}", self.by_hand_refusal()));
        }
        let checks = &self.record.checks;
        let picked: Result<Vec<(usize, String)>, String> = match numbers {
            [] => Ok(checks.iter().cloned().enumerate().collect()),
            numbers => numbers
                .iter()
                .map(|number| {
                    number
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=checks.len()).contains(n))
                        .map(|n| (n - 1, checks[n - 1].clone()))
                        .ok_or_else(|| {
                            format!("{number} is not a check number 1..={}", checks.len())
                        })
                })
                .collect(),
        };
        let picked = match picked {
            Ok(picked) if picked.is_empty() => {
                return refuse("the project declares no checks".into())
            }
            Ok(picked) => picked,
            Err(message) => return refuse(message),
        };
        if let Some((free_mb, min_gb)) = self.low_disk() {
            return refuse(disk_full(free_mb, min_gb));
        }
        let timeout = Duration::from_secs(self.record.check_timeout_secs);
        let working_copy = self.record.working_copy.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let results: Vec<CheckResult> = picked
                .iter()
                .map(|(_, command)| run_check(&working_copy, command, timeout))
                .collect();
            let mut stdout = String::new();
            for ((index, _), check) in picked.iter().zip(&results) {
                let ended = match (check.timed_out, check.exit) {
                    (true, _) => "timed out".to_string(),
                    (false, Some(code)) => format!("exit {code}"),
                    (false, None) => "no exit code".to_string(),
                };
                stdout.push_str(&format!(
                    "check {} `{}`: {ended}\n",
                    index + 1,
                    check.command
                ));
                if check.exit != Some(0) {
                    stdout.push_str(&check.output);
                    if !check.output.ends_with('\n') {
                        stdout.push('\n');
                    }
                }
            }
            let code = if results.iter().all(|check| check.exit == Some(0)) {
                0
            } else {
                1
            };
            let _ = reply.send(broker::Response {
                stdout,
                stderr: String::new(),
                code,
            });
            let _ = events.send(Event::CheckRunDone(results));
        });
    }

    fn start_checks(&mut self) {
        if self.record.state == TaskState::ByHand {
            return;
        }
        if self.wait_for_disk(DiskWait::Checks) {
            return;
        }
        self.set_state(TaskState::Checking);
        let checks = self.record.checks.clone();
        let timeout = Duration::from_secs(self.record.check_timeout_secs);
        let working_copy = self.record.working_copy.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let results = checks
                .iter()
                .map(|command| run_check(&working_copy, command, timeout))
                .collect();
            let _ = events.send(Event::ChecksDone(results));
        });
    }

    fn on_checks(&mut self, results: Vec<CheckResult>) {
        // ^ A full disk is not the worker's failure: the checks run again once there is space.
        if results.iter().any(|check| check.output.contains(NO_SPACE)) {
            self.log(LogEvent::Checks { results });
            self.wait_for_disk(DiskWait::Checks);
            if self.disk_wait.is_none() {
                self.disk_wait = Some(DiskWait::Checks);
                self.set_waiting(
                    TaskState::Blocked,
                    Some("Disk full: a check ran out of space".into()),
                );
            }
            return;
        }
        for check in &results {
            self.say(&format!(
                "check {}: {}",
                check.command,
                check
                    .exit
                    .map_or("no exit code".into(), |code| format!("exit {code}"))
            ));
        }
        self.log(LogEvent::Checks {
            results: results.clone(),
        });
        let result = self.last_result.clone().unwrap_or_default();
        let passed = result.status == "done" && results.iter().all(|check| check.exit == Some(0));
        match self.record_revision() {
            Ok((revision, commit)) => {
                let state = if passed {
                    TaskState::Ready
                } else {
                    TaskState::Failed
                };
                let landing = std::mem::take(&mut self.landing);
                // ^ Readers act on the state: the result and report must be complete before it.
                self.write_result(&result, &results);
                self.write_report(state);
                self.say(&format!("revision r{revision} at {commit:.12}"));
                // ^ A landing goes from checking straight to landed, or to ready with
                // land_stopped: readers never see a ready task that is still landing.
                let failed: Vec<&CheckResult> = results
                    .iter()
                    .filter(|check| check.exit != Some(0))
                    .collect();
                match landing {
                    // ^ A worker that says done but fails a check gets the failure back,
                    // bounded, before the task fails. ref: docs/tasks.md "Checks, revisions and report"
                    false
                        if state == TaskState::Failed
                            && result.status == "done"
                            && !failed.is_empty()
                            && self.pending.is_empty()
                            && self.check_retries < MAX_CHECK_RETRIES =>
                    {
                        self.check_retries += 1;
                        let attempt = self.check_retries;
                        self.log(LogEvent::CheckRetry {
                            revision,
                            attempt,
                            commands: failed.iter().map(|check| check.command.clone()).collect(),
                        });
                        self.say(&format!(
                            "checks failed on r{revision}: sent back to the worker ({attempt} of {MAX_CHECK_RETRIES})"
                        ));
                        let text = check_retry_request(&failed, attempt);
                        self.start_turn(&text);
                    }
                    // ^ A ready task lands at once when the project lands by itself and has
                    // checks to prove it; it goes from checking to landed without a stop at
                    // ready. A message that waits for the worker comes first: its answer is a
                    // new revision, which lands in turn. ref: docs/tasks.md "Landing"
                    false
                        if state == TaskState::Ready
                            && !results.is_empty()
                            && self.record.created_by == Actor::Owner
                            && self.pending.is_empty()
                            && self.auto_land() =>
                    {
                        let landed = self.land(true);
                        self.finish_landing(landed, state);
                    }
                    false => self.set_state(state),
                    true => {
                        let finished = if state == TaskState::Ready {
                            self.fast_forward()
                        } else {
                            Err(anyhow::anyhow!("checks failed on the rebased revision"))
                        };
                        self.finish_landing(finished, state);
                    }
                }
            }
            Err(error) => {
                self.say(&format!("cannot record a revision: {error:#}"));
                self.set_state(TaskState::Failed);
            }
        }
        self.deliver_pending();
    }

    /// Whether the project lands ready tasks by itself (`tasks.land.auto`).
    fn auto_land(&self) -> bool {
        load_task_config(&self.record.project_path).is_ok_and(|settings| settings.land_auto)
    }

    /// Say what a landing did, or log why it stopped and settle the task in `state`.
    fn finish_landing(&mut self, finished: Result<String>, state: TaskState) {
        match finished {
            Ok(message) => self.say(&message),
            Err(error) => {
                self.say(&format!("landing stopped: {error:#}"));
                self.log(LogEvent::LandStopped {
                    reason: format!("{error:#}"),
                });
                self.set_state(state);
            }
        }
    }

    /// Send the messages queued during the turn or the checks as one new turn.
    fn deliver_pending(&mut self) {
        if !self.pending.is_empty() {
            let text: Vec<String> = self.pending.drain(..).collect();
            self.start_turn(&text.join("\n\n"));
        }
    }

    /// Rewrite `report.md` for the current revision as it will read in `state`.
    fn write_report(&self, state: TaskState) {
        let log = self.dir.read_log().unwrap_or_default();
        let revision = self.record.revision;
        let Some(commit) = log.iter().rev().find_map(|entry| match &entry.event {
            LogEvent::Revision {
                revision: logged,
                commit,
                ..
            } if *logged == revision => Some(commit.clone()),
            _ => None,
        }) else {
            return;
        };
        let reported = TaskRecord {
            state,
            ..self.record.clone()
        };
        let text = report::render(&reported, &log, &self.current_result(), revision, &commit);
        if let Err(error) = self.dir.write_file(REPORT_FILE, text.as_bytes()) {
            self.say(&format!("cannot write the report: {error:#}"));
        }
    }

    /// Land the current revision on the default branch (docs/tasks.md "Landing").
    /// `auto`: the checks of this revision just passed, so the task is not yet `ready`.
    fn land(&mut self, auto: bool) -> Result<String> {
        let settings =
            load_task_config(&self.record.project_path).map_err(|error| anyhow::anyhow!(error))?;
        if settings.land_policy == LandPolicy::Pr {
            return self.land_pr(auto, &settings.land_remote);
        }
        self.require_ready(auto)?;
        self.require_committed()?;
        let working_copy = self.record.working_copy.clone();
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&working_copy, "HEAD")?;
        if copy::is_ancestor(&working_copy, &tip, &head)? {
            return self.fast_forward();
        }
        match copy::rebase(&working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.carried_verify = self.current_result().human_verify;
                self.log(LogEvent::Conflict {
                    onto: tip.clone(),
                    paths: paths.clone(),
                });
                self.set_state(TaskState::Conflict);
                self.start_turn(&format!(
                    "Landing needs branch {branch} rebased onto {tip} (the current {default}). \
                     Run `git rebase {tip}`, resolve the conflicts in {paths}, `git add` them and \
                     `git rebase --continue`, then report.",
                    branch = self.record.branch,
                    default = self.record.default_branch,
                    paths = paths.join(", "),
                ));
                Ok(format!(
                    "the rebase onto {} conflicted in {}; the worker is resolving it{}",
                    self.record.default_branch,
                    paths.join(", "),
                    if self.auto_land() {
                        ", and the task lands when its checks pass"
                    } else {
                        ". Land again when the task is ready"
                    }
                ))
            }
            Rebase::Done => {
                self.landing = true;
                self.start_checks();
                Ok(format!(
                    "rebased onto {} at {tip:.12}; checks run on the new revision and it lands when they pass",
                    self.record.default_branch
                ))
            }
        }
    }

    fn require_ready(&self, auto: bool) -> Result<()> {
        if !auto && self.record.state != TaskState::Ready {
            bail!(
                "task t{} is {}; only a ready task lands",
                self.record.id,
                self.record.state.as_str()
            );
        }
        Ok(())
    }

    fn require_committed(&self) -> Result<()> {
        let changes = copy::tracked_changes(&self.record.working_copy)?;
        if !changes.is_empty() {
            bail!(
                "the working copy has uncommitted changes: {}; ask the worker to commit them",
                changes.join(", ")
            );
        }
        Ok(())
    }

    /// `pr` policy: push and open a pull request, then on a later land read whether it merged.
    fn land_pr(&mut self, auto: bool, remote: &str) -> Result<String> {
        let Some(url) = self.record.pr_url.clone() else {
            self.require_ready(auto)?;
            self.require_committed()?;
            copy::push(&self.record.working_copy, remote, &self.record.branch)?;
            let title = view::title(&self.dir, &self.record)
                .unwrap_or_else(|| format!("Task t{}", self.record.id));
            let title: String = title.chars().take(PR_TITLE_CHARS).collect();
            let report_path = self.dir.file(REPORT_FILE);
            let created = run_gh(
                &self.record.working_copy,
                &[
                    "pr",
                    "create",
                    "--base",
                    &self.record.default_branch,
                    "--head",
                    &self.record.branch,
                    "--title",
                    &title,
                    "--body-file",
                    &report_path.to_string_lossy(),
                ],
            )?;
            let url = created
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| line.starts_with("https://") || line.starts_with("http://"))
                .with_context(|| {
                    format!("gh pr create printed no URL; the branch was pushed to {remote}")
                })?
                .to_string();
            return Ok(self.pushed(url, "opened"));
        };
        let ready = auto || self.record.state == TaskState::Ready;
        if !ready && self.record.state != TaskState::InReview {
            bail!(
                "task t{} is {}; land it again when it is ready or in review",
                self.record.id,
                self.record.state.as_str()
            );
        }
        let view = run_gh(
            &self.record.working_copy,
            &["pr", "view", &url, "--json", "state,mergeCommit"],
        )?;
        let view: serde_json::Value =
            serde_json::from_str(&view).context("gh pr view did not print JSON")?;
        match view["state"].as_str().unwrap_or_default() {
            "MERGED" => {
                let commit = view["mergeCommit"]["oid"]
                    .as_str()
                    .filter(|oid| !oid.is_empty())
                    .context("gh reports the PR merged but names no merge commit")?
                    .to_string();
                self.landed(&commit);
                Ok(format!("{url} merged as {commit:.12}; landed"))
            }
            "CLOSED" => {
                self.log(LogEvent::PrClosed { url: url.clone() });
                self.set_state(TaskState::InReview);
                Ok(format!(
                    "{url} was closed without a merge; reopen it, or close the task with --discard"
                ))
            }
            _ if ready && self.record.pr_revision != Some(self.record.revision) => {
                self.require_committed()?;
                copy::push(&self.record.working_copy, remote, &self.record.branch)?;
                Ok(self.pushed(url, "updated"))
            }
            state => Ok(format!("{url} is still {}", state.to_ascii_lowercase())),
        }
    }

    /// Record that the current revision is on the pull request at `url`.
    fn pushed(&mut self, url: String, verb: &str) -> String {
        let revision = self.record.revision;
        self.record.pr_url = Some(url.clone());
        self.record.pr_revision = Some(revision);
        self.log(LogEvent::InReview {
            url: url.clone(),
            revision,
        });
        self.set_state(TaskState::InReview);
        format!("{verb} {url} with r{revision}")
    }

    fn default_tip(&self) -> Result<String> {
        copy::resolve_commit(
            &self.record.project_path,
            &format!("refs/heads/{}", self.record.default_branch),
        )
    }

    fn fast_forward(&mut self) -> Result<String> {
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&self.record.working_copy, "HEAD")?;
        copy::fast_forward(
            &self.record.project_path,
            &self.record.default_branch,
            &tip,
            &head,
        )?;
        self.landed(&head);
        Ok(format!(
            "landed r{} on {} at {head:.12}",
            self.record.revision, self.record.default_branch
        ))
    }

    /// The revision is on the default branch: its Human Verify items join the project's
    /// "To check" list before readers see `landed` (docs/tasks.md "To check").
    fn landed(&mut self, commit: &str) {
        let mut verify = self.current_result().human_verify;
        for item in std::mem::take(&mut self.carried_verify) {
            if !verify.iter().any(|known| known.check == item.check) {
                verify.push(item);
            }
        }
        if let Err(error) = checks::collect(
            &self.record.project_id,
            self.record.id,
            self.record.revision,
            commit,
            &verify,
        ) {
            self.say(&format!("cannot add the items to check: {error:#}"));
        }
        self.log(LogEvent::Landed {
            commit: commit.to_string(),
        });
        self.write_report(TaskState::Landed);
        self.set_state(TaskState::Landed);
        self.after_landing();
    }

    /// Deploy the default branch when the project deploys by itself, then close this task:
    /// its work is on the default branch, and its copy and build only take space. Each runs
    /// as its own `wsx task` command that outlives this host.
    /// ref: docs/tasks.md "Deploy and release", "Close"
    fn after_landing(&self) {
        let project = self.record.project.as_str();
        let id = self.record.id.to_string();
        let deploys = load_task_config(&self.record.project_path)
            .is_ok_and(|settings| settings.deploy_auto && settings.deploy_command.is_some());
        let mut follow_ups = vec![vec!["task", "close", id.as_str(), "-p", project, "--json"]];
        if deploys {
            follow_ups.insert(
                0,
                vec!["task", "deploy", "-p", project, "--if-new", "--json"],
            );
        }
        for args in follow_ups {
            let started = store::follow_up_log(&self.record.project_id)
                .and_then(|log| hosting::spawn_wsx(&args, log));
            if let Err(error) = started {
                self.say(&format!("cannot start wsx {}: {error:#}", args.join(" ")));
            }
        }
    }

    /// The worker result of the current revision, from `result.json`.
    fn current_result(&self) -> WorkerResult {
        if let Some(result) = &self.last_result {
            return result.clone();
        }
        self.dir
            .read_file(RESULT_FILE, MAX_RESULT_BYTES as u64 * 2)
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| serde_json::from_value(value.get("result")?.clone()).ok())
            .unwrap_or_default()
    }

    fn record_revision(&mut self) -> Result<(u32, String)> {
        let working_copy = &self.record.working_copy;
        let head = git_cmd(working_copy)
            .args(["rev-parse", "HEAD"])
            .output()
            .context("cannot run git rev-parse")?;
        if !head.status.success() {
            bail!("git rev-parse HEAD failed in {}", working_copy.display());
        }
        let commit = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let revision = self.record.revision + 1;
        let reference = format!("refs/wsx/task/{}/r{revision}", self.record.id);
        let status = git_cmd(working_copy)
            .args(["update-ref", &reference, &commit])
            .status()
            .context("cannot run git update-ref")?;
        if !status.success() {
            bail!("git update-ref {reference} failed");
        }
        // ^ After a rebase the fork point moved: base..r<k> must name only the task's own work.
        if let Ok(base) = copy::merge_base(
            working_copy,
            &commit,
            &format!("refs/heads/{}", self.record.default_branch),
        ) {
            self.record.base = base;
        }
        let (paths, paths_total) = copy::changed_paths(working_copy, &self.record.base, &commit)?;
        self.record.revision = revision;
        self.record.paths = paths;
        self.record.paths_total = paths_total;
        self.log(LogEvent::Revision {
            revision,
            commit: commit.clone(),
            base: self.record.base.clone(),
            paths_total,
        });
        Ok((revision, commit))
    }

    fn write_result(&self, result: &WorkerResult, checks: &[CheckResult]) {
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "revision": self.record.revision,
            "result": result,
            "checks": checks,
            "denials": self.last_denials,
            "asks": self.last_asks,
        }))
        .unwrap_or_default();
        if bytes.len() > MAX_RESULT_BYTES {
            bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "revision": self.record.revision,
                "result": WorkerResult {
                    status: "failed".into(),
                    summary: format!("the worker result was larger than {MAX_RESULT_BYTES} bytes"),
                    ..WorkerResult::default()
                },
                "checks": checks,
            }))
            .unwrap_or_default();
        }
        if let Err(error) = self.dir.write_file(RESULT_FILE, &bytes) {
            self.say(&format!("cannot write the result: {error:#}"));
        }
    }

    fn set_state(&mut self, state: TaskState) {
        let changed = self.record.state != state;
        self.record.state = state;
        self.save();
        if changed {
            self.say(&format!("state {}", state.as_str()));
        }
        self.report_agent(state != TaskState::Closed);
    }

    fn save(&mut self) {
        self.record.updated_unix_ms = now_unix_ms();
        if let Err(error) = self.dir.write_record(&self.record) {
            self.say(&format!("cannot write the task record: {error:#}"));
        }
    }

    fn log(&self, event: LogEvent) {
        if let Err(error) = self.dir.append(event) {
            self.say(&format!("cannot append to the log: {error:#}"));
        }
    }

    fn say(&self, text: &str) {
        hosting::progress(self.started, text);
    }

    /// Show the task state in wsx lists through the existing agent report path.
    fn report_agent(&self, attached: bool) {
        let Some(pane_id) = self.record.pane_id else {
            return;
        };
        let state = match self.record.state {
            TaskState::Starting
            | TaskState::Working
            | TaskState::Checking
            | TaskState::Conflict => runtime::AgentState::Working,
            TaskState::Blocked | TaskState::Briefed => runtime::AgentState::Blocked,
            TaskState::Ready | TaskState::InReview | TaskState::Landed => runtime::AgentState::Done,
            TaskState::Failed => runtime::AgentState::Error,
            TaskState::Stopped | TaskState::ByHand | TaskState::Closed => runtime::AgentState::Idle,
        };
        if let Some(problem) = hosting::report_pane(pane_id, "task", state, attached) {
            self.say(&problem);
        }
    }
}

/// "Disk full: 3.2 GB free of the 5 GB a task needs".
fn disk_full(free_mb: u64, min_gb: u64) -> String {
    format!(
        "Disk full: {:.1} GB free of the {min_gb} GB a task needs",
        free_mb as f64 / 1024.0
    )
}

const PR_TITLE_CHARS: usize = 72;
const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// Run `gh` in `cwd` and return its stdout.
fn run_gh(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut command = std::process::Command::new("gh");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .env("GH_PROMPT_DISABLED", "1");
    let label = args[..2.min(args.len())].join(" ");
    let output = output_with_timeout_limit(&mut command, GH_TIMEOUT, 256 * 1024)
        .with_context(|| format!("gh {label} did not finish"))?;
    if !output.status.success() {
        bail!(
            "gh {label} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn check_message(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("the message is empty");
    }
    if text.len() > MAX_MESSAGE_BYTES {
        bail!("the message is larger than the limit of {MAX_MESSAGE_BYTES} bytes");
    }
    Ok(())
}

/// The first turn of a task with a brief: the owner's request and what the brief must answer.
/// ref: docs/tasks.md "Brief"
fn brief_request(prompt: &str) -> String {
    format!(
        "Before anything is built, write a brief for this task. Change no file: only read, \
         search and run read-only commands.\n\n\
         The owner asks:\n\n{prompt}\n\n\
         Answer as a top professional in this field would, for this repository:\n\
         1. additions: what a top professional would do here that the owner is not asking for, \
         and why it matters here.\n\
         2. raw_material: the rawest primary sources they would start from instead of what was \
         already digested: code paths, logs, specs, data, upstream docs. Give paths or links, not \
         summaries, and check that they exist.\n\
         3. methods: the specific methods and tools they would use, by name.\n\
         4. independent_check: what, other than you or the owner, would show the result is \
         right: a test, a real run, a measurement, a reference implementation, a standard.\n\
         5. concepts: each term or technique the owner is likely new to here, in two or three \
         plain sentences; none when there is none.\n\
         6. rewritten_prompt: the owner's request rewritten to ask for exactly that. It becomes \
         the build prompt, so it must be complete on its own.\n\
         7. open_questions: only the questions whose answer changes the work.\n\
         Leave answer empty. Write for the owner: short sentences and plain words."
    )
}

/// An owner's reply to a brief.
fn talk_request(text: &str) -> String {
    format!(
        "The owner replies to the brief:\n\n{text}\n\n\
         Answer the owner in `answer`: explain, and teach a concept when they ask about one. \
         Then return the whole brief again, revised where the reply changes it. Still change no file."
    )
}

/// The first build turn after `start`.
fn build_request(version: u32, prompt: &str) -> String {
    format!(
        "The owner approved brief v{version}. The build starts now; you may edit files. \
         Build this:\n\n{prompt}\n\n\
         Commit your changes with git before you finish, and return the result."
    )
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn run_check(working_copy: &Path, command: &str, timeout: Duration) -> CheckResult {
    let mut process = std::process::Command::new("/bin/sh");
    process
        .args(["-c", command])
        .current_dir(working_copy)
        .stdin(std::process::Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("WSX_") {
            process.env_remove(name);
        }
    }
    match output_with_timeout_limit(&mut process, timeout, CHECK_OUTPUT_BYTES) {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(stderr.trim_end());
            }
            CheckResult {
                command: command.into(),
                exit: output.status.code(),
                timed_out: false,
                output: text,
            }
        }
        Err(error) => CheckResult {
            command: command.into(),
            exit: None,
            timed_out: error.kind() == std::io::ErrorKind::TimedOut,
            output: error.to_string(),
        },
    }
}

/// `path` without `.` and `..` parts, without touching the disk.
fn normalize(path: &Path) -> std::path::PathBuf {
    let mut clean = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other),
        }
    }
    clean
}

/// The failed checks for the worker: each command, how it ended and the end of its output.
fn check_retry_request(failed: &[&CheckResult], attempt: u32) -> String {
    let mut text = format!(
        "The checks failed after your result (automatic retry {attempt} of {MAX_CHECK_RETRIES}). \
         Fix the cause, commit, and return your result again."
    );
    for check in failed {
        let ended = if check.timed_out {
            "timed out".to_string()
        } else {
            check
                .exit
                .map_or("ended without an exit code".into(), |code| {
                    format!("exit {code}")
                })
        };
        let output = check.output.trim_end();
        let mut start = output.len().saturating_sub(CHECK_TAIL_BYTES);
        while !output.is_char_boundary(start) {
            start += 1;
        }
        text.push_str(&format!(
            "\n\n`{}`: {ended}\n```\n{}\n```",
            check.command,
            &output[start..]
        ));
    }
    text
}
