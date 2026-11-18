//! `wsx task repo-host <repo-id>`: the repo's service. One always-on host per repo, with no
//! agent and no chat of its own. It watches the repo's store (task states, approvals, stalls,
//! deploys, routines, agent sessions), sweeps captures and stale worktrees, starts the idle
//! task, asks for the recap and keeps the check-ins. Each event goes to its owner chat
//! (`chats::owner`), else to the repo's home chat; `owner::say` starts that chat's host when it
//! is not running, and the service marks the event seen only after the chat took it, so a
//! deploy or a crash loses none. An event that needs no decision (`state_wakes`) is logged in
//! the chat as a message that starts no turn. ref: docs/tasks.md "Chats"

use super::manager::plain_event;
use super::{hosting, notes};
use crate::cli::existing_daemon_snapshot;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::routine::{store as routines, RunRecord, RunStatus, Trigger};
use wsx_core::stale;
use wsx_core::task::manager::{
    decided_request, open_requests, stall_wakes, state_wakes, ApprovalLine, Author, Checkin, Level,
    ManagerEntry, ManagerEvent, ManagerRequest, ManagerResponse, RepoState, Seen, Short,
};
use wsx_core::task::owner::ACTOR_ENV;
use wsx_core::task::replace::Binary;
use wsx_core::task::stall;
use wsx_core::task::store::{self, ManagerDir};
use wsx_core::task::view::{self, clip, project_tasks, read_result};
use wsx_core::task::{
    call_host, chats, checks, ledger, now_unix_ms, owner, session_news, test_secs, usage, Actor,
    HostRequest, TaskRecord, TaskState, MAX_MESSAGE_BYTES,
};

const MAX_REQUEST_BYTES: u64 = 2 * MAX_MESSAGE_BYTES as u64;
/// Events within this long of each other become one message.
/// What wsx asks after a quiet period. The reply is the recap the owner reads.
const RECAP_REQUEST: &str = "The project has been quiet. Write the owner a short recap, at most 8 lines: \
what landed and is live, what runs, and what waits for the owner and why. Read the ledger for it. Start no \
task and change nothing. Name a task by its number and what it does.";
/// The title of the task `tasks.workflow.idle` starts.
const IDLE_TITLE: &str = "Consolidate project knowledge";
/// A reminder's key is kept in `post-keys.json` for this long: a step lasts as long.
const OWNER_REMINDER_WINDOW_SECS: u64 = 24 * 60 * 60;
const EVENT_QUIET: Duration = Duration::from_secs(3);
/// `EVENT_QUIET`, or the seconds in `WSX_TEST_EVENT_QUIET_SECS`: for the task scenario only.
fn event_quiet() -> Duration {
    test_secs("WSX_TEST_EVENT_QUIET_SECS", EVENT_QUIET)
}
/// A chat that did not take its events is tried again after this long.
const RETRY: Duration = Duration::from_secs(10);
/// How often the host keeps the "To check" captures within their budget.
const CAPTURE_SWEEP: Duration = Duration::from_secs(60 * 60);
/// How often the host looks at wsxd for sessions that need the owner.
const SESSION_LOOK: Duration = Duration::from_secs(2);
const EVENT_TITLE_CHARS: usize = 80;
const EVENT_SUMMARY_CHARS: usize = 160;
/// A failed deploy's event carries this many last lines of its output.
const DEPLOY_EVENT_LINES: usize = 10;
/// An idle task runs at most this often.
const IDLE_EVERY_MS: u64 = 24 * 3_600_000;
/// A named chat with no message and no open task of its own for this long is suggested for
/// closing (the scenario backdates the chat's log instead of waiting).
const CHAT_IDLE_MS: u64 = 7 * 24 * 3_600_000;

/// How often the project's stale worktrees and idle chats are swept, or the seconds in
/// `WSX_TEST_PRUNE_SWEEP_SECS`: for the task scenario only, which would wait an hour.
fn prune_sweep() -> Duration {
    test_secs("WSX_TEST_PRUNE_SWEEP_SECS", Duration::from_secs(60 * 60))
}

enum Event {
    /// The news of this session did not reach the home chat: it is posted again.
    Unposted(u64),
    /// The owner's reminder under this key did not reach the home chat: the next sweep sends it again.
    Unreminded(String),
    Request(ManagerRequest, Sender<ManagerResponse>),
    Tick,
    /// A sweep of stale worktrees and sessions ended: its notes, or why it failed.
    Pruned(Result<Vec<stale::Note>, String>),
    /// `wsx task new` with `tasks.workflow.idle` ended: when it began, and the task number or
    /// the error.
    IdleStarted(u64, Result<u32, String>),
    /// The chat `to` took the lines `lines` (`ok`), or did not: they go to the home chat, or
    /// come back for a new try when `to` is the home chat.
    Delivered {
        to: String,
        lines: Vec<(String, Short)>,
        ok: bool,
    },
}

struct Repo {
    project: String,
    project_id: String,
    main: PathBuf,
    /// What the last save held; `save_seen` writes the fields below it only after every event
    /// was taken.
    state: RepoState,
    events: Sender<Event>,
    /// `tasks.deploy.auto`: a deploy that worked starts no turn. ref: `state_wakes`
    deploy_auto: bool,
    /// Event lines not yet sent, by the chat that hears them (empty: the home chat), and when
    /// the last one came.
    outbox: BTreeMap<String, Vec<(String, Short)>>,
    last_event: Option<Instant>,
    /// Sends to a chat that did not end yet: nothing is marked seen while one runs.
    in_flight: usize,
    retry_at: Option<Instant>,
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
    /// The stall each task shows now, and since when: it is reported after `stall::grace()`.
    stall_suspects: BTreeMap<u32, (String, Instant)>,
    /// The keys of the owner reminders sent or on their way, so a sweep sends none twice.
    owner_reminded: BTreeSet<String>,
    stalls_swept: Option<Instant>,
    suggest_looked: Option<Instant>,
    sessions_looked: Option<Instant>,
    /// When the last sweep of stale worktrees began, and whether one runs.
    /// ref: docs/tasks.md "Stale worktrees"
    pruned: Option<Instant>,
    pruning: bool,
    /// The worktrees the last sweep left for the owner, with the text posted: posted again
    /// only when the text changes.
    flagged: BTreeMap<PathBuf, String>,
    /// `tasks.manager.recapMinutes` as a duration; `None` is off.
    recap_quiet: Option<Duration>,
    /// When the host last removed old captures; `None` sweeps on the first tick.
    captures_swept: Option<Instant>,
    /// When an event or a recap last went out: the recap waits for a quiet period after it.
    last_activity: Instant,
    started: Instant,
    /// The waiting approvals that run now, by id (`run_waiting`).
    running_waits: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    /// The file this host runs from, and whether it must exec its replacement.
    binary: Option<Binary>,
    reexec: bool,
}

