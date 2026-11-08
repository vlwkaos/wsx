//! A task's status in words: a short label for a row or a pill, and a sentence that says what
//! the task waits for, why, how long, and what comes next. The web client, the TUI, `wsx task`
//! and the manager's events all show these words; none makes its own. Decisions never read
//! them: they test the record. ref: docs/tasks.md "Status"

use super::manager::ApprovalLine;
use super::store::TaskDir;
use super::view::{check_outcomes, clip, read_result, resolving_conflict};
use super::{AfterKind, LogEvent, TaskRecord, TaskState, WaitOn, Waiting};
use crate::config::project::load_task_config;
use serde::Serialize;
use serde_json::Value;

const FACT_CHARS: usize = 240;
/// The reason the host gives a stop the owner asked for. ref: crates/wsx/src/task/host.rs `stop`
pub(crate) const STOPPED_BY_REQUEST: &str = "stopped by request";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// Which row of the table in docs/tasks.md "Status" matched: "scenario_slot", "review",
    /// "owner". A client picks its tone or icon by it, never by the words.
    pub kind: &'static str,
    /// "Waiting for the scenario test slot", "Ready, waiting for the manager's review".
    pub label: String,
    /// The label, how long, the facts and what comes next: "Waiting for the scenario test
    /// slot for 4 min: #233 runs its scenario tests now, 2 more tasks ahead. Its scenario
    /// tests start when the slot is free."
    pub sentence: String,
    /// When the wait the sentence counts began: a page that shows the sentence reads it again
    /// as minutes pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_unix_ms: Option<u64>,
    /// What an open task that waits for another task (`wsx task after`) adds to its row, in few
    /// words (`kind_words`): "its checks fail".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<String>,
    /// The open tasks that wait for this one (`Facts::waiters`); the sentence names them too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waited_by: Vec<Waiter>,
}

/// An open task that waits for another task to land, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Waiter {
    pub task: u32,
    pub because: AfterKind,
}

/// The open tasks of `records` that wait for task `id`, by number.
pub fn waiters<'a>(records: impl IntoIterator<Item = &'a TaskRecord>, id: u32) -> Vec<Waiter> {
    let mut found: Vec<Waiter> = records
        .into_iter()
        .filter(|record| record.state.is_open())
        .filter_map(|record| {
            let after = record.after.as_ref().filter(|after| after.task == id)?;
            Some(Waiter {
                task: record.id,
                because: after.because,
            })
        })
        .collect();
    found.sort_by_key(|waiter| waiter.task);
    found
}

/// Why a task waits for `other`, as a phrase: "its checks fail on main until #283 lands".
fn because_phrase(kind: AfterKind, other: u32) -> String {
    match kind {
        AfterKind::Needs => format!("it builds on #{other}'s change"),
        AfterKind::Files => format!("it changes the same files as #{other}, so it lands after it"),
        AfterKind::Check => format!("its checks fail on main until #{other} lands"),
    }
}

/// `because_phrase` as a sentence: "Its checks fail on main until #283 lands."
fn because_sentence(kind: AfterKind, other: u32) -> String {
    let phrase = because_phrase(kind, other);
    let mut chars = phrase.chars();
    let first = chars.next().map(|c| c.to_uppercase().to_string());
    format!("{}{}.", first.unwrap_or_default(), chars.as_str())
}

/// The reason of a wait in few words, for a row and for "Waited for by #279 (same files)".
pub fn kind_words(kind: AfterKind) -> &'static str {
    match kind {
        AfterKind::Needs => "builds on it",
        AfterKind::Files => "same files",
        AfterKind::Check => "its checks fail",
    }
}

/// "Waited for by #279, #282."; the kinds follow each number when they differ.
fn waited_by_sentence(waiters: &[Waiter]) -> String {
    let differ = waiters
        .iter()
        .any(|waiter| waiter.because != waiters[0].because);
    let list: Vec<String> = waiters
        .iter()
        .map(|waiter| match differ {
            true => format!("#{} ({})", waiter.task, kind_words(waiter.because)),
            false => format!("#{}", waiter.task),
        })
        .collect();
    format!("Waited for by {}.", list.join(", "))
}

