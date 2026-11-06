//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

use super::agent::{
    new_session_id, Agent, AgentLine, Launch, TurnResult, KEEP_WARM_PINGS, KEEP_WARM_TEXT,
    PROMPT_CACHE_MS,
};
use super::broker;
use super::hosting;
use super::limit;
use super::slot::{self, Lane, Priority};
use super::trial;
use super::RECORDED_BY_HOST;
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
    build, call_host, checks,
    copy::{self, Rebase},
    failure::{self, NO_SPACE},
    manager::{Author, Short},
    now_unix_ms, owner,
    owner::HOST_TIMEOUT,
    report,
    store::{self, TaskDir, REPORT_FILE, RESULT_FILE},
    view, Actor, CheckResult, Evidence, HostRequest, HostResponse, HumanVerify, LogEvent,
    MessageKind, TaskRecord, TaskState, TurnTrigger, WaitOn, Waiting, WorkerResult,
    MAX_MESSAGE_BYTES,
};

/// A shared failure is posted once in this long (the store keeps a key for a day).
const SHARED_WINDOW_SECS: u64 = 24 * 60 * 60;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
pub(super) const CHECK_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
/// The turn a host hands to its replacement: queued messages joined, so a few messages.
/// The end of a `--plan` task's first prompt. ref: docs/tasks.md "Workflow"
const PLAN_REQUEST: &str = "First write a short plan and stop: edit no file and commit nothing \
in this turn. Put the plan in your result's summary with status done: the goal in one line, the \
files you expect to change, the approach, and how you will prove the result. The manager reads \
it and answers with go or what to change.";
/// At most this much of the manager's memo lines goes into a task's first turn.
const MAX_TASK_CONTEXT_BYTES: usize = 4 * 1024;
const HANDOFF_FILE: &str = "handoff.txt";
/// The first line of a handoff names the trigger of the turn it carries.
const HANDOFF_TRIGGER: &str = "trigger: ";
const MAX_HANDOFF_BYTES: u64 = 4 * 1024 * 1024;
/// Turns a failed check starts by itself before the task fails; an owner's message resets it.
const MAX_CHECK_RETRIES: u32 = 2;
/// The end of each failed check's output that goes back to the worker.
const CHECK_TAIL_BYTES: usize = 3 * 1024;

