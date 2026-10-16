//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

use super::claude::{new_session_id, AgentLine, ClaudeAgent, TurnResult};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::git::{git_cmd, output_with_timeout_limit};
use wsx_core::model::workspace::LandPolicy;
use wsx_core::runtime;
use wsx_core::task::{
    copy::{self, Rebase},
    now_unix_ms, report,
    store::{TaskDir, REPORT_FILE, RESULT_FILE},
    verification, CheckResult, HostRequest, HostResponse, LogEvent, MessageKind, TaskRecord,
    TaskState, Verdict, WorkerResult, MAX_MESSAGE_BYTES,
};

const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_TOOL_BYTES: usize = 256;
/// The worker rebases a conflicted landing itself and needs this rule for it.
const REBASE_TOOL: &str = "Bash(git rebase:*)";
/// `sun_path` holds 104 bytes on macOS, including the final NUL.
const MAX_SOCKET_PATH_BYTES: usize = 103;

enum Event {
    Request(HostRequest, Sender<HostResponse>),
    /// `None` when the agent process of that generation ended.
    Agent(u64, Option<AgentLine>),
    ChecksDone(Vec<CheckResult>),
}

/// A `land` waiting for the checks of its rebased revision.
struct Landing {
    skip_verify: bool,
    /// The verified revision whose pass carries to the rebased one, with its note.
    carried: Option<(u32, Option<String>)>,
}

struct Host {
    dir: TaskDir,
    record: TaskRecord,
    events: Sender<Event>,
    agent: Option<ClaudeAgent>,
    generation: u64,
    turn_active: bool,
    /// Adds sent during a turn or a check, delivered when it ends.
    pending: VecDeque<String>,
    /// An allowed tool takes effect when the agent next starts.
    restart_needed: bool,
    stop_requested: bool,
    /// The result of the latest turn, until its checks end.
    last_result: Option<WorkerResult>,
    /// Tool calls denied in that turn.
    last_denials: Vec<String>,
    landing: Option<Landing>,
    started: Instant,
}

pub fn run(project_id: &str, id: u32) -> Result<()> {
    let dir = TaskDir::open(project_id, id)?;
    let mut record = dir.read_record()?;
    if record.state == TaskState::Closed {
        bail!("task t{id} is closed");
    }
    let socket = dir.socket_path();
    if socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        bail!(
            "socket path {} is longer than {MAX_SOCKET_PATH_BYTES} bytes; use a shorter XDG_STATE_HOME",
            socket.display()
        );
    }
    let listener = bind(&socket)?;
    record.pane_id = std::env::var(runtime::WSX_PANE_ID_ENV)
        .ok()
        .and_then(|value| value.parse().ok());
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
        landing: None,
        started: Instant::now(),
    };
    std::thread::spawn(move || serve(listener, events));
    let result = host.run(receiver);
    if let Some(agent) = host.agent.take() {
        agent.stop();
    }
    let _ = std::fs::remove_file(&socket);
    host.report_agent(false);
    result
}