/// What a caller knows beyond the task's own files.
#[derive(Default)]
pub struct Facts<'a> {
    /// The project's pending approval requests (`manager::pending_approvals`).
    pub approvals: &'a [&'a ApprovalLine],
    /// `stall::stuck`: it asks the task host, so only a view of one task passes it.
    pub stuck: Option<&'a str>,
    /// For a landed task: whether the project's last deploy has it. None when the project
    /// does not deploy or the caller does not know.
    pub deployed: Option<bool>,
    /// `waiters` of the project's tasks; none when the caller has no list of them.
    pub waiters: &'a [Waiter],
}

/// The status of one task now.
pub fn status(dir: &TaskDir, record: &TaskRecord, facts: &Facts, now_ms: u64) -> Status {
    status_with_result(dir, record, facts, None, now_ms)
}

/// `status` for a caller that has read `result.json` already: `Some(&None)` is a task
/// without one, `None` lets the status read it when it needs it.
pub fn status_with_result(
    dir: &TaskDir,
    record: &TaskRecord,
    facts: &Facts,
    result: Option<&Option<Value>>,
    now_ms: u64,
) -> Status {
    let mut status = status_of(dir, record, facts, result, now_ms);
    if let (Some(after), true) = (&record.after, record.state.is_open()) {
        // ^ A ready row already says "Waiting for #m to land"; the reason is what it adds.
        status.wait = Some(match record.state == TaskState::Ready {
            true => kind_words(after.because).to_string(),
            false => format!(
                "then waits for #{}: {}",
                after.task,
                kind_words(after.because)
            ),
        });
    }
    if !facts.waiters.is_empty() && record.state.is_open() {
        status.sentence.push(' ');
        status.sentence.push_str(&waited_by_sentence(facts.waiters));
        status.waited_by = facts.waiters.to_vec();
    }
    status
}

/// What wsx ran on the task's latest revision: the caller's read of `result.json`, else a read.
fn outcomes(dir: &TaskDir, result: Option<&Option<Value>>) -> Vec<(String, &'static str)> {
    match result {
        Some(read) => read.as_ref().map(check_outcomes),
        None => read_result(dir).map(|read| check_outcomes(&read)),
    }
    .unwrap_or_default()
}

fn status_of(
    dir: &TaskDir,
    record: &TaskRecord,
    facts: &Facts,
    result: Option<&Option<Value>>,
    now_ms: u64,
) -> Status {
    let entered = record.state_unix_ms.max(record.created_unix_ms);
    let main = &record.default_branch;
    let approval = facts.approvals.iter().find_map(|line| match line {
        ApprovalLine::Request {
            task: Some(task),
            unix_ms,
            ..
        } if *task == record.id => Some((super::owner::approval_label(line), *unix_ms)),
        _ => None,
    });
    if let (Some((label, asked)), true) = (approval, record.state.is_open()) {
        return words(
            "approval",
            "Waiting for your approval",
            Some(asked),
            &format!("the manager asks: {label}"),
            "It goes on when you approve or deny it.",
            now_ms,
        );
    }
    if let (Some(why), true) = (facts.stuck, record.state.is_open()) {
        return words(
            "stuck",
            "Stuck",
            None,
            why,
            "The manager hears of the stall and decides what to do.",
            now_ms,
        );
    }
    let mut status = match (record.state, record.waiting.as_ref()) {
        // ^ A check wait holds in any state; every other wait only in Waiting.
        (state, Some(waiting))
            if state == TaskState::Waiting
                || matches!(waiting.on, WaitOn::CheckSlot | WaitOn::CheckRun) =>
        {
            waiting_on(dir, record, waiting, entered, now_ms)
        }
        (TaskState::Checking, _) => words(
            "checking",
            "Running checks",
            Some(entered),
            &format!(
                "{} of r{} run now",
                count(record.checks.len(), "check"),
                record.revision
            ),
            "When they pass it is ready; a failed check goes back to the worker.",
            now_ms,
        ),
        (TaskState::Working, _) if resolving_conflict(dir) => words(
            "conflict",
            "Resolving a conflict",
            None,
            &format!("the worker rebases it onto {main}"),
            "It lands when its checks pass.",
            now_ms,
        ),
        (TaskState::Working | TaskState::Waiting, _) => working(record, now_ms),
        (TaskState::Ready, _) => ready(dir, record, result, entered, now_ms),
        (TaskState::Failed, _) => failed(dir, result, now_ms),
        (TaskState::Landed, _) => words(
            "landed",
            "Landed",
            None,
            &match facts.deployed {
                Some(false) => format!("on {main}; the last deploy does not have it yet"),
                _ => format!("on {main}"),
            },
            "",
            now_ms,
        ),
        (TaskState::Closed, _) => words(
            "closed",
            "Closed",
            None,
            "it ended without landing",
            "",
            now_ms,
        ),
    };
    // ^ A hold matters once it is ready; before, the sentence says it is coming.
    if let (Some(hold), true) = (&record.hold, record.state.is_open()) {
        if record.state != TaskState::Ready {
            status.sentence.push_str(&format!(
                " Held: {}.",
                clip(hold.trim_end_matches('.'), FACT_CHARS)
            ));
        }
    }
    // ^ A wait for another task matters once it is ready; before, the sentence says it follows.
    if let (Some(after), true) = (&record.after, record.state.is_open()) {
        if record.state != TaskState::Ready {
            status
                .sentence
                .push_str(&format!(" Then it waits for #{} to land.", after.task));
            status.sentence.push(' ');
            status
                .sentence
                .push_str(&because_sentence(after.because, after.task));
        }
    }
    status
}