pub fn run(project_id: &str) -> Result<()> {
    let (project, main) = super::project_by_id(project_id)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let socket = store::repo_socket(project_id)?;
    let listener = hosting::bind(&socket)?;
    // ^ The home chat exists from the first start: an event with no owner has a place.
    ManagerDir::open_home(project_id)?;
    let (events, receiver) = channel();
    let mut repo = Repo {
        project,
        project_id: project_id.into(),
        main,
        state: RepoState::default(),
        events: events.clone(),
        deploy_auto: settings.deploy_auto,
        outbox: BTreeMap::new(),
        last_event: None,
        in_flight: 0,
        retry_at: None,
        observed: BTreeMap::new(),
        observed_decisions: 0,
        observed_deploys_ms: 0,
        observed_routines: 0,
        observed_stalls: BTreeMap::new(),
        stall_suspects: BTreeMap::new(),
        owner_reminded: BTreeSet::new(),
        stalls_swept: None,
        suggest_looked: None,
        sessions_looked: None,
        pruned: None,
        pruning: false,
        flagged: BTreeMap::new(),
        recap_quiet: (settings.manager_recap_secs > 0)
            .then(|| Duration::from_secs(settings.manager_recap_secs)),
        captures_swept: None,
        last_activity: Instant::now(),
        started: Instant::now(),
        running_waits: Default::default(),
        binary: Binary::capture(),
        reexec: false,
    };
    repo.restore()?;
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
    std::thread::spawn(move || {
        while events.send(Event::Tick).is_ok() {
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    // ^ A host that hangs in a turn under an older wsx is replaced after any install; a service
    // that starts looks for them, also after a deploy that ran on the older wsx, which has no
    // sweep of its own. ref: docs/tasks.md "Host" (A host hung in a turn)
    std::thread::spawn(|| {
        for error in wsx_core::task::replace::sweep() {
            eprintln!("cannot replace a hung host: {error}");
        }
    });
    // ^ A deploy waiting for a quiet branch dies with a reboot or a killed process: a service
    // that starts starts it again when the last deploy is not the default branch tip.
    if let Err(error) =
        super::cmd::deploy::spawn_quiet_deploy(&repo.project, &repo.project_id, &repo.main)
    {
        repo.say(&format!("cannot start the deploy that is due: {error:#}"));
    }
    if let Some(pane) = hosting::own_pane() {
        if let Some(problem) =
            hosting::report_pane(pane, "manager", wsx_core::runtime::AgentState::Idle, true)
        {
            repo.say(&problem);
        }
    }
    repo.say(&format!(
        "service of {} in {}",
        repo.project,
        repo.main.display()
    ));
    repo.run(receiver)?;
    let _ = std::fs::remove_file(&socket);
    if let (true, Some(binary)) = (repo.reexec, &repo.binary) {
        let error = binary.exec();
        anyhow::bail!("cannot start the replaced wsx binary: {error}");
    }
    Ok(())
}

impl Repo {
    fn run(&mut self, receiver: Receiver<Event>) -> Result<()> {
        while let Ok(event) = receiver.recv() {
            match event {
                Event::Request(request, reply) => {
                    let _ = reply.send(self.handle(request));
                }
                Event::Tick => self.tick(),
                Event::Pruned(result) => self.pruned_done(result),
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
                Event::Delivered { to, lines, ok } => self.delivered(to, lines, ok),
                Event::Unreminded(key) => {
                    self.owner_reminded.remove(&key);
                }
                Event::Unposted(session) => {
                    if let Some(seen) = self.state.sessions_seen.as_mut() {
                        seen.remove(&session);
                        self.save();
                    }
                }
            }
            // ^ A replaced binary: exec it once nothing is on its way, so a new service follows.
            if self.in_flight == 0
                && self.outbox.is_empty()
                && self.binary.as_ref().is_some_and(Binary::replaced)
            {
                self.say("the wsx binary was replaced: the service restarts with it");
                self.reexec = true;
                return Ok(());
            }
        }
        Ok(())
    }

    fn handle(&mut self, request: ManagerRequest) -> ManagerResponse {
        let answer = |ok: bool, message: &str| ManagerResponse {
            ok,
            id: None,
            busy: false,
            binary: None,
            message: message.into(),
        };
        match request {
            ManagerRequest::Status => ManagerResponse {
                binary: self.binary.clone(),
                ..answer(true, "the repo's service runs")
            },
            ManagerRequest::Checkin { at_ms, why, chat } => {
                let chat = chat.unwrap_or_default();
                self.state.checkins.insert(chat, Checkin { at_ms, why });
                self.save();
                answer(true, "check-in set")
            }
            ManagerRequest::Say { .. } | ManagerRequest::Post { .. } => answer(
                false,
                "the repo's service takes no message: say it to a chat",
            ),
        }
    }

    fn tick(&mut self) {
        self.fire_checkins();
        self.watch_tasks();
        self.watch_stalls();
        self.watch_sessions();
        self.ask_recap();
        self.sweep_captures();
        self.prune_stale();
        self.suggest_closes();
        self.flush_events();
        self.save_seen();
    }

    /// The repo's home chat by name, as the next event would find it.
    fn home(&self) -> String {
        store::home_chat(&self.project_id).unwrap_or_else(|| chats::home_name(&self.project_id))
    }

    /// Queue an event line for `chat` (empty: the home chat).
    fn emit(&mut self, chat: &str, line: (String, Short)) {
        self.outbox.entry(chat.to_string()).or_default().push(line);
        self.last_event = Some(Instant::now());
    }

    /// Send the queued lines once they have been quiet for a while, one message per chat. A
    /// chat that is not open, or the empty name, is the home chat, which is opened again when it
    /// was closed. `owner::say` starts a host that is not running; the lines count as taken only
    /// when it answers `ok` (`delivered`).
    fn flush_events(&mut self) {
        let quiet = self
            .last_event
            .is_some_and(|last| last.elapsed() >= event_quiet());
        if self.outbox.is_empty() || !quiet || self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let open = chats::open_names();
        let home = self.home();
        let mut target: BTreeMap<String, Vec<(String, Short)>> = BTreeMap::new();
        for (to, lines) in std::mem::take(&mut self.outbox) {
            let to = match open.contains(&to) {
                true => to,
                false => home.clone(),
            };
            target.entry(to).or_default().extend(lines);
        }
        if target.contains_key(&home) {
            if let Err(error) = ManagerDir::open_home(&self.project_id) {
                self.say(&format!("cannot open the home chat: {error:#}"));
                self.retry_at = Some(Instant::now() + RETRY);
                self.outbox = target;
                return;
            }
        }
        self.last_activity = Instant::now();
        for (to, lines) in target {
            self.in_flight += 1;
            let (own, events) = (self.project_id.clone(), self.events.clone());
            let project = self.project.clone();
            let exe = exe.clone();
            std::thread::spawn(move || {
                let ok = say_chat(&own, &project, &exe, &to, lines.clone(), Author::Events);
                let _ = events.send(Event::Delivered { to, lines, ok });
            });
        }
    }

    /// A chat took its lines, or did not. A chat that did not (its host does not start or
    /// refuses) gives them to the home chat with a line that says so; the home chat's own go
    /// back for a new try.
    fn delivered(&mut self, to: String, lines: Vec<(String, Short)>, ok: bool) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if ok {
            return;
        }
        let home = self.home();
        self.retry_at = Some(Instant::now() + RETRY);
        self.say(&format!("chat {to} did not take its events"));
        match to == home {
            true => self.outbox.entry(String::new()).or_default().extend(lines),
            false => {
                let head = plain_event(format!(
                    "chat {to} did not take the events of its tasks, so they are yours:"
                ));
                let outbox = self.outbox.entry(String::new()).or_default();
                outbox.push(head);
                outbox.extend(lines);
            }
        }
        self.last_event = Some(Instant::now());
    }

    /// Mark what was turned into events as seen, once every chat took its lines: a restart
    /// before that finds the same changes again, so no event is lost across a restart.
    fn save_seen(&mut self) {
        if !self.outbox.is_empty() || self.in_flight > 0 {
            return;
        }
        let seen = RepoState {
            seen: Some(self.observed.clone()),
            decisions_seen: self.observed_decisions,
            deploys_seen_ms: Some(self.observed_deploys_ms),
            routines_seen: Some(self.observed_routines),
            stalls: self.observed_stalls.clone(),
            ..self.state.clone()
        };
        if seen != self.state {
            self.state = seen;
            self.save();
        }
    }

    fn save(&mut self) {
        if let Err(error) = store::write_repo_state(&self.project_id, &self.state) {
            self.say(&format!("cannot write the repo state: {error:#}"));
        }
    }

    fn say(&self, text: &str) {
        hosting::progress(self.started, text);
    }

    /// Restore what was seen: `repo.json`, else what the first chat's host saved in its own
    /// `state.json` (the fields are the same), else the baseline of a first start.
    fn restore(&mut self) -> Result<()> {
        self.state = match store::read_repo_state(&self.project_id)? {
            Some(state) => state,
            None => ManagerDir::home(&self.project_id)?
                .and_then(|dir| {
                    std::fs::read(dir.path.join("state.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<RepoState>(&bytes).ok())
                })
                .unwrap_or_default(),
        };
        // ^ The one pending check-in the first chat's host held is that chat's.
        if let Some(checkin) = self.state.checkin.take() {
            self.state.checkins.entry(String::new()).or_insert(checkin);
        }
        // ^ A state from before `recap_ms`: now.
        self.state.recap_ms.get_or_insert_with(now_unix_ms);
        self.restore_seen();
        self.save();
        Ok(())
    }

    /// Send the check-in of each chat that is due as an event to that chat, also late after a
    /// restart. ref: docs/tasks.md "Check-in"
    fn fire_checkins(&mut self) {
        let now = now_unix_ms();
        let due: Vec<String> = self
            .state
            .checkins
            .iter()
            .filter(|(_, checkin)| checkin.at_ms <= now)
            .map(|(chat, _)| chat.clone())
            .collect();
        for chat in due {
            if let Some(checkin) = self.state.checkins.remove(&chat) {
                self.emit(&chat, plain_event(format!("check-in: {}", checkin.why)));
                self.save();
            }
        }
    }

    /// Ask the home chat for a recap once the repo has been quiet for `recap_quiet`: nothing
    /// sent, and nothing written to its log. The idle task starts and the `project.idle` event
    /// fires in the same look. ref: docs/tasks.md "Recap"
    fn ask_recap(&mut self) {
        let Some(quiet) = self.recap_quiet else {
            return;
        };
        let home = ManagerDir::home(&self.project_id).ok().flatten();
        let written = home
            .as_ref()
            .and_then(|dir| std::fs::metadata(dir.path.join("log.jsonl")).ok())
            .and_then(|meta| meta.modified().ok())
            .and_then(|at| at.elapsed().ok())
            .unwrap_or(Duration::MAX);
        let due = self.outbox.is_empty()
            && self.in_flight == 0
            && self.last_activity.elapsed() >= quiet
            && written >= quiet;
        if !due {
            return;
        }
        // ^ Once per quiet period: the next look waits a whole period.
        self.last_activity = Instant::now();
        self.start_idle();
        let now = now_unix_ms();
        self.fire(
            "project.idle",
            format!("idle-{now}"),
            json!({ "unix_ms": now }),
        );
        let since = self.state.recap_ms.unwrap_or(0);
        let log: Vec<_> = home
            .and_then(|dir| dir.read_log().ok())
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
        let (own, project, to) = (self.project_id.clone(), self.project.clone(), self.home());
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        std::thread::spawn(move || {
            let lines = vec![plain_event(text)];
            if !say_chat(&own, &project, &exe, &to, lines, Author::Recap) {
                eprintln!("cannot ask {to} for a recap");
            }
        });
    }

    /// Once an hour, tell the home chat of each named chat that has had no message and has no
    /// open task for a week, with one line each. It closes nothing: the owner decides, and a
    /// chat is suggested again only a week later. ref: docs/tasks.md "Chats"
    fn suggest_closes(&mut self) {
        if self
            .suggest_looked
            .is_some_and(|at| at.elapsed() < prune_sweep())
        {
            return;
        }
        self.suggest_looked = Some(Instant::now());
        let now = now_unix_ms();
        let tasks = project_tasks(&self.project_id).unwrap_or_default();
        for dir in chats::open_chats(&self.project_id) {
            let name = dir.name().to_string();
            if dir.is_home()
                || self
                    .state
                    .suggested_close
                    .get(&name)
                    .is_some_and(|at| now.saturating_sub(*at) < CHAT_IDLE_MS)
            {
                continue;
            }
            let written = std::fs::metadata(dir.path.join("log.jsonl"))
                .or_else(|_| std::fs::metadata(&dir.path))
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|at| at.elapsed().ok())
                .map_or(0, |age| age.as_millis() as u64);
            let owns = tasks.iter().any(|(_, record)| {
                record.as_ref().is_ok_and(|record| {
                    record.state.is_open() && record.created_by.chat() == Some(name.as_str())
                })
            });
            if written < CHAT_IDLE_MS || owns {
                continue;
            }
            self.state.suggested_close.insert(name.clone(), now);
            let (line, short) = plain_event(format!(
                "chat {name} has had no message and no open task for a while: close it with `wsx task chat close {name}` if it is done, or leave it"
            ));
            self.emit("", (line, short.quiet()));
        }
        self.save();
    }

    /// The duties start from what the saved state has seen; a first start takes what already
    /// happened as the baseline, not news.
    fn restore_seen(&mut self) {
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
            }
        }
        self.save_seen();
    }

    fn current_tasks(&self) -> Vec<(u32, Seen)> {
        project_tasks(&self.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(dir, record)| Some((dir, record.ok()?)))
            .map(|(dir, record)| (record.id, Seen::of(&dir, &record)))
            .collect()
    }

    fn approval_lines(&self) -> Vec<ApprovalLine> {
        store::read_approvals(&self.project_id).unwrap_or_default()
    }

    /// Drop open requests whose task left or moved on, or whose `after` task closed without
    /// landing; run those whose `after` task landed (the owner approved them in the chat); then
    /// turn new decisions into event lines, except those the owner's chat message approved: the
    /// manager ran them itself. ref: docs/tasks.md "Approvals"
    fn watch_approvals(&mut self) {
        let project_id = self.project_id.clone();
        let mut lines = self.approval_lines();
        let read =
            |task: u32| store::TaskDir::open(&project_id, task).and_then(|dir| dir.read_record());
        let (mut stale, mut due) = (Vec::new(), Vec::new());
        for request in open_requests(&lines) {
            let ApprovalLine::Request {
                id,
                action,
                task,
                after,
                ..
            } = request
            else {
                continue;
            };
            let why = task
                .and_then(|task| read(task).ok())
                .and_then(|record| owner::stale_reason(request, &record));
            let why = match (why, after) {
                (Some(why), _) => Some(why),
                (None, Some(after)) => match read(*after) {
                    Ok(record) if record.state == TaskState::Landed => {
                        due.push(id.clone());
                        None
                    }
                    Ok(record) if record.state.is_open() => None,
                    Ok(record) => Some(format!(
                        "#{after} is {}, so {} waits for nothing",
                        record.state.as_str(),
                        owner::action_name(*action)
                    )),
                    Err(_) => Some(format!(
                        "#{after} is gone, so {} waits for nothing",
                        owner::action_name(*action)
                    )),
                },
                (None, None) => None,
            };
            if let Some(why) = why {
                stale.push((id.clone(), why));
            }
        }
        if !stale.is_empty() {
            for (id, why) in stale {
                if let Err(error) = owner::drop_request(&project_id, &id, &why) {
                    self.say(&format!("cannot drop approval {id}: {error:#}"));
                }
            }
            lines = self.approval_lines();
        }
        for id in due
            .into_iter()
            .filter(|id| open_requests(&lines).iter().any(|line| line.id() == id))
        {
            self.run_waiting(id);
        }
        let open = chats::open_names();
        let decisions: Vec<usize> = (0..lines.len())
            .filter(|at| matches!(lines[*at], ApprovalLine::Decision { .. }))
            .collect();
        for at in decisions.iter().skip(self.observed_decisions as usize) {
            let ApprovalLine::Decision {
                id,
                approved,
                note,
                outcome,
                owner_said,
                ..
            } = &lines[*at]
            else {
                continue;
            };
            let request = decided_request(&lines, *at);
            // ^ The run of a request that waited for a land is news: the manager did not run it.
            if owner_said.is_some()
                && !matches!(request, Some(ApprovalLine::Request { after: Some(_), .. }))
            {
                continue;
            }
            let asked = match request {
                Some(ApprovalLine::Request {
                    action, task, path, ..
                }) => format!(
                    "{}{}",
                    format!("{action:?}").to_ascii_lowercase(),
                    match (task, path) {
                        (Some(task), _) => format!(" #{task}"),
                        (None, Some(path)) => format!(" {path}"),
                        (None, None) => String::new(),
                    }
                ),
                _ => String::new(),
            };
            let (verdict, short) = match (approved, outcome.strip_prefix(owner::DROPPED)) {
                (true, _) => (format!("approved; {outcome}"), format!("Approved {asked}")),
                (false, Some(why)) => (outcome.clone(), format!("Dropped {asked}: {why}")),
                (false, None) => (
                    format!("denied; {}", note.as_deref().unwrap_or("no note")),
                    format!("Denied {asked}"),
                ),
            };
            // ^ The chat whose agent asked hears the decision (`chats::owner`).
            let asker = match request {
                Some(ApprovalLine::Request { chat, .. }) => {
                    chats::owner(chat.as_deref(), &open, "")
                }
                _ => "",
            };
            let line = (
                format!(
                    "approval {id} ({asked}) {}",
                    clip(&verdict, EVENT_SUMMARY_CHARS * 2)
                ),
                Short::event(clip(&short, EVENT_SUMMARY_CHARS * 2), true),
            );
            self.emit(asker, line);
        }
        self.observed_decisions = decisions.len() as u64;
    }

    /// Run the waiting request `id` off the loop (a land or a release takes a while), once at a
    /// time. One that cannot even start is dropped with why, so it does not retry every tick.
    fn run_waiting(&mut self, id: String) {
        if !self
            .running_waits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone())
        {
            return;
        }
        let (name, main, project_id) = (
            self.project.clone(),
            self.main.clone(),
            self.project_id.clone(),
        );
        let running = self.running_waits.clone();
        std::thread::spawn(move || {
            let run = std::env::current_exe()
                .context("cannot find the wsx binary")
                .and_then(|exe| owner::approve_waiting(&name, &main, &id, &exe));
            if let Err(error) = run {
                let waiting = store::read_approvals(&project_id).unwrap_or_default();
                if wsx_core::task::manager::waiting_approvals(&waiting)
                    .iter()
                    .any(|line| line.id() == id)
                {
                    let _ = owner::drop_request(
                        &project_id,
                        &id,
                        &format!("it did not run: {error:#}"),
                    );
                }
            }
            running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
        });
    }

    fn newest_deploy_ms(&self) -> u64 {
        store::read_deploys(&self.project_id)
            .ok()
            .and_then(|records| records.iter().map(|record| record.unix_ms).max())
            .unwrap_or(0)
    }

    /// Turn each new failed deploy or release into an event line with the end of its output, and
    /// each new successful one into one line.
    /// ref: docs/tasks.md "Manager"
    fn watch_deploys(&mut self) {
        let Ok(records) = store::read_deploys(&self.project_id) else {
            return;
        };
        for record in &records {
            if record.unix_ms <= self.observed_deploys_ms {
                continue;
            }
            self.observed_deploys_ms = record.unix_ms;
            // ^ Deploys and releases run detached for minutes: a success is news too, or a
            // manager that waits for one never hears that it may go on.
            let outcome = match record.exit == Some(0) {
                true => "succeeded",
                false => "failed",
            };
            self.fire(
                &format!("{}.{outcome}", ledger::kind_name(record.kind)),
                format!("{}-{}", ledger::kind_name(record.kind), record.unix_ms),
                json!({ "commit": record.commit, "exit": record.exit }),
            );
            if record.exit == Some(0) {
                // ^ A trial puts an unlanded revision live: no landed task is live by it.
                let live = match record.trial {
                    Some(_) => String::new(),
                    None => self.made_live(&records, record),
                };
                let line = format!(
                    "{} of {:.12} succeeded after {} s{live}",
                    ledger::kind_name(record.kind),
                    record.commit,
                    record.duration_ms / 1000
                );
                let (line, short) = plain_event(line);
                // ^ An automatic deploy that worked is no news; a release that worked is.
                let short =
                    match self.deploy_auto && record.kind == wsx_core::task::DeployKind::Deploy {
                        true => short.quiet(),
                        false => short,
                    };
                self.emit("", (line, short));
                continue;
            }
            let failed = format!(
                "{} of {:.12} failed ({})",
                ledger::kind_name(record.kind),
                record.commit,
                wsx_core::routine::exit_words(record.exit),
            );
            let line = format!(
                "{failed}{}",
                output_ends(record.output.as_deref().unwrap_or_default())
            );
            self.emit("", (line, Short::plain(&failed)));
        }
    }

    /// ": #195, #197 live": the landed tasks a deploy or release of `deploy.commit` put live,
    /// those whose landed commit it holds and the previous success of its kind did not. The
    /// event says what is live, so no one has to read `deploys` for it.
    /// ref: docs/tasks.md "Deploy and release"
    fn made_live(
        &self,
        records: &[wsx_core::task::DeployRecord],
        deploy: &wsx_core::task::DeployRecord,
    ) -> String {
        let previous = records
            .iter()
            .rev()
            .find(|record| {
                record.kind == deploy.kind
                    && record.exit == Some(0)
                    && record.unix_ms < deploy.unix_ms
            })
            .map(|record| record.commit.as_str());
        let Ok(commits) = wsx_core::task::copy::commits_in(&self.main, previous, &deploy.commit)
        else {
            return String::new();
        };
        let mut live: Vec<u32> = project_tasks(&self.project_id)
            .unwrap_or_default()
            .iter()
            .filter_map(|(dir, record)| record.as_ref().ok().map(|record| (dir, record)))
            .filter(|(dir, record)| {
                wsx_core::memory::manage::landed_commit(dir, record)
                    .is_some_and(|commit| commits.contains(&commit))
            })
            .map(|(_, record)| record.id)
            .collect();
        live.sort_unstable();
        match live.is_empty() {
            true => String::new(),
            false => format!(": {} live", view::task_list(live)),
        }
    }

    fn newest_routine_run(&self) -> i64 {
        finished_routine_runs(&self.main)
            .iter()
            .filter_map(|run| run.finished_epoch)
            .max()
            .unwrap_or(0)
    }

    /// Make an event of each routine run that ended: for a routine that posts, one the manager
    /// answers to the owner in a turn of its own ([`routine_event`]); for one that does not, the
    /// end of its output for the manager's work. A success that printed nothing to stdout is no
    /// event and costs no turn. ref: docs/tasks.md "Routines"
    fn watch_routines(&mut self) {
        // ^ A run that ends in this second may be recorded after this read: wait the second out.
        let settled = (now_unix_ms() / 1000) as i64 - 1;
        let mut runs: Vec<RunRecord> = finished_routine_runs(&self.main)
            .into_iter()
            .filter(|run| {
                run.finished_epoch
                    .is_some_and(|end| end > self.observed_routines && end < settled)
            })
            .collect();
        runs.sort_by_key(|run| run.finished_epoch);
        let quiet: std::collections::HashSet<String> = match runs.is_empty() {
            true => Default::default(),
            false => stored_routines(&self.main)
                .routines
                .into_iter()
                .filter(|routine| !routine.post)
                .map(|routine| routine.name)
                .collect(),
        };
        for run in runs {
            self.observed_routines = run.finished_epoch.unwrap_or_default();
            if run.status == RunStatus::Succeeded && stdout_empty(&run) {
                continue;
            }
            let line = match quiet.contains(&run.routine) {
                true => routine_line(&run),
                false => routine_event(&run, &self.main),
            };
            self.emit("", plain_event(line));
        }
    }

    /// Post a project agent session that entered `exited`, `error` or `blocked`, once per
    /// entry, with no turn. The first look is the baseline, not news. A turn that runs holds
    /// the post: nothing is marked seen, so the next free look posts it.
    /// ref: docs/tasks.md "Session news"
    fn watch_sessions(&mut self) {
        if self
            .sessions_looked
            .is_some_and(|at| at.elapsed() < SESSION_LOOK)
        {
            return;
        }
        self.sessions_looked = Some(Instant::now());
        // ^ No wsxd, no change: a sleeping daemon must not read as every session gone.
        let Ok(snapshot) = existing_daemon_snapshot() else {
            return;
        };
        let found = session_news::attention(&snapshot, &self.main);
        let (fresh, keep) = session_news::unposted(found, self.state.sessions_seen.as_ref());
        // ^ A post starts the home chat's host and asks the service about itself (`ensure_manager`):
        // off the loop, which answers that. A post that did not reach the home chat (a turn runs, its
        // host does not start) is no news seen: `Unposted` takes it back, and the next look posts it.
        for news in fresh {
            let (project, main, events) =
                (self.project.clone(), self.main.clone(), self.events.clone());
            std::thread::spawn(move || {
                let posted = std::env::current_exe().ok().is_some_and(|exe| {
                    owner::post(
                        &project,
                        &main,
                        &exe,
                        super::launch_manager(&main),
                        news.event,
                        news.text,
                        None,
                    )
                    .is_ok_and(|response| response.ok)
                });
                if !posted {
                    let _ = events.send(Event::Unposted(news.session));
                }
            });
        }
        if self.state.sessions_seen.as_ref() != Some(&keep) {
            self.state.sessions_seen = Some(keep);
            self.save();
        }
    }

    /// Keep the project's captures within `tasks.captureBudgetMb`, at the start and hourly:
    /// over it the oldest decided items' files go. ref: docs/tasks.md "To check"
    fn sweep_captures(&mut self) {
        if self
            .captures_swept
            .is_some_and(|at| at.elapsed() < CAPTURE_SWEEP)
        {
            return;
        }
        self.captures_swept = Some(Instant::now());
        // ^ A config that does not parse names no budget, so nothing is removed.
        let Ok(settings) = load_task_config(&self.main) else {
            return;
        };
        let budget_mb = settings.capture_budget_mb;
        match checks::prune_captures(&self.project_id, budget_mb.saturating_mul(1 << 20)) {
            Ok(0) => {}
            Ok(removed) => self.say(&format!(
                "removed {removed} capture file(s) over the {budget_mb} MB capture budget"
            )),
            Err(error) => self.say(&format!("cannot prune captures: {error:#}")),
        }
    }

    /// Once an hour, off the loop: close the project's idle sessions and remove its stale
    /// worktrees that are clean, merged or pushed (`wsx_core::stale`), and stop the processes
    /// that outlived a closed task's working copy, also when the prune rules are off.
    /// ref: docs/tasks.md "Stale worktrees"
    fn prune_stale(&mut self) {
        if self.pruning || self.pruned.is_some_and(|at| at.elapsed() < prune_sweep()) {
            return;
        }
        self.pruned = Some(Instant::now());
        let Ok(settings) = load_task_config(&self.main) else {
            return;
        };
        let rules = stale::rules(&settings);
        let copies = stale::open_copies(&self.project_id);
        self.pruning = true;
        let (main, project_id, events) = (
            self.main.clone(),
            self.project_id.clone(),
            self.events.clone(),
        );
        std::thread::spawn(move || {
            let client = wsx_core::runtime::Client::new(wsx_core::runtime::default_socket_path());
            let leaked = stale::stop_leaked(&project_id, &main);
            let swept = if rules.off() {
                Ok(Vec::new())
            } else {
                stale::sweep(&client, &main, &copies, &rules).map_err(|e| format!("{e:#}"))
            };
            let result = swept.map(|mut notes| {
                notes.extend(leaked);
                notes
            });
            let _ = events.send(Event::Pruned(result));
        });
    }

    /// What a sweep did goes to the chat as events that start no turn, listed with the next one.
    fn pruned_done(&mut self, result: Result<Vec<stale::Note>, String>) {
        self.pruning = false;
        let notes = match result {
            Ok(notes) => notes,
            Err(error) => return self.say(&format!("cannot sweep stale worktrees: {error}")),
        };
        let left: BTreeMap<PathBuf, String> = notes
            .iter()
            .filter(|note| note.flag)
            .map(|note| (note.path.clone(), note.text.clone()))
            .collect();
        for note in notes {
            if note.flag && self.flagged.get(&note.path) == Some(&note.text) {
                continue;
            }
            let (line, short) = plain_event(format!("pruned: {}", note.text));
            self.emit("", (line, short.quiet()));
        }
        self.flagged = left;
    }

    /// Turn task state changes into event lines.
    fn watch_tasks(&mut self) {
        self.watch_approvals();
        self.watch_deploys();
        self.watch_routines();
        let Ok(tasks) = project_tasks(&self.project_id) else {
            return;
        };
        let open_chats = chats::open_names();
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
            let now = Seen::of(&dir, &record);
            if self.observed.get(&record.id) == Some(&now) {
                continue;
            }
            self.observed.insert(record.id, now);
            if !view::is_event(&record) {
                continue;
            }
            let title = view::title(&dir, &record).unwrap_or_default();
            let result = read_result(&dir).unwrap_or_default();
            let checks = view::check_outcomes(&result);
            let passed = checks
                .iter()
                .filter(|(_, outcome)| *outcome == "passed")
                .count();
            // ^ The checks the host ran come before the worker's own words.
            let summary = {
                let words = result["result"]["summary"].as_str().unwrap_or_default();
                // ^ The worker's own words, which nothing checked: the checks and the
                // recorded runs before them are the host's facts.
                let worker = format!(
                    "worker says (unchecked): {}",
                    clip(words, EVENT_SUMMARY_CHARS)
                );
                // ^ A check-run that passed after the worker's result is in the evidence, not in
                // its words, which may still say the run is pending.
                let recorded: String = result["result"]["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry["check"].as_str())
                    .filter(|check| check.ends_with(super::RECORDED_BY_HOST))
                    .filter_map(|check| check.strip_prefix("wsx task "))
                    .map(|check| format!("{check}; "))
                    .collect();
                let mut summary = match checks.len() {
                    0 => format!("{recorded}{worker}"),
                    total => format!("checks {passed} of {total} passed; {recorded}{worker}"),
                };
                // ^ `waits on <on>` is the record's key; the sentence is the words every view
                // shows (docs/tasks.md "Status").
                // ^ An idle worker whose task waits for another has no wait of its own: the
                // `waits for #m to land` below says it.
                if let Some(waiting) = record
                    .waiting
                    .as_ref()
                    .filter(|_| !wsx_core::task::status::owner_waits_after(&dir, &record))
                {
                    let status = wsx_core::task::status::status(
                        &dir,
                        &record,
                        &Default::default(),
                        wsx_core::task::now_unix_ms(),
                    );
                    summary = format!(
                        "waits on {}: {}; {summary}",
                        waiting.on.as_str(),
                        status.sentence
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
                // ^ A stopped task's worker never hears of its check-run: the manager does.
                if wsx_core::task::status::answered_after_stop(&dir, &record) {
                    summary.push_str(
                        "; its check-run ended after the stop: the answer joins the worker's next turn, which a `wsx task add` starts",
                    );
                }
                // ^ A ready task that waits for another is not for the manager to land yet.
                if let Some(after) = record.after.as_ref().filter(|_| record.state.is_open()) {
                    summary.push_str(&format!("; waits for #{} to land", after.task));
                }
                // ^ The worker was sent back once and still did not run the simplify skill.
                // ref: docs/tasks.md "Workflow"
                if record.state == TaskState::Ready && record.simplify_missing() {
                    summary.push_str(&format!("; {}", wsx_core::task::SIMPLIFY_MISSING));
                }
                // ^ An item that names an image file it has no capture of: the owner never sees it.
                if record.state == TaskState::Ready {
                    let result: wsx_core::task::WorkerResult =
                        serde_json::from_value(result["result"].clone()).unwrap_or_default();
                    let unattached = result.unattached_images();
                    if !unattached.is_empty() {
                        summary.push_str(&format!("; {unattached}"));
                    }
                }
                // ^ The worker's memo reaches the manager when its task leaves, never later:
                // the ledger lists only the last few. ref: docs/tasks.md "Memo"
                if !record.state.is_open() {
                    let memo = result["result"]["memo"].as_str().unwrap_or_default();
                    let memo = memo.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !memo.is_empty() {
                        let memo = clip(&memo, ledger::MEMO_CHARS);
                        self.fold_memo(&record, &dir, &memo);
                        summary.push_str(&format!("; memo: {memo}"));
                    }
                }
                // ^ A landing adds its Human Verify items to the project's To check list.
                if record.state == TaskState::Landed {
                    let open = checks::open_items(&self.project_id).map_or(0, |items| {
                        items.iter().filter(|item| item.task == record.id).count()
                    });
                    if open > 0 {
                        summary.push_str(&format!("; {open} to check"));
                    }
                }
                summary
            };
            let state_word = wsx_core::task::status::state_word(&record);
            let line = format!(
                "#{} {} r{}: {}; {}",
                record.id,
                state_word,
                record.revision,
                clip(&title, EVENT_TITLE_CHARS),
                summary,
            );
            // ^ The chat says `done` for a landing (docs/web-client-tasks.md "Project screen").
            let words = match record.state {
                TaskState::Landed => "done",
                _ => state_word,
            };
            let counts = (!checks.is_empty()).then_some([passed as u32, checks.len() as u32]);
            let mut short = Short::task(record.id, words).with_state(state_word, counts);
            short.wakes = state_wakes(&dir, &record);
            self.task_event(&record, &open_chats, line, short);
            let state = record.state.as_str();
            self.fire(
                &format!("task.{state}"),
                format!("{}-r{}-{state}", record.id, record.revision),
                json!({ "task": record.id, "title": title, "revision": record.revision, "state": state }),
            );
        }
    }

    /// Start the project's routines whose trigger is this event kind (docs/tasks.md
    /// "Routines"). Only a project with such a routine asks the scheduler, so the others send
    /// no request; a scheduler that does not answer drops the event, as a busy routine does.
    fn fire(&self, kind: &str, event_id: String, payload: serde_json::Value) {
        let wanted = stored_routines(&self.main).routines.iter().any(|routine| {
            routine.enabled
                && matches!(&routine.trigger, Trigger::Event { kind: wanted } if wanted == kind)
        });
        if wanted {
            let action = wsx_core::routine::Action::Fire {
                kind: kind.into(),
                payload,
                event_id,
            };
            let client = wsx_core::runtime::Client::local();
            let _ = wsx_core::routine::request(&client, &self.main, action);
        }
    }

    /// Add a finished task's memo to the manager's notes: a record (a landed task's fact holds
    /// its landed commit and the paths of its diff), then `notes/context.md` from the records.
    /// ref: docs/tasks.md "Memo", docs/memory.md "Capture"
    fn fold_memo(&self, record: &TaskRecord, dir: &wsx_core::task::store::TaskDir, memo: &str) {
        let task = record.id;
        let landed = wsx_core::memory::manage::landed_commit(dir, record);
        let landed = landed
            .as_deref()
            .map(|commit| (commit, record.paths.as_slice()));
        let done = memory_store(&self.project_id, &self.project, &self.main)
            .and_then(|store| notes::record_memo(&store, None, task, memo, landed))
            .map(|drift| self.say_drift(&drift));
        // ^ The manager that ran the task from another project keeps the fact too, in its own
        // scope. ref: docs/memory.md "Capture"
        let done = done.and_then(|()| match self.origin_repo(record) {
            Some(origin) => self.record_memo_at(&origin, task, memo, landed),
            None => Ok(()),
        });
        if let Err(error) = done {
            self.say(&format!("cannot add the memo: {error:#}"));
        }
    }

    /// The repo of the chat that created the task, when it is not this one: `chat.json` names it.
    fn origin_repo(&self, record: &TaskRecord) -> Option<String> {
        let chat = ManagerDir::existing(record.created_by.chat()?).ok()??;
        (chat.project_id != self.project_id).then_some(chat.project_id)
    }

    /// The memo of this project's task, as a record in the scope of `origin`, the repo of the
    /// chat that created it. Until the memory scopes of M4 (docs/design/orchestration-model.md).
    fn record_memo_at(
        &self,
        origin: &str,
        task: u32,
        memo: &str,
        landed: Option<(&str, &[String])>,
    ) -> anyhow::Result<()> {
        let (name, main) = super::project_by_id(origin)?;
        let store =
            wsx_core::memory::Store::project(&store::project_id(&name, &main), &name, &main)?;
        notes::record_memo(&store, Some(&self.project), task, memo, landed)?;
        Ok(())
    }

    fn say_drift(&self, drift: &[wsx_core::memory::manage::Drift]) {
        for d in drift {
            self.say(&d.notice(true));
        }
    }

    /// Queue a task's event line. Only the task's owner hears of it: the open chat that created
    /// it (`chats::owner`), else this repo's home chat, which is this manager.
    /// ref: docs/tasks.md "Chats"
    fn task_event(
        &mut self,
        record: &TaskRecord,
        open_chats: &[String],
        line: String,
        short: Short,
    ) {
        let owner = chats::owner(record.created_by.chat(), open_chats, "");
        self.emit(owner, (line, short));
    }

    /// Turn each new stall of an open task into one event line, after it lasted
    /// `stall::GRACE`. ref: docs/tasks.md "Manager" (Stalls)
    fn watch_stalls(&mut self) {
        if self
            .stalls_swept
            .is_some_and(|at| at.elapsed() < stall::sweep())
        {
            return;
        }
        self.stalls_swept = Some(Instant::now());
        let Ok(tasks) = project_tasks(&self.project_id) else {
            return;
        };
        let open_chats = chats::open_names();
        let now = now_unix_ms();
        let mut open = Vec::new();
        let mut suspects = BTreeMap::new();
        let mut reminders = BTreeSet::new();
        let states: BTreeMap<u32, TaskState> = tasks
            .iter()
            .filter_map(|(_, record)| record.as_ref().ok())
            .map(|record| (record.id, record.state))
            .collect();
        for (dir, record) in tasks {
            let Ok(record) = record else { continue };
            if !record.state.is_open() {
                continue;
            }
            open.push(record.id);
            if let Some(reminder) = stall::owner_wait(&dir, &record, now) {
                reminders.insert(self.remind_owner(&record, reminder));
            }
            if let Some(after) = &record.after {
                let state = states.get(&after.task).copied();
                if self.end_after(&dir, &record, state, &open_chats) {
                    continue;
                }
            }
            let Some(found) = stall::find(&dir, &record, now) else {
                continue;
            };
            let since = match self.stall_suspects.get(&record.id) {
                Some((marker, since)) if *marker == found.marker => *since,
                _ => Instant::now(),
            };
            suspects.insert(record.id, (found.marker.clone(), since));
            if since.elapsed() < stall::grace()
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
            let mut short = Short::task(record.id, "stalled");
            short.wakes = stall_wakes(&found);
            self.observed_stalls.insert(record.id, found.marker);
            self.task_event(&record, &open_chats, line, short);
        }
        self.stall_suspects = suspects;
        self.owner_reminded.retain(|key| reminders.contains(key));
        // ^ Saved with the next event's `seen`.
        self.observed_stalls.retain(|id, _| open.contains(id));
    }

    /// Push the owner's reminder once per key (task, wait, step): through the home chat as an
    /// event with no manager turn, off the loop (the post may wait for a turn that runs). The
    /// key is also in `post-keys.json`, so a restart or a deploy sends none twice.
    /// Returns the key. ref: docs/tasks.md "Manager" (Stalls)
    fn remind_owner(&mut self, record: &TaskRecord, reminder: stall::OwnerReminder) -> String {
        let key = format!(
            "owner.{}.{}.{}",
            record.id, reminder.since_unix_ms, reminder.step
        );
        if self.owner_reminded.insert(key.clone()) {
            let (project, project_id, main) = (
                self.project.clone(),
                self.project_id.clone(),
                self.main.clone(),
            );
            let line = format!("{project} #{} {}", record.id, reminder.words);
            let (events, sent) = (self.events.clone(), key.clone());
            std::thread::spawn(move || {
                let posted = store::post_once(
                    &project_id,
                    &sent,
                    now_unix_ms() / 1000,
                    OWNER_REMINDER_WINDOW_SECS,
                    || {
                        super::cmd::routine::post_when_free(
                            &main,
                            &project,
                            "waits on you",
                            Some(Level::Warn),
                            || vec![line.clone()],
                        )
                    },
                );
                if posted.is_err() {
                    let _ = events.send(Event::Unreminded(sent));
                }
            });
        }
        key
    }

    /// End the wait of `wsx task after` when its task `state` is landed (the task gets the
    /// text as an add), or closed or unknown (the manager hears of it once). A host that
    /// does not answer keeps the wait; the next sweep asks again.
    /// ref: docs/tasks.md "After another task"
    fn end_after(
        &mut self,
        dir: &store::TaskDir,
        record: &TaskRecord,
        state: Option<TaskState>,
        open_chats: &[String],
    ) -> bool {
        let Some(after) = &record.after else {
            return false;
        };
        let text = match state {
            Some(TaskState::Landed) => Some(after.text.clone()),
            Some(state) if state.is_open() => return false,
            _ => None,
        };
        let request = HostRequest::AfterEnd {
            task: after.task,
            text: text.clone(),
        };
        match call_host(&dir.socket_path(), &request, owner::HOST_TIMEOUT) {
            Ok(response) if response.ok => {}
            _ => return false,
        }
        if text.is_none() {
            let line = format!(
                "#{} wait over: #{} {}; {}",
                record.id,
                after.task,
                match state {
                    Some(_) => "was closed without landing",
                    None => "is not a task",
                },
                // ^ A task made with `new --after` has no text: its first turn starts instead.
                match after.text.is_empty() {
                    true => format!("#{} started without it", record.id),
                    false => format!("nothing was added to #{}", record.id),
                }
            );
            let short = Short::task(record.id, "wait over");
            self.task_event(record, open_chats, line, short);
        }
        true
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
        let to_check = checks::read(&self.project_id)
            .unwrap_or_default()
            .iter()
            .any(|item| item.unix_ms > since);
        heard || asked || to_check
    }

    /// The spend since `since` from the logs' `cost_micro_usd`: the manager's replies and the
    /// workers' results, the costliest tasks named. ref: docs/tasks.md "Recap"
    fn cost_since(&self, log: &[ManagerEntry], since: u64) -> String {
        let manager = usage::Usage::since(&usage::manager_log_turns(log), 0);
        let mut tasks: Vec<(u64, u32, u64)> = project_tasks(&self.project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(dir, record)| {
                let spent = usage::Usage::since(&usage::task_turns(&dir), since + 1);
                let id = record.ok()?.id;
                (spent.turns > 0).then_some((spent.cost_micro_usd, id, spent.turns))
            })
            .collect();
        tasks.sort_by(|left, right| right.cmp(left));
        let usd = |micro: u64| format!("{:.2} USD", micro as f64 / 1e6);
        let workers: u64 = tasks.iter().map(|(cost, ..)| cost).sum();
        let turns: u64 = tasks.iter().map(|(.., turns)| turns).sum();
        let top: Vec<String> = tasks
            .iter()
            .take(3)
            .map(|(cost, id, _)| format!("#{id} {}", usd(*cost)))
            .collect();
        format!(
            "Token cost since the last recap: {} in all; you {} in {} turn(s), workers {} in {turns} turn(s){}.",
            usd(manager.cost_micro_usd + workers),
            usd(manager.cost_micro_usd),
            manager.turns,
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
            store::TaskDir::open(&self.project_id, id)
                .and_then(|dir| dir.read_record())
                .is_ok_and(|record| record.state.is_open())
        });
        let notes = newest_ms(&self.main.join("knowledge/sessions").join(&self.project));
        let recent = self
            .state
            .idle_ms
            .is_some_and(|ms| now_unix_ms().saturating_sub(ms) < IDLE_EVERY_MS);
        if recent
            || prompt.is_empty()
            || open
            || notes.is_none_or(|ms| ms <= self.state.idle_ms.unwrap_or(0))
        {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return self.say("cannot find the wsx binary for the idle task");
        };
        let (project, home, events, began) = (
            self.project.clone(),
            self.home(),
            self.events.clone(),
            now_unix_ms(),
        );
        std::thread::spawn(move || {
            let _ = events.send(Event::IdleStarted(
                began,
                new_task(&exe, &project, &home, &prompt),
            ));
        });
    }
}

/// Give chat `to` `lines` as one message from `author`, starting its host when it is not
/// running. A chat of another repo hears the lines with this repo's name and no task chip,
/// which would link into that repo's tasks. True when the chat took the message.
fn say_chat(
    own: &str,
    project: &str,
    exe: &Path,
    to: &str,
    mut lines: Vec<(String, Short)>,
    author: Author,
) -> bool {
    let sent = ManagerDir::existing(to)
        .ok()
        .flatten()
        .context("the chat is gone")
        .and_then(|dir| {
            let (name, main) = super::project_by_id(&dir.project_id)?;
            if dir.project_id != own {
                for (line, short) in &mut lines {
                    short.text = format!("{project} {}", short.text);
                    short.task = None;
                    *line = format!("{project} {line}");
                }
            }
            let text = lines
                .iter()
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let short = match author {
                Author::Events => lines.iter().map(|(_, short)| short.clone()).collect(),
                _ => Vec::new(),
            };
            owner::say(
                &name,
                &main,
                exe,
                Some(to),
                super::launch_manager(&main),
                text,
                Some(author),
                short,
                None,
            )
        });
    sent.is_ok_and(|response| response.ok)
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

/// Whether a run wrote nothing to stdout; an unreadable file counts as output, so that the
/// manager still hears of the run.
fn stdout_empty(run: &RunRecord) -> bool {
    std::fs::metadata(&run.stdout_path).is_ok_and(|file| file.len() == 0)
}

/// A routine run that ended as an event line with the end of its output: what a routine that
/// prints wants the manager to read. ref: docs/tasks.md "Routines"
fn routine_line(run: &RunRecord) -> String {
    format!("{}{}", run.result_line(), output_ends(&run.final_output))
}

/// The most of a run's output the owner event holds: its end.
const EVENT_OUTPUT_CHARS: usize = 1500;
/// The most files an owner event links.
const EVENT_LINKS: usize = 3;
/// The second clause of an owner event line (`routine_event`), which `routine_for_owner` reads.
pub(super) const FOR_OWNER: &str = "for the owner";

/// A routine run's event for the owner, on one line, since the chat shows an event line only to
/// its first `; `: `routine nightly succeeded after 4 s; for the owner; output ends: <the last
/// 1,500 characters, newlines as ⏎>; files: [board.html](files/<path>) …`, a link to each file of
/// the main checkout the output names, which the web opens in its files page
/// (docs/web-client.md "Files"). ref: docs/tasks.md "Routines"
fn routine_event(run: &RunRecord, main: &Path) -> String {
    let mut line = format!("{}; {FOR_OWNER}", run.result_line());
    let output = run.final_output.trim();
    if !output.is_empty() {
        let skip = output.chars().count().saturating_sub(EVENT_OUTPUT_CHARS);
        let cut = if skip > 0 { "…" } else { "" };
        let end: String = output.chars().skip(skip).collect();
        line.push_str(&format!(
            "; output ends: {cut}{}",
            end.lines().collect::<Vec<_>>().join(" ⏎ ")
        ));
    }
    let links: Vec<String> = named_files(output, main)
        .iter()
        .map(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            format!("[{name}](files/{})", link_path(path))
        })
        .collect();
    if !links.is_empty() {
        line.push_str(&format!("; files: {}", links.join(" ")));
    }
    line
}

/// The files of the main checkout that `output` names, as paths relative to it: a word with a
/// `/` or a `.`, relative to the checkout or absolute inside it, never under `.git`.
fn named_files(output: &str, main: &Path) -> Vec<String> {
    let root = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
    let mut found: Vec<String> = Vec::new();
    let words = output.split(|c: char| c.is_whitespace() || "\"'`()[]<>,;=".contains(c));
    for word in words {
        let word = word.trim_end_matches(['.', ':']);
        if word.len() > 512 || !word.contains(['/', '.']) {
            continue;
        }
        let path = Path::new(word);
        let relative = match path.is_absolute() {
            true => match path
                .strip_prefix(&root)
                .or_else(|_| path.strip_prefix(main))
            {
                Ok(relative) => relative.to_path_buf(),
                Err(_) => continue,
            },
            false => path.to_path_buf(),
        };
        let normal = relative
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"));
        let Some(text) = relative.to_str().filter(|_| normal) else {
            continue;
        };
        if found.iter().any(|known| known == text)
            || !std::fs::symlink_metadata(root.join(&relative)).is_ok_and(|meta| meta.is_file())
        {
            continue;
        }
        found.push(text.to_string());
        if found.len() == EVENT_LINKS {
            break;
        }
    }
    found
}

/// A relative path as a link's address: each byte outside `A-Z a-z 0-9 - . _ ~ /` as `%XX`.
fn link_path(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
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

/// `wsx task new -p <project> --title <IDLE_TITLE> -` as the home chat; the new task's number.
fn new_task(exe: &Path, project: &str, home: &str, prompt: &str) -> Result<u32, String> {
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
        .env(ACTOR_ENV, String::from(Actor::Chat(home.into())))
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

/// The project's routines and runs as stored, without asking the scheduler; none when
/// unreadable.
pub(super) fn stored_routines(main: &Path) -> routines::ProjectRoutines {
    routines::canonical_project(main)
        .and_then(|project| routines::load(&routines::root(), &project))
        .unwrap_or_default()
}

/// The project's routine runs that ended, as wsxd's scheduler recorded them.
pub(super) fn finished_routine_runs(main: &Path) -> Vec<RunRecord> {
    stored_routines(main)
        .runs
        .into_values()
        .flatten()
        .filter(|run| run.finished_epoch.is_some())
        .collect()
}

/// The project's memory store, which holds the manager's notes as records.
pub(super) fn memory_store(
    project_id: &str,
    project: &str,
    main: &Path,
) -> anyhow::Result<wsx_core::memory::Store> {
    Ok(wsx_core::memory::Store::project(project_id, project, main)?)
}