fn bind(socket: &Path) -> Result<UnixListener> {
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("another host already serves {}", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove stale {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn serve(listener: UnixListener, events: Sender<Event>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
        let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
        let mut bytes = Vec::new();
        if (&mut stream)
            .take(MAX_REQUEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > MAX_REQUEST_BYTES
        {
            continue;
        }
        let Ok(request) = serde_json::from_slice::<HostRequest>(&bytes) else {
            continue;
        };
        let (reply, answer) = channel();
        if events.send(Event::Request(request, reply)).is_err() {
            return;
        }
        if let Ok(response) = answer.recv_timeout(Duration::from_secs(30)) {
            if let Ok(mut line) = serde_json::to_vec(&response) {
                line.push(b'\n');
                let _ = stream.write_all(&line);
            }
        }
    }
}

impl Host {
    fn run(&mut self, receiver: Receiver<Event>) -> Result<()> {
        self.say(&format!(
            "task t{} on {} in {}",
            self.record.id,
            self.record.branch,
            self.record.working_copy.display()
        ));
        match self.record.state {
            TaskState::Starting => {
                let prompt = self.dir.prompt()?.context("the task log has no prompt")?;
                self.start_turn(&prompt);
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
        while let Ok(event) = receiver.recv() {
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
            }
        }
        Ok(())
    }

    fn handle(&mut self, request: HostRequest) -> HostResponse {
        let message = match request {
            HostRequest::Status => Ok("".to_string()),
            HostRequest::Steer { text } => self.steer(text),
            HostRequest::Add { text } => self.add(text),
            HostRequest::Allow { tool } => self.allow(tool),
            HostRequest::Stop => self.stop(),
            HostRequest::Land { skip_verify } => self.land(skip_verify),
            HostRequest::Close { discard } => {
                if let Some(agent) = self.agent.take() {
                    agent.stop();
                }
                self.turn_active = false;
                self.log(LogEvent::Closed { discarded: discard });
                self.set_state(TaskState::Closed);
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
        if self.turn_active || self.record.state == TaskState::Checking {
            self.pending.push_back(text);
            "queued until the current turn ends".into()
        } else {
            self.start_turn(&text);
            "started a new turn".into()
        }
    }

    fn allow(&mut self, tool: String) -> Result<String> {
        if tool.is_empty() || tool.len() > MAX_TOOL_BYTES || tool.chars().any(char::is_control) {
            bail!("a tool pattern must be 1..{MAX_TOOL_BYTES} bytes without control characters");
        }
        if !self.record.tools.contains(&tool) {
            self.record.tools.push(tool.clone());
        }
        self.log(LogEvent::Allowed { tool: tool.clone() });
        self.restart_needed = true;
        if self.record.state == TaskState::Blocked && !self.turn_active {
            self.start_turn(&format!(
                "The owner allowed {tool}. Retry what was denied and continue the task."
            ));
            return Ok(format!("allowed {tool} and resumed the task"));
        }
        self.save();
        Ok(format!("allowed {tool}; it applies from the next turn"))
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

    fn start_turn(&mut self, text: &str) {
        if let Err(error) = self.try_start_turn(text) {
            self.say(&format!("cannot start the agent: {error:#}"));
            self.log(LogEvent::Stopped {
                reason: format!("cannot start the agent: {error:#}"),
            });
            self.set_state(TaskState::Failed);
        }
    }

    fn try_start_turn(&mut self, text: &str) -> Result<()> {
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
            self.agent = Some(ClaudeAgent::spawn(
                &self.record,
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
            usage: Some(turn.usage),
            denials: turn.denials.clone(),
        });
        self.say(&format!(
            "result {}: {}",
            result.status,
            first_line(&result.summary)
        ));
        self.last_denials = turn.denials.clone();
        self.write_result(&result, &[]);
        self.last_result = Some(result.clone());
        // ^ Only the worker's own word blocks a task. Denials it worked around are evidence.
        if result.status == "blocked" {
            self.log(LogEvent::Blocked {
                denials: turn.denials,
            });
            self.set_state(TaskState::Blocked);
            return;
        }
        self.start_checks();
    }

    fn start_checks(&mut self) {
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
                let landing = self.landing.take();
                if let Some((from, note)) = landing.as_ref().and_then(|l| l.carried.clone()) {
                    self.say(&format!("carried the pass of r{from} to r{revision}"));
                    self.log(LogEvent::Verified {
                        revision,
                        result: Verdict::Pass,
                        note,
                        carried_from: Some(from),
                    });
                }
                // ^ Readers act on the state: the result and report must be complete before it.
                self.write_result(&result, &results);
                let log = self.dir.read_log().unwrap_or_default();
                let reported = TaskRecord {
                    state,
                    ..self.record.clone()
                };
                let text = report::render(&reported, &log, &result, revision, &commit);
                if let Err(error) = self.dir.write_file(REPORT_FILE, text.as_bytes()) {
                    self.say(&format!("cannot write the report: {error:#}"));
                }
                self.say(&format!("revision r{revision} at {commit:.12}"));
                self.set_state(state);
                if let Some(landing) = landing {
                    let finished = if state == TaskState::Ready {
                        self.verify_gate(landing.skip_verify)
                            .and_then(|()| self.fast_forward())
                    } else {
                        Err(anyhow::anyhow!("checks failed on the rebased revision"))
                    };
                    match finished {
                        Ok(message) => self.say(&message),
                        Err(error) => {
                            self.say(&format!("landing stopped: {error:#}"));
                            self.log(LogEvent::LandStopped {
                                reason: format!("{error:#}"),
                            });
                        }
                    }
                }
            }
            Err(error) => {
                self.say(&format!("cannot record a revision: {error:#}"));
                self.set_state(TaskState::Failed);
            }
        }
        if !self.pending.is_empty() {
            let text: Vec<String> = self.pending.drain(..).collect();
            self.start_turn(&text.join("\n\n"));
        }
    }

    /// Land the current revision on the default branch (docs/tasks.md "Landing").
    fn land(&mut self, skip_verify: bool) -> Result<String> {
        if self.record.state != TaskState::Ready {
            bail!(
                "task t{} is {}; only a ready task lands",
                self.record.id,
                self.record.state.as_str()
            );
        }
        let settings =
            load_task_config(&self.record.project_path).map_err(|error| anyhow::anyhow!(error))?;
        if settings.land_policy == LandPolicy::Pr {
            bail!("the pr landing policy is not available yet");
        }
        self.verify_gate(skip_verify)?;
        let working_copy = self.record.working_copy.clone();
        let changes = copy::tracked_changes(&working_copy)?;
        if !changes.is_empty() {
            bail!(
                "the working copy has uncommitted changes: {}; ask the worker to commit them",
                changes.join(", ")
            );
        }
        let tip = self.default_tip()?;
        let head = copy::resolve_commit(&working_copy, "HEAD")?;
        if copy::is_ancestor(&working_copy, &tip, &head)? {
            return self.fast_forward();
        }
        let (old_revision, old_base) = (self.record.revision, self.record.base.clone());
        match copy::rebase(&working_copy, &tip)? {
            Rebase::Conflict(paths) => {
                self.log(LogEvent::Conflict {
                    onto: tip.clone(),
                    paths: paths.clone(),
                });
                if !self.record.tools.iter().any(|tool| tool == REBASE_TOOL) {
                    self.record.tools.push(REBASE_TOOL.into());
                    self.log(LogEvent::Allowed {
                        tool: REBASE_TOOL.into(),
                    });
                    self.restart_needed = true;
                }
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
                    "the rebase onto {} conflicted in {}; the worker is resolving it. Land again when the task is ready",
                    self.record.default_branch,
                    paths.join(", ")
                ))
            }
            Rebase::Done => {
                let carried = self.carried_pass(old_revision, &old_base, &tip);
                self.landing = Some(Landing {
                    skip_verify,
                    carried,
                });
                self.start_checks();
                Ok(format!(
                    "rebased onto {} at {tip:.12}; checks run on the new revision and it lands when they pass",
                    self.record.default_branch
                ))
            }
        }
    }

    fn default_tip(&self) -> Result<String> {
        copy::resolve_commit(
            &self.record.project_path,
            &format!("refs/heads/{}", self.record.default_branch),
        )
    }

    /// Refuse a revision with Human Verify items and no pass, unless the caller skips it (logged).
    fn verify_gate(&mut self, skip_verify: bool) -> Result<()> {
        let items = self.current_result().human_verify.len();
        if items == 0 {
            return Ok(());
        }
        let revision = self.record.revision;
        let log = self.dir.read_log()?;
        let passed = matches!(
            verification(&log, revision).map(|entry| &entry.event),
            Some(LogEvent::Verified {
                result: Verdict::Pass,
                ..
            })
        );
        if passed {
            return Ok(());
        }
        if skip_verify {
            self.log(LogEvent::VerifySkipped { revision });
            return Ok(());
        }
        bail!(
            "r{revision} has {items} Human Verify item(s) and no pass; check them and run `wsx task verify {} pass`, or land with --skip-verify",
            self.record.id
        )
    }

    /// A pass on `revision` carries over a rebase only when the default branch changed
    /// none of the task's paths between the old base and `tip`.
    fn carried_pass(
        &self,
        revision: u32,
        old_base: &str,
        tip: &str,
    ) -> Option<(u32, Option<String>)> {
        let log = self.dir.read_log().ok()?;
        let LogEvent::Verified {
            result: Verdict::Pass,
            note,
            ..
        } = &verification(&log, revision)?.event
        else {
            return None;
        };
        let (upstream, total) =
            copy::changed_paths(&self.record.working_copy, old_base, tip).ok()?;
        // ^ Capped lists cannot prove that no path is shared.
        let complete = upstream.len() as u32 == total
            && self.record.paths.len() as u32 == self.record.paths_total;
        let shared = upstream.iter().any(|path| self.record.paths.contains(path));
        (complete && !shared).then(|| (revision, note.clone()))
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
        self.log(LogEvent::Landed {
            commit: head.clone(),
        });
        self.set_state(TaskState::Landed);
        Ok(format!(
            "landed r{} on {} at {head:.12}",
            self.record.revision, self.record.default_branch
        ))
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
        let elapsed = self.started.elapsed().as_secs();
        println!(
            "{:02}:{:02}:{:02} {}",
            elapsed / 3600,
            elapsed / 60 % 60,
            elapsed % 60,
            text
        );
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
            TaskState::Blocked => runtime::AgentState::Blocked,
            TaskState::Ready | TaskState::Landed => runtime::AgentState::Done,
            TaskState::Failed => runtime::AgentState::Error,
            TaskState::Stopped | TaskState::Closed => runtime::AgentState::Idle,
        };
        let request = runtime::Request::AgentReport {
            pane_id: runtime::PaneId(pane_id),
            runtime_generation: std::env::var(runtime::WSX_RUNTIME_GENERATION_ENV).ok(),
            provider: "task".into(),
            state,
            attached,
            presence_id: None,
            conversation_id: None,
            session_ref: None,
            wake_token: None,
            capabilities: runtime::AgentCapabilities::default(),
        };
        match runtime::Client::new(runtime::default_socket_path()).call(&request) {
            Ok(runtime::Response::Ack { .. }) => {}
            Ok(runtime::Response::Error(error)) => {
                self.say(&format!("agent report refused: {}", error.message))
            }
            Ok(_) => {}
            Err(error) => self.say(&format!("agent report failed: {error:#}")),
        }
    }
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