/// A task that waits on `waiting`: the owner, its plan reviewed, the owner's hand, disk,
/// the usage limit, a slot for its checks or a check run.
fn waiting_on(
    dir: &TaskDir,
    record: &TaskRecord,
    waiting: &Waiting,
    entered: u64,
    now_ms: u64,
) -> Status {
    match waiting.on {
        WaitOn::Owner => {
            let stopped = dir
                .last_event(|event| match event {
                    LogEvent::Stopped { reason } => Some(Some(reason.clone())),
                    LogEvent::Blocked { .. } | LogEvent::Result { .. } => Some(None),
                    _ => None,
                })
                .ok()
                .flatten()
                .flatten();
            match stopped {
                Some(reason) if reason == STOPPED_BY_REQUEST => words(
                    "stopped",
                    "Stopped",
                    Some(entered),
                    STOPPED_BY_REQUEST,
                    "Tell the manager to resume or close it.",
                    now_ms,
                ),
                Some(reason) => words(
                    "interrupted",
                    "Interrupted",
                    Some(entered),
                    &reason,
                    "The manager resumes it or tells you why not.",
                    now_ms,
                ),
                None => words(
                    "owner",
                    "Waiting for your answer",
                    Some(entered),
                    &waiting.reason,
                    "Answer the manager; the worker goes on then.",
                    now_ms,
                ),
            }
        }
        WaitOn::Plan => words(
            "plan",
            "Plan ready, waiting for the manager's review",
            Some(entered),
            "the worker wrote its plan",
            "The manager answers go or what to change, then the build starts.",
            now_ms,
        ),
        WaitOn::Hand => words(
            "hand",
            "Waiting for you to hand it back",
            Some(entered),
            "you work on it in its session and no agent runs",
            "Commit, then hand it back: its checks run and it lands as usual.",
            now_ms,
        ),
        WaitOn::Disk => words(
            "disk",
            "Waiting for disk space",
            Some(entered),
            &queue_words(record, &waiting.reason, "goes on first"),
            "It goes on by itself once there is space.",
            now_ms,
        ),
        WaitOn::Limit => words(
            "limit",
            "Waiting for the usage limit",
            Some(entered),
            &queue_words(record, &waiting.reason, "goes on first"),
            "It goes on by itself then, one task at a time.",
            now_ms,
        ),
        WaitOn::CheckSlot => slot(dir, record, &waiting.reason, entered, now_ms),
        WaitOn::CheckRun => held(&waiting.reason, now_ms),
    }
}

