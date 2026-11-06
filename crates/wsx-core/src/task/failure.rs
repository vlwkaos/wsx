//! A check that fails the same way in two tasks of a project fails on main, not in the tasks.
//! ref: docs/tasks.md "Checks, revisions and report" ("Shared failure")

use super::store::TaskDir;
use super::{CheckResult, LogEvent};
use std::collections::BTreeMap;

/// A check whose output says this ran out of disk: the machine's fault, never a shared failure.
pub const NO_SPACE: &str = "No space left on device";

/// Only tasks with a log entry this young count.
const WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
/// The most tasks looked at (the newest) and the most of each log read.
const MAX_TASKS: usize = 40;
const LOG_TAIL_BYTES: u64 = 128 * 1024;
const LINE_CHARS: usize = 160;
/// The cargo summaries that follow every failure and tell nothing of its cause.
const GENERIC: [&str; 3] = ["could not compile", "previous error", "test run failed"];

/// One command that fails with one line in `tasks` (ascending, this task among them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFailure {
    pub command: String,
    pub line: String,
    pub tasks: Vec<u32>,
}

impl SharedFailure {
    /// The key of a post once per failure.
    pub fn key(&self) -> String {
        format!("check:{}\n{}", clip(&self.command), self.line)
    }

    /// The event line for the manager.
    pub fn event(&self) -> String {
        format!(
            "checks fail the same way in {}, likely on main: `{}` ends with `{}`; one fix task, \
             not one per task",
            self.tasks(),
            clip(&self.command),
            self.line
        )
    }

    /// The sentence for the worker whose check-run just failed.
    pub fn sentence(&self, own: u32) -> String {
        let others: Vec<String> = self
            .tasks
            .iter()
            .filter(|task| **task != own)
            .map(|task| format!("#{task}"))
            .collect();
        format!(
            "`{}`: the same failure is in {}; it is likely on main, not your change.",
            clip(&self.command),
            others.join(", ")
        )
    }

    fn tasks(&self) -> String {
        let all: Vec<String> = self.tasks.iter().map(|task| format!("#{task}")).collect();
        all.join(", ")
    }
}

/// The command on one line, cut.
fn clip(command: &str) -> String {
    let one: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(120).collect()
}

/// The last line of a failed result's output that tells its cause, digits as `0`: a
/// `FAIL`/`AssertionError` line, else an `error` line that is no generic summary.
fn failure_line(result: &CheckResult) -> Option<String> {
    if result.exit == Some(0) || result.timed_out || result.output.contains(NO_SPACE) {
        return None;
    }
    let lines = || result.output.lines().rev().map(str::trim);
    let line = lines()
        .find(|line| line.contains("FAIL") || line.contains("AssertionError"))
        .or_else(|| {
            lines().find(|line| {
                line.to_lowercase().contains("error") && !GENERIC.iter().any(|g| line.contains(g))
            })
        })?;
    let mut out = String::new();
    for c in line.chars().take(LINE_CHARS) {
        match (c.is_ascii_digit(), out.ends_with('0')) {
            (true, true) => {}
            (true, false) => out.push('0'),
            _ => out.push(c),
        }
    }
    Some(out)
}

/// What the latest result of each command of `results` says: its failure line, or none.
fn lines_of<'a>(
    results: impl IntoIterator<Item = &'a CheckResult>,
) -> BTreeMap<String, Option<String>> {
    results
        .into_iter()
        .map(|result| (result.command.clone(), failure_line(result)))
        .collect()
}

/// The failures of `results` (task `task`'s newest) that the latest check results of the
/// project's other tasks, logged in the last 24 h, also have: bounded reads of the newest
/// tasks' log ends. Those with the same command and line in two or more tasks, this one included.
pub fn shared_failures(
    project_id: &str,
    task: u32,
    results: &[CheckResult],
    now_ms: u64,
) -> Vec<SharedFailure> {
    let own = lines_of(results);
    if own.values().all(Option::is_none) {
        return Vec::new();
    }
    let mut found: BTreeMap<(String, String), Vec<u32>> = own
        .into_iter()
        .filter_map(|(command, line)| Some(((command, line?), vec![task])))
        .collect();
    let numbers = super::store::task_numbers(project_id).unwrap_or_default();
    for other in numbers
        .into_iter()
        .rev()
        .filter(|n| *n != task)
        .take(MAX_TASKS)
    {
        let Ok(entries) =
            TaskDir::open(project_id, other).and_then(|dir| dir.read_log_tail(LOG_TAIL_BYTES))
        else {
            continue;
        };
        let since = now_ms.saturating_sub(WINDOW_MS);
        let latest = lines_of(
            entries
                .iter()
                .filter(|entry| entry.unix_ms >= since)
                .flat_map(|entry| match &entry.event {
                    LogEvent::CheckRun { results } | LogEvent::Checks { results } => {
                        results.as_slice()
                    }
                    _ => &[],
                }),
        );
        for (command, line) in latest {
            if let Some(tasks) = line.and_then(|line| found.get_mut(&(command, line))) {
                tasks.push(other);
            }
        }
    }
    found
        .into_iter()
        .filter(|(_, tasks)| tasks.len() > 1)
        .map(|((command, line), mut tasks)| {
            tasks.sort_unstable();
            SharedFailure {
                command,
                line,
                tasks,
            }
        })
        .collect()
}
