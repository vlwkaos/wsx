//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

use super::broker;
use super::claude::{new_session_id, AgentLine, ClaudeAgent, Launch, TurnResult};
use super::hosting;
use super::slot::{self, Priority};
use super::trial;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::git::{git_cmd, output_with_timeout_tail};
use wsx_core::runtime;
use wsx_core::task::view::checked_files;
use wsx_core::task::{
    checks,
    copy::{self, Rebase},
    now_unix_ms, report,
    store::{self, TaskDir, REPORT_FILE, RESULT_FILE},
    view, Access, Actor, CheckResult, HostRequest, HostResponse, LogEvent, MessageKind, TaskRecord,
    TaskState, WaitOn, Waiting, WorkerResult, MAX_MESSAGE_BYTES,
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
    CheckRunDone(Vec<CheckResult>, broker::Response),
    /// Checks wait for the machine's check slot, for this reason.
    CheckQueued(String),
    /// Checks took the slot, or run without it after this error.
    CheckSlot(Option<String>),
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
    /// A grant takes effect when the agent next starts.
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
    /// A `land` or a `trial` waits for the checks of its rebased revision.
    after_rebase: Option<AfterRebase>,
    disk_wait: Option<DiskWait>,
    /// The worker's running `check-run`: its check indexes and every call that waits for its
    /// answer. A repeat call (a client that timed out and asks again) joins it.
    check_run: Option<(Vec<usize>, Vec<Sender<broker::Response>>)>,
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
        after_rebase: None,
        disk_wait: None,
        check_run: None,
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
    // ^ A land answers after its rebase, bounded by git's own timeouts.
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
        let reason = format!("cannot start the replaced wsx binary: {error}");
        host.log(LogEvent::Stopped {
            reason: reason.clone(),
        });
        host.wait(WaitOn::Owner, reason);
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
                Event::CheckRunDone(results, response) => {
                    for reply in self.check_run.take().map(|run| run.1).unwrap_or_default() {
                        let _ = reply.send(response.clone());
                    }
                    self.log(LogEvent::CheckRun { results });
                }
                Event::CheckQueued(reason) => self.check_queued(reason),
                Event::CheckSlot(error) => self.check_slot(error),
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
            // ^ A new task: no conversation yet, so its first turn has not run.
            TaskState::Working if self.record.agent_session.is_none() => {
                let prompt = self.dir.prompt()?.context("the task log has no prompt")?;
                let text = match self.step_text() {
                    Some(step) => format!("{prompt}\n\n{step}"),
                    None => prompt,
                };
                self.start_turn(&text);
            }
            // ^ The checks of a result do not depend on the host that lost them: they run
            // again on the worker's last result, as the land or retry that follows would.
            TaskState::Checking => {
                self.last_result = Some(self.current_result());
                self.say("the host restarted during the checks: they run again");
                self.start_checks();
            }
            TaskState::Working => {
                // ^ A restarted pane cannot rejoin a turn of the process it lost.
                let reason = "the host restarted during a turn".to_string();
                self.log(LogEvent::Stopped {
                    reason: reason.clone(),
                });
                self.wait(WaitOn::Owner, reason);
            }
            state => {
                self.set_state(state);
            }
        }
        Ok(())
    }

    fn handle(&mut self, request: HostRequest) -> HostResponse {
        // ^ While the owner works by hand nothing else writes the copy or its conversation.
        let held = self.record.waits_on(WaitOn::Hand)
            && matches!(
                request,
                HostRequest::Steer { .. }
                    | HostRequest::Add { .. }
                    | HostRequest::Land { .. }
                    | HostRequest::Trial
            );
        let message = match request {
            _ if held => Err(self.by_hand_refusal()),
            HostRequest::Status => Ok("".to_string()),
            HostRequest::Steer { text } => self.steer(text),
            HostRequest::Add { text, files } => self.add(text, files),
            HostRequest::Grant {
                access,
                project,
                by,
            } => self.grant(access, project, by),
            HostRequest::Stop => self.stop(),
            HostRequest::Land { tried } => self.land(false, tried),
            HostRequest::Hold { reason, by } => self.hold(reason, by),
            HostRequest::Trial => self.trial(),
            HostRequest::Hand => self.hand(),
            HostRequest::Handback => self.handback(),
            HostRequest::Close { discard } => {
                if let Some(agent) = self.agent.take() {
                    agent.stop();
                }
                self.turn_active = false;
                self.end_trial("the task closed");
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

    fn add(&mut self, text: String, files: Vec<String>) -> Result<String> {
        check_message(&text)?;
        if !files.is_empty() {
            self.record.files = checked_files(&self.record.files, &files)?;
            self.save();
        }
        self.log(LogEvent::Message {
            mode: MessageKind::Add,
            text: text.clone(),
        });
        // ^ The answer is a new revision, so the one on trial is no longer what lands.
        self.end_trial("a message went to the worker");
        Ok(self.deliver_or_queue(text))
    }

    /// Hold the task for the owner to try before it lands, or release it.
    fn hold(&mut self, reason: Option<String>, by: Actor) -> Result<String> {
        if !self.record.state.is_open() {
            bail!("task t{} is {}", self.record.id, self.state_words());
        }
        let reason = reason.as_deref().map(view::checked_hold).transpose()?;
        if self.record.hold == reason {
            return Ok("unchanged".into());
        }
        self.record.hold = reason.clone();
        self.save();
        let message = match &reason {
            Some(reason) => format!("held for the owner to try: {reason}"),
            None => "no longer held".into(),
        };
        self.log(LogEvent::Hold { reason, by });
        self.say(&message);
        Ok(message)
    }

    /// Put the ready revision on the live site: rebased onto the default branch first, so
    /// the owner tries what would land. The caller claimed the trial slot.
    fn trial(&mut self) -> Result<String> {
        self.require_ready(false)?;
        self.require_committed()?;
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&self.record.working_copy, "HEAD")?;
        if copy::is_ancestor(&self.record.working_copy, &tip, &head)? {
            return self.start_trial();
        }
        match copy::rebase(&self.record.working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.end_trial("the rebase onto the default branch conflicted");
                Ok(self.resolve_conflict(&tip, &paths))
            }
            Rebase::Done => {
                self.after_rebase = Some(AfterRebase::Trial);
                self.start_checks();
                Ok(format!(
                    "rebased onto {} at {tip:.12}; checks run on the new revision and it goes live when they pass",
                    self.record.default_branch
                ))
            }
        }
    }

    /// Name the current revision in the trial slot and deploy it in a `wsx task deploy` of
    /// its own, which outlives this host.
    fn start_trial(&mut self) -> Result<String> {
        let commit = copy::resolve_commit(&self.record.working_copy, "HEAD")?;
        let revision = self.record.revision;
        if !trial::set_commit(&self.record.project_id, self.record.id, revision, &commit)? {
            bail!("the trial of t{} ended before it started", self.record.id);
        }
        self.log(LogEvent::Trial {
            revision,
            commit: commit.clone(),
        });
        let id = self.record.id.to_string();
        let started = store::follow_up_log(&self.record.project_id).and_then(|log| {
            hosting::spawn_wsx(
                &[
                    "task",
                    "deploy",
                    "-p",
                    &self.record.project,
                    "--trial",
                    &id,
                    "--json",
                ],
                log,
            )
        });
        if let Err(error) = started {
            self.end_trial("its deploy did not start");
            return Err(error);
        }
        Ok(format!(
            "r{revision} at {commit:.12} deploys to the live site for the owner to try"
        ))
    }

    /// End this task's trial, if it holds the slot, and deploy the default branch again.
    fn end_trial(&self, reason: &str) {
        let ended = trial::end(
            &self.record.project_id,
            &self.record.project,
            Some(self.record.id),
            reason,
            true,
        );
        match ended {
            Ok(Some(_)) => {
                self.log(LogEvent::TrialEnded {
                    reason: reason.to_string(),
                });
                self.say(&format!("trial ended: {reason}"));
            }
            Ok(None) => {}
            Err(error) => self.say(&format!("cannot end the trial: {error:#}")),
        }
    }

    fn deliver_or_queue(&mut self, text: String) -> String {
        self.check_retries = 0;
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
        // ^ A task that waits for a grant or an answer resumes; disk and hands wait on.
        let blocked = (self.record.waits_on(WaitOn::Permission)
            || self.record.waits_on(WaitOn::Owner))
            && !self.turn_active;
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
        self.wait(WaitOn::Owner, "stopped by request".into());
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
        if !state.is_open() || state == TaskState::Checking || self.record.waits_on(WaitOn::Hand) {
            bail!(
                "task t{} is {}; an open task that is not checking is worked by hand",
                self.record.id,
                self.state_words()
            );
        }
        if let Some(agent) = self.agent.take() {
            self.stop_requested = true;
            agent.stop();
        }
        self.turn_active = false;
        self.end_trial("the owner took it by hand");
        let dropped = self.pending.len() as u32;
        self.pending.clear();
        self.disk_wait = None;
        self.log(LogEvent::Hand {
            by: Actor::Owner,
            dropped,
        });
        self.wait(WaitOn::Hand, "the owner works on it by hand".into());
        Ok(match dropped {
            0 => "the owner works on it by hand".into(),
            n => format!("the owner works on it by hand; dropped {n} queued message(s)"),
        })
    }

    /// The owner's commits become the next revision: its checks run and the task becomes
    /// ready or failed as after a worker result, so auto land and the close after it apply.
    fn handback(&mut self) -> Result<String> {
        if !self.record.waits_on(WaitOn::Hand) {
            bail!(
                "task t{} is {}; only a task worked by hand is handed back",
                self.record.id,
                self.state_words()
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
        // ^ Commits after the last revision (after the base before the first).
        let since = match self.record.revision {
            0 => self.record.base.clone(),
            revision => format!("refs/wsx/task/{}/r{revision}", self.record.id),
        };
        let commits = copy::unlanded_commits(&self.record.working_copy, "HEAD", &since)?;
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
        self.set_state(TaskState::Working);
        self.start_checks();
        Ok(format!(
            "handed back with {commits} commit(s); the checks run on the next revision"
        ))
    }

    fn start_turn(&mut self, text: &str) {
        // ^ Every path that starts a turn ends here: none runs while the owner works by hand.
        if self.record.waits_on(WaitOn::Hand) {
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
            self.set_state(TaskState::Working);
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
                    &self.dir.path,
                )?,
                &session,
                resume,
                if resume { self.session_cost() } else { 0 },
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
        self.set_state(TaskState::Working);
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
            // ^ What the sandbox stopped is granted; anything else is the owner's to answer.
            if self.last_asks.is_empty() {
                self.wait(WaitOn::Owner, first_line(&result.summary).to_string());
            } else {
                let asks: Vec<String> = self.last_asks.iter().map(Access::describe).collect();
                self.wait(WaitOn::Permission, format!("needs to {}", asks.join(", ")));
            }
            // ^ A worker stopped by a full disk continues once there is space again.
            self.wait_for_disk(DiskWait::Turn(
                "The disk was full; there is space again. Continue the task where it stopped."
                    .into(),
            ));
            return;
        }
        // ^ A done step that is not the last starts the next one, with any queued message.
        // ref: docs/tasks.md "Workflow"
        if result.status == "done" && (self.record.step as usize) + 1 < self.record.steps.len() {
            self.record.step += 1;
            self.save();
            let mut text: Vec<String> = self.pending.drain(..).collect();
            text.extend(self.step_text());
            self.start_turn(&text.join("\n\n"));
            return;
        }
        // ^ A command is the worker's to run: an item that names one goes back once, within
        // the check retries. ref: docs/tasks.md "To check"
        let commands: Vec<&str> = result
            .human_verify
            .iter()
            .filter(|item| item.names_command())
            .map(|item| item.check.as_str())
            .collect();
        if result.status == "done"
            && !commands.is_empty()
            && self.pending.is_empty()
            && self.check_retries < MAX_CHECK_RETRIES
        {
            self.check_retries += 1;
            self.say("human_verify names a command: sent back to the worker");
            let text = format!(
                "These human_verify items name a command: {}. A command is yours to run: run \
                 it, put what it shows in evidence, and keep in human_verify only what a person \
                 looks at or feels, in plain words with where to look. Then finish as before.",
                commands.join(" | ")
            );
            self.start_turn(&text);
            return;
        }
        self.start_checks();
    }

    /// The turn text of the current workflow step, when the task has steps.
    fn step_text(&self) -> Option<String> {
        let total = self.record.steps.len();
        let index = self.record.step as usize;
        let step = self.record.steps.get(index)?;
        let end = match index + 1 == total {
            true => "This is the last step: finish the task and return your final result.",
            false => {
                "Do only this step. When it is done, return status done with what it found \
                      or did; the next step follows."
            }
        };
        self.say(&format!("step {} of {total}: {}", index + 1, step.name));
        Some(format!(
            "Step {} of {total}, {}: {}\n{end}",
            index + 1,
            step.name,
            step.prompt
        ))
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
        self.wait(WaitOn::Disk, reason);
        true
    }

    fn resume_after_disk(&mut self) {
        if self.low_disk().is_some() {
            return;
        }
        let Some(what) = self.disk_wait.take() else {
            return;
        };
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

    /// Wait for `on`: nothing runs until it happens. ref: docs/tasks.md "Store"
    fn wait(&mut self, on: WaitOn, reason: String) {
        self.record.waiting = Some(Waiting { on, reason });
        self.set_state(TaskState::Waiting);
    }

    /// "ready", "waiting (the owner works on it by hand)" or "checking (queued for ...)".
    fn state_words(&self) -> String {
        match &self.record.waiting {
            Some(waiting) => format!("{} ({})", self.record.state.as_str(), waiting.reason),
            None => self.record.state.as_str().into(),
        }
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
        if self.record.waits_on(WaitOn::Hand) {
            return refuse(format!("{:#}", self.by_hand_refusal()));
        }
        self.follow_checks();
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
        // ^ Never two runs in one working copy: the same checks join, other ones wait.
        let indexes: Vec<usize> = picked.iter().map(|(index, _)| *index).collect();
        if let Some((running, replies)) = &mut self.check_run {
            if *running != indexes {
                let numbers: Vec<String> = running.iter().map(|i| (i + 1).to_string()).collect();
                return refuse(format!(
                    "a check-run of checks {} still runs; run it again to wait for its answer",
                    numbers.join(" ")
                ));
            }
            replies.push(reply);
            self.say("check-run: joined the run of the same checks");
            return;
        }
        if let Some((free_mb, min_gb)) = self.low_disk() {
            return refuse(disk_full(free_mb, min_gb));
        }
        self.check_run = Some((indexes, vec![reply]));
        // ^ A run of checks that only format or compile is light: it takes no slot.
        let priority =
            (!picked.iter().all(|(_, command)| light(command))).then_some(Priority::Normal);
        let commands = picked.iter().map(|(_, command)| command.clone()).collect();
        let events = self.events.clone();
        self.spawn_checks(priority, commands, move |results| {
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
            let response = broker::Response {
                stdout,
                stderr: String::new(),
                code,
            };
            let _ = events.send(Event::CheckRunDone(results, response));
        });
    }

    /// Take `tasks.checks` and their timeout from the main checkout's config as it is now:
    /// the gate a task lands through is the default branch's, also for a task started
    /// before it changed. A config that does not load keeps the record's.
    /// ref: docs/tasks.md "Checks, revisions and report"
    fn follow_checks(&mut self) {
        let Ok(settings) = load_task_config(&self.record.project_path) else {
            return;
        };
        if settings.checks == self.record.checks
            && settings.check_timeout_secs == self.record.check_timeout_secs
        {
            return;
        }
        self.say(&format!(
            "the project's checks changed: {}",
            settings.checks.join(" | ")
        ));
        self.record.checks = settings.checks;
        self.record.check_timeout_secs = settings.check_timeout_secs;
        self.save();
    }

    fn start_checks(&mut self) {
        if self.record.waits_on(WaitOn::Hand) {
            return;
        }
        if self.wait_for_disk(DiskWait::Checks) {
            return;
        }
        self.follow_checks();
        self.set_state(TaskState::Checking);
        let priority = match self.after_rebase {
            Some(AfterRebase::Land { .. }) => Priority::Land,
            _ => Priority::Normal,
        };
        let events = self.events.clone();
        self.spawn_checks(Some(priority), self.record.checks.clone(), move |results| {
            let _ = events.send(Event::ChecksDone(results));
        });
    }

    /// Run `commands` in a thread, holding the machine's check slot unless `priority` is
    /// `None`; each check's timeout counts from the slot. ref: docs/tasks.md "Checks, revisions and report"
    fn spawn_checks(
        &self,
        priority: Option<Priority>,
        commands: Vec<String>,
        done: impl FnOnce(Vec<CheckResult>) + Send + 'static,
    ) {
        let timeout = Duration::from_secs(self.record.check_timeout_secs);
        let working_copy = self.record.working_copy.clone();
        let env = self.record.env.clone();
        let events = self.events.clone();
        let label = format!("{} t{}", self.record.project, self.record.id);
        std::thread::spawn(move || {
            let results = {
                let _slot = priority.and_then(|priority| {
                    let taken = slot::take(priority, &label, |reason| {
                        let _ = events.send(Event::CheckQueued(reason));
                    });
                    let error = taken.as_ref().err().map(ToString::to_string);
                    let _ = events.send(Event::CheckSlot(error));
                    taken.ok()
                });
                commands
                    .iter()
                    .map(|command| run_check(&working_copy, command, &env, timeout))
                    .collect()
            };
            done(results);
        });
    }

    /// The checks wait for the slot: the record shows it while the task works or checks.
    fn check_queued(&mut self, reason: String) {
        if self.record.state == TaskState::Waiting {
            return;
        }
        if !self.record.waits_on(WaitOn::CheckSlot) {
            self.log(LogEvent::CheckQueued {
                reason: reason.clone(),
            });
        }
        self.say(&reason);
        self.record.waiting = Some(Waiting {
            on: WaitOn::CheckSlot,
            reason,
        });
        self.save();
    }

    fn check_slot(&mut self, error: Option<String>) {
        if let Some(error) = error {
            self.say(&format!(
                "no check slot, so the checks run unqueued: {error}"
            ));
        }
        if self.record.waits_on(WaitOn::CheckSlot) {
            self.say("took the check slot");
            self.record.waiting = None;
            self.save();
        }
    }

    fn on_checks(&mut self, results: Vec<CheckResult>) {
        // ^ A full disk is not the worker's failure: the checks run again once there is space.
        if results.iter().any(|check| check.output.contains(NO_SPACE)) {
            self.log(LogEvent::Checks { results });
            self.wait_for_disk(DiskWait::Checks);
            if self.disk_wait.is_none() {
                self.disk_wait = Some(DiskWait::Checks);
                self.wait(WaitOn::Disk, "Disk full: a check ran out of space".into());
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
                let after_rebase = self.after_rebase.take();
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
                match after_rebase {
                    // ^ A worker that says done but fails a check gets the failure back,
                    // bounded, before the task fails. ref: docs/tasks.md "Checks, revisions and report"
                    None if state == TaskState::Failed
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
                    None if state == TaskState::Ready
                        && !results.is_empty()
                        && self.record.created_by == Actor::Owner
                        && self.record.hold.is_none()
                        && self.pending.is_empty()
                        && self.auto_land() =>
                    {
                        let landed = self.land(true, false);
                        self.finish_landing(landed, state);
                    }
                    None => self.set_state(state),
                    Some(AfterRebase::Land { tried }) => {
                        let finished = if state == TaskState::Ready {
                            self.fast_forward(tried)
                        } else {
                            Err(anyhow::anyhow!("checks failed on the rebased revision"))
                        };
                        self.finish_landing(finished, state);
                    }
                    Some(AfterRebase::Trial) => {
                        self.set_state(state);
                        if state == TaskState::Ready {
                            if let Err(error) = self.start_trial() {
                                self.say(&format!("the trial did not start: {error:#}"));
                            }
                        } else {
                            self.end_trial("checks failed on the rebased revision");
                        }
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

    /// The worker conversation's last `session_cost_micro_usd`: a task has one conversation.
    fn session_cost(&self) -> u64 {
        let log = self.dir.read_log().unwrap_or_default();
        log.iter()
            .rev()
            .find_map(|entry| match &entry.event {
                LogEvent::Result { usage, .. } => usage.as_ref(),
                _ => None,
            })
            .map_or(0, |usage| usage.session_cost_micro_usd)
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
    /// `tried`: the owner passed this task's trial.
    fn land(&mut self, auto: bool, tried: bool) -> Result<String> {
        self.require_ready(auto)?;
        self.require_committed()?;
        if tried && !self.on_trial()? {
            bail!(
                "no trial of t{} runs; land it without --tried",
                self.record.id
            );
        }
        let working_copy = self.record.working_copy.clone();
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&working_copy, "HEAD")?;
        if copy::is_ancestor(&working_copy, &tip, &head)? {
            return self.fast_forward(tried);
        }
        match copy::rebase(&working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.carried_verify = self.current_result().human_verify;
                Ok(self.resolve_conflict(&tip, &paths))
            }
            Rebase::Done => {
                self.after_rebase = Some(AfterRebase::Land { tried });
                self.start_checks();
                Ok(format!(
                    "rebased onto {} at {tip:.12}; checks run on the new revision and it lands when they pass",
                    self.record.default_branch
                ))
            }
        }
    }

    /// Log a rebase conflict and start the worker turn that resolves it.
    fn resolve_conflict(&mut self, tip: &str, paths: &[String]) -> String {
        self.log(LogEvent::Conflict {
            onto: tip.to_string(),
            paths: paths.to_vec(),
        });
        self.start_turn(&format!(
            "Landing needs branch {branch} rebased onto {tip} (the current {default}). \
             Run `git rebase {tip}`, resolve the conflicts in {paths}, `git add` them and \
             `git rebase --continue`, then report.",
            branch = self.record.branch,
            default = self.record.default_branch,
            paths = paths.join(", "),
        ));
        format!(
            "the rebase onto {} conflicted in {}; the worker is resolving it{}",
            self.record.default_branch,
            paths.join(", "),
            if self.auto_land() && self.record.hold.is_none() {
                ", and the task lands when its checks pass"
            } else {
                ". Act again when the task is ready"
            }
        )
    }

    /// Whether the trial slot holds this task's current revision.
    fn on_trial(&self) -> Result<bool> {
        Ok(
            store::read_trial(&self.record.project_id)?.is_some_and(|trial| {
                trial.task == self.record.id && trial.revision == Some(self.record.revision)
            }),
        )
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

    fn default_tip(&self) -> Result<String> {
        copy::resolve_commit(
            &self.record.project_path,
            &format!("refs/heads/{}", self.record.default_branch),
        )
    }

    fn fast_forward(&mut self, tried: bool) -> Result<String> {
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&self.record.working_copy, "HEAD")?;
        copy::fast_forward(
            &self.record.project_path,
            &self.record.default_branch,
            &tip,
            &head,
        )?;
        self.landed(&head, tried);
        Ok(format!(
            "landed r{} on {} at {head:.12}",
            self.record.revision, self.record.default_branch
        ))
    }

    /// The revision is on the default branch: its Human Verify items join the project's
    /// "To check" list before readers see `landed` (docs/tasks.md "To check"), unless the
    /// owner tried them on the live site. A trial of this task ends.
    fn landed(&mut self, commit: &str, tried: bool) {
        if tried {
            self.carried_verify.clear();
        } else {
            self.collect_verify(commit);
        }
        self.log(LogEvent::Landed {
            commit: commit.to_string(),
        });
        self.end_trial(if tried {
            "the owner passed it and it landed"
        } else {
            "it landed"
        });
        self.write_report(TaskState::Landed);
        self.set_state(TaskState::Landed);
        self.after_landing();
    }

    /// Add the revision's Human Verify items, with their captures, to "To check".
    fn collect_verify(&mut self, commit: &str) {
        let mut verify = self.current_result().human_verify;
        for item in std::mem::take(&mut self.carried_verify) {
            if !verify.iter().any(|known| known.check == item.check) {
                verify.push(item);
            }
        }
        for (index, item) in verify.iter_mut().enumerate() {
            if item.capture.is_empty() {
                continue;
            }
            let kept = checks::keep_capture(
                &self.dir,
                &self.record.working_copy,
                self.record.revision,
                index + 1,
                &item.capture,
            );
            if let Err(error) = &kept {
                self.say(&format!("capture not kept: {error:#}"));
            }
            item.capture = kept.unwrap_or_default();
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
        if state != TaskState::Waiting {
            self.record.waiting = None;
        }
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
        let state = match (
            self.record.state,
            self.record.waiting.as_ref().map(|w| w.on),
        ) {
            (TaskState::Working | TaskState::Checking, _) => runtime::AgentState::Working,
            (TaskState::Waiting, Some(WaitOn::Permission | WaitOn::Owner)) => {
                runtime::AgentState::Blocked
            }
            (TaskState::Waiting, _) | (TaskState::Closed, _) => runtime::AgentState::Idle,
            (TaskState::Ready | TaskState::Landed, _) => runtime::AgentState::Done,
            (TaskState::Failed, _) => runtime::AgentState::Error,
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

/// What a land or a trial does after the checks of its rebased revision pass.
#[derive(Clone, Copy)]
enum AfterRebase {
    Land { tried: bool },
    Trial,
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// Checks a worker's `check-run` runs without the slot: one plain command that only formats
/// or compiles, so no timing test meets the load of another task's checks.
fn light(command: &str) -> bool {
    const LIGHT: [&str; 3] = ["cargo fmt", "cargo build", "cargo check"];
    let plain = !command.contains(['&', '|', ';', '\n', '`', '$', '(', '<', '>']);
    let words: Vec<&str> = command.split_whitespace().take(2).collect();
    plain
        && LIGHT
            .iter()
            .any(|light| light.split(' ').eq(words.iter().copied()))
}

/// `env`: `tasks.env`, as the worker's commands have it. ref: docs/tasks.md "Config"
fn run_check(
    working_copy: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> CheckResult {
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
    process.envs(env);
    // ^ The tail: a long check output (a test run) ends with its failures and summary.
    match output_with_timeout_tail(&mut process, timeout, CHECK_OUTPUT_BYTES) {
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
