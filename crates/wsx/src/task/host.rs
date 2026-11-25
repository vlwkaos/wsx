//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

use super::agent::{
    self, new_session_id, Agent, AgentLine, Ask, Launch, Next, TurnResult, KEEP_WARM_PINGS,
    KEEP_WARM_TEXT, PROMPT_CACHE_MS,
};
use super::broker;
use super::hosting;
use super::limit;
use super::slot::{self, Lane, Priority, Taken, Want};
use super::RECORDED_BY_HOST;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::{load_project_config, load_task_config};
use wsx_core::git::{git_cmd, output_with_timeout_tail};
use wsx_core::memory::start::{self as memory_start, Start};
use wsx_core::memory::{manage as memory_manage, Store};
use wsx_core::model::workspace::WorkflowStep;
use wsx_core::runtime;
use wsx_core::task::chat_host::asks_owner;
use wsx_core::task::replace::{Binary, HANDOFF_FILE};
use wsx_core::task::schedule;
use wsx_core::task::status::{self, STOPPED_BY_REQUEST};
use wsx_core::task::text::question;
use wsx_core::task::view::{checked_files, checked_title, short_commit};
use wsx_core::task::{
    build, call_host,
    chat_host::{Author, Level, Short},
    checks,
    copy::{self, Rebase},
    failure::{self, NO_SPACE},
    now_unix_ms, owner,
    owner::HOST_TIMEOUT,
    report, stall,
    store::{self, TaskDir, RECORD_FILE, REPORT_FILE, RESULT_FILE},
    view, Actor, After, AfterKind, CheckResult, Evidence, Home, HostRequest, HostResponse,
    HumanVerify, LogEvent, MessageKind, TaskRecord, TaskState, TurnTrigger, WaitOn, Waiting,
    WorkerResult, MAX_MESSAGE_BYTES,
};
use wsx_core::task::{ReviewVerdict, TurnUsage};

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
/// The first line of a handoff names the trigger of the turn it carries.
const HANDOFF_TRIGGER: &str = "trigger: ";
const MAX_HANDOFF_BYTES: u64 = 4 * 1024 * 1024;
/// Turns a failed check starts by itself before the task fails; an owner's message resets it.
const MAX_CHECK_RETRIES: u32 = 2;
/// The end of each failed check's output that goes back to the worker.
const CHECK_TAIL_BYTES: usize = 3 * 1024;
/// EX_TEMPFAIL: a check that fails for a reason outside the tree exits with it. Its result is never kept, so the next check-run runs it again.
/// ref: docs/tasks.md "Check-run"
const TEMPFAIL: i32 = 75;

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
    /// Checks wait for the machine's check slot, for this reason; true for a `check-run`'s.
    CheckQueued(String, bool),
    /// Checks took the slot, or run without it after this error; true for a `check-run`'s.
    CheckSlot(Option<String>, bool),
    /// A line of the review job of that generation; `None` when its process ended.
    Review(u64, Option<AgentLine>),
}

/// The review job of a ready revision: one fresh agent call that lands it or sends findings
/// back (`start_review`). ref: docs/tasks.md "Review"
struct ReviewJob {
    generation: u64,
    agent: Agent,
    revision: u32,
    round: u32,
    /// `adapter/model`, logged with the verdict.
    label: String,
    started: Instant,
}

/// The most rounds of changes a review sends back before the owner decides.
const MAX_REVIEW_ROUNDS: u32 = 2;
/// The longest a review job runs before it ends without a verdict.
const REVIEW_LIMIT: Duration = Duration::from_secs(20 * 60);

/// What waits for free disk, resumed once `tasks.minFreeGb` is free again; a turn held for the
/// load (`wait_for_load`) rides the same wait.
enum DiskWait {
    Turn(String),
    Checks,
}

/// A turn the own agent's usage limit or failed sign-in holds: the text that continues it,
/// and when it may. The account's record ends the wait (`resume_after_limit`).
struct LimitWait {
    text: String,
    until_ms: u64,
    /// When the host looks next: `until_ms`, later while another task holds the limit slot.
    next_ms: u64,
}

/// Why the conversation ends after its handoff notes. ref: docs/tasks.md "Context rotation"
enum Handoff {
    /// The context reached `tasks.agent.rotateTokens`: a fresh conversation follows.
    Size,
    /// Idle past `tasks.agent.rotateIdleSeconds` (the words): the prompt cache is gone.
    Idle(String),
    /// The task goes home: its own conversation continues with the fallback's notes.
    Home,
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
/// How often a task that waits for disk looks again (30 s).
fn disk_poll() -> Duration {
    test_secs("WSX_TEST_DISK_POLL_SECS").unwrap_or(Duration::from_secs(30))
}
/// How long a task resumed from a disk wait holds the disk slot, so the next one sees the
/// space its turn takes before it resumes (60 s).
fn disk_settle() -> Duration {
    test_secs("WSX_TEST_DISK_SETTLE_SECS").unwrap_or(Duration::from_secs(60))
}
/// The seconds in `var`: for the task scenario only, which would wait out the real timers.
fn test_secs(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_secs)
}
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
    /// The result settled without the answer (a run never holds a task): the answer comes in
    /// the background, a pass as an event line, a failure as a turn that reopens the task.
    /// ref: docs/tasks.md "Check-run"
    background: bool,
    /// The run queued for the check slot, so its calls got `queued` at once: the answer goes
    /// to the worker as a turn after its result, never to a client that waits.
    queued: bool,
    /// The answer of a queued run that ended before the result: the result gets it as a turn.
    answer: Option<CheckAnswer>,
    /// The checks it runs, for the evidence name: scenario names and check numbers.
    label: String,
    /// `--part`: it runs one part of a scenario, which is never a full pass.
    part: Option<String>,
    /// The longest it may run: each check's timeout.
    limit: Duration,
    /// When the run must have ended (unix ms): its start, or its slot's, plus `limit`. `None`
    /// while it waits for the check slot.
    deadline_ms: Option<u64>,
    /// The working copy's HEAD when it started: a call on another commit never joins it.
    commit: String,
}

/// `check-run.json`: the run a host owes an answer for, written when it starts and removed
/// when its answer is handled. A host that restarts reads it and runs the same checks again
/// (`resume_check_run`).
#[derive(serde::Serialize, serde::Deserialize)]
struct CheckRunMark {
    picked: Vec<(usize, String)>,
    label: String,
    part: Option<String>,
    commit: String,
}

const CHECK_RUN_FILE: &str = "check-run.json";

/// A check-run's answer to the worker, the proof the host keeps when the run passed on
/// an unchanged tree, and whether it failed in a way that is the task's own (not a known
/// failure): only that reopens a settled task.
struct CheckAnswer {
    text: String,
    proof: Option<Evidence>,
    fails: bool,
}

/// What a `check-run` that queues for the check slot answers at once: a
/// background job of the agent dies long before a 20-90 min queue ends. ref: docs/tasks.md "Check-run"
const CHECK_RUN_QUEUED: &str = "its answer comes after your result. Do not wait or poll for it: \
finish this step and return its result now. A pass is recorded; a failure comes back to you \
as a turn.\n";

/// The scenario names and numbers of a `check-run`, and its `--part <p>` (or `--part=<p>`).
/// ref: docs/tasks.md "Scenarios"
fn split_part(args: &[String]) -> Result<(Vec<&String>, Option<String>), String> {
    let (mut words, mut part) = (Vec::new(), None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--part=") {
            Some(value) => value,
            None if arg == "--part" => args.next().map_or("", String::as_str),
            None => {
                words.push(arg);
                continue;
            }
        };
        let valid = !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid || part.replace(value.to_string()).is_some() {
            return Err("--part takes one name of letters, digits, - and _".into());
        }
    }
    Ok((words, part))
}

/// A scenario's command with its part: the command reads `WSX_SCENARIO_PART`. The result's
/// command shows it, so a part never answers for the full run. ref: docs/tasks.md "Scenarios"
fn with_part(command: &str, part: &str) -> String {
    format!("export WSX_SCENARIO_PART='{part}'; {command}")
}

struct Host {
    dir: TaskDir,
    record: TaskRecord,
    /// The policy of the agent that runs now, which the broker checks commands against.
    policy: broker::SharedPolicy,
    events: Sender<Event>,
    agent: Option<Agent>,
    generation: u64,
    /// The running turn's record (`stall::Turn`, `turn.json`); none between turns.
    turn: Option<stall::Turn>,
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
    binary: Option<Binary>,
    reexec: bool,
    /// A `land` waits for the checks of its rebased revision.
    after_rebase: Option<AfterRebase>,
    disk_wait: Option<DiskWait>,
    /// The place of a disk wait in the machine's disk queue, and the disk slot it took to
    /// resume, with when it took it: held for `disk_settle()` or until its turn or checks end.
    disk_queue: Option<slot::Ticket>,
    disk_slot: Option<(slot::Slot, Instant)>,
    /// When the turn held in the turn queue (`wait_for_load`) began to wait.
    load_since: Option<Instant>,
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
    /// The HEAD the scenarios a change touches last started for: once per commit.
    touched_for: Option<String>,
    /// A `check-run` on a newer commit than the pending one's (its call got `queued`): it
    /// starts when that run ends.
    check_run_next: Option<Vec<String>>,
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
    /// The contract files this land adds to and changes no line of: the main chat hears of them
    /// once the land is done (`require_contracts_kept`).
    stricter_contracts: Vec<String>,
    /// The context of the worker's last model call, from the log when the host starts.
    context_tokens: u64,
    /// The turn that waits while the worker writes its handoff notes, and why the
    /// conversation ends: it starts the conversation that follows.
    /// ref: docs/tasks.md "Context rotation"
    rotating: Option<(String, Handoff)>,
    /// What started the running turn, logged on its result.
    trigger: TurnTrigger,
    /// When the agent last answered (unix ms), from the log when the host starts.
    answered_ms: u64,
    /// Ping turns since the last real turn, and what starts once the running ping ends.
    /// ref: docs/tasks.md "Context rotation"
    pings: u32,
    after_ping: Option<TurnTrigger>,
    started: Instant,
    /// The running review job of the ready revision, and the count of jobs started: a line of
    /// an older job is dropped. The worker's `generation` stays its own.
    review: Option<ReviewJob>,
    reviews: u64,
}

/// A host that cannot load its record posts one alert to the project's main chat (`wsx task
/// post --level bad`, once per 10 minutes) with the file and the cause, and ends; it ended
/// without a word before. When the alert cannot be posted either, the error says both.
fn alert_unreadable(dir: &TaskDir, error: anyhow::Error) -> Result<()> {
    use super::cmd::routine::{post_when_free, POST_KEY_WINDOW_SECS};
    let id = dir.id;
    let line = format!(
        "task t{id} has no host: {} does not load: {error:#}",
        dir.file(RECORD_FILE).display()
    );
    let key: String = format!("host.{id}.{}", dir.project_id)
        .chars()
        .take(64)
        .collect();
    store::post_once(
        &dir.project_id,
        &key,
        now_unix_ms() / 1000,
        POST_KEY_WINDOW_SECS,
        || {
            let (name, main) = super::project_by_id(&dir.project_id)?;
            post_when_free(&main, &name, "task host", Some(Level::Bad), || {
                vec![line.clone()]
            })
        },
    )
    .map(drop)
    .with_context(|| format!("{line}; the alert was not posted"))
}