enum Event {
    Request(HostRequest, Sender<HostResponse>),
    /// `None` when the agent process of that generation ended.
    Agent(u64, Option<AgentLine>),
    /// The results of the checks started under that `checks_epoch`.
    ChecksDone(u64, Vec<CheckResult>),
    /// The worker's `wsx task check-run` through the broker, and its results to log; the
    /// last part is what to store when the tree stayed clean and unchanged during the run.
    CheckRun(Vec<String>, Sender<broker::Response>),
    /// The worker's `wsx task result -`: its turn's result, for an adapter without
    /// structured output.
    Reported(Option<String>, Sender<broker::Response>),
    CheckRunDone(Vec<CheckResult>, broker::Response, Option<StoredChecks>),
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

/// A turn the agent's usage limit ended: the text that continues it, and when it may.
struct LimitWait {
    text: String,
    until_ms: u64,
    /// When the host looks next: `until_ms`, later while another task holds the limit slot.
    next_ms: u64,
}

/// A turn without a result that ended while background jobs ran: Claude starts its own turn
/// when each ends. The wait ends at `deadline`, counted from the first such turn.
struct BackgroundWait {
    deadline: Instant,
    /// The turn that waits, taken when the deadline passes.
    turn: Option<TurnResult>,
}

/// The turn that continues after a usage limit.
const LIMIT_CONTINUE: &str = "The usage limit has reset. Continue the task where it stopped.";
/// How often a resumable task behind another in the limit queue looks again.
const LIMIT_POLL_MS: u64 = 2_000;
/// How often a wait on a fallback looks for the task's own agent coming free.
const HOME_POLL_MS: u64 = 10_000;
/// How often a task that waits for disk looks again.
const DISK_POLL: Duration = Duration::from_secs(30);
/// How long a task resumed from a disk wait holds the disk slot, so the next one sees the
/// space its turn takes before it resumes.
const DISK_SETTLE: Duration = Duration::from_secs(60);
/// What a check prints when the disk is full: the check then waits instead of failing.
/// The handoff notes a worker writes before its conversation rotates, in its task folder.
const ROTATE_NOTES: &str = "rotate-notes.md";
/// At most this much of the notes goes into the fresh conversation's first turn.
const MAX_ROTATE_NOTES_BYTES: u64 = 16 * 1024;
/// The one turn a worker gets when its turn ended without a result.
const RESULT_RETRY: &str = "Your turn ended without a result. Return your result now: \
StructuredOutput is the last call of a turn, and nothing runs after it.";

/// A worker's `check-run`: its check indexes and every call that waits for its answer. A
/// repeat call (a client that timed out and asks again) joins it.
struct CheckRun {
    indexes: Vec<usize>,
    replies: Vec<Sender<broker::Response>>,
    /// The worker returned its result before the answer: the answer goes to it in one turn,
    /// and the checks after the result wait for that turn's result.
    result_waits: bool,
    /// The run queued for the check slot, so its calls got `queued` at once: the answer goes
    /// to the worker as a turn after its result, never to a client that waits.
    queued: bool,
    /// The answer of a queued run that ended before the result: the result gets it as a turn.
    answer: Option<CheckAnswer>,
    /// The checks it runs, for the evidence name: scenario names and check numbers.
    label: String,
    /// It runs a `ci:` scenario, so it answered `queued` at once: CI takes 20 min or more.
    ci: bool,
}

/// A check-run's answer to the worker, and the proof the host keeps when the run passed on
/// an unchanged tree: a pass that comes after the worker's last result needs no turn.
struct CheckAnswer {
    text: String,
    proof: Option<Evidence>,
}

/// What a `check-run` that queues for the check slot, or runs on CI, answers at once: a
/// background job of the agent dies long before a 20-90 min queue ends. ref: docs/tasks.md "Check-run"
const CHECK_RUN_QUEUED: &str = "its answer comes after your result. Do not wait or poll for it: \
finish this step and return its result now. The host sends you the answer as your next turn, \
in this step.\n";

/// A `ci:` scenario's command runs on CI and waits there: its own time limit, beyond the job's
/// 60 min and the runner's queue. ref: docs/tasks.md "Scenarios"
const CI_TIMEOUT: Duration = Duration::from_secs(75 * 60);

/// The command of a `ci:` scenario: it runs on CI and takes no check slot.
fn ci_command(command: &str) -> Option<&str> {
    command.strip_prefix("ci:").map(str::trim)
}

struct Host {
    dir: TaskDir,
    record: TaskRecord,
    events: Sender<Event>,
    agent: Option<Agent>,
    generation: u64,
    turn_active: bool,
    /// Adds sent during a turn or a check, delivered when it ends.
    pending: VecDeque<String>,
    /// Land notices: they join a turn that starts anyway and never start one.
    notices: Vec<String>,
    /// The running turn came through a deploy re-exec: its `result` logs it.
    carried: bool,
    stop_requested: bool,
    /// `wsx task stop` ran and no add or steer came since: nothing else starts a turn.
    /// ref: docs/tasks.md "Host" (Stop)
    stopped: bool,
    /// Counts check runs and stops: the results of an older run (one a stop left running) are
    /// logged and nothing more.
    checks_epoch: u64,
    /// The result of the latest turn, until its checks end.
    last_result: Option<WorkerResult>,
    /// Tool calls denied in that turn.
    last_denials: Vec<String>,
    /// Turns started for failed checks since the last message from a person.
    check_retries: u32,
    /// The worker was asked once this turn for the result its last turn left out.
    result_asked: bool,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<hosting::Binary>,
    reexec: bool,
    /// A `land` or a `trial` waits for the checks of its rebased revision.
    after_rebase: Option<AfterRebase>,
    disk_wait: Option<DiskWait>,
    /// The place of a disk wait in the machine's disk queue, and the disk slot it took to
    /// resume, with when it took it: held for `DISK_SETTLE` or until its turn or checks end.
    disk_queue: Option<slot::Ticket>,
    disk_slot: Option<(slot::Slot, Instant)>,
    limit_wait: Option<LimitWait>,
    /// Usage limits in a row, for the backoff when no reset time is known.
    limit_attempt: u32,
    /// The place of a limit wait in the machine's limit queue, and the limit slot it took
    /// to resume: held until the agent answers again, so one task at a time tries.
    limit_queue: Option<slot::Ticket>,
    limit_slot: Option<slot::Slot>,
    background_wait: Option<BackgroundWait>,
    /// The worker's queued or running `check-run`.
    check_run: Option<CheckRun>,
    /// The result the worker reported with `wsx task result -` in the running turn.
    reported: Option<serde_json::Value>,
    /// The text of the running turn: a switch to another agent sends it again.
    turn_text: String,
    /// The queued or running checks of a result: the commit they check, and whether a newer
    /// result came meanwhile.
    gate: Option<(Option<String>, bool)>,
    /// The check-run results of one clean tree: a repeat on that tree answers from them.
    stored_checks: Option<StoredChecks>,
    /// The Human Verify items of the revision whose landing conflicted: the turn that
    /// resolves it rarely repeats them, and they still need a person.
    carried_verify: Vec<HumanVerify>,
    /// The context of the worker's last model call, from the log when the host starts.
    context_tokens: u64,
    /// The turn that waits while the worker writes its handoff notes, and the idle reason
    /// when an idle gap rotated it: it starts the fresh conversation.
    /// ref: docs/tasks.md "Context rotation"
    rotating: Option<(String, Option<String>)>,
    /// What started the running turn, logged on its result.
    trigger: TurnTrigger,
    /// When the agent last answered (unix ms), from the log when the host starts.
    answered_ms: u64,
    /// Ping turns since the last real turn, and what starts once the running ping ends.
    /// ref: docs/tasks.md "Context rotation"
    pings: u32,
    after_ping: Option<TurnTrigger>,
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
    let (events, receiver) = channel();
    let mut host = Host {
        dir,
        record,
        events: events.clone(),
        agent: None,
        generation: 0,
        turn_active: false,
        pending: VecDeque::new(),
        notices: Vec::new(),
        carried: false,
        stop_requested: false,
        stopped: false,
        checks_epoch: 0,
        last_result: None,
        last_denials: Vec::new(),
        check_retries: 0,
        result_asked: false,
        binary: hosting::Binary::capture(),
        reexec: false,
        after_rebase: None,
        disk_wait: None,
        disk_queue: None,
        disk_slot: None,
        limit_wait: None,
        limit_attempt: 0,
        limit_queue: None,
        limit_slot: None,
        background_wait: None,
        check_run: None,
        reported: None,
        turn_text: String::new(),
        gate: None,
        stored_checks: None,
        carried_verify: Vec::new(),
        context_tokens: 0,
        rotating: None,
        trigger: TurnTrigger::Step,
        answered_ms: 0,
        pings: 0,
        after_ping: None,
        started: Instant::now(),
    };
    (host.context_tokens, host.answered_ms) = host.last_context();
    let to_host = events.clone();
    std::thread::spawn(move || {
        broker::serve(brokered, move |request| {
            let (reply, answer) = channel();
            // ^ An adapter without structured output returns its result here (docs/tasks.md
            // "Agents"): `wsx task result -` with the JSON on stdin.
            let event = match request.argv.get(1).map(String::as_str) {
                Some("result") => Event::Reported(request.stdin, reply),
                _ => Event::CheckRun(request.argv, reply),
            };
            if to_host.send(event).is_err() {
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
        if let Some((text, trigger)) = self.take_handoff() {
            self.say("restarted with the replaced wsx binary");
            self.carried = true;
            self.start_turn(&text, trigger);
        } else {
            self.resume_state()?;
        }
        let mut disk_checked = Instant::now();
        loop {
            // ^ Here, not after an event: a turn a timer starts (after a full disk, a limit or
            // a background wait's deadline) hands over to a replaced binary too, and no event
            // may come for hours.
            if self.reexec {
                std::thread::sleep(Duration::from_millis(200));
                return Ok(());
            }
            // ^ The limit slot is held only while the agent runs a turn.
            // ref: docs/tasks.md "Claude adapter"
            if !self.turn_active {
                self.limit_slot = None;
            }
            if self
                .disk_slot
                .as_ref()
                .is_some_and(|(_, taken)| taken.elapsed() >= DISK_SETTLE)
            {
                self.disk_slot = None;
            }
            // ^ On a deadline, not after a quiet spell: status calls arrive all the time.
            let disk_due = self
                .disk_wait
                .as_ref()
                .map(|_| DISK_POLL.saturating_sub(disk_checked.elapsed()));
            let settle_due = self
                .disk_slot
                .as_ref()
                .map(|(_, taken)| DISK_SETTLE.saturating_sub(taken.elapsed()));
            let limit_due = self
                .limit_wait
                .as_ref()
                .map(|wait| Duration::from_millis(wait.next_ms.saturating_sub(now_unix_ms())));
            let background_due = self
                .background_wait
                .as_ref()
                .filter(|wait| wait.turn.is_some())
                .map(|wait| wait.deadline.saturating_duration_since(Instant::now()));
            let ping_due = self
                .ping_due()
                .map(|due| Duration::from_millis(due.saturating_sub(now_unix_ms())));
            let event = match disk_due
                .into_iter()
                .chain(limit_due)
                .chain(background_due)
                .chain(settle_due)
                .chain(ping_due)
                .min()
            {
                None => receiver.recv().ok(),
                Some(due) => match receiver.recv_timeout(due) {
                    Ok(event) => Some(event),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if settle_due == Some(due) {
                            // ^ Dropped at the top of the loop.
                        } else if disk_due == Some(due) {
                            disk_checked = Instant::now();
                            self.resume_after_disk();
                        } else if background_due == Some(due) {
                            self.background_timed_out();
                        } else if ping_due == Some(due) {
                            self.ping();
                        } else {
                            self.resume_after_limit();
                        }
                        continue;
                    }
                    Err(_) => None,
                },
            };
            let Some(event) = event else { break };
            match event {
                Event::Request(HostRequest::Land { .. }, reply)
                    if self.record.state == TaskState::Ready && self.must_follow_binary() =>
                {
                    // ^ Only a ready task lands, and has no agent turn to follow a deploy at, so its land
                    // would run the old code. The request is dropped unanswered: the caller
                    // sends it again (`call_host_waiting`, 3 s) and the new host lands it.
                    drop(reply);
                    self.reexec = true;
                    self.say(
                        "the wsx binary was replaced: the host restarts with it before the land",
                    );
                }
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
                Event::ChecksDone(epoch, results) => self.on_checks(epoch, results),
                Event::CheckRun(argv, reply) => self.check_run(&argv, reply),
                Event::Reported(text, reply) => {
                    let _ = reply.send(self.reported(text));
                }
                Event::CheckRunDone(results, mut response, stored) => {
                    let shared = self.shared_failures(&results);
                    if !shared.is_empty() {
                        response.stdout = format!("{}\n{shared}", response.stdout.trim_end());
                    }
                    let run = self.check_run.take();
                    let label = run
                        .as_ref()
                        .map_or_else(String::new, |run| run.label.clone());
                    for reply in run.iter().flat_map(|run| &run.replies) {
                        let _ = reply.send(response.clone());
                    }
                    let tree_kept = stored.is_some();
                    if let Some((tree, ran)) = stored {
                        match &mut self.stored_checks {
                            Some((on, results)) if *on == tree => results.extend(ran),
                            _ => self.stored_checks = Some((tree, ran)),
                        }
                    }
                    self.log(LogEvent::CheckRun { results });
                    // ^ The worker finishes with the proof it asked for, then the checks run.
                    let answer = CheckAnswer {
                        text: format!(
                            "Your check-run ended. Its answer:\n{}\nPut it in your result's \
                             evidence (fix what fails in code you changed) and return your \
                             result again.",
                            response.stdout.trim_end()
                        ),
                        proof: (response.code == 0 && tree_kept).then(|| Evidence {
                            check: format!("wsx task check-run {label} passed {RECORDED_BY_HOST}"),
                            proof: response.stdout.trim_end().into(),
                        }),
                    };
                    match run {
                        // ^ A stopped task's answer joins the turn an add starts: no turn now.
                        Some(_) if self.stopped => {
                            self.say(
                                "check-run ended while stopped: its answer joins the next turn",
                            );
                            self.notices.push(answer.text);
                        }
                        Some(run) if run.result_waits => self.answer_check_run(answer),
                        Some(run) if run.queued && !self.turn_active => {
                            self.send_check_answer(answer.text)
                        }
                        // ^ The worker still works after `queued`: its result gets the answer.
                        Some(run) if run.queued => {
                            self.check_run = Some(CheckRun {
                                answer: Some(answer),
                                ..run
                            });
                        }
                        _ => {}
                    }
                }
                Event::CheckQueued(reason) => self.check_queued(reason),
                Event::CheckSlot(error) => self.check_slot(error),
            }
        }
        Ok(())
    }

    /// Whether this host must exec the wsx file that replaced it before it starts more work:
    /// not while a check-run waits or owes its answer (the exec would drop the run), nor
    /// before the turn after a limit (the exec would drop the limit slot).
    fn must_follow_binary(&self) -> bool {
        self.check_run.is_none()
            && self.limit_slot.is_none()
            && self.binary.as_ref().is_some_and(hosting::Binary::replaced)
    }

    /// The turn a replaced host handed over, and what started it. A file from an older host
    /// has no trigger line: it reads as `Restart`.
    fn take_handoff(&self) -> Option<(String, TurnTrigger)> {
        let bytes = self
            .dir
            .read_file(HANDOFF_FILE, MAX_HANDOFF_BYTES)
            .ok()
            .flatten()?;
        let _ = std::fs::remove_file(self.dir.file(HANDOFF_FILE));
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let named = text.strip_prefix(HANDOFF_TRIGGER).and_then(|rest| {
            let (name, text) = rest.split_once('\n')?;
            Some((text.to_string(), serde_json::from_str(name).ok()?))
        });
        Some(named.unwrap_or((text, TurnTrigger::Restart)))
    }

    fn resume_state(&mut self) -> Result<()> {
        match self.record.state {
            // ^ A new task: no conversation yet, so its first turn has not run.
            TaskState::Working if self.record.agent_session.is_none() => {
                let prompt = self.dir.prompt()?.context("the task log has no prompt")?;
                let mut text = vec![prompt];
                text.extend(self.task_context());
                text.extend(self.step_text());
                if self.record.plan {
                    text.push(PLAN_REQUEST.into());
                }
                self.start_turn(&text.join("\n\n"), TurnTrigger::Step);
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
            // ^ A limit wait outlives its host: the restarted one continues at the same time.
            TaskState::Waiting if self.record.waits_on(WaitOn::Limit) => {
                let until_ms = self
                    .record
                    .waiting
                    .as_ref()
                    .and_then(|w| w.until_ms)
                    .unwrap_or_default();
                // ^ A long wait from an older host or from before a fallback was configured
                // moves to the fallback now instead of waiting days. ref: docs/tasks.md "Agents"
                let now = now_unix_ms();
                let account = self.account();
                if self.move_after_limit(Some("Continue the task where it stopped."), until_ms, now)
                {
                    let _ = store::mark_limited(&account, until_ms, now, true);
                } else {
                    self.wait_for_limit(LIMIT_CONTINUE.into(), until_ms);
                }
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
                    | HostRequest::Notice { .. }
                    | HostRequest::LandBatch { .. }
            );
        let message = match request {
            _ if held => Err(self.by_hand_refusal()),
            HostRequest::Status => Ok("".to_string()),
            HostRequest::Steer { text } => self.steer(text),
            HostRequest::Add { text, files } => self.add(text, files),
            HostRequest::Stop => self.stop(),
            HostRequest::Land { tried, batch_tree } => self.land(false, tried, batch_tree),
            HostRequest::Hold { reason, by } => self.hold(reason, by),
            HostRequest::Trial => self.trial(),
            HostRequest::Hand => self.hand(),
            HostRequest::Handback => self.handback(),
            HostRequest::ChildLanded { child, commits } => self.child_landed(child, commits),
            HostRequest::Notice { text } => self.notice(text),
            HostRequest::LandBatch { tasks } => self.land_batch(&tasks),
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

    /// Another task's land changed files this one shares. The text joins the worker's next
    /// turn, at its step boundary, and starts none; a task with no turn to come (ready,
    /// checking, failed) gets nothing, as its own land rebases it. ref: docs/tasks.md "Landing"
    fn notice(&mut self, text: String) -> Result<String> {
        check_message(&text)?;
        if !self.turn_active && self.record.state != TaskState::Waiting {
            return Ok(format!(
                "not sent: t{} is {}",
                self.record.id,
                self.state_words()
            ));
        }
        self.log(LogEvent::Message {
            mode: MessageKind::Add,
            text: text.clone(),
        });
        self.notices.push(text);
        Ok("queued for the worker's next turn".into())
    }

    /// Start `wsx task land-batch` as this host's own process: the caller may be the
    /// manager, whose boundary cannot write the state folder. ref: docs/tasks.md "Batch land"
    fn land_batch(&mut self, tasks: &[u32]) -> Result<String> {
        self.require_ready(false)?;
        let names: Vec<String> = tasks.iter().map(u32::to_string).collect();
        let mut args = vec!["task", "land-batch"];
        args.extend(names.iter().map(String::as_str));
        args.extend(["-p", self.record.project.as_str()]);
        let log = store::follow_up_log(&self.record.project_id)?;
        hosting::spawn_wsx(&args, log, None)?;
        Ok(format!(
            "the batch land of {} started: its checks run once on them all; its lines go to {}",
            view::task_list(tasks.iter().copied()),
            store::follow_up_log_path(&self.record.project_id)?.display()
        ))
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
                None,
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
        self.stopped = false;
        if self.turn_active || self.record.state == TaskState::Checking {
            self.pending.push_back(text);
            "queued until the current turn ends".into()
        } else {
            self.start_turn(&text, TurnTrigger::Message);
            match self.disk_wait {
                Some(_) => "joined the work that waits for disk".into(),
                None => "started a new turn".into(),
            }
        }
    }

    fn stop(&mut self) -> Result<String> {
        if self.stopped {
            return Ok("already stopped; the next add resumes the same conversation".into());
        }
        if !self.record.state.is_open() || self.record.waits_on(WaitOn::Hand) {
            bail!(
                "task t{} is {}; only an open task that is not worked by hand is stopped",
                self.record.id,
                self.state_words()
            );
        }
        self.stopped = true;
        if let Some(agent) = self.agent.take() {
            self.stop_requested = true;
            agent.stop();
        }
        // ^ The killed process may still have lines in the channel: they belong to no turn.
        // Every source that would start work by itself ends here, so a wake in the same second
        // cannot outlive the stop. A `check_run` stays to record its answer.
        self.generation += 1;
        self.turn_active = false;
        self.limit_wait = None;
        self.limit_queue = None;
        self.limit_slot = None;
        self.disk_wait = None;
        self.disk_queue = None;
        self.rotating = None;
        self.after_ping = None;
        self.after_rebase = None;
        self.background_wait = None;
        self.reported = None;
        self.result_asked = false;
        self.gate = None;
        self.checks_epoch += 1;
        // ^ The result it held is dead with the turn: an add's new result is not its answer's.
        if let Some(run) = &mut self.check_run {
            run.result_waits = false;
        }
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
        self.stopped = false;
        self.turn_active = false;
        self.end_trial("the owner took it by hand");
        let dropped = self.pending.len() as u32;
        self.pending.clear();
        self.disk_wait = None;
        self.disk_queue = None;
        self.rotating = None;
        self.limit_wait = None;
        self.limit_queue = None;
        self.background_wait = None;
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
        self.revise(format!(
            "The owner worked on the task by hand: {commits} commit(s) after r{}.",
            self.record.revision
        ));
        Ok(format!(
            "handed back with {commits} commit(s); the checks run on the next revision"
        ))
    }

    /// A child task landed on this task's branch: its commits become the next revision, as
    /// a handback's do. A parent that is not at rest refused the land, so only a race gets
    /// here otherwise: its next checks, or its own land, check the commits.
    /// ref: docs/tasks.md "Parent tasks"
    fn child_landed(&mut self, child: u32, commits: u32) -> Result<String> {
        self.log(LogEvent::ChildLanded { child, commits });
        if !self.record.at_rest() {
            return Ok(format!("#{child} landed; the next checks include it"));
        }
        self.revise(format!(
            "#{child} landed {commits} commit(s) on this task after r{}.",
            self.record.revision
        ));
        Ok(format!(
            "#{child} landed; the checks run on the next revision"
        ))
    }

    /// Commits made outside the worker's turns become a `done` result whose checks run as
    /// the next revision.
    fn revise(&mut self, summary: String) {
        // ^ The worker's Human Verify items and memo still describe the work.
        let previous = self.current_result();
        let result = WorkerResult {
            status: "done".into(),
            summary,
            human_verify: previous.human_verify,
            memo: previous.memo,
            ..WorkerResult::default()
        };
        self.last_denials.clear();
        self.write_result(&result, &[]);
        self.last_result = Some(result);
        // ^ A failed check goes back to whoever made the commits, not to the worker.
        self.check_retries = MAX_CHECK_RETRIES;
        self.set_state(TaskState::Working);
        self.start_checks();
    }

    fn start_turn(&mut self, text: &str, trigger: TurnTrigger) {
        // ^ Every path that starts a turn ends here: none runs while the owner works by hand.
        if self.record.waits_on(WaitOn::Hand) {
            self.say("by hand: no agent turn starts until handback");
            return;
        }
        // ^ A stopped task starts no turn but an add's (which clears `stopped`): the text joins it.
        if self.stopped {
            self.say("stopped: no agent turn starts until the next add");
            self.notices.push(text.to_string());
            return;
        }
        self.reported = None;
        let carried;
        let text = if self.notices.is_empty() {
            text
        } else {
            carried = format!(
                "{text}\n\n{}",
                self.notices.drain(..).collect::<Vec<_>>().join("\n\n")
            );
            carried.as_str()
        };
        if let Err(error) = self.try_start_turn(text, trigger) {
            let reason = format!("cannot start the agent: {error:#}");
            self.say(&reason);
            self.log(LogEvent::Stopped {
                reason: reason.clone(),
            });
            // ^ As the result, so the ledger and the report name the reason too.
            let result = WorkerResult {
                status: "failed".into(),
                summary: reason,
                ..WorkerResult::default()
            };
            self.write_result(&result, &[]);
            self.last_result = Some(result);
            self.set_state(TaskState::Failed);
        }
    }

    fn try_start_turn(&mut self, text: &str, trigger: TurnTrigger) -> Result<()> {
        // ^ A turn that comes during a ping starts once the ping's answer is in.
        if self.pinging() {
            self.pending.push_back(text.to_string());
            self.after_ping.get_or_insert(trigger);
            return Ok(());
        }
        // ^ A message during a usage limit joins the turn that continues after it.
        if let Some(wait) = &mut self.limit_wait {
            wait.text = format!("{}\n\n{text}", wait.text);
            return Ok(());
        }
        // ^ A fallback's limit another host met, or its spent budget: no paid call to learn it.
        // On Claude the refusal costs nothing and the limit queue orders the resumes.
        // ref: docs/tasks.md "Agents"
        let now = now_unix_ms();
        let limited = (self.record.adapter != "claude")
            .then(|| store::limited_until(&self.account(), now))
            .flatten();
        if let Some(until_ms) = limited {
            if !self.move_after_limit(Some(text), until_ms, now) {
                self.limit_slot = None;
                self.log(LogEvent::LimitWait { until_ms });
                self.wait_for_limit(text.to_string(), until_ms);
            }
            return Ok(());
        }
        // ^ A message during a disk wait joins the held turn, or follows the held checks: it
        // never starts a turn of its own, which would skip the disk queue and drop the held
        // prompt. ref: docs/tasks.md "Free disk"
        match &mut self.disk_wait {
            Some(DiskWait::Turn(waiting)) => {
                *waiting = format!("{waiting}\n\n{text}");
                return Ok(());
            }
            Some(DiskWait::Checks) => {
                self.pending.push_back(text.to_string());
                return Ok(());
            }
            None => {}
        }
        // ^ Not while a check-run waits or owes its answer: the exec would drop the run. Not for
        // the turn after a limit either: the exec would drop the limit slot before the agent
        // answers, so the next task in the limit queue would go first; the next turn follows.
        if self.must_follow_binary() {
            // ^ Every turn gets the replaced binary's launch: this host hands the turn over
            // and execs it once the caller has its answer; the new host resumes the agent.
            let handoff = format!(
                "{HANDOFF_TRIGGER}{}\n{text}",
                serde_json::to_string(&trigger)?
            );
            self.dir.write_file(HANDOFF_FILE, handoff.as_bytes())?;
            self.reexec = true;
            self.set_state(TaskState::Working);
            self.say("the wsx binary was replaced: the host restarts with it");
            return Ok(());
        }
        if self.wait_for_disk(DiskWait::Turn(text.to_string())) {
            return Ok(());
        }
        let handoff;
        let text = match self.rotation_due(trigger) {
            Some(idle) => {
                handoff = self.handoff_request(idle.as_deref());
                self.rotating = Some((text.to_string(), idle));
                handoff.as_str()
            }
            None => text,
        };
        self.send_to_agent(text)?;
        self.turn_text = text.to_string();
        self.turn_active = true;
        self.trigger = trigger;
        self.pings = 0;
        self.set_state(TaskState::Working);
        Ok(())
    }

    /// Starts the agent when it does not run, and sends it one turn's message.
    fn send_to_agent(&mut self, text: &str) -> Result<()> {
        if self.agent.is_none() {
            let (session, resume) = match &self.record.agent_session {
                Some(session) => (session.clone(), true),
                None => (new_session_id()?, false),
            };
            self.generation += 1;
            let generation = self.generation;
            self.stop_requested = false;
            self.agent = Some(Agent::spawn(
                &self.record.adapter,
                &Launch::worker(&self.record, &self.dir.broker_path(), &self.dir.path)?,
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
            .send(text)
    }

    fn pinging(&self) -> bool {
        self.turn_active && self.trigger == TurnTrigger::Ping
    }

    /// When the next ping is due (unix ms): the worker waits on the checks or on a queued
    /// check-run, its last answer is younger than Claude's 1-hour prompt cache, and fewer than
    /// `KEEP_WARM_PINGS` came since its last turn. Never in a usage limit, a disk wait, by
    /// hand, or when the next turn rotates by size anyway. ref: docs/tasks.md "Context rotation"
    fn ping_due(&self) -> Option<u64> {
        let waits = self.record.state == TaskState::Checking
            || self.check_run.as_ref().is_some_and(|run| run.queued);
        // ^ Claude's cache lives 1 hour; Pi's default lives 5 minutes, which no ping keeps.
        if !waits
            || self.stopped
            || self.record.adapter != "claude"
            || self.turn_active
            || self.pings >= KEEP_WARM_PINGS
            || self.answered_ms == 0
            || self.record.agent_session.is_none()
            || self.limit_wait.is_some()
            || self.disk_wait.is_some()
            || self.rotating.is_some()
            || self.reexec
            || self.record.waits_on(WaitOn::Hand)
        {
            return None;
        }
        let config = load_task_config(&self.record.project_path).unwrap_or_default();
        let now = now_unix_ms();
        let cold = now.saturating_sub(self.answered_ms) >= PROMPT_CACHE_MS;
        if config.keep_warm_seconds == 0
            || cold
            || self.context_tokens >= config.rotate_tokens
            || store::limited_until(&self.account(), now).is_some()
        {
            return None;
        }
        Some(self.answered_ms + config.keep_warm_seconds * 1000)
    }

    /// One no-op turn that keeps the prompt cache warm: no step, no result, no checks.
    fn ping(&mut self) {
        if self.ping_due().is_none_or(|due| due > now_unix_ms()) {
            return;
        }
        // ^ Counted before the send: a failed send never loops.
        self.pings += 1;
        match self.send_to_agent(KEEP_WARM_TEXT) {
            Ok(()) => {
                self.turn_active = true;
                self.trigger = TurnTrigger::Ping;
                self.say(&format!(
                    "ping {} of {KEEP_WARM_PINGS}: keeps the prompt cache warm",
                    self.pings
                ));
            }
            Err(error) => self.say(&format!("cannot ping the agent: {error:#}")),
        }
    }

    /// A ping's answer: logged as `ping`, then the turn that came meanwhile starts.
    fn on_ping(&mut self, turn: TurnResult) {
        self.turn_active = false;
        let now = now_unix_ms();
        if turn.usage.context_tokens > 0 {
            self.context_tokens = turn.usage.context_tokens;
        }
        // ^ A limit is no answer, and the ping waits for nothing: Claude's refusal costs
        // nothing, and the next real turn meets the limit and waits as usual.
        if turn.limit.is_none() {
            // ^ The cache is warm again: the idle rotation counts from here.
            self.answered_ms = now;
        }
        if let Some(message) = super::agent::note_spend(
            &self.record.adapter,
            &self.record.model,
            turn.usage.cost_micro_usd,
            now,
        ) {
            self.say(&message);
        }
        self.log(LogEvent::Ping {
            usage: Some(turn.usage),
        });
        self.after_ping_ends();
    }

    /// What came during a ping starts now; a message held for the checks keeps waiting.
    fn after_ping_ends(&mut self) {
        let after = self.after_ping.take();
        if after.is_some() || self.record.state != TaskState::Checking {
            self.deliver_pending(after.unwrap_or(TurnTrigger::Message));
        }
    }

    fn on_agent(&mut self, line: Option<AgentLine>) {
        match line {
            Some(AgentLine::Init(session)) => {
                if self.record.agent_session.as_deref() != Some(&session) {
                    self.record.agent_session = Some(session);
                    self.save();
                }
            }
            Some(AgentLine::Progress { text, said }) => {
                // ^ The agent answers again: the next task in the limit queue may try.
                self.limit_slot = None;
                self.say(&text);
                // ^ A ping's one word is no progress of the task.
                if !self.pinging() {
                    self.log(LogEvent::Agent { text, said });
                }
            }
            Some(AgentLine::Result(mut result)) => {
                if result.structured.is_none() {
                    result.structured = self.reported.take();
                }
                self.on_result(*result)
            }
            None => {
                self.agent = None;
                // ^ A handoff turn that never ended leaves no notes: no later result is one.
                self.rotating = None;
                // ^ A ping lost with its process costs the task nothing: the next turn resumes.
                if self.pinging() {
                    self.turn_active = false;
                    self.after_ping_ends();
                } else if self.turn_active && !self.stop_requested {
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
        if turn.answers_nothing() {
            self.log(LogEvent::Agent {
                said: false,
                text: "claude ended its own turn with no result: ignored".into(),
            });
            return;
        }
        if self.pinging() {
            return self.on_ping(turn);
        }
        // ^ A result is the agent answering again, also one that waits for background jobs.
        self.limit_slot = None;
        let Some(turn) = self.wait_for_background(turn) else {
            return;
        };
        self.turn_active = false;
        self.answered_ms = now_unix_ms();
        // ^ An error without a model call (a session limit) says nothing of the context.
        if turn.usage.context_tokens > 0 {
            self.context_tokens = turn.usage.context_tokens;
        }
        // ^ A usage limit is no result: no checks, no check retry. The same session
        // continues once it resets. ref: docs/tasks.md "Claude adapter"
        if let Some(limit) = &turn.limit {
            self.limit_attempt += 1;
            let now = now_unix_ms();
            let until_ms = limit::resume_at(limit.resets_at, self.limit_attempt, now);
            let backoff = limit::known_reset(limit.resets_at, now).is_none();
            if let Err(error) = store::mark_limited(&self.account(), until_ms, now, backoff) {
                self.say(&format!("cannot record the usage limit: {error:#}"));
            }
            if self.move_after_limit(None, until_ms, now) {
                return;
            }
            let mut text = vec![LIMIT_CONTINUE.to_string()];
            text.extend(self.pending.drain(..));
            self.log(LogEvent::LimitWait { until_ms });
            self.wait_for_limit(text.join("\n\n"), until_ms);
            return;
        }
        self.limit_attempt = 0;
        if let Some(message) = super::agent::note_spend(
            &self.record.adapter,
            &self.record.model,
            turn.usage.cost_micro_usd,
            now_unix_ms(),
        ) {
            self.say(&message);
        }
        if let Some((waiting, idle)) = self.rotating.take() {
            self.log(LogEvent::Agent {
                said: false,
                text: format!("handoff notes: {}", first_line(&turn.text)),
            });
            self.rotate(waiting, idle);
            return;
        }
        let structured = turn
            .structured
            .and_then(|value| serde_json::from_value::<WorkerResult>(value).ok());
        // ^ Each such job's notice later starts Claude's own turn: the log counts them.
        // ref: docs/tasks.md "Claude adapter"
        if structured.is_some() && turn.background > 0 {
            self.log(LogEvent::Agent {
                said: false,
                text: format!(
                    "the result came while {} background job(s) run",
                    turn.background
                ),
            });
        }
        // ^ A model turn that left out its result is asked for it once.
        if structured.is_none()
            && !turn.is_error
            && !std::mem::replace(&mut self.result_asked, true)
        {
            self.say("the turn ended without a result: asked for it once");
            self.start_turn(RESULT_RETRY, TurnTrigger::ResultRetry);
            return;
        }
        self.result_asked = false;
        self.background_wait = None;
        let mut result = structured.unwrap_or_else(|| WorkerResult {
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
        let reexec = std::mem::take(&mut self.carried);
        self.log(LogEvent::Result {
            status: result.status.clone(),
            summary: result.summary.clone(),
            usage: Some(turn.usage.clone()),
            denials: turn.denials.clone(),
            step: self
                .record
                .steps
                .get(self.record.step as usize)
                .map(|step| step.name.clone()),
            trigger: Some(self.trigger),
            reexec,
        });
        self.say(&format!(
            "result {}: {}",
            result.status,
            first_line(&result.summary)
        ));
        self.last_denials = turn.denials.clone();
        self.write_result(&result, &[]);
        self.last_result = Some(result.clone());
        // ^ A result before its own check-run's answer waits for it: no second run queues.
        // ref: docs/tasks.md "Checks, revisions and report"
        if let Some(run) = &mut self.check_run {
            if let Some(answer) = run.answer.take() {
                self.check_run = None;
                self.answer_check_run(answer);
                return;
            }
            run.result_waits = true;
            self.say("the result waits for its check-run");
            return;
        }
        self.settle_result();
    }

    /// What follows the worker's result, held in `last_result`: the block, plan and step
    /// rules, then the checks.
    fn settle_result(&mut self) {
        let Some(result) = self.last_result.clone() else {
            return;
        };
        // ^ Claude's own turn (a background task's notice) can answer while the checks of an
        // earlier result run: those checks answer it, never a second run beside them.
        if let Some((_, newer)) = &mut self.gate {
            *newer = true;
        }
        // ^ Only the worker's own word blocks a task. Denials it worked around are evidence.
        if result.status == "blocked" {
            self.log(LogEvent::Blocked {
                denials: self.last_denials.clone(),
            });
            self.wait(WaitOn::Owner, first_line(&result.summary).to_string());
            // ^ A worker stopped by a full disk continues once there is space again.
            self.wait_for_disk(DiskWait::Turn(
                "The disk was full; there is space again. Continue the task where it stopped."
                    .into(),
            ));
            return;
        }
        // ^ A plan has nothing to check: the manager reads it and continues the task with an
        // add. ref: docs/tasks.md "Workflow"
        if self.record.plan {
            self.record.plan = false;
            self.wait(WaitOn::Plan, "the worker wrote its plan".into());
            return;
        }
        // ^ A done step that is not the last starts the next one, with any queued message.
        // ref: docs/tasks.md "Workflow"
        if result.status == "done" && (self.record.step as usize) + 1 < self.record.steps.len() {
            self.record.step += 1;
            self.save();
            let mut text: Vec<String> = self.pending.drain(..).collect();
            text.extend(self.step_text());
            self.start_turn(&text.join("\n\n"), TurnTrigger::Step);
            return;
        }
        // ^ What a test, an assert or a capture shows is the worker's to prove: such an item
        // goes back once, within the check retries. ref: docs/tasks.md "To check"
        let items: Vec<&str> = result
            .human_verify
            .iter()
            .filter(|item| item.is_for_worker())
            .map(|item| item.check.as_str())
            .collect();
        if result.status == "done"
            && !items.is_empty()
            && self.pending.is_empty()
            && self.check_retries < MAX_CHECK_RETRIES
        {
            self.check_retries += 1;
            self.say("human_verify has items the worker proves: sent back to the worker");
            let text = format!(
                "These human_verify items are yours to prove: {}. A command, a test, an assert \
                 or a capture is yours: stage the state it needs in a fixture, run it and put \
                 what it shows in evidence. Keep in human_verify only how a screen looks or \
                 feels, with a capture of that state; an item without a capture is only for a \
                 real device or the owner's real data, and its where says so. Then finish as \
                 before.",
                items.join(" | ")
            );
            self.start_turn(&text, TurnTrigger::Verify);
            return;
        }
        if self.gate.is_some() {
            self.say("the checks of an earlier result still run: they answer this one");
            if self.record.state != TaskState::Checking {
                self.set_state(TaskState::Checking);
            }
            return;
        }
        self.start_checks();
    }

    /// A turn without a result that ends while its background jobs run is no missing result:
    /// it waits for Claude's own turn after they end, at most `tasks.checkTimeoutSeconds`
    /// from the first such turn. ref: docs/tasks.md "Claude adapter"
    fn wait_for_background(&mut self, turn: TurnResult) -> Option<TurnResult> {
        if turn.background == 0
            || turn.is_error
            || turn.structured.is_some()
            || self.rotating.is_some()
        {
            return Some(turn);
        }
        let now = Instant::now();
        let bound = Duration::from_secs(self.record.check_timeout_secs);
        let wait = self.background_wait.get_or_insert(BackgroundWait {
            deadline: now + bound,
            turn: None,
        });
        if wait.deadline <= now {
            return Some(turn);
        }
        let text = format!(
            "the turn ended while {} background job(s) run: waiting for their notice",
            turn.background
        );
        wait.turn = Some(turn);
        self.say(&text);
        self.log(LogEvent::Agent { text, said: false });
        None
    }

    /// No notice came in time: the waiting turn ended without its result.
    fn background_timed_out(&mut self) {
        let Some(mut turn) = self
            .background_wait
            .as_mut()
            .and_then(|wait| wait.turn.take())
        else {
            return;
        };
        // ^ Claude's own turn after a result, or an agent that ended: no turn of the host's
        // waits, and the next one gets its own bound.
        if !self.turn_active {
            self.background_wait = None;
            return;
        }
        let text = format!(
            "no notice from {} background job(s) in {} s: the turn ended without a result",
            turn.background, self.record.check_timeout_secs
        );
        self.say(&text);
        self.log(LogEvent::Agent { text, said: false });
        turn.background = 0;
        self.on_result(turn);
    }

    /// Whether the next turn goes to a fresh conversation, after the handoff notes: the
    /// context reached `tasks.agent.rotateTokens`, or it is at least half of that and the
    /// agent was idle past `tasks.agent.rotateIdleSeconds` (`Some(Some(reason))`), since the
    /// prompt cache is gone then and a resume writes the whole context again. Never for the
    /// result the last turn left out, nor while a `check-run` waits (its answer goes to this
    /// conversation).
    fn rotation_due(&self, trigger: TurnTrigger) -> Option<Option<String>> {
        let config = load_task_config(&self.record.project_path).unwrap_or_default();
        if self.rotating.is_some()
            || self.check_run.is_some()
            || trigger == TurnTrigger::ResultRetry
            || self.record.agent_session.is_none()
        {
            return None;
        }
        if self.context_tokens >= config.rotate_tokens {
            return Some(None);
        }
        let idle_ms = now_unix_ms().saturating_sub(self.answered_ms);
        (config.rotate_idle_seconds > 0
            && self.answered_ms > 0
            && idle_ms > config.rotate_idle_seconds * 1000
            && self.context_tokens >= config.rotate_tokens / 2)
            .then(|| Some(format!("idle {} min", idle_ms / 60_000)))
    }

    fn handoff_request(&self, idle: Option<&str>) -> String {
        self.say(&format!(
            "context {}k{}: the worker writes handoff notes, then a fresh conversation continues",
            self.context_tokens / 1000,
            idle.map(|idle| format!(", {idle}")).unwrap_or_default()
        ));
        let why = match idle {
            Some(_) => {
                "this conversation was idle past this task's limit, so its prompt cache is gone"
            }
            None => "past this task's limit",
        };
        format!(
            "Your context is {}k tokens, {why}. This conversation ends after \
             this turn and a fresh one continues the task; your working copy and commits stay. \
             Do no other work now. Write {} for that fresh conversation, at most 12 KB: what \
             the task is doing now, what is done, what remains and the next action, decisions \
             and why, the files and functions that matter, the checks you ran and how they \
             ended, and what you learned that the code does not show. Then return your result: \
             status done and a one-line summary. That result is not checked.",
            self.context_tokens / 1000,
            self.dir.file(ROTATE_NOTES).display()
        )
    }

    /// End the conversation after its handoff notes and start `waiting` in a fresh one,
    /// with the task's prompt, the notes and the current step.
    fn rotate(&mut self, waiting: String, reason: Option<String>) {
        let tokens = self.context_tokens;
        if let Some(agent) = self.agent.take() {
            self.stop_requested = true;
            agent.stop();
        }
        self.record.agent_session = None;
        self.context_tokens = 0;
        self.save();
        self.log(LogEvent::Rotated { tokens, reason });
        // ^ Notes too long to inline are read from their file.
        let notes = match self.dir.read_file(ROTATE_NOTES, MAX_ROTATE_NOTES_BYTES) {
            Ok(Some(bytes)) if !bytes.trim_ascii().is_empty() => format!(
                "The earlier conversation's handoff notes:\n{}",
                String::from_utf8_lossy(&bytes)
            ),
            Ok(_) => "The earlier conversation wrote no handoff notes.".into(),
            Err(_) => format!(
                "Read the earlier conversation's handoff notes first: {}",
                self.dir.file(ROTATE_NOTES).display()
            ),
        };
        let mut text = vec![format!(
            "You continue a task whose earlier conversation reached {}k tokens of context and \
             was closed. Its working copy and commits are as that conversation left them; see \
             them with git status, git log and git diff. Load the project knowledge you need \
             again (the recall or seek skill when you have it), and read only the parts of \
             files you need.",
            tokens / 1000
        )];
        if let Ok(Some(prompt)) = self.dir.prompt() {
            text.push(format!("The task as it was asked:\n{prompt}"));
        }
        text.push(notes);
        let index = self.record.step as usize;
        if let Some(step) = self.record.steps.get(index) {
            if !waiting.contains(&step.prompt) {
                text.push(format!(
                    "You are in step {} of {}, {}: {}",
                    index + 1,
                    self.record.steps.len(),
                    step.name,
                    step.prompt
                ));
            }
        }
        text.push(waiting);
        text.extend(self.pending.drain(..));
        self.start_turn(&text.join("\n\n"), TurnTrigger::Rotation);
    }

    /// The host's task context for the first turn: the task's files, the open tasks that
    /// share them and the manager's `context.md` lines that name them, so the worker needs no
    /// turn of its own to load context. None when it has nothing. ref: docs/tasks.md "Workflow"
    fn task_context(&self) -> Option<String> {
        let mut out = Vec::new();
        if !self.record.files.is_empty() {
            out.push(format!("Files: {}", self.record.files.join(", ")));
        }
        if let Ok(tasks) = view::project_tasks(&self.record.project_id) {
            let records: Vec<&TaskRecord> = tasks
                .iter()
                .filter_map(|(_, record)| record.as_ref().ok())
                .collect();
            for row in view::overlaps(&self.record, &records, true)
                .as_array()
                .into_iter()
                .flatten()
            {
                let Some(other) = records.iter().find(|other| row["id"] == other.id) else {
                    continue;
                };
                let paths: Vec<&str> = row["paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|path| path.as_str())
                    .collect();
                out.push(format!(
                    "Open task #{} \"{}\" ({}) shares: {}",
                    other.id,
                    other.title.as_deref().unwrap_or(""),
                    other.state.as_str(),
                    paths.join(", ")
                ));
            }
        }
        let words = file_words(&self.record.files);
        let notes = store::ManagerDir::existing(&self.record.project_id)
            .ok()
            .flatten()
            .map(|dir| {
                super::manager::read_note(
                    &dir.path.join("notes").join(super::manager::CONTEXT_FILE),
                )
            })
            .unwrap_or_default();
        let mut room = MAX_TASK_CONTEXT_BYTES;
        let mut lines = Vec::new();
        // ^ Newest first: the host appends each landed task's memo.
        for line in notes.lines().rev() {
            if !words.iter().any(|word| line.contains(word.as_str())) {
                continue;
            }
            if line.len() > room {
                break;
            }
            room -= line.len();
            lines.push(line);
        }
        if !lines.is_empty() {
            out.push(format!(
                "Project memos (manager context.md) about these files:\n{}",
                lines.join("\n")
            ));
        }
        (!out.is_empty()).then(|| format!("Task context from the host:\n{}", out.join("\n")))
    }

    /// The turn text of the current workflow step, when the task has steps.
    fn step_text(&self) -> Option<String> {
        let total = self.record.steps.len();
        let index = self.record.step as usize;
        let step = self.record.steps.get(index)?;
        let end = match index + 1 == total {
            true => {
                "This is the last step: finish the task and return your final result, with \
                 its memo."
            }
            false => {
                "Do only this step. When it is done, return status done with what it found \
                      or did; the next step follows."
            }
        };
        self.say(&format!("step {} of {total}: {}", index + 1, step.name));
        // ^ A job that ends after the result wakes the worker for a turn that answers nothing
        // new. ref: docs/tasks.md "Workflow"
        Some(format!(
            "Step {} of {total}, {}: {}\n{end} Before the result, wait for or stop each \
             background job you started.",
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
            (Some(DiskWait::Turn(waiting)), DiskWait::Checks) => DiskWait::Turn(waiting),
            (_, what) => what,
        });
        self.log(LogEvent::DiskWait { free_mb, min_gb });
        let reason = disk_full(free_mb, min_gb);
        self.say(&format!("waiting for disk space: {reason}"));
        self.wait(WaitOn::Disk, reason);
        true
    }

    /// Resume once there is space. Tasks resume one at a time, oldest wait first, each
    /// `DISK_SETTLE` after the one before: resumed at once, they would fill the disk again.
    /// ref: docs/tasks.md "Checks, revisions and report"
    fn resume_after_disk(&mut self) {
        if self.low_disk().is_some() {
            return;
        }
        if let Some(queue) = &self.disk_queue {
            let label = format!("{} t{}", self.record.project, self.record.id);
            match queue.try_take(&label) {
                Ok(taken) => self.disk_slot = Some((taken, Instant::now())),
                Err(reason) => {
                    if self.record.waiting.as_ref().map(|w| &w.reason) != Some(&reason) {
                        self.say(&reason);
                        self.wait(WaitOn::Disk, reason);
                    }
                    return;
                }
            }
        }
        self.disk_queue = None;
        let Some(what) = self.disk_wait.take() else {
            return;
        };
        match what {
            DiskWait::Turn(text) => self.start_turn(&text, TurnTrigger::Disk),
            DiskWait::Checks => self.start_checks(),
        }
    }

    /// Hold the turn `text` until `until_ms`, in the machine's limit queue.
    fn wait_for_limit(&mut self, text: String, until_ms: u64) {
        // ^ A wait on a fallback also looks for the task's own agent coming free.
        let next_ms = match self.record.adapter.as_str() {
            "claude" => until_ms,
            _ => until_ms.min(now_unix_ms() + HOME_POLL_MS),
        };
        self.limit_wait = Some(LimitWait {
            text,
            until_ms,
            next_ms,
        });
        let reason = limit::reason(&self.record.adapter, until_ms, now_unix_ms());
        self.say(&reason);
        self.wait(WaitOn::Limit, reason);
    }

    /// After a limit on this task's account until `until_ms`, move to the agent
    /// `worker_next` names, continuing `unfinished` (else the current turn), and say whether it
    /// moved; else the caller waits. One conversation is worth more than a switch: a limit
    /// within `maxWaitMinutes` is waited out. ref: docs/tasks.md "Agents"
    fn move_after_limit(&mut self, unfinished: Option<&str>, until_ms: u64, now: u64) -> bool {
        let settings = load_task_config(&self.record.project_path).unwrap_or_default();
        let Some(next) = super::agent::worker_next(
            &settings,
            &self.record.adapter,
            &self.record.model,
            until_ms,
            now,
        ) else {
            return false;
        };
        if let Some(text) = unfinished {
            self.turn_text = text.to_string();
        }
        self.switch_agent(next, until_ms, now);
        true
    }

    /// The account this task's turns run on.
    fn account(&self) -> String {
        super::agent::account(&self.record.adapter, &self.record.model)
    }

    /// Continue after a usage limit. Tasks resume one at a time, oldest wait first: each
    /// holds the limit slot until its agent answers, so a limit that has not lifted stops
    /// only one. ref: docs/tasks.md "Claude adapter"
    fn resume_after_limit(&mut self) {
        let now = now_unix_ms();
        // ^ On a fallback, a wait ends as soon as the task's own agent is free.
        if let Some(wait) = self.limit_wait.take() {
            if self.move_after_limit(Some(&wait.text), wait.until_ms, now) {
                return;
            }
            self.limit_wait = Some(wait);
        }
        let Some(wait) = self.limit_wait.as_mut().filter(|wait| wait.next_ms <= now) else {
            return;
        };
        if now < wait.until_ms {
            wait.next_ms = wait.until_ms.min(now + HOME_POLL_MS);
            return;
        }
        if let Some(queue) = &self.limit_queue {
            let label = format!("{} t{}", self.record.project, self.record.id);
            match queue.try_take(&label) {
                Ok(taken) => self.limit_slot = Some(taken),
                Err(reason) => {
                    wait.next_ms = now + LIMIT_POLL_MS;
                    let reason = format!("usage limit has reset; {reason}");
                    if self.record.waiting.as_ref().map(|w| &w.reason) != Some(&reason) {
                        self.say(&reason);
                        self.wait(WaitOn::Limit, reason);
                    }
                    return;
                }
            }
        }
        self.limit_queue = None;
        if let Some(wait) = self.limit_wait.take() {
            self.say("the usage limit has reset: the agent continues");
            self.start_turn(&wait.text, TurnTrigger::Limit);
        }
    }

    /// Free MiB and the GB a task needs, when less than that is free.
    fn low_disk(&self) -> Option<(u64, u64)> {
        let min_gb =
            load_task_config(&self.record.project_path).map_or(5, |settings| settings.min_free_gb);
        store::free_mb(&self.record.working_copy)
            .filter(|free_mb| *free_mb < min_gb * 1024)
            .map(|free_mb| (free_mb, min_gb))
    }

    /// Wait for `on`: nothing runs until it happens. ref: docs/tasks.md "Store"
    fn wait(&mut self, on: WaitOn, reason: String) {
        let queue = match on {
            WaitOn::Disk => Some((&mut self.disk_queue, "disk")),
            WaitOn::Limit => Some((&mut self.limit_queue, "limit")),
            _ => None,
        };
        if let Some((queue @ None, what)) = queue {
            match slot::Ticket::join(&format!("{what}-slot"), what, Priority::Normal) {
                Ok(ticket) => *queue = Some(ticket),
                Err(error) => self.say(&format!("cannot join the {what} queue: {error}")),
            }
        }
        // ^ A host that restarts during a limit wait resumes at the same time.
        let until_ms = match on {
            WaitOn::Limit => self.limit_wait.as_ref().map(|wait| wait.until_ms),
            _ => None,
        };
        self.record.waiting = Some(Waiting {
            on,
            reason,
            until_ms,
        });
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
    /// Continue the task on `next` in a new conversation: the limited agent wrote no notes,
    /// so the turn it could not finish opens it after the usual rotation context. The task
    /// stays on `next` until it ends: a second switch would cost another cold start.
    /// ref: docs/tasks.md "Agents"
    fn switch_agent(&mut self, next: super::agent::Choice, until_ms: u64, now: u64) {
        let from = format!("{}/{}", self.record.adapter, self.record.model);
        let to = format!("{} ({}/{})", next.name, next.adapter, next.model);
        let reason = format!(
            "{} usage limit until {}",
            self.record.adapter,
            limit::resume_time(until_ms, now)
        );
        self.say(&format!("switched to {to}: {reason}"));
        self.log(LogEvent::AgentSwitch {
            from,
            to,
            reason: reason.clone(),
        });
        let unfinished = match self.rotating.take() {
            Some((waiting, _)) => waiting,
            None => std::mem::take(&mut self.turn_text),
        };
        self.record.adapter = next.adapter;
        self.record.model = next.model;
        self.limit_attempt = 0;
        let waiting = format!(
            "The earlier conversation's agent reached its usage limit during this turn and \
             could write no handoff notes; another agent continues. Check what the working copy \
             already holds before you redo anything. The turn it could not finish:\n{unfinished}"
        );
        self.rotate(waiting, Some(reason));
    }

    /// Keep a reported result for the turn that ends next; the last one of a turn counts.
    fn reported(&mut self, text: Option<String>) -> broker::Response {
        let Some(text) = text.filter(|_| self.turn_active) else {
            return broker::refused(
                "`wsx task result -` takes the result JSON on stdin during a turn".into(),
            );
        };
        match serde_json::from_str::<serde_json::Value>(&text).map(|value| {
            let valid = serde_json::from_value::<WorkerResult>(value.clone())
                .map(|result| matches!(result.status.as_str(), "done" | "blocked" | "failed"));
            (value, valid)
        }) {
            Ok((value, Ok(true))) => {
                self.reported = Some(value);
                broker::Response {
                    stdout: "result kept: it counts when this turn ends; end the turn now\n".into(),
                    stderr: String::new(),
                    code: 0,
                }
            }
            Ok((_, Ok(false))) => broker::refused("status must be done, blocked or failed".into()),
            Ok((_, Err(error))) | Err(error) => {
                broker::refused(format!("the result is not valid: {error}"))
            }
        }
    }

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
        // ^ A late call after the result: the checks of that result run now, in one slot.
        if self.record.state == TaskState::Checking {
            return refuse("the checks after your result run now".into());
        }
        self.follow_checks();
        // ^ The scenarios come after the checks, numbered on from them, and run only when
        // named: like the checks, they are the main checkout's, never the worker's.
        let scenarios = load_task_config(&self.record.project_path)
            .map(|settings| settings.scenarios)
            .unwrap_or_default();
        let mut runnable = self.record.checks.clone();
        runnable.extend(scenarios.values().cloned());
        let checks = self.record.checks.len();
        let picked: Result<Vec<(usize, String)>, String> = match numbers {
            [] => Ok(runnable.iter().take(checks).cloned().enumerate().collect()),
            numbers => numbers
                .iter()
                .map(|word| {
                    word.parse::<usize>()
                        .ok()
                        .filter(|n| (1..=runnable.len()).contains(n))
                        .map(|n| n - 1)
                        .or_else(|| {
                            scenarios
                                .keys()
                                .position(|name| name == word)
                                .map(|at| checks + at)
                        })
                        .map(|index| (index, runnable[index].clone()))
                        .ok_or_else(|| {
                            let names: Vec<&str> = scenarios.keys().map(String::as_str).collect();
                            format!(
                                "{word} is not a check number 1..={} or a scenario ({})",
                                runnable.len(),
                                if names.is_empty() {
                                    "none".into()
                                } else {
                                    names.join(", ")
                                }
                            )
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
        if let Some(run) = &mut self.check_run {
            // ^ A queued run answers only after the result, also when it already ended.
            if run.queued && run.indexes == indexes {
                let _ = reply.send(queued_response(run.ci));
                return;
            }
            if run.indexes != indexes {
                let numbers: Vec<String> =
                    run.indexes.iter().map(|i| (i + 1).to_string()).collect();
                return refuse(format!(
                    "a check-run of checks {} still runs; run it again to wait for its answer",
                    numbers.join(" ")
                ));
            }
            run.replies.push(reply);
            self.say("check-run: joined the run of the same checks");
            return;
        }
        let working_copy = self.record.working_copy.clone();
        let (tree, stored, to_run) = self.kept_checks(&picked);
        if to_run.is_empty() {
            self.say("check-run: every check already ran on this tree");
            let _ = reply.send(check_run_response(&picked, &stored, &BTreeMap::new()));
            return;
        }
        if let Some((free_mb, min_gb)) = self.low_disk() {
            return refuse(format!(
                "not run, disk space is low: {}",
                disk_full(free_mb, min_gb)
            ));
        }
        // ^ A `ci:` scenario runs on CI: it takes no slot and answers `queued` at once.
        // ref: docs/tasks.md "Scenarios"
        let ci = to_run
            .iter()
            .any(|(_, command)| ci_command(command).is_some());
        self.check_run = Some(CheckRun {
            indexes,
            replies: vec![reply],
            result_waits: false,
            queued: false,
            answer: None,
            label: picked
                .iter()
                .map(|(index, _)| match index.checked_sub(checks) {
                    Some(at) => scenarios.keys().nth(at).cloned().unwrap_or_default(),
                    None => (index + 1).to_string(),
                })
                .collect::<Vec<_>>()
                .join(" "),
            ci,
        });
        // ^ A run of checks that only format or compile, or only runs on CI, is light: it takes
        // no slot. One that names a local scenario takes the scenarios lane, else the checks lane.
        let local: Vec<&(usize, String)> = to_run
            .iter()
            .filter(|(_, command)| ci_command(command).is_none())
            .collect();
        let lane = match local.iter().any(|(index, _)| *index >= checks) {
            true => Lane::Scenarios,
            false => Lane::Checks,
        };
        let priority =
            (!local.iter().all(|(_, command)| light(command))).then_some((lane, Priority::Normal));
        let events = self.events.clone();
        self.spawn_checks(priority, "check-run", to_run.clone(), move |results| {
            let ran: BTreeMap<usize, CheckResult> = to_run
                .iter()
                .map(|(index, _)| *index)
                .zip(results.iter().cloned())
                .collect();
            let response = check_run_response(&picked, &stored, &ran);
            // ^ A tree that changed during the run, a timeout or a full disk is not stored.
            let unchanged = tree.filter(|tree| {
                copy::clean_tree(&working_copy).ok().flatten().as_ref() == Some(tree)
            });
            let ran = ran
                .into_iter()
                .filter(|(_, check)| check.exit.is_some() && !check.output.contains(NO_SPACE))
                .collect();
            let _ = events.send(Event::CheckRunDone(
                results,
                response,
                unchanged.map(|tree| (tree, ran)),
            ));
        });
        if ci {
            self.say("check-run: a scenario runs on CI; its answer comes after the result");
            if let Some(run) = &mut self.check_run {
                run.queued = true;
                for reply in run.replies.drain(..) {
                    let _ = reply.send(queued_response(true));
                }
            }
        }
    }

    /// A check that ran on this clean tree with the same command gives the same answer: the
    /// clean tree, those kept results (`stored: true`, no wait) and the checks left to run.
    fn kept_checks(&self, checks: &[(usize, String)]) -> KeptChecks {
        let tree = copy::clean_tree(&self.record.working_copy).ok().flatten();
        let kept = match (&tree, &self.stored_checks) {
            (Some(tree), Some((on, results))) if tree == on => checks
                .iter()
                .filter_map(|(index, command)| {
                    let check = results
                        .get(index)
                        .filter(|check| check.command == *command)?;
                    let check = CheckResult {
                        queued_ms: 0,
                        stored: true,
                        ..check.clone()
                    };
                    Some((*index, check))
                })
                .collect(),
            _ => BTreeMap::new(),
        };
        let to_run = checks
            .iter()
            .filter(|(index, _)| !kept.contains_key(index))
            .cloned()
            .collect();
        (tree, kept, to_run)
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
        self.checks_epoch += 1;
        let epoch = self.checks_epoch;
        self.gate = Some((head(&self.record.working_copy), false));
        let priority = match self.after_rebase {
            Some(AfterRebase::Land { .. }) => (Lane::Checks, Priority::Land),
            _ => (Lane::Checks, Priority::Normal),
        };
        let events = self.events.clone();
        let checks: Vec<(usize, String)> = self.record.checks.iter().cloned().enumerate().collect();
        // ^ #137 ran fmt, build and nextest twice on one tree.
        let (_, mut stored, to_run) = self.kept_checks(&checks);
        if to_run.is_empty() {
            self.say("every check already ran on this tree");
            let _ = events.send(Event::ChecksDone(epoch, stored.into_values().collect()));
            return;
        }
        let indexes: Vec<usize> = to_run.iter().map(|(index, _)| *index).collect();
        self.spawn_checks(Some(priority), "check", to_run, move |results| {
            stored.extend(indexes.into_iter().zip(results));
            let _ = events.send(Event::ChecksDone(epoch, stored.into_values().collect()));
        });
    }

    /// Run `checks` in a thread, holding a machine check slot (lane and priority) unless `priority` is
    /// `None`; each check's timeout counts from the slot. A `ci:` command takes no slot: it starts
    /// at once beside the others, with `CI_TIMEOUT`. Check `n`'s whole output goes to
    /// `<kind>-<n>.log` in the task folder. ref: docs/tasks.md "Checks, revisions and report"
    fn spawn_checks(
        &self,
        priority: Option<(Lane, Priority)>,
        kind: &str,
        checks: Vec<(usize, String)>,
        done: impl FnOnce(Vec<CheckResult>) + Send + 'static,
    ) {
        let timeout = Duration::from_secs(self.record.check_timeout_secs);
        let working_copy = self.record.working_copy.clone();
        let env = self.record.env.clone();
        let born = std::time::UNIX_EPOCH + Duration::from_millis(self.record.created_unix_ms);
        let events = self.events.clone();
        let label = format!("{} t{}", self.record.project, self.record.id);
        let checks = checks
            .into_iter()
            .map(|(index, command)| (command, self.dir.file(&format!("{kind}-{}.log", index + 1))))
            .collect::<Vec<_>>();
        let (remote, local): (Vec<_>, Vec<_>) = checks
            .into_iter()
            .enumerate()
            .partition(|(_, (command, _))| ci_command(command).is_some());
        let priority = priority.filter(|_| !local.is_empty());
        std::thread::spawn(move || {
            let on_ci = {
                let (working_copy, env) = (working_copy.clone(), env.clone());
                std::thread::spawn(move || {
                    remote
                        .into_iter()
                        .map(|(at, (command, log))| {
                            let ran = run_check(
                                &working_copy,
                                ci_command(&command).unwrap_or_default(),
                                &env,
                                CI_TIMEOUT,
                                &log,
                            );
                            (at, CheckResult { command, ..ran })
                        })
                        .collect::<Vec<_>>()
                })
            };
            let mut results = {
                let queued = std::time::Instant::now();
                let _slot = priority.and_then(|(lane, priority)| {
                    let taken = slot::take(lane, priority, &label, |reason| {
                        let _ = events.send(Event::CheckQueued(reason));
                    });
                    let error = taken.as_ref().err().map(ToString::to_string);
                    let _ = events.send(Event::CheckSlot(error));
                    taken.ok()
                });
                // ^ The wait is the run's: each of its results carries it.
                let queued_ms = queued.elapsed().as_millis() as u64;
                local
                    .into_iter()
                    .map(|(at, (command, log))| {
                        let ran = run_check(&working_copy, &command, &env, timeout, &log);
                        (at, CheckResult { queued_ms, ..ran })
                    })
                    .collect::<Vec<_>>()
            };
            results.extend(on_ci.join().unwrap_or_default());
            results.sort_by_key(|(at, _)| *at);
            build::prune(&working_copy, born);
            done(results.into_iter().map(|(_, result)| result).collect());
        });
    }

    /// A check-run that ended after the worker's result, which is held. A pass on an unchanged tree
    /// starts no turn: on the last step the host records it as evidence in the held result,
    /// on an earlier one the next step's prompt carries it. Else (a failure, a result that is
    /// not `done`) the worker gets it as a turn. ref: docs/tasks.md "Check-run"
    fn answer_check_run(&mut self, answer: CheckAnswer) {
        let done = self
            .last_result
            .as_mut()
            .filter(|result| result.status == "done");
        let (Some(proof), Some(result)) = (answer.proof, done) else {
            return self.send_check_answer(answer.text);
        };
        if (self.record.step as usize) + 1 < self.record.steps.len() {
            self.pending.push_back(answer.text);
        } else {
            result.evidence.push(proof);
            let held = result.clone();
            self.write_result(&held, &[]);
        }
        self.say("check-run passed after the result: no turn for its answer");
        self.settle_result();
    }

    /// A check-run's answer goes to the worker as one turn, after its current turn if any.
    fn send_check_answer(&mut self, answer: String) {
        self.say("check-run: its answer goes to the worker");
        self.pending.push_back(answer);
        if !self.turn_active {
            self.deliver_pending(TurnTrigger::CheckRun);
        } else if self.pinging() {
            self.after_ping.get_or_insert(TurnTrigger::CheckRun);
        }
    }

    /// The checks wait for the slot: the record shows it while the task works or checks.
    fn check_queued(&mut self, reason: String) {
        // ^ A worker's check-run (never beside the checks after a result) answers at once.
        if let Some(run) = self
            .check_run
            .as_mut()
            .filter(|_| self.record.state != TaskState::Checking)
        {
            run.queued = true;
            for reply in run.replies.drain(..) {
                let _ = reply.send(queued_response(run.ci));
            }
        }
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
            until_ms: None,
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

    fn on_checks(&mut self, epoch: u64, results: Vec<CheckResult>) {
        // ^ A run a stop left running: its output is logged, it decides nothing.
        if epoch != self.checks_epoch {
            self.log(LogEvent::Checks { results });
            return;
        }
        // ^ A result that came during the checks: a block keeps its wait, a turn that runs
        // gives the next result, and a new commit runs them once more.
        // ref: docs/tasks.md "Checks, revisions and report"
        if let Some((commit, true)) = self.gate.take() {
            let blocked = self
                .last_result
                .as_ref()
                .is_some_and(|r| r.status == "blocked");
            if blocked || self.turn_active {
                self.log(LogEvent::Checks { results });
                return;
            }
            if commit.is_none() || commit != head(&self.record.working_copy) {
                self.say("a newer result is on another commit: the checks run again");
                self.start_checks();
                return;
            }
        }
        // ^ A full disk is not the worker's failure: the checks run again once there is space.
        if results.iter().any(|check| check.output.contains(NO_SPACE)) {
            self.log(LogEvent::Checks { results });
            self.wait_for_disk(DiskWait::Checks);
            if self.disk_wait.is_none() {
                self.disk_wait = Some(DiskWait::Checks);
                self.wait(WaitOn::Disk, "a check ran out of disk space".into());
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
        self.shared_failures(&results);
        // ^ A host that started again at ready has no last_result: the stored one is the worker's.
        let result = self.current_result();
        let passed = result.status == "done" && results.iter().all(|check| check.exit == Some(0));
        // ^ Read before this revision is logged: the manager reviewed and landed it already.
        let relands = view::resolving_conflict(&self.dir);
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
                        self.start_turn(&text, TurnTrigger::CheckRetry);
                    }
                    // ^ Every ready task waits for the manager's review; one that resolved
                    // the conflict of a landing lands again at once when the project lands
                    // by itself and has checks to prove it, from checking to landed without
                    // a stop at ready. A message that waits for the worker comes first: its
                    // answer is a new revision. ref: docs/tasks.md "Landing"
                    None if state == TaskState::Ready
                        && relands
                        && !results.is_empty()
                        && self.record.hold.is_none()
                        && self.pending.is_empty()
                        && self.auto_land() =>
                    {
                        let landed = self.land(true, false, None);
                        self.finish_landing(landed, state);
                    }
                    None => self.set_state(state),
                    Some(AfterRebase::Land { tried }) => {
                        let finished = if state == TaskState::Ready {
                            self.land_on_tip(tried, None)
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
        self.deliver_pending(TurnTrigger::Message);
        // ^ No turn came: the land rebases this task, a notice would only wake the worker.
        self.notices.clear();
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
    fn deliver_pending(&mut self, trigger: TurnTrigger) {
        if !self.pending.is_empty() {
            let text: Vec<String> = self.pending.drain(..).collect();
            self.start_turn(&text.join("\n\n"), trigger);
        }
    }

    /// The last result's context (a session limit's error result has none), or 0 when a
    /// rotation came after it: a host that starts again (after a session limit, a re-exec)
    /// rotates a large conversation on its first turn. Then when the agent last answered: a
    /// result or a session limit.
    fn last_context(&self) -> (u64, u64) {
        let context = self.dir.last_event(|event| match event {
            LogEvent::Result {
                usage: Some(usage), ..
            } if usage.context_tokens > 0 => Some(usage.context_tokens),
            LogEvent::Rotated { .. } => Some(0),
            _ => None,
        });
        let answered = self.dir.last_entry(|entry| match entry.event {
            LogEvent::Result { .. } | LogEvent::LimitWait { .. } | LogEvent::Ping { .. } => {
                Some(entry.unix_ms)
            }
            _ => None,
        });
        (
            context.ok().flatten().unwrap_or(0),
            answered.ok().flatten().unwrap_or(0),
        )
    }

    /// The worker conversation's last `session_cost_micro_usd`.
    fn session_cost(&self) -> u64 {
        self.dir
            .last_event(|event| match event {
                LogEvent::Result { usage, .. } => usage.as_ref().map(|u| u.session_cost_micro_usd),
                _ => None,
            })
            .ok()
            .flatten()
            .unwrap_or(0)
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
    /// `batch_tree`: the tree a batch land checked with this task on top (see land_on_tip).
    fn land(&mut self, auto: bool, tried: bool, batch_tree: Option<String>) -> Result<String> {
        self.require_ready(auto)?;
        view::refuse_open_children(&self.record.project_id, self.record.id, "land")?;
        self.require_committed()?;
        if tried && !self.on_trial()? {
            bail!(
                "no trial of t{} runs; land it without --tried",
                self.record.id
            );
        }
        self.land_on_tip(tried, batch_tree.as_deref())
    }

    /// Fast-forward when the branch has the default branch tip, else rebase onto it. The
    /// checks of a rebased revision come back here, so a tip that moved while they ran
    /// rebases and checks again (#84 stopped on it, 2026-10-27).
    /// A rebase that gives `batch_tree` lands at once: a batch land checked that tree, with
    /// the tasks before this one already on the default branch (docs/tasks.md "Batch land").
    fn land_on_tip(&mut self, tried: bool, batch_tree: Option<&str>) -> Result<String> {
        self.require_parent_at_rest()?;
        let working_copy = self.record.working_copy.clone();
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&working_copy, "HEAD")?;
        // ^ Only a checked tree fast-forwards: a child that landed on this task moved HEAD
        // past the ready revision.
        let revision = format!("refs/wsx/task/{}/r{}", self.record.id, self.record.revision);
        if copy::is_ancestor(&working_copy, &tip, &head)? {
            let tree = copy::tree(&working_copy, "HEAD")?;
            if copy::tree(&working_copy, &revision)? == tree || batch_tree == Some(&tree) {
                return self.fast_forward(tried);
            }
            return Ok(self.check_then_land(tried, "HEAD moved after the ready revision"));
        }
        match copy::rebase(&working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.carried_verify = self.current_result().human_verify;
                Ok(self.resolve_conflict(&tip, &paths))
            }
            // ^ The rebase kept the tree whose checks passed: their results stand.
            Rebase::Done if self.rebase_kept_tree()? => self.fast_forward(tried),
            Rebase::Done
                if batch_tree.is_some()
                    && copy::tree(&working_copy, "HEAD").ok().as_deref() == batch_tree =>
            {
                self.say("the rebase gave the tree the batch land checked");
                self.fast_forward(tried)
            }
            Rebase::Done => Ok(self.check_then_land(
                tried,
                &format!("rebased onto {} at {tip:.12}", self.record.default_branch),
            )),
        }
    }

    /// Run the checks on HEAD as a new revision; it lands when they pass.
    fn check_then_land(&mut self, tried: bool, why: &str) -> String {
        self.after_rebase = Some(AfterRebase::Land { tried });
        self.start_checks();
        format!("{why}; checks run on the new revision and it lands when they pass")
    }

    /// A child lands only on a parent at rest, ready or failed: its commits would join a
    /// running turn or checks, and its new revision would drop a wait for the owner or a
    /// plan. The child stays ready and the manager lands it again on the parent's next
    /// event. ref: docs/tasks.md "Parent tasks"
    fn require_parent_at_rest(&self) -> Result<()> {
        let Some(parent) = self.record.parent else {
            return Ok(());
        };
        let record = TaskDir::open(&self.record.project_id, parent)?.read_record()?;
        if !record.at_rest() {
            let state = match (&record.waiting, record.state) {
                (Some(waiting), TaskState::Waiting) => {
                    format!("waiting on {}", waiting.on.as_str())
                }
                _ => record.state.as_str().to_string(),
            };
            bail!(
                "parent #{parent} is busy ({state}); #{} stays ready and lands when #{parent} is ready",
                self.record.id
            );
        }
        Ok(())
    }

    /// Log a rebase conflict and start the worker turn that resolves it.
    fn resolve_conflict(&mut self, tip: &str, paths: &[String]) -> String {
        self.log(LogEvent::Conflict {
            onto: tip.to_string(),
            paths: paths.to_vec(),
        });
        self.start_turn(
            &format!(
                "Landing needs branch {branch} rebased onto {tip} (the current {default}). \
             Run `git rebase {tip}`, resolve the conflicts in {paths}, `git add` them and \
             `git rebase --continue`, then report. Run no `wsx task check-run`: the host \
             runs the checks on your report.",
                branch = self.record.branch,
                default = self.record.default_branch,
                paths = paths.join(", "),
            ),
            TurnTrigger::Conflict,
        );
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

    /// Whether HEAD has the tree of the current revision and the project's checks are the
    /// ones that passed on it.
    fn rebase_kept_tree(&mut self) -> Result<bool> {
        let working_copy = &self.record.working_copy;
        let revision = format!("refs/wsx/task/{}/r{}", self.record.id, self.record.revision);
        if copy::tree(working_copy, &revision)? != copy::tree(working_copy, "HEAD")? {
            return Ok(false);
        }
        self.follow_checks();
        let passed: Vec<String> = view::read_result(&self.dir)
            .and_then(|result| {
                serde_json::from_value::<Vec<CheckResult>>(result["checks"].clone()).ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|check| check.command)
            .collect();
        Ok(!passed.is_empty() && passed == self.record.checks)
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
        let cleared = copy::move_branch(
            &self.record.project_path,
            &self.record.default_branch,
            &tip,
            &head,
            false,
        )?;
        if let Some(lock) = cleared {
            self.log(LogEvent::LockCleared {
                path: lock.path.display().to_string(),
                age_s: lock.age_s,
            });
        }
        self.landed(&head, tried);
        if let Some(parent) = self.record.parent {
            self.tell_parent(parent, &tip, &head);
        }
        Ok(format!(
            "landed r{} on {} at {head:.12}",
            self.record.revision, self.record.default_branch
        ))
    }

    /// Tell each open task that shares files with this land to rebase onto `commit` at its
    /// next step boundary, without a check-run. ref: docs/tasks.md "Landing"
    fn tell_overlaps(&self, commit: &str) {
        let Ok(tasks) = view::project_tasks(&self.record.project_id) else {
            return;
        };
        let records: Vec<&TaskRecord> = tasks
            .iter()
            .filter_map(|(_, record)| record.as_ref().ok())
            .collect();
        let overlaps = view::overlaps(&self.record, &records, true);
        for row in overlaps.as_array().into_iter().flatten() {
            let Some((dir, _)) = tasks.iter().find(|(dir, _)| row["id"] == dir.id) else {
                continue;
            };
            let paths: Vec<&str> = row["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|path| path.as_str())
                .collect();
            let text = format!(
                "#{} landed on {} at {commit}, changing files this task shares: {}. At your \
                 next step boundary run `git rebase {commit}`, resolve any conflict and commit, \
                 then go on. Run no `wsx task check-run` for it: the checks after your result \
                 run on the rebased tree.",
                self.record.id,
                self.record.default_branch,
                paths.join(", ")
            );
            let request = HostRequest::Notice { text };
            match call_host(&dir.socket_path(), &request, HOST_TIMEOUT) {
                Ok(response) => self.say(&format!("#{}: {}", row["id"], response.message)),
                Err(error) => self.say(&format!("#{} did not hear the land: {error:#}", row["id"])),
            }
        }
    }

    /// Ask the parent's host to check the commits this land put on its branch.
    fn tell_parent(&self, parent: u32, old_tip: &str, head: &str) {
        let told =
            copy::unlanded_commits(&self.record.working_copy, head, old_tip).and_then(|commits| {
                let dir = TaskDir::open(&self.record.project_id, parent)?;
                let request = HostRequest::ChildLanded {
                    child: self.record.id,
                    commits,
                };
                let response = call_host(&dir.socket_path(), &request, HOST_TIMEOUT)?;
                Ok(response.message)
            });
        match told {
            Ok(message) => self.say(&format!("parent #{parent}: {message}")),
            // ^ Its land checks a HEAD that moved past its ready revision first.
            Err(error) => self.say(&format!("parent #{parent} did not answer: {error:#}")),
        }
    }

    /// The revision is on the default branch: its Human Verify items join the project's
    /// "To check" list before readers see `landed` (docs/tasks.md "To check"), unless the
    /// owner tried them on the live site. A child's items wait in its `result.json`, its
    /// captures kept, until its parent lands. A trial of this task ends.
    fn landed(&mut self, commit: &str, tried: bool) {
        if tried {
            self.carried_verify.clear();
        } else if self.record.parent.is_some() {
            self.keep_verify_for_parent();
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
        if self.record.parent.is_none() {
            self.tell_overlaps(commit);
        }
        self.after_landing();
    }

    /// Add the revision's Human Verify items, with their captures, to "To check", then those
    /// of the children that landed on this task. ref: docs/tasks.md "Parent tasks"
    fn collect_verify(&mut self, commit: &str) {
        let verify = self.kept_verify();
        let mut items = vec![(self.record.id, self.record.revision, verify)];
        for (dir, child) in
            view::children(&self.record.project_id, self.record.id).unwrap_or_default()
        {
            if child.state != TaskState::Landed {
                continue;
            }
            let verify = view::read_result(&dir)
                .and_then(|result| {
                    serde_json::from_value::<WorkerResult>(result["result"].clone()).ok()
                })
                .unwrap_or_default()
                .human_verify;
            items.push((child.id, child.revision, verify));
        }
        for (task, revision, verify) in items {
            if let Err(error) =
                checks::collect(&self.record.project_id, task, revision, commit, &verify)
            {
                self.say(&format!("cannot add the items to check: {error:#}"));
            }
        }
    }

    /// A child's items with their captures kept in its task folder, written back to its
    /// `result.json`: its working copy goes before the parent lands.
    fn keep_verify_for_parent(&mut self) {
        let mut result = self.current_result();
        result.human_verify = self.kept_verify();
        let checks: Vec<CheckResult> = view::read_result(&self.dir)
            .and_then(|stored| serde_json::from_value(stored["checks"].clone()).ok())
            .unwrap_or_default();
        self.write_result(&result, &checks);
        self.last_result = Some(result);
    }

    /// The revision's Human Verify items with their captures copied into the task folder.
    fn kept_verify(&mut self) -> Vec<HumanVerify> {
        let mut verify = self.current_result().human_verify;
        for item in std::mem::take(&mut self.carried_verify) {
            if !verify.iter().any(|known| known.check == item.check) {
                verify.push(item);
            }
        }
        for (index, item) in verify.iter_mut().enumerate() {
            if item.capture.is_empty() {
                item.poster.clear();
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
            // ^ A poster only goes with a video capture.
            if !item.capture.ends_with(".mp4") || item.poster.is_empty() {
                item.poster.clear();
                continue;
            }
            let kept = checks::keep_poster(
                &self.dir,
                &self.record.working_copy,
                self.record.revision,
                index + 1,
                &item.poster,
            );
            if let Err(error) = &kept {
                self.say(&format!("poster not kept: {error:#}"));
            }
            item.poster = kept.unwrap_or_default();
        }
        verify
    }

    /// Deploy the default branch when the project deploys by itself, then close this task:
    /// its work is on the default branch, and its copy and build only take space. Each runs
    /// as its own `wsx task` command that outlives this host.
    /// ref: docs/tasks.md "Deploy and release", "Close"
    fn after_landing(&self) {
        let project = self.record.project.as_str();
        let id = self.record.id.to_string();
        // ^ A child's land moves its parent's branch, not the default branch.
        let deploys = self.record.parent.is_none()
            && load_task_config(&self.record.project_path)
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
                .and_then(|log| hosting::spawn_wsx(&args, log, None));
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
        let commit = head(working_copy)
            .with_context(|| format!("git rev-parse HEAD failed in {}", working_copy.display()))?;
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
        if changed {
            self.record.state_unix_ms = now_unix_ms();
        }
        self.record.state = state;
        if !matches!(state, TaskState::Working | TaskState::Checking) {
            self.disk_slot = None;
        }
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

    /// The failures of `results` that other tasks of the project show too: each is posted to
    /// the manager once, as one event that wakes a turn; returns the sentences for the worker
    /// whose run it was. ref: docs/tasks.md "Checks, revisions and report"
    fn shared_failures(&self, results: &[CheckResult]) -> String {
        let now = now_unix_ms();
        let found = failure::shared_failures(&self.record.project_id, self.record.id, results, now);
        let (project, main) = (
            self.record.project.clone(),
            self.record.project_path.clone(),
        );
        let project_id = self.record.project_id.clone();
        for failure in &found {
            let (failure, project, main, project_id) = (
                failure.clone(),
                project.clone(),
                main.clone(),
                project_id.clone(),
            );
            // ^ The manager starts if it does not run, and a busy host takes a while to answer.
            std::thread::spawn(move || {
                let Ok(exe) = std::env::current_exe() else {
                    return;
                };
                let _ = store::post_once(
                    &project_id,
                    &failure.key(),
                    now / 1000,
                    SHARED_WINDOW_SECS,
                    || {
                        let sent = owner::say(
                            &project,
                            &main,
                            &exe,
                            None,
                            super::launch_manager(&main),
                            failure.event(),
                            Some(Author::Events),
                            vec![Short::plain(&failure.event())],
                        )?;
                        anyhow::ensure!(sent.ok, "{}", sent.message);
                        Ok(())
                    },
                );
            });
        }
        found
            .iter()
            .map(|failure| failure.sentence(self.record.id))
            .collect::<Vec<_>>()
            .join("\n")
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
            (TaskState::Waiting, Some(WaitOn::Owner)) => runtime::AgentState::Blocked,
            (TaskState::Waiting, _) | (TaskState::Closed, _) => runtime::AgentState::Idle,
            (TaskState::Ready | TaskState::Landed, _) => runtime::AgentState::Done,
            (TaskState::Failed, _) => runtime::AgentState::Error,
        };
        if let Some(problem) = hosting::report_pane(pane_id, "task", state, attached) {
            self.say(&problem);
        }
    }
}

/// "3.2 GB free of the 5 GB a task needs".
fn disk_full(free_mb: u64, min_gb: u64) -> String {
    format!(
        "{:.1} GB free of the {min_gb} GB a task needs",
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

/// The working copy's HEAD commit.
fn head(working_copy: &Path) -> Option<String> {
    let output = git_cmd(working_copy)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A tree id and the check results that ran on it, by check index.
type StoredChecks = (String, BTreeMap<usize, CheckResult>);
type KeptChecks = (
    Option<String>,
    BTreeMap<usize, CheckResult>,
    Vec<(usize, String)>,
);

/// The answer to a worker's `check-run`: each picked check and how it ended (the output of a
/// failed one), exit 0 when all passed: from `ran`, or `stored` from an earlier run.
fn check_run_response(
    picked: &[(usize, String)],
    stored: &BTreeMap<usize, CheckResult>,
    ran: &BTreeMap<usize, CheckResult>,
) -> broker::Response {
    let result = |index| ran.get(index).or_else(|| stored.get(index));
    let mut stdout = String::new();
    for (index, _) in picked {
        let Some(check) = result(index) else {
            continue;
        };
        let ended = match (check.timed_out, check.exit) {
            (true, _) => "timed out".to_string(),
            (false, Some(code)) => format!("exit {code}"),
            (false, None) => "no exit code".to_string(),
        };
        let note = if ran.contains_key(index) {
            ""
        } else {
            " (stored: it ran on this tree before)"
        };
        stdout.push_str(&format!(
            "check {} `{}`: {ended}{note}\n",
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
    let passed = picked
        .iter()
        .all(|(index, _)| result(index).is_some_and(|check| check.exit == Some(0)));
    broker::Response {
        stdout,
        stderr: String::new(),
        code: if passed { 0 } else { 1 },
    }
}

fn queued_response(ci: bool) -> broker::Response {
    let on = match ci {
        true => "queued on CI",
        false => "queued for the machine's check slot",
    };
    broker::Response {
        stdout: format!("{on}: {CHECK_RUN_QUEUED}"),
        stderr: String::new(),
        code: 0,
    }
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
pub(super) fn run_check(
    working_copy: &Path,
    command: &str,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    log: &Path,
) -> CheckResult {
    // ^ The shell sends the whole output to `log`, in the order it was written; the result
    // keeps its end, so a check-run's answer stays small however much a check prints.
    let quoted = log.to_string_lossy().replace('\'', "'\\''");
    let mut process = std::process::Command::new("/bin/sh");
    process
        .args(["-c", &format!("exec >'{quoted}' 2>&1\n{command}")])
        .current_dir(working_copy)
        .stdin(std::process::Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("WSX_") {
            process.env_remove(name);
        }
    }
    process.envs(env);
    // ^ The pipes get only what the shell says before its redirection, such as a log it
    // cannot open; then no earlier run's log stands in for this one.
    let _ = std::fs::remove_file(log);
    let started = std::time::Instant::now();
    let (exit, timed_out, said) =
        match output_with_timeout_tail(&mut process, timeout, CHECK_OUTPUT_BYTES) {
            Ok(output) => (output.status.code(), false, output_text(&output)),
            Err(error) => (
                None,
                error.kind() == std::io::ErrorKind::TimedOut,
                error.to_string(),
            ),
        };
    CheckResult {
        command: command.into(),
        exit,
        timed_out,
        output: joined([log_tail(log).unwrap_or_default(), said]),
        ms: started.elapsed().as_millis() as u64,
        queued_ms: 0,
        stored: false,
    }
}

/// The last `CHECK_OUTPUT_BYTES` of a check's log: a long output (a test run) ends with its
/// failures and summary. A cut log is named first, so the reader can open the whole of it.
fn log_tail(log: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(log)?;
    let (bytes, cut) = store::read_tail_bytes(&mut file, CHECK_OUTPUT_BYTES as u64)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if cut == 0 {
        return Ok(text);
    }
    Ok(format!(
        "[the first {cut} bytes are cut; the whole output is in {}]\n{text}",
        log.display()
    ))
}

/// A command's output as one text: stdout, then stderr.
pub(super) fn output_text(output: &std::process::Output) -> String {
    joined([&output.stdout, &output.stderr].map(|bytes| String::from_utf8_lossy(bytes).into()))
}

/// `parts` with trailing whitespace trimmed, the empty ones left out, one per line.
fn joined(parts: [String; 2]) -> String {
    let parts: Vec<&str> = parts
        .iter()
        .map(|part| part.trim_end())
        .filter(|part| !part.is_empty())
        .collect();
    parts.join("\n")
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

/// The words of `files` globs a memo line names: each glob's part before its first wildcard
/// and its last literal path component, of at least 3 characters.
fn file_words(files: &[String]) -> Vec<String> {
    let mut words = Vec::new();
    for glob in files {
        let literal = &glob[..glob.find(['*', '?', '[', '{']).unwrap_or(glob.len())];
        let last = glob
            .rsplit('/')
            .find(|part| !part.contains(['*', '?', '[', '{']));
        for word in [Some(literal.trim_end_matches('/')), last]
            .into_iter()
            .flatten()
        {
            if word.len() >= 3 && !words.iter().any(|known| known == word) {
                words.push(word.to_string());
            }
        }
    }
    words
}