/// A working task: the step it is on and what follows its result.
fn working(record: &TaskRecord, now_ms: u64) -> Status {
    let label = match record.steps.get(record.step as usize) {
        Some(step) => format!(
            "Working on {} ({} of {})",
            step.name,
            record.step + 1,
            record.steps.len()
        ),
        None => "Working".into(),
    };
    let next = if record.plan {
        "Its plan goes to the manager after its result."
    } else if record.checks.is_empty() {
        "It is ready after its result."
    } else {
        "Its checks run after its result."
    };
    words(
        "working",
        &label,
        None,
        "the worker runs its turn",
        next,
        now_ms,
    )
}

/// A failed task: how many of its checks failed after the worker's retries.
fn failed(dir: &TaskDir, result: Option<&Option<Value>>, now_ms: u64) -> Status {
    let outcomes = outcomes(dir, result);
    let failed = outcomes.iter().filter(|(_, o)| *o != "passed").count();
    match failed {
        0 => words(
            "failed",
            "Failed",
            None,
            "the worker could not finish",
            "Tell the manager what to do next.",
            now_ms,
        ),
        _ => words(
            "failed",
            "Failed",
            None,
            &format!(
                "{failed} of {} failed after the worker's retries",
                count(outcomes.len(), "check")
            ),
            "Tell the manager, so the worker fixes it.",
            now_ms,
        ),
    }
}

/// A ready task: its landing stopped, it is held, it waits for another task, the manager
/// reviews it, or the owner lands it.
fn ready(
    dir: &TaskDir,
    record: &TaskRecord,
    result: Option<&Option<Value>>,
    entered: u64,
    now_ms: u64,
) -> Status {
    let stopped = dir
        .last_event(|event| match event {
            LogEvent::LandStopped { reason } => Some(Some(reason.clone())),
            LogEvent::Revision { .. } => Some(None),
            _ => None,
        })
        .ok()
        .flatten()
        .flatten();
    if let Some(reason) = stopped {
        return words(
            "land_stopped",
            "Landing stopped",
            Some(entered),
            &reason,
            "Land it again when that is fixed.",
            now_ms,
        );
    }
    if let Some(hold) = &record.hold {
        return words(
            "held",
            "Held",
            Some(entered),
            hold,
            "It lands only when you say so, or after a trial you pass.",
            now_ms,
        );
    }
    if let Some(after) = &record.after {
        let other = TaskDir::open(&record.project_id, after.task)
            .and_then(|dir| dir.read_record())
            .map_or("unknown", |other| other.state.as_str());
        return words(
            "after",
            &format!("Waiting for #{} to land", after.task),
            Some(after.since_unix_ms),
            &format!("#{} is {other}", after.task),
            &format!(
                "{} When it lands, wsx sends the task the manager's next instruction. If it is closed instead, the manager is told.",
                because_sentence(after.because, after.task)
            ),
            now_ms,
        );
    }
    let outcomes = outcomes(dir, result);
    let passed = outcomes.iter().filter(|(_, o)| *o == "passed").count();
    let checks = match outcomes.len() {
        0 => "no checks prove it".to_string(),
        total => format!("checks {passed} of {total} passed"),
    };
    let land_auto = load_task_config(&record.project_path).is_ok_and(|s| s.land_auto);
    match record.manager_reviews(land_auto) {
        true => words(
            "review",
            "Ready, waiting for the manager's review",
            Some(entered),
            &checks,
            "The manager reads the diff, then lands it or asks for changes.",
            now_ms,
        ),
        false => words(
            "ready",
            "Ready, waiting for you to land it",
            Some(entered),
            &checks,
            &format!("Land it to put it on {}.", record.default_branch),
            now_ms,
        ),
    }
}

/// A worker's result that waits for its own check-run's answer (a `ci:` one up to 125 min).
fn held(reason: &str, now_ms: u64) -> Status {
    words(
        "check_run",
        "Waiting for its check-run",
        None,
        reason,
        "A failure goes back to the worker; a pass on its committed tree goes on by itself.",
        now_ms,
    )
}