pub fn run(project_id: &str, id: u32) -> Result<()> {
    let dir = TaskDir::open(project_id, id)?;
    let mut record = match dir.read_record() {
        Ok(record) => record,
        Err(error) => return alert_unreadable(&dir, error),
    };
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
        policy: broker::SharedPolicy::default(),
        events: events.clone(),
        agent: None,
        generation: 0,
        turn: None,
        pending: VecDeque::new(),
        notices: Vec::new(),
        carried: false,
        stop_requested: false,
        stopped: false,
        checks_epoch: 0,
        last_result: None,
        last_denials: Vec::new(),
        check_retries: 0,
        review: None,
        reviews: 0,
        result_asked: false,
        binary: Binary::capture(),
        reexec: false,
        after_rebase: None,
        disk_wait: None,
        disk_queue: None,
        disk_slot: None,
        load_since: None,
        limit_wait: None,
        limit_attempt: 0,
        limit_queue: None,
        limit_slot: None,
        background_wait: None,
        check_run: None,
        touched_for: None,
        check_run_next: None,
        reported: None,
        turn_text: String::new(),
        gate: None,
        stored_checks: None,
        carried_verify: Vec::new(),
        stricter_contracts: Vec::new(),
        context_tokens: 0,
        rotating: None,
        trigger: TurnTrigger::Step,
        answered_ms: 0,
        pings: 0,
        after_ping: None,
        started: Instant::now(),
    };
    (host.context_tokens, host.answered_ms) = host.last_context();
    if host.dir.restore_question(&mut host.record) {
        host.save();
    }
    // ^ A record from before `home`: its own agent is the one in use, or when a switch is on
    // record, the project's configured one.
    if host.record.home.is_none() {
        let settings = load_task_config(&host.record.project_path).unwrap_or_default();
        let (adapter, model) = match host.record.fallback {
            true => (settings.adapter, settings.model),
            false => (host.record.adapter.clone(), host.record.model.clone()),
        };
        host.record.home = Some(Home {
            adapter,
            model,
            session: None,
            turn: String::new(),
        });
        host.save();
    }
    let to_host = events.clone();
    let policy = host.policy.clone();
    let (project_id, copy) = (
        host.record.project_id.clone(),
        host.record.working_copy.clone(),
    );
    let (task_id, project_name) = (host.record.id, host.record.project.clone());
    std::thread::spawn(move || {
        broker::serve(brokered, move |request| {
            // ^ A `wsx memory` command runs for the worker in its project's scope.
            if request.argv.first().is_some_and(|first| first == "memory") {
                return match std::env::current_exe() {
                    Ok(exe) => {
                        broker::run_memory(&exe, request, &copy, (&project_id, ""), policy.get())
                    }
                    Err(error) => broker::refused(format!("cannot find the wsx binary: {error}")),
                };
            }
            // ^ The worker's role grants `result`, `check-run` and `run`, nothing else.
            if let Err(error) = broker::allowed(&request.argv, policy.get()) {
                return broker::refused(format!("{error:#}"));
            }
            // ^ A run request names this task as the asker, in this task's project only.
            if request.argv.get(1).is_some_and(|sub| sub == "run") {
                return ask_run(&request.argv, &copy, &project_name, task_id);
            }
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
    // ^ A replaced host's review job starts again in the new one (`resume_state`).
    if let Some(job) = host.review.take() {
        job.agent.stop();
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
            if self.turn.is_none() {
                self.limit_slot = None;
            }
            if self
                .disk_slot
                .as_ref()
                .is_some_and(|(_, taken)| taken.elapsed() >= disk_settle())
            {
                self.disk_slot = None;
            }
            // ^ On a deadline, not after a quiet spell: status calls arrive all the time.
            let disk_due = self
                .disk_wait
                .as_ref()
                .map(|_| disk_poll().saturating_sub(disk_checked.elapsed()));
            let settle_due = self
                .disk_slot
                .as_ref()
                .map(|(_, taken)| disk_settle().saturating_sub(taken.elapsed()));
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
            // ^ A turn with no output for the quiet bound ends: a hung agent must not hold the
            // task forever. ref: docs/tasks.md "Agents"
            let quiet_due = self.turn.as_ref().map(stall::Turn::left);
            let review_due = self
                .review
                .as_ref()
                .map(|job| REVIEW_LIMIT.saturating_sub(job.started.elapsed()));
            let event = match disk_due
                .into_iter()
                .chain(limit_due)
                .chain(background_due)
                .chain(settle_due)
                .chain(ping_due)
                .chain(quiet_due)
                .chain(review_due)
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
                        } else if quiet_due == Some(due) {
                            self.turn_hung();
                        } else if review_due == Some(due) {
                            let why = format!(
                                "it took longer than {}",
                                status::span(REVIEW_LIMIT.as_millis() as u64)
                            );
                            self.finish_review(None, &why, None);
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
                Event::Request(HostRequest::Land, reply)
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
                Event::CheckRunDone(results, response, stored) => {
                    self.check_run_done(results, response, stored)
                }
                Event::CheckQueued(reason, run) => self.check_queued(reason, run),
                Event::CheckSlot(error, run) => self.check_slot(error, run),
                Event::Review(generation, line) => self.on_review(generation, line),
            }
        }
        Ok(())
    }

    /// The worker's check-run ended: its waiting calls get the answer, and the worker gets it
    /// as a turn or, after its result, as recorded evidence. A run on a newer commit that
    /// waited for this one starts now. ref: docs/tasks.md "Check-run"
    fn check_run_done(
        &mut self,
        results: Vec<CheckResult>,
        mut response: broker::Response,
        stored: Option<StoredChecks>,
    ) {
        let found = self.shared_failures(&results);
        let shared = found
            .iter()
            .map(|failure| failure.sentence(self.record.id))
            .collect::<Vec<_>>()
            .join("\n");
        if !shared.is_empty() {
            response.stdout = format!("{}\n{shared}", response.stdout.trim_end());
        }
        let known = self.known_failure(&results, &found);
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
        let commit = run.as_ref().map_or("", |run| short_commit(&run.commit));
        // ^ Every failing check is one other tasks or main fail with too: a short answer with no
        // rerun, and when main's own CI run fails the same way, or the check is a scenario the
        // change does not touch, the host records it as evidence.
        // ref: docs/tasks.md "Shared failure"
        let answer = match known {
            Some(outside) => CheckAnswer {
                text: format!(
                    "Your check-run ended on commit {commit}. {shared}\nIt is a known failure: \
                     do not run it again. Name it in your result and return your result again."
                ),
                proof: (outside && tree_kept).then(|| Evidence {
                    check: format!(
                        "wsx task check-run {label} failed outside the task's changed paths: {} {RECORDED_BY_HOST}",
                        found
                            .iter()
                            .map(|failure| failure.shown.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                    proof: shared.clone(),
                }),
                fails: false,
            },
            None => CheckAnswer {
                text: format!(
                    "Your check-run ended. Its answer, on commit {commit}:\n{}\nPut it in your \
                     result's evidence (fix what fails in code you changed) and return your \
                     result again.",
                    response.stdout.trim_end()
                ),
                proof: (response.code == 0 && tree_kept).then(|| Evidence {
                    check: format!("wsx task check-run {label} passed {RECORDED_BY_HOST}"),
                    proof: response.stdout.trim_end().into(),
                }),
                fails: response.code != 0,
            },
        };
        // ^ A run on a newer commit waited for this one: it supersedes this answer, which joins
        // the next turn. One that does not start (refused, or every check kept) adds its words,
        // and this answer takes its usual way.
        if let Some(argv) = self.check_run_next.take().filter(|_| !self.stopped) {
            let (reply, said) = std::sync::mpsc::channel();
            self.check_run(&argv, reply);
            // ^ A turn that runs gets the answer with its result; with none, it is settled.
            let background = self.turn.is_none();
            if let Some(next) = &mut self.check_run {
                next.queued = true;
                next.background = background;
                self.say("check-run: the run on the newer commit starts");
                self.notices.push(answer.text);
                return;
            }
            if let Ok(said) = said.try_recv() {
                self.notices.push(format!(
                    "Your check-run on the newer commit did not start: {}{}",
                    said.stdout,
                    said.stderr.trim_end()
                ));
            }
        }
        match run {
            // ^ A stopped task's answer joins the turn an add starts: no turn now.
            Some(_) if self.stopped => {
                self.say("check-run ended while stopped: its answer joins the next turn");
                self.notices.push(answer.text);
            }
            Some(run) if run.background => self.answer_in_background(answer, &run.commit),
            Some(run) if run.queued && self.turn.is_none() => self.send_check_answer(answer.text),
            // ^ The worker still works after `queued`: its result gets the answer.
            Some(run) if run.queued => {
                self.check_run = Some(CheckRun {
                    answer: Some(answer),
                    ..run
                });
            }
            _ => {}
        }
        // ^ The answer is handled unless the run holds it for the worker's result.
        if self.check_run.is_none() {
            self.drop_check_run_mark();
        }
    }

    /// Whether every check that failed in `results` is a failure `found` shows in other tasks
    /// or on main, and no other check of the tree's kept results failed: `Some(true)` when
    /// their scenarios' paths name no changed path (the host records that as evidence).
    fn known_failure(
        &self,
        results: &[CheckResult],
        found: &[failure::SharedFailure],
    ) -> Option<bool> {
        let failed: Vec<&CheckResult> = results.iter().filter(|c| c.exit != Some(0)).collect();
        let kept_failure = self
            .stored_checks
            .iter()
            .flat_map(|(_, kept)| kept.values())
            .any(|c| c.exit != Some(0) && !results.iter().any(|r| r.command == c.command));
        let known = |check: &CheckResult| found.iter().find(|f| f.command == check.command);
        (!failed.is_empty() && !kept_failure && failed.iter().all(|c| known(c).is_some()))
            .then(|| failed.iter().all(|c| known(c).is_some_and(|f| f.outside)))
    }

    /// Whether this host must exec the wsx file that replaced it before it starts more work:
    /// not while a check-run waits or owes its answer (the exec would drop the run), nor
    /// before the turn after a limit (the exec would drop the limit slot).
    fn must_follow_binary(&self) -> bool {
        self.check_run.is_none()
            && self.limit_slot.is_none()
            && self.binary.as_ref().is_some_and(Binary::replaced)
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

    /// The first turn of a new task: its prompt, the context and the first step.
    fn first_turn(&mut self) -> Result<()> {
        let prompt = self.dir.prompt()?.context("the task log has no prompt")?;
        // ^ The adds that came before the first turn (the task waited on `after`) follow the
        // prompt: each is in the log, so a restart of the host loses none.
        let mut text = vec![prompt];
        text.extend(self.dir.adds()?);
        text.extend(self.task_context());
        text.extend(self.step_text());
        if self.record.plan {
            text.push(PLAN_REQUEST.into());
        }
        self.start_turn(&text.join("\n\n"), TurnTrigger::Step);
        Ok(())
    }

    /// Whether the task was created with `new --after` and has not had its first turn: it
    /// waits on the owner only in name, with no conversation and no result.
    fn awaits_start(&self) -> bool {
        self.record.waits_on(WaitOn::Owner)
            && self.record.agent_session.is_none()
            && self
                .dir
                .last_event(|event| matches!(event, LogEvent::Result { .. }).then_some(()))
                .ok()
                .flatten()
                .is_none()
    }

    /// The step that waits for the `after` task to land, for a task parked `--because files`
    /// or `check` whose steps have a `plan`: the one after it. Every step before it runs at
    /// once; `needs` and a `--plan` task wait whole. ref: docs/tasks.md "After another task"
    fn build_step(&self) -> Option<usize> {
        let after = self.record.after.as_ref()?;
        if after.because == AfterKind::Needs || self.record.plan {
            return None;
        }
        let plan = self
            .record
            .steps
            .iter()
            .position(|step| step.name == "plan")?;
        (plan + 1 < self.record.steps.len()).then_some(plan + 1)
    }

    /// Whether the task planned and its build waits for the `after` task (`build_step`).
    /// The build step has never run: a stop or a restart while parked keeps it parked, and a
    /// worker that ran it and asks the owner is not parked.
    fn build_parked(&self) -> bool {
        let Some(build) = self
            .build_step()
            .filter(|at| *at == self.record.step as usize)
        else {
            return false;
        };
        let name = self.record.steps.get(build).map(|step| step.name.clone());
        let ran = self.dir.last_event(|event| match event {
            LogEvent::Result { step, .. } => Some(step.clone()),
            _ => None,
        });
        self.record.agent_session.is_some()
            && self.record.state == TaskState::Waiting
            && self.record.waits_on(WaitOn::Owner)
            && ran.ok().flatten() != Some(name)
    }

    /// The adds since the worker's last result: they wait for the parked build. They are in the
    /// log, so a host restart loses none.
    fn adds_since_result(&self) -> Vec<String> {
        let log = self.dir.read_log().unwrap_or_default();
        let from = log
            .iter()
            .rposition(|entry| matches!(entry.event, LogEvent::Result { .. }))
            .map_or(0, |at| at + 1);
        log[from..]
            .iter()
            .filter_map(|entry| match &entry.event {
                LogEvent::Message {
                    mode: MessageKind::Add,
                    text,
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The copy follows the default branch, where the task waited for landed. A conflict is
    /// left to the worker: its words join the turn that starts.
    fn follow_default_branch(&mut self) -> Result<Option<String>> {
        let tip = self.default_tip()?;
        let note = match copy::rebase(&self.record.working_copy, &tip) {
            Ok(Rebase::Conflict(paths)) => Some(format!(
                "Your branch does not rebase cleanly onto {} at {tip:.12} ({}): rebase it first.",
                self.record.default_branch,
                paths.join(", ")
            )),
            // ^ A dirty copy refuses the rebase: the worker rebases it with its changes.
            Err(error) => Some(format!(
                "Your copy did not move onto {} at {tip:.12} ({error:#}): rebase it first.",
                self.record.default_branch
            )),
            Ok(_) => None,
        };
        // ^ The copy moved without a revision: the stored fork point follows it.
        let working_copy = &self.record.working_copy;
        self.record.base = copy::fork_point(working_copy, "HEAD", &self.record.default_branch)?;
        self.save();
        self.say(&format!(
            "follows {} at {tip:.12}",
            self.record.default_branch
        ));
        Ok(note)
    }

    /// The parked build starts: the copy follows the default branch, then the build step's
    /// turn carries the adds that waited and `text`, the lead's words for the end of the
    /// wait.
    fn start_parked_build(&mut self, text: Option<String>) -> Result<String> {
        let note = self.follow_default_branch()?;
        // ^ A ping's text waits in `pending`; the adds are read from the log, less those already
        // queued as notices for the turn (a land notice is both).
        let mut words: Vec<String> = self.pending.drain(..).collect();
        words.extend(
            self.adds_since_result()
                .into_iter()
                .filter(|text| !self.notices.contains(text)),
        );
        words.extend(note);
        words.extend(text);
        words.extend(self.step_text());
        self.start_turn(&words.join("\n\n"), TurnTrigger::Step);
        Ok("the build starts".into())
    }

    /// The wait of a task that has not started is over: its copy follows the default branch,
    /// where the task it waited for landed, and its first turn starts.
    fn start_after_wait(&mut self) -> Result<()> {
        if let Some(conflict) = self.follow_default_branch()? {
            bail!("{conflict}");
        }
        self.first_turn()
    }

    fn resume_state(&mut self) -> Result<()> {
        match self.record.state {
            // ^ A new task: no conversation yet, so its first turn has not run.
            // ^ One created with `new --after` waits for that task first.
            TaskState::Working if self.record.agent_session.is_none() => match &self.record.after {
                // ^ Parked for its files: it plans now, only its build waits.
                Some(_) if self.build_step().is_some() => self.first_turn()?,
                Some(after) => {
                    let reason = format!("waits for #{} to land", after.task);
                    self.wait(WaitOn::Owner, reason);
                }
                None => self.first_turn()?,
            },
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
            // ^ A limit wait outlives its host: the restarted one asks the account's record
            // at once, as every look of the wait does.
            TaskState::Waiting if self.record.waits_on(WaitOn::Limit) => {
                let now = now_unix_ms();
                self.limit_wait = Some(LimitWait {
                    text: LIMIT_CONTINUE.into(),
                    until_ms: now,
                    next_ms: now,
                });
                self.resume_after_limit();
            }
            // ^ A stopped landing stays: its reason is the last landing event after the revision,
            // also in a record from before `WaitOn::LandStopped`. So does a question to the owner.
            TaskState::Ready if self.record.waits_on(WaitOn::Asks) => {}
            TaskState::Ready => {
                let waiting = self
                    .dir
                    .last_event(|event| match event {
                        LogEvent::LandStopped { reason } => Some(Some(reason.clone())),
                        LogEvent::Revision { .. } => Some(None),
                        _ => None,
                    })
                    .ok()
                    .flatten()
                    .flatten()
                    .map(|reason| Waiting {
                        on: WaitOn::LandStopped,
                        reason,
                        until_ms: None,
                    });
                self.move_to(TaskState::Ready, waiting);
                self.start_review();
            }
            state => {
                self.set_state(state);
            }
        }
        self.resume_check_run();
        Ok(())
    }

    fn drop_check_run_mark(&self) {
        let _ = std::fs::remove_file(self.dir.file(CHECK_RUN_FILE));
    }

    /// The run a host owed an answer for when it ended (`check-run.json`) runs again on the
    /// same commit, and its answer comes as a queued run's does. A task
    /// that was stopped keeps the answer for its next add. ref: docs/tasks.md "Check-run"
    fn resume_check_run(&mut self) {
        let mark = match self.dir.read_file(CHECK_RUN_FILE, 1 << 20) {
            Ok(Some(bytes)) => serde_json::from_slice::<CheckRunMark>(&bytes).ok(),
            _ => None,
        };
        let Some(mark) = mark else {
            self.drop_check_run_mark();
            return;
        };
        let same = head(&self.record.working_copy).is_some_and(|head| head == mark.commit);
        if !self.record.state.is_open() || self.record.state == TaskState::Checking || !same {
            self.drop_check_run_mark();
            return;
        }
        self.stopped = self
            .record
            .waiting
            .as_ref()
            .is_some_and(|w| w.on == WaitOn::Owner && w.reason == STOPPED_BY_REQUEST);
        self.say("the host restarted during a check-run: it runs again");
        let indexes = mark.picked.iter().map(|(index, _)| *index).collect();
        let (reply, _) = channel();
        self.start_check_run(
            mark.picked,
            (mark.label, indexes, mark.part, mark.commit),
            reply,
        );
        // ^ Nobody waits for the call: its answer is a turn, or a background answer when the
        // worker's result already settled.
        let settled = self.record.state != TaskState::Working;
        match &mut self.check_run {
            Some(run) => {
                run.queued = true;
                run.background = settled;
                run.replies.clear();
            }
            None => {
                self.drop_check_run_mark();
            }
        }
    }

    fn handle(&mut self, request: HostRequest) -> HostResponse {
        // ^ While the owner works by hand nothing else writes the copy or its conversation.
        let held = self.record.waits_on(WaitOn::Hand)
            && matches!(
                request,
                HostRequest::Steer { .. }
                    | HostRequest::Add { .. }
                    | HostRequest::Land
                    | HostRequest::Notice { .. }
                    | HostRequest::AfterEnd { .. }
            );
        let message = match request {
            _ if held => Err(self.by_hand_refusal()),
            HostRequest::Status => Ok("".to_string()),
            HostRequest::Steer { text } => self.steer(text),
            HostRequest::Add { text, files, title } => self.add(text, files, title),
            HostRequest::Stop => self.stop(),
            HostRequest::Land => self.land(false),
            HostRequest::Hold { reason, by } => self.hold(reason, by),
            HostRequest::Move { chat, by } => Ok(self.move_to_chat(chat, by)),
            HostRequest::After {
                task,
                text,
                because,
                by,
            } => self.after(task, text, because, by),
            HostRequest::AfterEnd { task, text } => self.after_end(task, text),
            HostRequest::Hand => self.hand(),
            HostRequest::Handback => self.handback(),
            HostRequest::ChildLanded { child, commits } => self.child_landed(child, commits),
            HostRequest::Notice { text } => self.notice(text),
            HostRequest::Close { discard } => {
                if let Some(agent) = self.agent.take() {
                    agent.stop();
                }
                self.end_turn("the task closed");
                self.record.after = None;
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
            binary: self.binary.clone(),
        }
    }

    /// A landed or closed task takes no message: a turn would make it `working` again, and the
    /// close that follows its landing would then leave it `closed`, its commits on the default
    /// branch.
    fn require_open(&self) -> Result<()> {
        if !self.record.state.is_open() {
            bail!(
                "task t{} is {}: it takes no more messages",
                self.record.id,
                self.state_words()
            );
        }
        Ok(())
    }

    fn steer(&mut self, text: String) -> Result<String> {
        check_message(&text)?;
        self.require_open()?;
        if self.turn.is_none() {
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

    fn add(&mut self, text: String, files: Vec<String>, title: Option<String>) -> Result<String> {
        check_message(&text)?;
        self.require_open()?;
        let title = title.as_deref().map(checked_title).transpose()?;
        if !files.is_empty() {
            self.record.files = checked_files(&self.record.files, &files)?;
        }
        // ^ The words an add widened the task by name it: the title is what the lists, the
        // events and the owner read (docs/tasks.md "Title").
        let retitled = title.is_some();
        if retitled {
            self.record.title = title;
        }
        if retitled || !files.is_empty() {
            self.save();
        }
        self.log(LogEvent::Message {
            mode: MessageKind::Add,
            text: text.clone(),
        });
        Ok(self.deliver_or_queue(text))
    }

    /// Another task's land changed files this one shares. The text joins the worker's next
    /// turn, at its step boundary, and starts none; a task with no turn to come (ready,
    /// checking, failed) gets nothing, as its own land rebases it. ref: docs/tasks.md "Landing"
    fn notice(&mut self, text: String) -> Result<String> {
        check_message(&text)?;
        // ^ A task that has not started follows the default branch when it does.
        if self.awaits_start() {
            return Ok(format!("not sent: t{} has not started", self.record.id));
        }
        if self.turn.is_none() && self.record.state != TaskState::Waiting {
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

    /// `task move`: the chat owns the task from now on, in any state.
    fn move_to_chat(&mut self, chat: String, by: Actor) -> String {
        self.record.moved_to = Some(chat.clone());
        self.save();
        self.log(LogEvent::Moved {
            chat: chat.clone(),
            by,
        });
        format!("moved to chat {chat}")
    }

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

    /// Wait for task `task` to land and then add `text`, or wait no more (`None`). The
    /// chat host ends the wait. ref: docs/tasks.md "After another task"
    fn after(
        &mut self,
        task: Option<u32>,
        text: String,
        because: Option<AfterKind>,
        by: Actor,
    ) -> Result<String> {
        if !self.record.state.is_open() {
            bail!("task t{} is {}", self.record.id, self.state_words());
        }
        let wait = match task {
            Some(task) => {
                check_message(&text)?;
                if task == self.record.id {
                    bail!("a task cannot wait for itself");
                }
                let Some(because) = because else {
                    bail!("a wait needs a reason: --because needs|files|check");
                };
                Some(After {
                    task,
                    text: text.clone(),
                    since_unix_ms: now_unix_ms(),
                    because,
                })
            }
            None => None,
        };
        if self.record.after.is_none() && wait.is_none() {
            return Ok("unchanged".into());
        }
        // ^ The build starts while the wait is still set: a failure keeps it for the next try.
        if task.is_none() && self.build_parked() {
            self.start_parked_build(None)?;
        }
        self.record.after = wait;
        self.save();
        let message = match task {
            Some(task) => format!("waits for #{task} to land, then gets: {text}"),
            None => "waits for no task".into(),
        };
        self.log(LogEvent::After {
            task,
            text,
            because,
            by,
        });
        self.say(&message);
        if task.is_none() && self.awaits_start() {
            self.start_after_wait()?;
        }
        Ok(message)
    }

    /// The task waited for `task`, which landed (`text` is the add) or was closed. A wait
    /// that is gone, or on another task, is left as it is.
    fn after_end(&mut self, task: u32, text: Option<String>) -> Result<String> {
        if self
            .record
            .after
            .as_ref()
            .is_none_or(|after| after.task != task)
        {
            return Ok("unchanged".into());
        }
        let parked = self.build_parked();
        if parked {
            self.start_parked_build(text.clone())?;
        }
        self.record.after = None;
        self.save();
        self.log(LogEvent::AfterEnded {
            task,
            landed: text.is_some(),
        });
        // ^ A task that has not started starts, also when `task` was closed: the lead hears
        // of it from the event of the closing.
        if self.awaits_start() {
            self.start_after_wait()?;
            return Ok(format!("#{task} is over: the first turn started"));
        }
        if parked {
            return Ok(format!("#{task} is over: the build started"));
        }
        match text {
            Some(text) => self.add(text, Vec::new(), None),
            None => Ok(format!("#{task} was closed: the wait is over")),
        }
    }

    fn deliver_or_queue(&mut self, text: String) -> String {
        // ^ A task that waits on `after` has not read its prompt: the add is in the log and
        // joins the first turn, which starts when the wait ends.
        let unstarted = self.awaits_start();
        if unstarted && self.record.after.is_some() {
            return "queued: it follows the prompt when the wait ends".into();
        }
        // ^ The add is in the log: the build turn reads it from there (`adds_since_result`).
        if self.build_parked() {
            return "queued: the build starts with it when the wait ends".into();
        }
        self.check_retries = 0;
        self.stopped = false;
        if self.turn.is_some() || self.record.state == TaskState::Checking {
            self.pending.push_back(text);
            "queued until the current turn ends".into()
        } else if unstarted {
            // ^ Never started, and no wait: the first turn carries the prompt, then this add.
            match self.first_turn() {
                Ok(()) => "started the first turn".into(),
                Err(error) => format!("cannot start the first turn: {error:#}"),
            }
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
        self.end_turn(STOPPED_BY_REQUEST);
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
        self.check_run_next = None;
        let dropped = self.pending.len();
        self.pending.clear();
        self.log(LogEvent::Stopped {
            reason: STOPPED_BY_REQUEST.into(),
        });
        self.wait(WaitOn::Owner, STOPPED_BY_REQUEST.into());
        Ok(if dropped == 0 {
            "stopped; the next add resumes the same conversation".into()
        } else {
            format!("stopped and dropped {dropped} queued add(s); the next add resumes")
        })
    }

    fn by_hand_refusal(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "the owner works on task t{} by hand; run `wsx task hand {} --back` first",
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
        self.end_turn("the owner took it by hand");
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
                "the working copy has uncommitted changes: {}; commit them, then run `wsx task hand {} --back` again",
                changes.join(", "),
                self.record.id
            );
        }
        // ^ Commits after the last revision (before the first: after where the branch left the
        // default branch, since a wait's end moves the copy without a revision).
        let since = match self.record.revision {
            0 => copy::fork_point(
                &self.record.working_copy,
                "HEAD",
                &self.record.default_branch,
            )
            .unwrap_or_else(|_| self.record.base.clone()),
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
        // ^ Every path that starts a turn ends here: none runs for a task that left (`landed`),
        // and none while the owner works by hand.
        if !self.record.state.is_open() {
            self.say(&format!("{}: no agent turn starts", self.state_words()));
            return;
        }
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
        // ^ Before a turn, the one rule of where the task goes (`agent::next`): home once its
        // own agent is free, or a wait while its account refuses turns. A steer into a running
        // turn goes to that turn. ref: docs/tasks.md "Agents"
        let mut home = false;
        if self.turn.is_none() {
            let now = now_unix_ms();
            match self.ask(Ask::Turn, now) {
                Next::Stay => {}
                Next::Home => home = true,
                Next::Wait(until_ms) => {
                    // ^ Away, the wait is on the own account: the record goes home first.
                    if self.away() {
                        let did = "could not take the turn: its account is limited";
                        self.wait_away(did, text, until_ms, now);
                    } else {
                        self.wait_at_home(text.to_string(), until_ms, now);
                    }
                    return Ok(());
                }
            }
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
        if self.wait_for_disk(DiskWait::Turn(text.to_string()))
            || self.wait_for_load(DiskWait::Turn(text.to_string()))
        {
            return Ok(());
        }
        // ^ Going home: a fallback whose conversation answered writes its handoff notes first
        // (`Handoff::Home`); else the own agent takes the turn now. ref: docs/tasks.md "Agents"
        if home && !(self.record.agent_session.is_some() && self.context_tokens > 0) {
            // ^ No request went: a notes file here is an earlier rotation's.
            self.clear_notes();
            self.return_home(text.to_string(), None);
            return Ok(());
        }
        let handoff;
        let why = match home {
            true => Some(Handoff::Home),
            false => self.rotation_due(trigger),
        };
        let (sent, rotating) = match why {
            Some(why) => {
                // ^ The notes the next conversation gets are this request's or none.
                self.clear_notes();
                handoff = self.handoff_request(&why);
                (handoff.as_str(), Some((text.to_string(), why)))
            }
            None => (text, None),
        };
        self.send_to_agent(sent)?;
        // ^ Only once the request went: a failed send left it set, and the next real result was
        // taken as the notes.
        self.rotating = rotating;
        self.turn_text = sent.to_string();
        self.trigger = trigger;
        self.pings = 0;
        self.set_state(TaskState::Working);
        Ok(())
    }

    /// Starts the agent when it does not run, and sends it one turn's message.
    fn send_to_agent(&mut self, text: &str) -> Result<()> {
        // ^ No turn runs here: a program updated since the agent started is replaced by one
        // that resumes the same conversation, through the start below.
        if self.record.agent_session.is_some() {
            if let Some(change) = self.agent.as_ref().and_then(Agent::replaced) {
                self.say(&format!("agent {} {change}", self.record.adapter));
                self.stop_agent();
            }
        }
        if self.agent.is_none() {
            let (session, resume) = match &self.record.agent_session {
                Some(session) => (session.clone(), true),
                None => (new_session_id()?, false),
            };
            self.generation += 1;
            let generation = self.generation;
            self.stop_requested = false;
            let launch = Launch::worker(&self.record, &self.dir.broker_path(), &self.dir.path)?;
            self.policy.set(launch.boundary.policy());
            self.agent = Some(Agent::spawn(
                &self.record.adapter,
                &launch,
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
        let agent = format!("{}/{}", self.record.adapter, self.record.model);
        let quiet_ms = stall::turn_quiet_ms(self.settings().check_timeout_secs);
        self.turn = Some(stall::Turn::start(&self.dir.path, agent, quiet_ms));
        Ok(())
    }

    /// End the running turn's record with `reason`; none runs after it.
    fn end_turn(&mut self, reason: &str) {
        if let Some(turn) = self.turn.take() {
            turn.end(&self.dir.path, reason);
        }
    }

    fn pinging(&self) -> bool {
        self.turn.is_some() && self.trigger == TurnTrigger::Ping
    }

    /// When the next ping is due (unix ms): the worker waits on the checks, its last answer is younger than Claude's 1-hour prompt cache, and fewer than
    /// `KEEP_WARM_PINGS` came since its last turn. Never in a usage limit, a disk wait, by
    /// hand, or when the next turn rotates by size anyway. ref: docs/tasks.md "Context rotation"
    fn ping_due(&self) -> Option<u64> {
        let waits = self.record.state == TaskState::Checking;
        // ^ Claude's cache lives 1 hour; Pi's default lives 5 minutes, which no ping keeps.
        if !waits
            || self.stopped
            || self.record.adapter != "claude"
            || self.turn.is_some()
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
        self.end_turn(&turn.ended());
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
        self.spend(turn.usage.cost_micro_usd, now);
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
                if let Some(turn) = &mut self.turn {
                    turn.output(&self.dir.path);
                }
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
            // ^ Read at the next turn's start (`rotation_due`), never inside a turn: a steer with
            // the handoff request raced the turn's end, so a turn's own result was taken as the
            // notes, or the notes answer as the task's result (2026-11-25).
            Some(AgentLine::Context(tokens)) => self.context_tokens = tokens,
            None => {
                self.agent = None;
                // ^ A handoff turn that never ended leaves no notes: no later result is one.
                self.rotating = None;
                // ^ A ping lost with its process costs the task nothing: the next turn resumes.
                if self.pinging() {
                    self.end_turn("the agent process ended");
                    self.after_ping_ends();
                } else if self.turn.is_some() && !self.stop_requested {
                    self.end_turn("the agent process ended during a turn");
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
        if turn.simplified && !self.record.simplified {
            self.record.simplified = true;
            self.save();
        }
        // ^ A result is the agent answering again, also one that waits for background jobs.
        self.limit_slot = None;
        let Some(turn) = self.wait_for_background(turn) else {
            return;
        };
        self.end_turn(&turn.ended());
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
            let (until_ms, problem) = agent::record_limit(
                &self.record.adapter,
                &self.record.model,
                limit,
                self.limit_attempt,
                now,
            );
            if let Some(message) = problem {
                self.say(&message);
            }
            // ^ The agent has the turn in its history: the resume continues it.
            let mut text = vec![LIMIT_CONTINUE.to_string()];
            text.extend(self.pending.drain(..));
            self.refused(text.join("\n\n"), &turn.text, until_ms, now);
            return;
        }
        // ^ A failed sign-in: the running process keeps its old credentials, so the turn is
        // sent again to a new one, and the sign-in record on the account decides where.
        if turn.is_error && agent::auth_failed(&turn.text) {
            let now = now_unix_ms();
            let (until_ms, problem) =
                agent::record_sign_in_failed(&self.record.adapter, &self.record.model, now);
            if let Some(message) = problem {
                self.say(&message);
            }
            self.stop_agent();
            let mut text = vec![std::mem::take(&mut self.turn_text)];
            text.extend(self.pending.drain(..));
            self.refused(text.join("\n\n"), &turn.text, until_ms, now);
            return;
        }
        self.limit_attempt = 0;
        // ^ The turn ended: nothing of it is unfinished (`come_home`).
        self.turn_text.clear();
        self.spend(turn.usage.cost_micro_usd, now_unix_ms());
        if !turn.is_error {
            store::clear_sign_in_failed(&self.account());
        }
        if let Some((waiting, why)) = self.rotating.take() {
            self.log(LogEvent::Agent {
                said: false,
                text: format!("handoff notes: {}", first_line(&turn.text)),
            });
            match why {
                Handoff::Home => self.return_home(waiting, None),
                Handoff::Size => self.rotate(waiting, None),
                Handoff::Idle(idle) => self.rotate(waiting, Some(idle)),
            }
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
        // ^ A check-run never holds the result: it settles now, and the run's answer comes in
        // the background. One that ended during the turn answers right after the settle.
        // ref: docs/tasks.md "Check-run"
        let ended = match &mut self.check_run {
            Some(run) => match run.answer.take() {
                Some(answer) => Some((answer, run.commit.clone())),
                None => {
                    run.background = true;
                    None
                }
            },
            None => None,
        };
        if ended.is_some() {
            self.check_run = None;
        }
        // ^ A failure on the current head that is already in is the next turn: nothing to
        // settle before it. Any other answer follows the settle.
        let on_head = |commit: &str| head(&self.record.working_copy).is_some_and(|h| h == commit);
        match ended {
            Some((answer, commit)) if answer.fails && on_head(&commit) => {
                self.answer_in_background(answer, &commit)
            }
            ended => {
                self.settle_result();
                if let Some((answer, commit)) = ended {
                    self.answer_in_background(answer, &commit);
                }
            }
        }
    }

    /// A done result starts the scenarios its change touches, as the worker's own check-run of
    /// them would, in the background: the result settles without them. Once per commit; while
    /// a check-run is pending they start after its answer. ref: docs/tasks.md "Scenarios"
    fn run_touched_scenarios(&mut self) {
        let done = self
            .last_result
            .as_ref()
            .is_some_and(|result| result.status == "done");
        // ^ Only a clean tree keeps the answer: on a dirty one each result would run them again.
        let clean = || {
            copy::clean_tree(&self.record.working_copy)
                .ok()
                .flatten()
                .is_some()
        };
        let commit = head(&self.record.working_copy);
        if !done
            || !self.record.state.is_open()
            || self.stopped
            || self.record.plan
            || self.check_run.is_some()
            || commit.is_none()
            || self.touched_for == commit
            || !clean()
        {
            return;
        }
        let names: Vec<String> = self
            .scenarios_touched("HEAD")
            .into_iter()
            .filter_map(|(name, _, touched)| touched.then_some(name))
            .collect();
        if names.is_empty() {
            return;
        }
        let argv: Vec<String> = ["task", "check-run"]
            .into_iter()
            .map(String::from)
            .chain(names)
            .collect();
        // ^ Nobody waits on the call: the answer comes in the background.
        self.touched_for = commit;
        let (reply, _answer) = std::sync::mpsc::channel();
        self.check_run(&argv, reply);
        if let Some(run) = &mut self.check_run {
            run.background = true;
            let label = run.label.clone();
            self.say(&format!(
                "the scenarios its change touches run in the background: {label}"
            ));
        }
    }

    /// Each scenario with `paths` as (name, command, whether the change of `head` touches
    /// it); empty when the config or git cannot tell.
    fn scenarios_touched(&self, head: &str) -> Vec<(String, String, bool)> {
        let Ok(config) = load_task_config(&self.record.project_path) else {
            return Vec::new();
        };
        let touched = copy::touched_scenarios(
            &self.record.working_copy,
            head,
            &self.record.default_branch,
            &config,
        )
        .unwrap_or_else(|error| {
            self.say(&format!(
                "cannot tell the scenarios the change touches: {error:#}"
            ));
            BTreeMap::new()
        });
        touched
            .into_iter()
            .filter_map(|(name, touched)| {
                let command = config.scenarios.get(&name)?.command.clone();
                Some((name, command, touched))
            })
            .collect()
    }

    /// What follows the worker's result, held in `last_result`: the block, plan and step
    /// rules, then the checks.
    fn settle_result(&mut self) {
        let Some(result) = self.last_result.clone() else {
            return;
        };
        self.run_touched_scenarios();
        // ^ Claude's own turn (a background task's notice) can answer while the checks of an
        // earlier result run: those checks answer it, never a second run beside them.
        if let Some((_, newer)) = &mut self.gate {
            *newer = true;
        }
        // ^ A status is no question: the task waits for the owner, whose add continues it. No
        // checks run. ref: docs/tasks.md "Status"
        if result.status == "paused" {
            self.wait(WaitOn::Paused, question(&result.summary));
            return;
        }
        // ^ Only the worker's own word blocks a task. Denials it worked around are evidence.
        if result.status == "blocked" {
            self.log(LogEvent::Blocked {
                denials: self.last_denials.clone(),
            });
            self.wait(WaitOn::Owner, question(&result.summary));
            // ^ A worker stopped by a full disk continues once there is space again.
            self.wait_for_disk(DiskWait::Turn(
                "The disk was full; there is space again. Continue the task where it stopped."
                    .into(),
            ));
            return;
        }
        // ^ A plan has nothing to check: the lead reads it and continues the task with an
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
            if let (Some(build), Some(after)) = (self.build_step(), &self.record.after) {
                if build == self.record.step as usize {
                    let reason = format!("planned; its build waits for #{} to land", after.task);
                    self.wait(WaitOn::Owner, reason);
                    return;
                }
            }
            let mut text: Vec<String> = self.pending.drain(..).collect();
            text.extend(self.step_text());
            self.start_turn(&text.join("\n\n"), TurnTrigger::Step);
            return;
        }
        // ^ A full task whose worker was lent the simplify skill and never loaded it goes back
        // once; a second miss shows in the ready event. Claude's stream is the only record of a
        // loaded skill. ref: docs/tasks.md "Workflow"
        if result.status == "done"
            && !self.record.small
            && !self.record.steps.is_empty()
            && self.record.adapter == "claude"
            && !self.record.simplified
            && !self.record.simplify_asked
            && self.pending.is_empty()
            && super::claude::lends_simplify(&self.dir.path)
        {
            self.record.simplify_asked = true;
            self.save();
            self.say("simplify did not run: sent back to the worker");
            let text = format!(
                "The simplify skill did not run in this task. Run {}:simplify \
                 on your changes now, commit what it changes, then finish as before.",
                super::claude::SKILLS_PLUGIN_NAME
            );
            self.start_turn(&text, TurnTrigger::Simplify);
            return;
        }
        // ^ What a test, an assert or a capture shows is the worker's to prove: such an item
        // goes back once, within the check retries. ref: docs/tasks.md "To check"
        let unattached = result.unattached_images();
        if !unattached.is_empty() {
            self.say(&unattached);
            self.log(LogEvent::Agent {
                text: unattached,
                said: false,
            });
        }
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
        if self.turn.is_none() {
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
    fn rotation_due(&self, trigger: TurnTrigger) -> Option<Handoff> {
        let config = load_task_config(&self.record.project_path).unwrap_or_default();
        if self.rotating.is_some()
            || self.check_run.is_some()
            || trigger == TurnTrigger::ResultRetry
            || self.record.agent_session.is_none()
        {
            return None;
        }
        if self.context_tokens >= config.rotate_tokens {
            return Some(Handoff::Size);
        }
        let idle_ms = now_unix_ms().saturating_sub(self.answered_ms);
        (config.rotate_idle_seconds > 0
            && self.answered_ms > 0
            && idle_ms > config.rotate_idle_seconds * 1000
            && self.context_tokens >= config.rotate_tokens / 2)
            .then(|| Handoff::Idle(format!("idle {} min", idle_ms / 60_000)))
    }

    /// The message of the turn before a rotation: the worker writes its notes and ends the old
    /// conversation; the waiting turn goes on in a fresh one (`rotate`). Sent as a turn of its
    /// own, never into a running turn. ref: docs/tasks.md "Context rotation"
    fn handoff_request(&self, why: &Handoff) -> String {
        let context = format!("context {}k", self.context_tokens / 1000);
        let (said, why, next) = match why {
            Handoff::Size => (context, "past this task's limit".to_string(), "a fresh one"),
            Handoff::Idle(idle) => (
                format!("{context}, {idle}"),
                "this conversation was idle past this task's limit, so its prompt cache is gone"
                    .to_string(),
                "a fresh one",
            ),
            Handoff::Home => (
                "its own agent is free again".to_string(),
                "and this task's own agent is free again".to_string(),
                "its own conversation",
            ),
        };
        self.say(&format!(
            "{said}: the worker writes handoff notes, then {next} continues"
        ));
        format!(
            "Your context is {}k tokens, {why}. This conversation ends after \
             this turn and {next} continues the task; your working copy and commits stay. \
             Do no other work now. Write {} for that conversation, at most 12 KB: what \
             the task is doing now, what is done, what remains and the next action, decisions \
             and why, the files and functions that matter, the checks you ran and how they \
             ended, and what you learned that the code does not show. Then return your result: \
             status done and a one-line summary. That result is not checked.",
            self.context_tokens / 1000,
            self.dir.file(ROTATE_NOTES).display()
        )
    }

    /// Remove `rotate-notes.md`: before each handoff request and once a conversation change took
    /// the notes in, so no later change reads them as current. A file left from an earlier
    /// rotation was fed to a fresh conversation and stored in memory again (2026-11-25).
    fn clear_notes(&self) {
        if let Err(error) = std::fs::remove_file(self.dir.file(ROTATE_NOTES)) {
            if error.kind() != std::io::ErrorKind::NotFound {
                self.say(&format!("cannot clear the handoff notes: {error}"));
            }
        }
    }

    /// The handoff notes of a conversation that ends, as an episode of the project's memory.
    /// The worker wrote them for the next conversation: no extra turn. ref: docs/memory.md "Capture"
    fn remember_rotation(&self, notes: &str) {
        let done = Store::project(
            &self.record.project_id,
            &self.record.project,
            &self.record.project_path,
        )
        .and_then(|store| {
            memory_manage::add_episode(
                &store.without_rules(),
                &format!("rotation-{}", self.record.id),
                &format!("task:{}", self.record.id),
                notes,
                &self.dir.file(ROTATE_NOTES),
            )
        });
        if let Err(error) = done {
            self.say(&format!("cannot remember the handoff notes: {error}"));
        }
    }

    /// End the conversation after its handoff notes and start `waiting` in a fresh one,
    /// with the task as asked, what was sent since, the notes and the step (`rotation_prompt`).
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
            Ok(Some(bytes)) if !bytes.trim_ascii().is_empty() => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                self.remember_rotation(&text);
                // ^ Inlined: the file has done its job. Notes too long to inline stay for the
                // fresh conversation to read; the next request clears them.
                self.clear_notes();
                format!("The earlier conversation's handoff notes:\n{text}")
            }
            Ok(_) => "The earlier conversation wrote no handoff notes.".into(),
            Err(_) => format!(
                "Read the earlier conversation's handoff notes first: {}",
                self.dir.file(ROTATE_NOTES).display()
            ),
        };
        // ^ The step belongs to the turn that ends, not to the record: a message after the
        // result is a free turn, and the record's step stays at the last one.
        let index = self.record.step as usize;
        let step = matches!(self.trigger, TurnTrigger::Step | TurnTrigger::CheckRetry)
            .then(|| self.record.steps.get(index))
            .flatten()
            .map(|step| (index, self.record.steps.len(), step));
        let prompt = self.dir.prompt().ok().flatten();
        let text = rotation_prompt(
            tokens,
            prompt.as_deref(),
            step,
            &waiting,
            self.pending.drain(..).collect(),
            &notes,
        );
        self.start_turn(&text, TurnTrigger::Rotation);
    }

    /// The host's task context for the first turn: the task's files, the open tasks that
    /// share them and the memory's start block (area briefs, and
    /// rules, the records about the files and the task's words), so the worker needs no turn of its own to
    /// load context. None when it has nothing. ref: docs/tasks.md "Workflow"
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
        // ^ The area briefs and the rules that meet the files: one rule, in the core. No fact
        // records: the paid evaluation found they gave no gain. ref: docs/memory.md "Start set"
        let areas = load_project_config(&self.record.project_path).memory_areas;
        if let Ok(store) = Store::project(
            &self.record.project_id,
            &self.record.project,
            &self.record.project_path,
        ) {
            let start = Start {
                files: &self.record.files,
                areas: &areas,
            };
            let block =
                memory_start::block(&store.without_rules(), &start, memory_start::BUDGET_BYTES);
            if !block.is_empty() {
                out.push(block);
            }
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

    /// A turn starts only when the scheduler lets it (the load, in the
    /// order of `schedule::urgency`), one at a time `disk_settle()` apart so the next sees the
    /// load this one adds. The turn waits in the `turn` queue and resumes by itself; its text
    /// joins `disk_wait` like a turn held for disk. True when it waits.
    /// ref: docs/design/scheduler.md
    fn wait_for_load(&mut self, what: DiskWait) -> bool {
        // ^ A fixture (`WSX_SCENARIO_SLOTS`) starts its turns at once: its tests read the first wait.
        if self.disk_slot.is_some() || !slot::ruled() {
            return false;
        }
        let want = Want::new(Lane::Turns, self.urgency().0);
        let ticket = match slot::Ticket::join("turn-slot", "the turn slot", 1, want.priority) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.say(&format!("cannot join the turn queue: {error}"));
                return false;
            }
        };
        match slot::admit(&ticket, &want, &self.slot_label(), &mut self.load_since) {
            Ok(Taken::Slot(taken)) => {
                self.disk_slot = Some((taken, Instant::now()));
                false
            }
            Ok(Taken::Ci(reason)) | Err(reason) => {
                self.disk_queue = Some(ticket);
                self.disk_wait = Some(what);
                self.say(&format!("waiting for the load: {reason}"));
                self.wait(WaitOn::Load, reason);
                true
            }
        }
    }

    /// "<project> t<id>", how a queue names this task's holder.
    fn slot_label(&self) -> String {
        format!("{} t{}", self.record.project, self.record.id)
    }

    /// Resume once there is space. Tasks resume one at a time, oldest wait first, each
    /// `disk_settle()` after the one before: resumed at once, they would fill the disk again.
    /// ref: docs/tasks.md "Checks, revisions and report"
    fn resume_after_disk(&mut self) {
        if self.low_disk().is_some() {
            return;
        }
        if let Some(queue) = &self.disk_queue {
            let label = self.slot_label();
            let want = Want::new(Lane::Turns, self.urgency().0);
            let taken = match self.record.waiting.as_ref().map(|waiting| waiting.on) {
                Some(WaitOn::Load) => {
                    match slot::admit(queue, &want, &label, &mut self.load_since) {
                        Ok(Taken::Slot(slot)) => Ok(slot),
                        Ok(Taken::Ci(reason)) | Err(reason) => Err(reason),
                    }
                }
                _ => queue.try_take(&label),
            };
            match taken {
                Ok(taken) => self.disk_slot = Some((taken, Instant::now())),
                Err(reason) => {
                    // ^ Not again for new CPU figures in a load reason (`schedule::wait_key`).
                    let same = self.record.waiting.as_ref().is_some_and(|w| {
                        schedule::wait_key(&w.reason) == schedule::wait_key(&reason)
                    });
                    if !same {
                        self.say(&reason);
                        let on = self.record.waiting.as_ref().map_or(WaitOn::Disk, |w| w.on);
                        self.wait(on, reason);
                    }
                    return;
                }
            }
        }
        self.disk_queue = None;
        self.load_since = None;
        let Some(what) = self.disk_wait.take() else {
            return;
        };
        match what {
            DiskWait::Turn(text) => self.start_turn(&text, TurnTrigger::Disk),
            DiskWait::Checks => self.start_checks(),
        }
    }

    /// The settings of the project, as every turn reads them.
    fn settings(&self) -> wsx_core::model::workspace::TaskConfig {
        load_task_config(&self.record.project_path).unwrap_or_default()
    }

    /// The task's own agent, filled in at the host's start.
    fn home(&self) -> &Home {
        self.record
            .home
            .as_ref()
            .expect("the host fills in the record's home")
    }

    /// The task runs on a fallback.
    fn away(&self) -> bool {
        let home = self.home();
        (home.adapter.as_str(), home.model.as_str())
            != (self.record.adapter.as_str(), self.record.model.as_str())
    }

    /// Where the task goes (`agent::next`, the one rule): asked before every turn and at every
    /// look of a wait. ref: docs/tasks.md "Agents"
    fn ask(&self, ask: Ask, now: u64) -> Next {
        let home = self.home();
        agent::next(
            (&home.adapter, &home.model),
            (&self.record.adapter, &self.record.model),
            ask,
            now,
        )
    }

    /// Stop the agent process; its end is no lost turn.
    fn stop_agent(&mut self) {
        if let Some(agent) = self.agent.take() {
            self.stop_requested = true;
            agent.stop();
        }
    }

    /// The current account refused the turn (a usage limit or a failed sign-in, on record
    /// until `until_ms`), with `why` its words: `text` continues the task where `agent::next`
    /// says: a wait on the own agent; a task still away from an earlier wsx goes home, for the
    /// turn or for the wait. ref: docs/tasks.md "Agents"
    fn refused(&mut self, text: String, why: &str, until_ms: u64, now: u64) {
        let did = format!("was refused: {}", first_line(why));
        let until_ms = match self.ask(Ask::Refused(until_ms), now) {
            Next::Home => return self.return_home(text, Some(did)),
            Next::Wait(until_ms) => until_ms,
            Next::Stay => until_ms,
        };
        if !self.away() {
            return self.wait_at_home(text, until_ms, now);
        }
        self.wait_away(&did, "", until_ms, now);
    }

    /// A task on a fallback (from an earlier wsx) that was refused goes home for the wait on the
    /// own account: `did` is what the fallback did, `text` the message the turn adds.
    fn wait_away(&mut self, did: &str, text: &str, until_ms: u64, now: u64) {
        let reason = format!("back on {}: {did}; waits for it", self.home().adapter);
        match self.come_home(did, reason.clone()) {
            Some(opening) => self.wait_at_home(join_turn(opening, text), until_ms, now),
            None => self.rotate(self.fresh_turn(did, text), Some(reason)),
        }
    }

    /// Back on the own agent with `text` as the turn that starts now, and `did` what the
    /// fallback answered when it was refused, else what its handoff notes say.
    /// ref: docs/tasks.md "Agents"
    fn return_home(&mut self, text: String, did: Option<String>) {
        let reason = format!("back on {}", self.home().adapter);
        let did = did.unwrap_or_else(|| self.notes_did());
        match self.come_home(&did, reason.clone()) {
            Some(opening) => {
                // ^ `did` carries the notes on.
                self.clear_notes();
                self.start_turn(&join_turn(opening, &text), TurnTrigger::Limit)
            }
            None => self.rotate(self.fresh_turn(&did, &text), Some(reason)),
        }
    }

    /// What the fallback did, from its handoff notes.
    fn notes_did(&self) -> String {
        match self.dir.read_file(ROTATE_NOTES, MAX_ROTATE_NOTES_BYTES) {
            Ok(Some(bytes)) if !bytes.trim_ascii().is_empty() => format!(
                "worked on the task; its handoff notes:\n{}",
                String::from_utf8_lossy(&bytes)
            ),
            _ => "worked on the task and wrote no handoff notes".into(),
        }
    }

    /// The record goes back to the own agent and the fallback's process ends, logged as the
    /// switch `reason`. With the own conversation kept, returns the opening of the turn that
    /// resumes it: the turn the own agent could not finish, what the fallback `did`, and the
    /// turn the fallback could not finish when it had answered before. None when the task
    /// had no own conversation: a fresh one starts (`fresh_turn`, `rotate`).
    fn come_home(&mut self, did: &str, reason: String) -> Option<String> {
        self.stop_agent();
        let fallback = format!("{}/{}", self.record.adapter, self.record.model);
        // ^ The fallback's turn is lost only when its conversation had answered before; its
        // first turn is the one the own agent left, which that agent has. A turn that waited
        // on its handoff notes is the lost one.
        let unfinished = match self.rotating.take() {
            Some((waiting, _)) => waiting,
            None if self.context_tokens > 0 => std::mem::take(&mut self.turn_text),
            None => String::new(),
        };
        let home = self
            .record
            .home
            .as_mut()
            .expect("the host fills in the record's home");
        let to = format!("{} ({}/{})", home.adapter, home.adapter, home.model);
        self.record.adapter = home.adapter.clone();
        self.record.model = home.model.clone();
        self.record.agent_session = home.session.take();
        let turn = std::mem::take(&mut home.turn);
        self.record.fallback = false;
        self.context_tokens = 0;
        self.limit_attempt = 0;
        // ^ What a fresh own conversation continues (`fresh_turn`).
        self.turn_text = [turn.as_str(), unfinished.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.save();
        self.say(&format!("{reason}: {fallback} leaves"));
        self.log(LogEvent::AgentSwitch {
            from: fallback.clone(),
            to,
            reason,
        });
        self.record.agent_session.is_some().then(|| {
            let turn = match turn.is_empty() {
                true => LIMIT_CONTINUE.to_string(),
                false => turn,
            };
            let lost = match unfinished.is_empty() {
                true => String::new(),
                false => format!("; the turn it could not finish:\n{unfinished}"),
            };
            format!(
                "{turn}\n\nWhile your agent was unavailable, {fallback} {did}{lost}\nCheck what \
                 the working copy already holds (git status, git log, git diff) before you \
                 continue."
            )
        })
    }

    /// The turn a fresh own conversation starts with after `come_home` (the rotation text
    /// carries the task's prompt and the fallback's notes): what the fallback `did` when it
    /// was refused, the turn the own agent could not finish, and `text`.
    fn fresh_turn(&self, did: &str, text: &str) -> String {
        let mut turn = Vec::new();
        if did.starts_with("was refused") {
            turn.push(format!("The earlier conversation's agent {did}."));
        }
        if !self.turn_text.is_empty() {
            turn.push(self.turn_text.clone());
        }
        if !text.is_empty() && !self.turn_text.contains(text) {
            turn.push(text.to_string());
        }
        turn.join("\n\n")
    }

    /// Wait at home for the own account until `until_ms` (as the record names it now) with
    /// `text` the turn that continues; the record ends the wait (`resume_after_limit`).
    fn wait_at_home(&mut self, text: String, until_ms: u64, now: u64) {
        // ^ A sign-in's try must run in a new process: the running one keeps its credentials.
        if store::sign_in_failed_until(&self.account(), now).is_some() {
            self.stop_agent();
        }
        self.limit_slot = None;
        self.log(LogEvent::LimitWait { until_ms });
        self.wait_for_limit(text, until_ms);
    }

    /// Hold the turn `text` until the account's limit ends, in the machine's limit queue.
    /// `until_ms` is the record's reset as shown, not what ends the wait: the record does
    /// (`resume_after_limit`).
    fn wait_for_limit(&mut self, text: String, until_ms: u64) {
        let now = now_unix_ms();
        // ^ A wait looks every 10 s, for the account's record ending early.
        self.limit_wait = Some(LimitWait {
            text,
            until_ms,
            next_ms: until_ms.min(now + HOME_POLL_MS),
        });
        let reason = self.limit_reason(until_ms, now);
        self.say(&reason);
        self.wait(WaitOn::Limit, reason);
    }

    /// The reason of the wait now: the failed sign-in's retry, else the window and reset the
    /// limit record names.
    fn limit_reason(&self, until_ms: u64, now: u64) -> String {
        limit::wait_reason(&self.record.adapter, &self.account(), until_ms, now)
    }

    /// The account this task's turns run on.
    fn account(&self) -> String {
        agent::account(&self.record.adapter, &self.record.model)
    }

    /// A turn with no line from the agent past its record's deadline ends: the agent is stopped and
    /// the turn is its error. On a fallback it counts as a refusal with no reset, so the task
    /// moves on (`refused`); on the own agent it is the turn's failed result.
    /// ref: docs/tasks.md "Agents"
    fn turn_hung(&mut self) {
        let quiet = self
            .turn
            .as_ref()
            .map_or(Duration::ZERO, stall::Turn::quiet);
        let turn = agent::hung_turn(quiet, self.away());
        self.say(&turn.text);
        self.stop_agent();
        self.on_result(turn);
    }

    /// Continue after a usage limit or a failed sign-in. The account's record decides: it ends
    /// the wait when it is gone (a probe, or another agent's call, found the account answering)
    /// or past, and it names the reset shown meanwhile. Tasks resume one at a time, oldest wait
    /// first, on their own agent: each holds the limit
    /// slot until its agent answers, so a limit that has not lifted stops only one.
    /// ref: docs/tasks.md "Claude adapter"
    fn resume_after_limit(&mut self) {
        let now = now_unix_ms();
        let Some(wait) = self.limit_wait.as_ref().filter(|wait| wait.next_ms <= now) else {
            return;
        };
        let shown_ms = wait.until_ms;
        // ^ The own account's probe, whatever agent the task is on now.
        if self.home().adapter == "claude" {
            super::claude::probe_limit();
        }
        match self.ask(Ask::Look, now) {
            Next::Wait(until_ms) => {
                let wait = self.limit_wait.as_mut().expect("the wait is on");
                wait.until_ms = until_ms;
                wait.next_ms = until_ms.min(now + HOME_POLL_MS);
                if shown_ms != until_ms {
                    let reason = self.limit_reason(until_ms, now);
                    self.say(&reason);
                    self.wait(WaitOn::Limit, reason);
                }
                return;
            }
            Next::Home | Next::Stay => {}
        }
        if let Some(queue) = &self.limit_queue {
            let label = format!("{} t{}", self.record.project, self.record.id);
            match queue.try_take(&label) {
                Ok(taken) => self.limit_slot = Some(taken),
                Err(reason) => {
                    let wait = self.limit_wait.as_mut().expect("the wait is on");
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
            self.say("the account answers again: the agent continues");
            self.start_turn(&wait.text, TurnTrigger::Limit);
        }
    }

    /// Free MiB and the GB a task needs, when less than that is free.
    fn low_disk(&self) -> Option<(u64, u64)> {
        let min_gb =
            load_task_config(&self.record.project_path).map_or(5, |settings| settings.min_free_gb);
        let free = || store::free_mb(&self.record.working_copy);
        if free().is_some_and(|free_mb| free_mb < min_gb * 1024) {
            // ^ Before a task waits, the idle copies of the project give their build folders back.
            let goal = (min_gb * 1024).max(build::RECLAIM_BELOW_MB);
            build::reclaim(
                &self.record.project_id,
                Some(&self.record.working_copy),
                goal,
                free,
            );
        }
        free()
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
            match slot::Ticket::join(&format!("{what}-slot"), what, 1, Priority::Normal) {
                Ok(ticket) => *queue = Some(ticket),
                Err(error) => self.say(&format!("cannot join the {what} queue: {error}")),
            }
        }
        // ^ A host that restarts during a limit wait resumes at the same time.
        let until_ms = match on {
            WaitOn::Limit => self.limit_wait.as_ref().map(|wait| wait.until_ms),
            _ => None,
        };
        let waiting = Waiting {
            on,
            reason,
            until_ms,
        };
        self.move_to(TaskState::Waiting, Some(waiting));
    }

    /// "ready", "waiting (the owner works on it by hand)" or "checking (queued for ...)".
    fn state_words(&self) -> String {
        match &self.record.waiting {
            Some(waiting) => format!(
                "{} ({})",
                self.record.state.as_str(),
                first_line(&waiting.reason)
            ),
            None => self.record.state.as_str().into(),
        }
    }

    /// Keep a reported result for the turn that ends next; the last one of a turn counts.
    fn reported(&mut self, text: Option<String>) -> broker::Response {
        let Some(text) = text.filter(|_| self.turn.is_some()) else {
            return broker::refused(
                "`wsx task result -` takes the result JSON on stdin during a turn".into(),
            );
        };
        match serde_json::from_str::<serde_json::Value>(&text).map(|value| {
            let valid = serde_json::from_value::<WorkerResult>(value.clone()).map(|result| {
                matches!(
                    result.status.as_str(),
                    "done" | "blocked" | "paused" | "failed"
                )
            });
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
            Ok((_, Ok(false))) => {
                broker::refused("status must be done, blocked, paused or failed".into())
            }
            Ok((_, Err(error))) | Err(error) => {
                broker::refused(format!("the result is not valid: {error}"))
            }
        }
    }

    /// `wsx task check-run [<n>...]` from the worker: the declared checks (all, or by number)
    /// run as after a result, in a thread, and answer the worker; no revision is recorded.
    fn check_run(&mut self, argv: &[String], reply: Sender<broker::Response>) {
        let refuse = |message: String| {
            let _ = reply.send(broker::refused(message));
        };
        let (numbers, part) = match argv {
            [task, sub, args @ ..] if task == "task" && sub == "check-run" => {
                match split_part(args) {
                    Ok(split) => split,
                    Err(message) => return refuse(message),
                }
            }
            _ => {
                return refuse(
                    "a worker may run only `wsx task check-run [<n>...] [--part <p>]`".into(),
                )
            }
        };
        if self.record.waits_on(WaitOn::Hand) {
            return refuse(format!("{:#}", self.by_hand_refusal()));
        }
        // ^ A late call after the result: the checks of that result run now, in one slot.
        if self.record.state == TaskState::Checking {
            return refuse("the checks after your result run now".into());
        }
        self.follow_checks();
        let (picked, label) = match self.pick_checks(&numbers, &part) {
            Ok(picked) => picked,
            Err(message) => return refuse(message),
        };
        let indexes: Vec<usize> = picked.iter().map(|(index, _)| *index).collect();
        let commit = head(&self.record.working_copy).unwrap_or_default();
        let Some(reply) = self.join_check_run(argv, (&indexes, &part, &commit), reply) else {
            return;
        };
        self.start_check_run(picked, (label, indexes, part, commit), reply);
    }

    /// The checks a `check-run` names, as (index, command) with `--part` applied, and the label
    /// of the run. A scenario's number or name comes after the checks; a scenario runs only when
    /// named. Refuses with the reason for the worker.
    fn pick_checks(
        &self,
        numbers: &[&String],
        part: &Option<String>,
    ) -> Result<(Vec<(usize, String)>, String), String> {
        // ^ The scenarios come after the checks, numbered on from them, and run only when
        // named: like the checks, they are the main checkout's, never the worker's.
        let scenarios = load_task_config(&self.record.project_path)
            .map(|settings| settings.scenarios)
            .unwrap_or_default();
        let mut runnable = self.record.checks.clone();
        runnable.extend(scenarios.values().map(|scenario| scenario.command.clone()));
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
                                .position(|name| name == *word)
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
        let picked = picked?;
        if picked.is_empty() {
            return Err("the project declares no checks".into());
        }
        // ^ A part is one scenario's: the command runs with `WSX_SCENARIO_PART`.
        let picked = match &part {
            Some(_) if !matches!(picked.as_slice(), [(index, _)] if *index >= checks) => {
                return Err(
                    "--part needs exactly one scenario, such as `check-run web --part tasks`"
                        .into(),
                )
            }
            Some(part) => picked
                .into_iter()
                .map(|(index, command)| (index, with_part(&command, part)))
                .collect(),
            None => picked,
        };
        let checks = self.record.checks.len();
        let label = picked
            .iter()
            .map(|(index, _)| match index.checked_sub(checks) {
                Some(at) => scenarios.keys().nth(at).cloned().unwrap_or_default(),
                None => (index + 1).to_string(),
            })
            .chain(part.iter().map(|part| format!("--part {part}")))
            .collect::<Vec<_>>()
            .join(" ");
        Ok((picked, label))
    }

    /// A `check-run` while one is pending in this working copy: the same checks on the same
    /// commit join it, other checks wait, and a newer commit's run starts when the pending one
    /// ends. Gives the reply back when none is pending.
    fn join_check_run(
        &mut self,
        argv: &[String],
        (indexes, part, commit): (&[usize], &Option<String>, &str),
        reply: Sender<broker::Response>,
    ) -> Option<Sender<broker::Response>> {
        let refuse = |message: String| {
            let _ = reply.send(broker::refused(message));
        };
        if let Some(run) = self.check_run.as_ref().filter(|run| run.commit != commit) {
            let _ = reply.send(broker::Response {
                stdout: format!(
                    "queued behind your check-run on commit {}: {CHECK_RUN_QUEUED}",
                    short_commit(&run.commit)
                ),
                stderr: String::new(),
                code: 0,
            });
            self.say("check-run: on a newer commit, it starts when the pending run ends");
            self.check_run_next = Some(argv.to_vec());
            return None;
        }
        if let Some(run) = &mut self.check_run {
            // ^ A queued run answers only after the result, also when it already ended.
            if run.queued && run.indexes == indexes && run.part == *part {
                let _ = reply.send(queued_response());
                return None;
            }
            if run.indexes != indexes || run.part != *part {
                let numbers: Vec<String> =
                    run.indexes.iter().map(|i| (i + 1).to_string()).collect();
                refuse(format!(
                    "a check-run of checks {}{} still runs; run it again to wait for its answer",
                    numbers.join(" "),
                    run.part
                        .iter()
                        .map(|p| format!(" --part {p}"))
                        .collect::<String>()
                ));
                return None;
            }
            // ^ The call gets the answer itself: no background turn repeats it.
            run.background = false;
            run.replies.push(reply);
            self.say("check-run: joined the run of the same checks");
            return None;
        }
        Some(reply)
    }

    /// Run the picked checks in a thread and answer the worker (or `queued` for a CI scenario);
    /// no revision is recorded. `label` names the run in the replies that wait for it.
    fn start_check_run(
        &mut self,
        picked: Vec<(usize, String)>,
        (label, indexes, part, commit): (String, Vec<usize>, Option<String>, String),
        reply: Sender<broker::Response>,
    ) {
        let checks = self.record.checks.len();
        let working_copy = self.record.working_copy.clone();
        let (tree, stored, to_run) = self.kept_checks(&picked);
        if to_run.is_empty() {
            self.say("check-run: every check already ran on this tree");
            let _ = reply.send(check_run_response(&picked, &stored, &BTreeMap::new()));
            return;
        }
        // ^ Scenarios only: with the disk low after the cleanup this Mac cannot run them, so they go
        // to CI. A plain check has no CI to go to and is refused.
        let scenarios_only = to_run.iter().all(|(index, _)| *index >= checks);
        let mut place = None;
        if let Some((free_mb, min_gb)) = self.low_disk() {
            let full = disk_full(free_mb, min_gb);
            if !scenarios_only {
                let _ = reply.send(broker::refused(format!(
                    "not run, disk space is low: {full}"
                )));
                return;
            }
            place = Some(Place::Ci(format!(
                "this Mac has {full}, after cleaning idle copies"
            )));
        }
        let limit = Duration::from_secs(self.record.check_timeout_secs) * to_run.len() as u32;
        let mark = CheckRunMark {
            picked: picked.clone(),
            label: label.clone(),
            part: part.clone(),
            commit: commit.clone(),
        };
        if let Err(error) = serde_json::to_vec(&mark)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| self.dir.write_file(CHECK_RUN_FILE, &bytes))
        {
            self.say(&format!("check-run: cannot keep the run's mark: {error:#}"));
        }
        self.check_run = Some(CheckRun {
            indexes,
            replies: vec![reply],
            background: false,
            queued: false,
            answer: None,
            label,
            part: part.clone(),
            limit,
            deadline_ms: None,
            commit,
        });
        // ^ A run of checks that only format or compile is light: it takes no slot. One that
        // names a scenario takes the scenarios lane, else the checks lane.
        let place =
            place.unwrap_or_else(|| match to_run.iter().all(|(_, command)| light(command)) {
                true => Place::Free,
                false => Place::Slot(self.want(&to_run, checks)),
            });
        // ^ A run without a slot starts now; one with a slot when it takes it (`check_slot`).
        if matches!(place, Place::Free) {
            self.start_check_run_clock();
        }
        let events = self.events.clone();
        let kept = (tree, stored, to_run.clone());
        self.spawn_checks(place, "check-run", to_run, move |results| {
            let _ = events.send(check_run_event(
                &picked,
                kept,
                (&part, &working_copy),
                results,
            ));
        });
    }

    /// The place of a worker's run in its lane: a scenario's in the scenarios lane, by the task's
    /// urgency (`schedule::urgency`); plain checks in the checks lane.
    /// How urgent this task's next heavy run is, and who waits for it (`schedule::urgency`).
    fn urgency(&self) -> (Priority, Option<String>) {
        let records: Vec<_> = wsx_core::task::view::project_tasks(&self.record.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, record)| record.ok())
            .collect();
        schedule::urgency(&self.record, &status::waiters(&records, self.record.id))
    }

    fn want(&self, to_run: &[(usize, String)], checks: usize) -> Want {
        if !to_run.iter().any(|(index, _)| *index >= checks) {
            return Want::new(Lane::Checks, Priority::Normal);
        }
        let (priority, urgent) = self.urgency();
        let command = to_run.first().map_or("", |(_, command)| command.as_str());
        let mut want = Want::new(Lane::Scenarios, priority).command(command);
        if urgent.is_some() {
            // ^ Only an urgent run can go to CI, so only it needs the estimate.
            want.typical_ms = schedule::typical_ms(&self.record.project_id, command);
        }
        want.urgent = urgent;
        want.min_free_mb = load_task_config(&self.record.project_path)
            .map_or(0, |settings| settings.min_free_gb * 1024);
        want
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
            Some(AfterRebase::Land) => Priority::Land,
            _ => Priority::Normal,
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
        self.spawn_checks(
            Place::Slot(Want::new(Lane::Checks, priority)),
            "check",
            to_run,
            move |results| {
                stored.extend(indexes.into_iter().zip(results));
                let _ = events.send(Event::ChecksDone(epoch, stored.into_values().collect()));
            },
        );
    }

    /// Run `checks` in a thread, holding a machine check slot (`Place::Slot`) unless it is `Free`;
    /// each check's timeout counts from the slot. The checks of a run in the scenarios lane
    /// run at once (a scenario starts its own parts); others run one after another. Check `n`'s
    /// whole output goes to `<kind>-<n>.log` in the task folder. ref: docs/tasks.md "Checks, revisions and report"
    fn spawn_checks(
        &self,
        place: Place,
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
        let of_run = kind == "check-run";
        let together = match &place {
            Place::Slot(want) => matches!(want.lane, Lane::Scenarios),
            Place::Ci(_) => true,
            Place::Free => false,
        };
        std::thread::spawn(move || {
            let queued = std::time::Instant::now();
            let (mut held, mut ci) = (None, None);
            match place {
                Place::Free => {}
                Place::Ci(why) => ci = Some(why),
                Place::Slot(want) => {
                    let taken = slot::take(&want, &label, |reason| {
                        let _ = events.send(Event::CheckQueued(reason, of_run));
                    });
                    let error = taken.as_ref().err().map(ToString::to_string);
                    match taken {
                        Ok(Taken::Ci(why)) => ci = Some(why),
                        taken => {
                            let _ = events.send(Event::CheckSlot(error, of_run));
                            held = taken.ok();
                        }
                    }
                }
            }
            // ^ The wait is the run's: each of its results carries it.
            let queued_ms = queued.elapsed().as_millis() as u64;
            // ^ On CI the project's own scenario command takes the run to GitHub (`WSX_SCENARIO_ON`);
            // a CI round takes longer than a check's timeout.
            let run = |(command, log): &(String, PathBuf), on_ci: bool| {
                let mut env = env.clone();
                let mut timeout = timeout;
                if on_ci {
                    env.insert("WSX_SCENARIO_ON".into(), "ci".into());
                    timeout = timeout.max(schedule::CI_TIMEOUT);
                }
                let ran = run_check(&working_copy, command, &env, timeout, log);
                CheckResult { queued_ms, ..ran }
            };
            let on_ci = |why: &str| {
                let _ = events.send(Event::CheckQueued(
                    format!("{}{why}", schedule::ON_CI),
                    of_run,
                ));
            };
            if let Some(why) = &ci {
                on_ci(why);
            }
            let mut went_ci = ci.is_some();
            let from_start = went_ci;
            let mut results: Vec<CheckResult> = match together {
                true => std::thread::scope(|scope| {
                    let started: Vec<_> = checks
                        .iter()
                        .map(|check| scope.spawn(move || run(check, from_start)))
                        .collect();
                    started
                        .into_iter()
                        .map(|check: std::thread::ScopedJoinHandle<CheckResult>| {
                            check
                                .join()
                                .unwrap_or_else(|e| std::panic::resume_unwind(e))
                        })
                        .collect()
                }),
                false => checks.iter().map(|check| run(check, false)).collect(),
            };
            // ^ A scenario that says this Mac cannot run it (a tool is missing) ends at once; its
            // slot goes back and the run goes to CI with that line as the reason.
            if held.is_some() && ci.is_none() {
                let missing: Vec<usize> = (0..results.len())
                    .filter(|n| results[*n].exit == Some(schedule::CANNOT_RUN_HERE))
                    .collect();
                if !missing.is_empty() {
                    held = None;
                    for n in missing {
                        let why = results[n]
                            .output
                            .lines()
                            .last()
                            .unwrap_or("a tool is missing");
                        on_ci(why.trim());
                        results[n] = run(&checks[n], true);
                    }
                    went_ci = true;
                }
            }
            drop(held);
            if went_ci {
                let _ = events.send(Event::CheckSlot(None, of_run));
            }
            build::prune(&working_copy, born);
            done(results);
        });
    }

    /// A check-run's answer after the worker's result settled. A pass is an event line, and its
    /// proof joins a done result. A failure of the task's own reopens it as a turn, unless the
    /// run's tree is no longer the task's head or the task has ended. ref: docs/tasks.md "Check-run"
    fn answer_in_background(&mut self, answer: CheckAnswer, commit: &str) {
        self.drop_check_run_mark();
        let label = short_commit(commit).to_string();
        let current = self.record.state.is_open()
            && head(&self.record.working_copy).is_some_and(|head| head == commit);
        if !answer.fails {
            let done = self
                .last_result
                .as_mut()
                .filter(|result| current && result.status == "done");
            if let (Some(proof), Some(result)) = (answer.proof, done) {
                result.evidence.push(proof);
                let kept = result.clone();
                self.write_result(&kept, &[]);
            }
            self.say(&format!(
                "check-run on {label} ended with no failure of the task's own"
            ));
            // ^ The scenarios its change touches waited for this run: they start now.
            return self.run_touched_scenarios();
        }
        if !current {
            return self.say(&format!(
                "check-run on {label} failed, but the task moved on: no turn for it"
            ));
        }
        self.say(&format!("check-run on {label} failed: the task reopens"));
        // ^ The checks of a result run now: the failure is the turn after them.
        if self.record.state == TaskState::Checking {
            self.pending.push_back(answer.text);
            return;
        }
        self.send_check_answer(answer.text);
    }

    /// A check-run's answer goes to the worker as one turn, after its current turn if any.
    fn send_check_answer(&mut self, answer: String) {
        self.say("check-run: its answer goes to the worker");
        self.pending.push_back(answer);
        if self.turn.is_none() {
            self.deliver_pending(TurnTrigger::CheckRun);
        } else if self.pinging() {
            self.after_ping.get_or_insert(TurnTrigger::CheckRun);
        }
    }

    /// Checks wait for the slot. The checks after a result show it on the record; a check-run's
    /// wait is only logged, since a run never holds the task, and its calls get `queued` at once.
    /// ref: docs/tasks.md "Check-run"
    fn check_queued(&mut self, reason: String, run: bool) {
        if run {
            if let Some(run) = &mut self.check_run {
                run.queued = true;
                for reply in run.replies.drain(..) {
                    let _ = reply.send(queued_response());
                }
            }
            self.log(LogEvent::CheckQueued {
                reason: reason.clone(),
            });
            return self.say(&format!("check-run: {reason}"));
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
        let waiting = Waiting {
            on: WaitOn::CheckSlot,
            reason,
            until_ms: None,
        };
        self.move_to(self.record.state, Some(waiting));
    }

    fn check_slot(&mut self, error: Option<String>, run: bool) {
        if let Some(error) = error {
            self.say(&format!(
                "no check slot, so the checks run unqueued: {error}"
            ));
        }
        if run {
            return self.start_check_run_clock();
        }
        if self.record.waits_on(WaitOn::CheckSlot) {
            self.say(match self.record.waiting.as_ref() {
                Some(w) if w.reason.starts_with(schedule::ON_CI) => "the CI run ended",
                _ => "took the check slot",
            });
            self.move_to(self.record.state, None);
        }
    }

    /// The worker's check-run starts now: its time limit counts from here, as its checks'.
    fn start_check_run_clock(&mut self) {
        if let Some(run) = self
            .check_run
            .as_mut()
            .filter(|run| run.deadline_ms.is_none())
        {
            run.deadline_ms = Some(now_unix_ms() + run.limit.as_millis() as u64);
        }
    }

    /// Results that decide nothing now: a result that came during the checks (a block keeps its
    /// wait, a turn that runs gives the next result, a new commit runs the checks once more) or
    /// a full disk, which is not the worker's failure. They are logged; true when they are.
    fn checks_deferred(&mut self, results: &[CheckResult]) -> bool {
        // ref: docs/tasks.md "Checks, revisions and report"
        if let Some((commit, true)) = self.gate.take() {
            let blocked = self
                .last_result
                .as_ref()
                .is_some_and(|r| matches!(r.status.as_str(), "blocked" | "paused"));
            if blocked || self.turn.is_some() {
                self.log(LogEvent::Checks {
                    results: results.to_vec(),
                });
                return true;
            }
            if commit.is_none() || commit != head(&self.record.working_copy) {
                self.say("a newer result is on another commit: the checks run again");
                self.start_checks();
                return true;
            }
        }
        // ^ A full disk is not the worker's failure: the checks run again once there is space.
        if results.iter().any(|check| check.output.contains(NO_SPACE)) {
            self.log(LogEvent::Checks {
                results: results.to_vec(),
            });
            self.wait_for_disk(DiskWait::Checks);
            if self.disk_wait.is_none() {
                self.disk_wait = Some(DiskWait::Checks);
                self.wait(WaitOn::Disk, "a check ran out of disk space".into());
            }
            return true;
        }
        false
    }

    /// The revision the checks produced is recorded: the result and report are written, then the
    /// task is ready, sent back to the worker (a failed check, bounded), landed again (a
    /// conflict it resolved) or continues the landing or push that waited for them.
    fn checked_revision(
        &mut self,
        (revision, commit): (u32, &str),
        (result, results): (&WorkerResult, &[CheckResult]),
        (passed, relands): (bool, bool),
    ) {
        let state = if passed {
            TaskState::Ready
        } else {
            TaskState::Failed
        };
        let after_rebase = self.after_rebase.take();
        // ^ Readers act on the state: the result and report must be complete before it.
        self.write_result(result, results);
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
            // ^ A ready task goes to its review job (`start_review`); one that resolved
            // the conflict of a landing lands again at once when the project lands
            // by itself and has checks to prove it, from checking to landed without
            // a stop at ready. A message that waits for the worker comes first: its
            // answer is a new revision. ref: docs/tasks.md "Landing"
            None if state == TaskState::Ready
                && relands
                && !results.is_empty()
                && self.record.hold.is_none()
                && self.record.after.is_none()
                && self.pending.is_empty()
                && self.auto_land() =>
            {
                let landed = self.land(true);
                self.finish_landing(landed, state);
            }
            // ^ A result that ends with a question for the owner is the owner's need, not the
            // review job's: the task waits ready on the answer. ref: docs/tasks.md "Chat host"
            None if state == TaskState::Ready && asks_owner(&result.summary) => {
                let question = result.summary.trim_end().lines().last().unwrap_or_default();
                self.move_to(
                    state,
                    Some(Waiting {
                        on: WaitOn::Asks,
                        reason: question.trim().to_string(),
                        until_ms: None,
                    }),
                );
            }
            None => {
                self.set_state(state);
                self.start_review();
            }
            Some(AfterRebase::Land) => {
                let finished = if state == TaskState::Ready {
                    self.land_on_tip()
                } else {
                    Err(anyhow::anyhow!("checks failed on the rebased revision"))
                };
                self.finish_landing(finished, state);
            }
        }
    }

    /// Start the review job of a ready revision when it is due (`status::under_review`: the
    /// project lands by itself, checks prove the task, nothing holds it, no review of this
    /// revision ended) and no message waits for the worker. A fresh agent of the project's
    /// lead reads what the task was asked and its diff and answers `land` or `changes`.
    /// After `MAX_REVIEW_ROUNDS` rounds of changes the owner decides. ref: docs/tasks.md "Review"
    fn start_review(&mut self) {
        if self.review.is_some()
            || !self.pending.is_empty()
            || !status::under_review(&self.dir, &self.record, self.auto_land())
        {
            return;
        }
        let revision = self.record.revision;
        // ^ The rounds of changes since the last prompt, add or steer: a person's word starts
        // a new chain of rounds.
        let mut earlier: Vec<Vec<String>> = Vec::new();
        for entry in self.dir.read_log().unwrap_or_default() {
            match entry.event {
                LogEvent::Message { .. } => earlier.clear(),
                LogEvent::Review {
                    verdict: ReviewVerdict::Changes,
                    findings,
                    ..
                } => earlier.push(findings),
                _ => {}
            }
        }
        let round = earlier.len() as u32 + 1;
        if round > MAX_REVIEW_ROUNDS {
            let why = format!(
                "the review asked for changes {} times; you decide",
                earlier.len()
            );
            return self.no_verdict(revision, round, why);
        }
        let started = (|| -> Result<ReviewJob> {
            let material = super::cmd::work::ReviewMaterial::read(&self.dir)?.text();
            let folder = self.dir.path.join("review");
            std::fs::create_dir_all(&folder)?;
            let (adapter, launch) = Launch::reviewer(&self.record, &folder)?;
            let label = format!("{adapter}/{}", launch.model);
            self.reviews += 1;
            let generation = self.reviews;
            let mut agent = Agent::spawn(
                &adapter,
                &launch,
                &new_session_id()?,
                false,
                0,
                self.events.clone(),
                move |line| Event::Review(generation, line),
            )?;
            let mut prompt = format!("Review round {round} of {MAX_REVIEW_ROUNDS}.\n\n");
            if let Some(last) = earlier.last() {
                prompt += &format!(
                    "Your findings of the round before, which the worker answered with this revision:\n- {}\n\n",
                    last.join("\n- ")
                );
            }
            if let Ok(Some(report)) = self.dir.read_file(store::REPORT_FILE, 64 * 1024) {
                prompt += &format!(
                    "== report.md ==\n{}\n\n",
                    String::from_utf8_lossy(&report).trim_end()
                );
            }
            prompt += &material;
            agent.send(prompt.trim_end())?;
            Ok(ReviewJob {
                generation,
                agent,
                revision,
                round,
                label,
                started: Instant::now(),
            })
        })();
        match started {
            Ok(job) => {
                self.say(&format!(
                    "review round {round} of r{revision} on {}",
                    job.label
                ));
                self.review = Some(job);
            }
            Err(error) => self.no_verdict(
                revision,
                round,
                format!("the review could not start: {error:#}"),
            ),
        }
    }

    /// A review of `revision` that ended before any agent answered: the owner decides.
    fn no_verdict(&mut self, revision: u32, round: u32, why: String) {
        self.log(LogEvent::Review {
            revision,
            round,
            verdict: ReviewVerdict::None,
            findings: vec![why],
            agent: String::new(),
            usage: None,
        });
        self.save();
    }

    /// A line of the review job: its result ends the review; a process that ended without
    /// one ends it with no verdict.
    fn on_review(&mut self, generation: u64, line: Option<AgentLine>) {
        if self.review.as_ref().map(|job| job.generation) != Some(generation) {
            return;
        }
        match line {
            Some(AgentLine::Result(result)) => {
                if result.limit.is_some() {
                    return self.finish_review(
                        None,
                        "the reviewer's agent is at its usage limit",
                        None,
                    );
                }
                let answer = (!result.is_error)
                    .then(|| wsx_core::task::parse_review(result.structured.as_ref(), &result.text))
                    .flatten();
                match answer {
                    Some(answer) => self.finish_review(Some(answer), "", Some(result.usage)),
                    None => {
                        self.finish_review(None, "the reviewer gave no verdict", Some(result.usage))
                    }
                }
            }
            Some(_) => {}
            None => {
                self.finish_review(None, "the reviewer's process ended without a verdict", None)
            }
        }
    }

    /// Act on the review's end, then log it: `land` lands the revision, `changes` sends the
    /// findings to the worker as a turn, no verdict leaves the task ready for the owner.
    fn finish_review(
        &mut self,
        answer: Option<(ReviewVerdict, Vec<String>)>,
        why: &str,
        usage: Option<TurnUsage>,
    ) {
        let Some(job) = self.review.take() else {
            return;
        };
        job.agent.stop();
        // ^ A land would run the old code: the new host reviews the revision again and lands
        // it (the same rule as a `land` request, `must_follow_binary`).
        if matches!(answer, Some((ReviewVerdict::Land, _))) && self.must_follow_binary() {
            self.say(
                "the wsx binary was replaced: the host restarts with it before the review's land",
            );
            self.reexec = true;
            return;
        }
        // ^ The task moved on while the reviewer read (a new revision, a hold, a wait, a message
        // for the worker): its verdict is logged and decides nothing.
        let current = self.record.revision == job.revision
            && self.pending.is_empty()
            && status::under_review(&self.dir, &self.record, self.auto_land());
        let (verdict, findings) = answer.unwrap_or((ReviewVerdict::None, vec![why.to_string()]));
        let review = LogEvent::Review {
            revision: job.revision,
            round: job.round,
            verdict,
            findings: findings.clone(),
            agent: job.label,
            usage,
        };
        // ^ The verdict is logged after the act: a logged verdict ends `under_review`, and a
        // ready task out of review is the owner's, so logged first, a landing or a sent-back
        // revision showed as a need (a push counted it, 2026-11-23). A host that dies before the
        // log reviews the revision again.
        match verdict {
            ReviewVerdict::Land if current => {
                self.say(&format!("the review lands r{}", job.revision));
                let landed = self.land(true);
                self.log(review);
                self.finish_landing(landed, TaskState::Ready);
            }
            ReviewVerdict::Changes if current => {
                self.say(&format!(
                    "the review sent {} finding(s) on r{} back to the worker",
                    findings.len(),
                    job.revision
                ));
                let text = format!(
                    "A review of your revision r{} asks for changes before it lands. Fix each, \
                     commit, and return your result:\n- {}",
                    job.revision,
                    findings.join("\n- ")
                );
                self.start_turn(&text, TurnTrigger::Review);
                self.log(review);
            }
            _ => {
                if current {
                    self.say(&format!("the review ended without a verdict: {why}"));
                }
                self.log(review);
                self.save();
            }
        }
    }

    fn on_checks(&mut self, epoch: u64, results: Vec<CheckResult>) {
        // ^ A run a stop left running: its output is logged, it decides nothing.
        if epoch != self.checks_epoch {
            self.log(LogEvent::Checks { results });
            return;
        }
        if self.checks_deferred(&results) {
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
        // ^ Read before this revision is logged: its review job (or the owner) landed it already.
        let relands = view::resolving_conflict(&self.dir);
        match self.record_revision() {
            Ok((revision, commit)) => {
                self.checked_revision((revision, &commit), (&result, &results), (passed, relands))
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
        status::land_auto(&self.record)
    }

    /// Say what a landing did, or log why it stopped and settle the task in `state`.
    fn finish_landing(&mut self, finished: Result<String>, state: TaskState) {
        match finished {
            Ok(message) => self.say(&message),
            Err(error) => {
                let reason = format!("{error:#}");
                self.say(&format!("landing stopped: {reason}"));
                self.log(LogEvent::LandStopped {
                    reason: reason.clone(),
                });
                // ^ A ready task waits on the owner to land it again; a failed one is failed.
                let waiting = (state == TaskState::Ready).then_some(Waiting {
                    on: WaitOn::LandStopped,
                    reason,
                    until_ms: None,
                });
                self.move_to(state, waiting);
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
    fn land(&mut self, auto: bool) -> Result<String> {
        self.require_ready(auto)?;
        view::refuse_open_children(&self.record.project_id, self.record.id, "land")?;
        self.require_committed()?;
        self.require_contracts_kept(auto)?;
        self.land_on_tip()
    }

    /// A change that edits a line of a contract file (`tasks.contracts`) lands only by a land the
    /// owner asked for, never by itself; one that only adds lines is a stricter contract: it
    /// lands, and the main chat hears of it with no turn once it landed. ref: docs/tasks.md
    /// "Contracts"
    fn require_contracts_kept(&mut self, auto: bool) -> Result<()> {
        // ^ A config wsx cannot read names no contract it could skip: the land stops.
        let settings = load_task_config(&self.record.project_path)
            .map_err(|error| anyhow::anyhow!("{error}; nothing was landed"))?;
        let (changed, stricter) = copy::contract_change(
            &self.record.working_copy,
            "HEAD",
            &self.record.default_branch,
            &settings.contracts,
        )?;
        if auto && !changed.is_empty() {
            anyhow::bail!(
                "it changes the contract {}: the owner lands it with `wsx task land {}`",
                changed.join(", "),
                self.record.id
            );
        }
        self.stricter_contracts = stricter;
        Ok(())
    }

    /// Fast-forward when the branch has the default branch tip, else rebase onto it. The
    /// checks of a rebased revision come back here, so a tip that moved while they ran
    /// rebases and checks again (#84 stopped on it, 2026-10-27).
    fn land_on_tip(&mut self) -> Result<String> {
        self.require_parent_at_rest()?;
        let working_copy = self.record.working_copy.clone();
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&working_copy, "HEAD")?;
        // ^ Only a checked tree fast-forwards: a child that landed on this task moved HEAD
        // past the ready revision.
        let revision = format!("refs/wsx/task/{}/r{}", self.record.id, self.record.revision);
        if copy::is_ancestor(&working_copy, &tip, &head)? {
            let tree = copy::tree(&working_copy, "HEAD")?;
            if copy::tree(&working_copy, &revision)? == tree {
                return self.fast_forward();
            }
            return Ok(self.check_then_land("HEAD moved after the ready revision"));
        }
        match copy::rebase(&working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.carried_verify = self.current_result().human_verify;
                Ok(self.resolve_conflict(&tip, &paths))
            }
            // ^ The rebase kept the tree whose checks passed: their results stand.
            Rebase::Done if self.rebase_kept_tree()? => self.fast_forward(),
            Rebase::Done => Ok(self.check_then_land(&format!(
                "rebased onto {} at {tip:.12}",
                self.record.default_branch
            ))),
        }
    }

    /// Run the checks on HEAD as a new revision; it lands when they pass.
    fn check_then_land(&mut self, why: &str) -> String {
        self.after_rebase = Some(AfterRebase::Land);
        self.start_checks();
        format!("{why}; checks run on the new revision and it lands when they pass")
    }

    /// A child lands only on a parent at rest, ready or failed: its commits would join a
    /// running turn or checks, and its new revision would drop a wait for the owner or a
    /// plan. The child stays ready and the lead lands it again on the parent's next
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
            if self.auto_land() && self.record.hold.is_none() && self.record.after.is_none() {
                ", and the task lands when its checks pass"
            } else {
                ". Act again when the task is ready"
            }
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

    fn fast_forward(&mut self) -> Result<String> {
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&self.record.working_copy, "HEAD")?;
        // ^ A `land: pr` project hands the checked revision to GitHub instead; a child lands
        // into its parent's branch as always.
        let pr = self.record.parent.is_none()
            && load_task_config(&self.record.project_path).is_ok_and(|settings| settings.land_pr);
        if pr {
            return self.open_pr(&head);
        }
        let cleared = match copy::move_branch(
            &self.record.project_path,
            &self.record.default_branch,
            &tip,
            &head,
            false,
        ) {
            Ok(cleared) => cleared,
            // ^ Each task has its own host: another task can land between `land_on_tip`'s
            // check and this move. A tip HEAD no longer contains is rebased onto and checked
            // like a tip that moved during the checks (docs/tasks.md "Landing" 3).
            Err(_)
                if !copy::is_ancestor(&self.record.working_copy, &self.default_tip()?, &head)? =>
            {
                return self.land_on_tip();
            }
            Err(error) => return Err(error),
        };
        if let Some(lock) = cleared {
            self.log(LogEvent::LockCleared {
                path: lock.path.display().to_string(),
                age_s: lock.age_s,
            });
        }
        self.landed(&head);
        if let Some(parent) = self.record.parent {
            self.tell_parent(parent, &tip, &head);
        }
        Ok(format!(
            "landed r{} on {} at {head:.12}",
            self.record.revision, self.record.default_branch
        ))
    }

    /// Push the task's branch to `origin` and open (or find) its pull request into the default
    /// branch, then end the task as landed: the default branch does not move, so no open task is
    /// told to rebase and no deploy is due. ref: docs/tasks.md "Landing"
    fn open_pr(&mut self, head: &str) -> Result<String> {
        let title = self
            .record
            .title
            .clone()
            .unwrap_or_else(|| format!("Task #{}", self.record.id));
        let report =
            std::fs::read_to_string(self.dir.path.join(store::REPORT_FILE)).unwrap_or_default();
        let body = format!(
            "{}\n\nOpened by `wsx task land` for task #{} at revision r{}.",
            report.trim(),
            self.record.id,
            self.record.revision
        );
        let url = copy::open_pr(
            &self.record.project_path,
            super::cmd::deploy::PUSH_REMOTE,
            (&self.record.branch, &self.record.default_branch),
            head,
            (&title, body.trim()),
        )?;
        self.log(LogEvent::PrOpened {
            url: url.clone(),
            branch: self.record.branch.clone(),
            commit: head.to_string(),
        });
        self.settle_landed(head, false);
        Ok(format!(
            "opened {url} for r{} at {head:.12}",
            self.record.revision
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
    /// "To check" list before readers see `landed` (docs/tasks.md "To check"). A child's items
    /// wait in its `result.json`, its captures kept, until its parent lands.
    fn landed(&mut self, commit: &str) {
        self.settle_landed(commit, true);
    }

    /// The task is `landed` at `commit`. `moved`: its target branch moved (a local land), so open
    /// tasks that overlap hear of it and a deploy may be due; a pull request moves nothing.
    fn settle_landed(&mut self, commit: &str, moved: bool) {
        if self.record.parent.is_some() {
            self.keep_verify_for_parent();
        } else {
            self.collect_verify(commit);
        }
        self.log(LogEvent::Landed {
            commit: commit.to_string(),
        });
        self.refuse_queued(commit);
        self.write_report(TaskState::Landed);
        self.record.after = None;
        self.set_state(TaskState::Landed);
        if moved && self.record.parent.is_none() {
            self.tell_overlaps(commit);
        }
        let stricter = std::mem::take(&mut self.stricter_contracts);
        if !stricter.is_empty() {
            self.tell_chat(
                format!(
                    "contract-stricter-{}-r{}",
                    self.record.id, self.record.revision
                ),
                format!(
                    "#{} adds to the contract {} and changes none of its lines",
                    self.record.id,
                    stricter.join(", ")
                ),
                None,
                false,
            );
        }
        self.after_landing(moved);
    }

    /// The adds that waited for the worker while the checks of this land ran are not delivered:
    /// the task is `landed`. Each is logged as it came; the lead hears of them once, with
    /// their first lines, since their sender was told "queued".
    fn refuse_queued(&mut self, commit: &str) {
        let adds: Vec<String> = self.pending.drain(..).collect();
        self.notices.clear();
        if adds.is_empty() {
            return;
        }
        let lines: Vec<String> = adds
            .iter()
            .map(|add| format!("\n  {}", view::clip(first_line(add), 120)))
            .collect();
        let text = format!(
            "#{} landed at {commit:.12} while {} add(s) to it waited for the worker; they were not delivered because the task landed:{}",
            self.record.id,
            adds.len(),
            lines.concat()
        );
        self.say(&text);
        // ^ The add was its sender's action: the chat that owns the task hears it and acts again.
        let owner = self.record.owner_chat().map(str::to_string);
        self.tell_chat(
            format!("add-dropped-{}-{commit}", self.record.id),
            text,
            owner,
            true,
        );
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
            for (k, path) in std::mem::take(&mut item.captures).iter().enumerate() {
                let kept = checks::keep_capture(
                    &self.dir,
                    &self.record.working_copy,
                    self.record.revision,
                    index + 1,
                    k + 1,
                    path,
                );
                match kept {
                    Ok(name) => item.captures.push(name),
                    Err(error) => self.say(&format!("capture not kept: {error:#}")),
                }
            }
            // ^ A poster only goes with a video capture.
            if !item.captures.iter().any(|name| name.ends_with(".mp4")) || item.poster.is_empty() {
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
    fn after_landing(&self, moved: bool) {
        let project = self.record.project.as_str();
        let id = self.record.id.to_string();
        // ^ The deploy is due when the default branch is ahead of the last deploy: a child's land
        // moves its parent's branch, so it starts none; a pull request moves no branch.
        let deploy = match moved {
            true => super::cmd::deploy::spawn_quiet_deploy(
                project,
                &self.record.project_id,
                &self.record.project_path,
            ),
            false => Ok(()),
        };
        let close = store::follow_up_log(&self.record.project_id).and_then(|log| {
            hosting::spawn_wsx(
                &["task", "close", id.as_str(), "-p", project, "--json"],
                log,
                None,
            )
        });
        for error in [deploy, close].into_iter().filter_map(Result::err) {
            self.say(&format!("cannot start a follow-up of the land: {error:#}"));
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
        if let Ok(base) = copy::fork_point(working_copy, &commit, &self.record.default_branch) {
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

    /// Move to `state`: a wait stays only in `waiting` (see `wait`).
    fn set_state(&mut self, state: TaskState) {
        let waiting = self
            .record
            .waiting
            .clone()
            .filter(|_| state == TaskState::Waiting);
        self.move_to(state, waiting);
    }

    /// Every move of the task goes through `TaskRecord::move_to`'s table; a refused one is
    /// logged and changes nothing. ref: docs/tasks.md "Status"
    fn move_to(&mut self, state: TaskState, waiting: Option<Waiting>) {
        let changed = self.record.state != state;
        if let Err(error) = self.record.move_to(state, waiting) {
            let reason = format!("{error:#}");
            self.say(&reason);
            self.log(LogEvent::MoveRefused { reason });
            return;
        }
        if !matches!(state, TaskState::Working | TaskState::Checking) {
            self.disk_slot = None;
        }
        // ^ A review reads one ready revision: any other move ends it unheard.
        if state != TaskState::Ready {
            if let Some(job) = self.review.take() {
                job.agent.stop();
                self.say("the review stopped: the task left ready");
            }
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
    /// the lead once, as one event that wakes a turn; returns them for the worker whose
    /// run it was (and for main's, see `check_run_done`). ref: docs/tasks.md "Checks, revisions and report"
    fn shared_failures(&self, results: &[CheckResult]) -> Vec<failure::SharedFailure> {
        let now = now_unix_ms();
        let mut found =
            failure::shared_failures(&self.record.project_id, self.record.id, results, now);
        // ^ A scenario whose paths the run's change does not touch fails on main: a finding.
        let head = self
            .check_run
            .as_ref()
            .map(|run| run.commit.clone())
            .filter(|commit| !commit.is_empty())
            .unwrap_or_else(|| "HEAD".into());
        let outside: Vec<String> = self
            .scenarios_touched(&head)
            .into_iter()
            .filter_map(|(_, command, touched)| (!touched).then_some(command))
            .collect();
        for result in results.iter().filter(|r| outside.contains(&r.command)) {
            match found.iter_mut().find(|f| f.command == result.command) {
                Some(failure) => failure.outside = true,
                None => found.extend(failure::outside(self.record.id, result)),
            }
        }
        // ^ A failure only main's CI run has is the project's known one: no event, the ready
        // event names it from the evidence.
        for failure in found.iter().filter(|f| f.tasks.len() > 1 || f.outside) {
            self.tell_chat(failure.key(), failure.event(), None, false);
        }
        found
    }

    /// Post `text` as one event, once per `key` a day, to the chat `chat` (none: the main chat).
    /// With `wakes` it starts a turn; without, the owner reads it and the chat reads it with its
    /// next turn, as a task's events (docs/tasks.md "Chat host"). The chat starts if it does not
    /// run, and a busy host takes a while to answer, so it goes from its own thread.
    fn tell_chat(&self, key: String, text: String, chat: Option<String>, wakes: bool) {
        let (project, main, project_id) = (
            self.record.project.clone(),
            self.record.project_path.clone(),
            self.record.project_id.clone(),
        );
        let now = now_unix_ms();
        std::thread::spawn(move || {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let _ = store::post_once(&project_id, &key, now / 1000, SHARED_WINDOW_SECS, || {
                let short = Short::plain(&text);
                let sent = owner::say(
                    &project,
                    &main,
                    &exe,
                    chat.as_deref(),
                    super::launch_chat(&main),
                    text.clone(),
                    Some(Author::Events),
                    vec![if wakes { short } else { short.quiet() }],
                )?;
                anyhow::ensure!(sent.ok, "{}", sent.message);
                Ok(())
            });
        });
    }

    fn log(&self, event: LogEvent) {
        if let Err(error) = self.dir.append(event) {
            self.say(&format!("cannot append to the log: {error:#}"));
        }
    }

    /// Count a turn's cost on this task's account and say what that did.
    fn spend(&self, micro_usd: u64, now_ms: u64) {
        if let Some(message) =
            super::agent::note_spend(&self.record.adapter, &self.record.model, micro_usd, now_ms)
        {
            self.say(&message);
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
        // ^ An idle worker whose task waits for another to land asks nobody.
        let idle = status::owner_waits_after(&self.dir, &self.record);
        let state = match (
            self.record.state,
            self.record.waiting.as_ref().map(|w| w.on),
        ) {
            (TaskState::Working | TaskState::Checking, _) => runtime::AgentState::Working,
            (TaskState::Waiting, Some(WaitOn::Owner)) if !idle => runtime::AgentState::Blocked,
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

/// Where a run of checks goes: no slot, a slot of this Mac's lane, or GitHub CI (the reason).
enum Place {
    Free,
    Slot(Want),
    Ci(String),
}

/// What a land does after the checks of its rebased revision pass.
#[derive(Clone, Copy)]
enum AfterRebase {
    Land,
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

/// The turn that opens a conversation, then `text` unless it says nothing new.
fn join_turn(opening: String, text: &str) -> String {
    match text.is_empty() || opening.contains(text) {
        true => opening,
        false => format!("{opening}\n\n{text}"),
    }
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
/// The event of a finished `check-run`: the worker's answer, and the results to keep when the
/// tree did not change. `kept` is what the tree already had and what ran; a part's pass is
/// stored for no one.
fn check_run_event(
    picked: &[(usize, String)],
    (tree, stored, to_run): KeptChecks,
    (part, working_copy): (&Option<String>, &Path),
    results: Vec<CheckResult>,
) -> Event {
    let ran: BTreeMap<usize, CheckResult> = to_run
        .iter()
        .map(|(index, _)| *index)
        .zip(results.iter().cloned())
        .collect();
    let mut response = check_run_response(picked, &stored, &ran);
    // ^ A part's pass is not the scenario's pass: nothing is stored, so no later run or
    // evidence answers for the full run.
    if part.is_some() {
        response.stdout.push_str(
            "this ran one part: it is not a full pass; the full result still gates the \
             task (run the scenario without --part)\n",
        );
    }
    // ^ A tree that changed during the run, a timeout or a full disk is not stored.
    let unchanged = tree.filter(|tree| {
        part.is_none() && copy::clean_tree(working_copy).ok().flatten().as_ref() == Some(tree)
    });
    let ran = ran
        .into_iter()
        .filter(|(_, check)| {
            check.exit.is_some() && check.exit != Some(TEMPFAIL) && !check.output.contains(NO_SPACE)
        })
        .collect();
    Event::CheckRunDone(results, response, unchanged.map(|tree| (tree, ran)))
}

type KeptChecks = (
    Option<String>,
    BTreeMap<usize, CheckResult>,
    Vec<(usize, String)>,
);

/// What a temporary failure (exit 75) tells the worker: nothing was kept for it.
fn tempfail_note(check: &CheckResult) -> &'static str {
    match check.exit == Some(TEMPFAIL) {
        true => " (a temporary failure, not kept: the next check-run runs it again)",
        false => "",
    }
}

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
            (false, Some(code)) => format!("exit {code}{}", tempfail_note(check)),
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

fn queued_response() -> broker::Response {
    broker::Response {
        stdout: format!("queued for the machine's check slot: {CHECK_RUN_QUEUED}"),
        stderr: String::new(),
        code: 0,
    }
}

/// Checks a worker's `check-run` runs without the slot: one plain command that only formats
/// or compiles, so no timing test meets the load of another task's checks.
pub(super) fn light(command: &str) -> bool {
    const LIGHT: [&str; 3] = ["cargo fmt", "cargo build", "cargo check"];
    let plain = !command.contains(['&', '|', ';', '\n', '`', '$', '(', '<', '>']);
    let words: Vec<&str> = command.split_whitespace().take(2).collect();
    plain
        && LIGHT
            .iter()
            .any(|light| light.split(' ').eq(words.iter().copied()))
}

/// A project command's environment: no `WSX_*` variable from the caller, so its tests act as
/// the owner, not as the worker or lead that started it. Checks and deploys share it.
pub(super) fn clean_env(process: &mut std::process::Command) {
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("WSX_") {
            process.env_remove(name);
        }
    }
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
    clean_env(&mut process);
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
                    format!("exit {code}{}", tempfail_note(check))
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

/// The first prompt of the conversation after a rotation. The task as asked comes first, then
/// the turn and the messages since, then the notes: the newest instruction wins where they
/// differ. `step` is (index, count, step) and only for a turn that was a step's.
fn rotation_prompt(
    tokens: u64,
    asked: Option<&str>,
    step: Option<(usize, usize, &WorkflowStep)>,
    waiting: &str,
    pending: Vec<String>,
    notes: &str,
) -> String {
    let mut text = vec![format!(
        "You continue a task whose earlier conversation reached {}k tokens of context and \
         was closed. Its working copy and commits are as that conversation left them; see \
         them with git status, git log and git diff. Load the project knowledge you need \
         again (the recall or seek skill when you have it), and read only the parts of \
         files you need.",
        tokens / 1000
    )];
    if let Some(asked) = asked {
        text.push(format!("The task as it was asked:\n{asked}"));
    }
    text.push(
        "What follows came after: where it differs from the task as asked or from the notes, \
         the newest instruction wins."
            .into(),
    );
    if let Some((index, count, step)) = step.filter(|(_, _, step)| !waiting.contains(&step.prompt))
    {
        text.push(format!(
            "You are in step {} of {count}, {}: {}",
            index + 1,
            step.name,
            step.prompt
        ));
    }
    text.push(waiting.to_string());
    text.extend(pending);
    text.push(notes.to_string());
    text.join("\n\n")
}

/// A worker's `wsx project run`: the CLI records the request with this task as the asker
/// (`WORKER_ENV`), in the task's own project. ref: docs/tasks.md "Approved runs"
fn ask_run(argv: &[String], copy: &Path, project: &str, task: u32) -> broker::Response {
    let names_project = argv[2..]
        .iter()
        .take_while(|word| *word != "--")
        .any(|word| word.starts_with("-p") || word.starts_with("--project"));
    if names_project {
        return broker::refused("a worker asks for a run in its own project only".into());
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return broker::refused(format!("cannot find the wsx binary: {error}")),
    };
    // ^ `-p` goes before the run's own words: after `--` it would be part of the command.
    let output = std::process::Command::new(exe)
        .args(&argv[..2])
        .arg("-p")
        .arg(project)
        .args(&argv[2..])
        .current_dir(copy)
        .env(super::cmd::run::WORKER_ENV, task.to_string())
        .env_remove(wsx_core::task::owner::ACTOR_ENV)
        .env_remove(broker::BROKER_ENV)
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) => broker::Response {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(1),
        },
        Err(error) => broker::refused(format!("cannot run wsx: {error}")),
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    fn step() -> WorkflowStep {
        WorkflowStep {
            name: "review".into(),
            prompt: "Run the simplify skill".into(),
        }
    }

    // The studio-app #120 failure: a message after the result rotated and was told it was in the
    // last step, so the worker reviewed work that was never built.
    #[test]
    fn free_turn_names_no_step_and_step_turn_does() {
        let step = step();
        let free = rotation_prompt(
            210_000,
            Some("stop after mockups"),
            None,
            "Build it now",
            vec!["and add tests".into()],
            "NOTES",
        );
        assert!(!free.contains("You are in step"), "{free}");
        let at = |needle: &str| {
            free.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {free}"))
        };
        assert!(at("stop after mockups") < at("Build it now"));
        assert!(at("Build it now") < at("and add tests"));
        assert!(at("and add tests") < at("NOTES"));
        assert!(free.contains("newest instruction wins"));
        let stepped = rotation_prompt(
            210_000,
            None,
            Some((2, 3, &step)),
            "continue",
            vec![],
            "NOTES",
        );
        assert!(stepped.contains("You are in step 3 of 3, review: Run the simplify skill"));
        let started = rotation_prompt(
            1000,
            None,
            Some((2, 3, &step)),
            "Run the simplify skill",
            vec![],
            "N",
        );
        assert!(!started.contains("You are in step"), "{started}");
    }
}
