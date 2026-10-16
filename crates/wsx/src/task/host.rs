//! `wsx task host`: the pane process that owns one task while it runs.
//! ref: docs/tasks.md "Host"
//!
//! One main loop owns the task files. The socket thread and the agent reader
//! send to it over one channel; it answers each request after acting on it.

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
    copy::{self, Rebase},
    now_unix_ms, passed, report,
    store::{TaskDir, BRIEF_FILE, BRIEF_TEXT_FILE, REPORT_FILE, RESULT_FILE},
    verification, view, CheckResult, HostRequest, HostResponse, LogEvent, MessageKind, Phase,
    TaskRecord, TaskState, Verdict, WorkerResult, MAX_MESSAGE_BYTES,
};

const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
const CHECK_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_TOOL_BYTES: usize = 256;
/// The worker rebases a conflicted landing itself and needs this rule for it.
const REBASE_TOOL: &str = "Bash(git rebase:*)";

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
    let listener = hosting::bind(&socket)?;
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
        restart_needed: false,
        stop_requested: false,
        last_result: None,
        last_denials: Vec::new(),
        landing: None,
        started: Instant::now(),
    };
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
            HostRequest::Start => self.start(),
            HostRequest::Land { skip_verify } => self.land(skip_verify),
            HostRequest::Verify { result, note } => self.verify(result, note),
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

    fn allow(&mut self, tool: String) -> Result<String> {
        if tool.is_empty() || tool.len() > MAX_TOOL_BYTES || tool.chars().any(char::is_control) {
            bail!("a tool pattern must be 1..{MAX_TOOL_BYTES} bytes without control characters");
        }
        // ^ During the brief an allowed tool joins the brief turns' tools only.
        let tools = match self.record.phase {
            Phase::Brief => &mut self.record.brief_tools,
            Phase::Build => &mut self.record.tools,
        };
        if !tools.contains(&tool) {
            tools.push(tool.clone());
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

    /// Build the latest brief's rewritten prompt. The agent restarts with the build's
    /// tools and result schema and resumes the conversation, so it keeps what it read.
    fn start(&mut self) -> Result<String> {
        if self.record.phase != Phase::Brief || self.record.state != TaskState::Briefed {
            bail!(
                "task t{} is {}; only a briefed task starts",
                self.record.id,
                self.record.state.as_str()
            );
        }
        let file = brief::read(&self.dir).context("the task has no readable brief")?;
        let prompt = file.brief.rewritten_prompt.trim().to_string();
        self.record.phase = Phase::Build;
        self.restart_needed = true;
        self.log(LogEvent::Started {
            version: file.version,
            prompt: prompt.clone(),
        });
        self.say(&format!("build of brief v{} started", file.version));
        self.start_turn(&build_request(file.version, &prompt));
        Ok(format!("started the build of brief v{}", file.version))
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
                &Launch::worker(&self.record),
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
            Ok((version, answer)) => {
                self.say(&format!("brief v{version} ready"));
                self.log(LogEvent::Brief {
                    version,
                    answer,
                    usage: Some(turn.usage),
                    denials: turn.denials,
                });
                self.set_state(TaskState::Briefed);
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
        if !self.pending.is_empty() {
            let text: Vec<String> = self.pending.drain(..).collect();
            self.start_turn(&text.join("\n\n"));
        }
    }

    /// Write `brief.json` and `brief.md` as the next version; returns it and the answer.
    fn save_brief(&self, brief: Brief) -> Result<(u32, String)> {
        let version = brief::read(&self.dir).map_or(1, |file| file.version + 1);
        let answer = brief.answer.trim().to_string();
        let file = BriefFile { version, brief };
        let bytes = serde_json::to_vec_pretty(&file)?;
        if bytes.len() > brief::MAX_BRIEF_BYTES {
            bail!("the brief is larger than {} bytes", brief::MAX_BRIEF_BYTES);
        }
        let title = view::title(&self.dir).unwrap_or_default();
        self.dir.write_file(BRIEF_FILE, &bytes)?;
        self.dir.write_file(
            BRIEF_TEXT_FILE,
            brief::render(self.record.id, &title, &file).as_bytes(),
        )?;
        Ok((version, answer))
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
                self.write_report(state);
                self.say(&format!("revision r{revision} at {commit:.12}"));
                // ^ A landing goes from checking straight to landed, or to ready with
                // land_stopped: readers never see a ready task that is still landing.
                match landing {
                    None => self.set_state(state),
                    Some(landing) => {
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
                                self.set_state(state);
                            }
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

    /// Record a human verdict on the current revision. A fail goes to the worker.
    fn verify(&mut self, result: Verdict, note: Option<String>) -> Result<String> {
        let revision = self.record.revision;
        if revision == 0 {
            bail!(
                "task t{} has no revision yet; verify after its first result",
                self.record.id
            );
        }
        let note = note.filter(|note| !note.trim().is_empty());
        if let Some(note) = &note {
            check_message(note)?;
        }
        if result == Verdict::Fail && note.is_none() {
            bail!("a fail needs --note saying what is wrong; the worker gets it");
        }
        self.log(LogEvent::Verified {
            revision,
            result,
            note: note.clone(),
            carried_from: None,
        });
        self.write_report(self.record.state);
        let Some(note) = note.filter(|_| result == Verdict::Fail) else {
            return Ok(format!("r{revision} passed human verification"));
        };
        self.log(LogEvent::Message {
            mode: MessageKind::Verify,
            text: note.clone(),
        });
        let text = format!(
            "Human verification of r{revision} failed. The owner wrote:\n\n{note}\n\nFix it, commit, and report."
        );
        if self.turn_active {
            self.agent
                .as_mut()
                .context("the agent is not running")?
                .send(&text)?;
            return Ok(format!(
                "r{revision} failed; the note steered the running turn"
            ));
        }
        Ok(format!(
            "r{revision} failed; the note was sent to the worker: {}",
            self.deliver_or_queue(text)
        ))
    }

    /// Land the current revision on the default branch (docs/tasks.md "Landing").
    fn land(&mut self, skip_verify: bool) -> Result<String> {
        let settings =
            load_task_config(&self.record.project_path).map_err(|error| anyhow::anyhow!(error))?;
        if settings.land_policy == LandPolicy::Pr {
            return self.land_pr(skip_verify, &settings.land_remote);
        }
        self.require_ready()?;
        self.verify_gate(skip_verify)?;
        self.require_committed()?;
        let working_copy = self.record.working_copy.clone();
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

    fn require_ready(&self) -> Result<()> {
        if self.record.state != TaskState::Ready {
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
    fn land_pr(&mut self, skip_verify: bool, remote: &str) -> Result<String> {
        let Some(url) = self.record.pr_url.clone() else {
            self.require_ready()?;
            self.verify_gate(skip_verify)?;
            self.require_committed()?;
            copy::push(&self.record.working_copy, remote, &self.record.branch)?;
            let title = self
                .dir
                .prompt()?
                .and_then(|prompt| prompt.lines().next().map(str::to_string))
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
        if !matches!(self.record.state, TaskState::InReview | TaskState::Ready) {
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
                self.log(LogEvent::Landed {
                    commit: commit.clone(),
                });
                self.set_state(TaskState::Landed);
                Ok(format!("{url} merged as {commit:.12}; landed"))
            }
            "CLOSED" => {
                self.log(LogEvent::PrClosed { url: url.clone() });
                self.set_state(TaskState::InReview);
                Ok(format!(
                    "{url} was closed without a merge; reopen it, or close the task with --discard"
                ))
            }
            _ if self.record.state == TaskState::Ready
                && self.record.pr_revision != Some(self.record.revision) =>
            {
                self.verify_gate(skip_verify)?;
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

    /// Refuse a revision with Human Verify items and no pass, unless the caller skips it (logged).
    fn verify_gate(&mut self, skip_verify: bool) -> Result<()> {
        let items = self.current_result().human_verify.len();
        if items == 0 {
            return Ok(());
        }
        let revision = self.record.revision;
        let log = self.dir.read_log()?;
        if passed(&log, revision) {
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
            TaskState::Stopped | TaskState::Closed => runtime::AgentState::Idle,
        };
        if let Some(problem) = hosting::report_pane(pane_id, "task", state, attached) {
            self.say(&problem);
        }
    }
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