/// A run that waits for the machine's check slot, in its lane, since its `check_queued`.
fn slot(dir: &TaskDir, record: &TaskRecord, reason: &str, entered: u64, now_ms: u64) -> Status {
    let since = dir
        .last_entry(|entry| match entry.event {
            LogEvent::CheckQueued { .. } => Some(entry.unix_ms),
            _ => None,
        })
        .ok()
        .flatten()
        .unwrap_or(entered);
    let scenarios = queue(reason).is_some_and(|queue| queue.what.contains("scenario"));
    let (kind, label, run, next) = match scenarios {
        true => (
            "scenario_slot",
            "Waiting for the scenario test slot",
            "runs its scenario tests now",
            "Its scenario tests start when the slot is free.",
        ),
        false => (
            "check_slot",
            "Waiting for the check slot",
            "runs its checks now",
            "Its checks start when the slot is free.",
        ),
    };
    words(
        kind,
        label,
        Some(since),
        &queue_words(record, reason, run),
        next,
        now_ms,
    )
}

/// A queue reason as "#233 runs its checks now, 2 more tasks ahead"; any other reason as is.
fn queue_words(record: &TaskRecord, reason: &str, holder_does: &str) -> String {
    let Some(queue) = queue(reason) else {
        return reason.to_string();
    };
    let holder = match task_label(queue.holder) {
        Some((project, id)) if project == record.project => format!("#{id} {holder_does}"),
        Some((project, id)) => format!("{project} #{id} {holder_does}"),
        // ^ `scripts/check-slot.py` names its command and pid.
        None => format!("{} runs now", queue.holder),
    };
    match queue.ahead {
        0 => holder,
        1 => format!("{holder}, 1 more task ahead"),
        n => format!("{holder}, {n} more tasks ahead"),
    }
}

fn count(n: usize, what: &str) -> String {
    match n {
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// "<label> for <span>: <facts>. <next>"; facts and next may be empty. `kind`: `Status::kind`.
fn words(
    kind: &'static str,
    label: &str,
    since: Option<u64>,
    facts: &str,
    next: &str,
    now_ms: u64,
) -> Status {
    let mut sentence = label.to_string();
    if let Some(since) = since {
        sentence.push_str(&format!(" for {}", span(now_ms.saturating_sub(since))));
    }
    let facts = clip(facts.trim(), FACT_CHARS);
    if !facts.is_empty() {
        sentence.push_str(": ");
        sentence.push_str(&facts);
    }
    if !sentence.ends_with(['.', '?', '!', '…']) {
        sentence.push('.');
    }
    if !next.is_empty() {
        sentence.push(' ');
        sentence.push_str(next);
    }
    Status {
        kind,
        label: label.to_string(),
        sentence,
        since_unix_ms: since,
        wait: None,
        waited_by: Vec::new(),
    }
}

/// A slot's queue reason, made by crates/wsx/src/task/slot.rs `try_take`:
/// "queued for the check slot behind demo t3 and 2 more".
pub(crate) struct Queue<'a> {
    pub what: &'a str,
    pub holder: &'a str,
    pub ahead: u32,
}

pub(crate) fn queue(reason: &str) -> Option<Queue<'_>> {
    let (what, holder) = reason.strip_prefix("queued for ")?.split_once(" behind ")?;
    let more = holder
        .rsplit_once(" and ")
        .and_then(|(holder, more)| Some((holder, more.strip_suffix(" more")?.parse().ok()?)));
    let (holder, ahead) = more.unwrap_or((holder, 0));
    Some(Queue {
        what,
        holder,
        ahead,
    })
}

/// A holder's label as the task it names: "demo t3" is ("demo", 3).
pub(crate) fn task_label(holder: &str) -> Option<(&str, u32)> {
    let (project, id) = holder.rsplit_once(" t")?;
    Some((project, id.parse().ok()?))
}

/// "45 min", "3 h 10 min", "2 d 4 h".
pub(crate) fn span(ms: u64) -> String {
    let minutes = ms / 60_000;
    match minutes {
        0 => "less than a minute".into(),
        1..60 => format!("{minutes} min"),
        60..1440 => format!("{} h {} min", minutes / 60, minutes % 60),
        _ => format!("{} d {} h", minutes / 1440, minutes % 1440 / 60),
    }
}
