//! A check that fails the same way in two tasks of a project, or as main's latest CI run does,
//! fails on main, not in the tasks. ref: docs/tasks.md "Checks, revisions and report" ("Shared failure")

use super::store::TaskDir;
use super::{CheckResult, LogEvent};
use std::collections::BTreeMap;

/// A check whose output says this ran out of disk: the machine's fault, never a shared failure.
pub const NO_SPACE: &str = "No space left on device";

/// The line `scripts/ci-scenario.py` prints before the failed log of main's latest CI run of the
/// scenario: what follows it is main's, never the task's own.
pub const MAIN_MARK: &str = "== main's latest CI run";

/// Only tasks with a log entry this young count.
const WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
/// The most tasks looked at (the newest) and the most of each log read.
const MAX_TASKS: usize = 40;
const LOG_TAIL_BYTES: u64 = 128 * 1024;
const LINE_CHARS: usize = 160;
/// The summaries that follow every failure and tell nothing of its cause: cargo's, and the line
/// GitHub Actions prints after every failed step. Lower case.
const GENERIC: [&str; 4] = [
    "could not compile",
    "previous error",
    "test run failed",
    "process completed with exit code",
];

/// One command that fails with one line in `tasks` (ascending, this task among them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFailure {
    pub command: String,
    /// The line with digits as `0`: the compare and the key.
    pub line: String,
    /// The newest task's line as printed (clipped): the event and the worker sentence.
    pub shown: String,
    pub tasks: Vec<u32>,
    /// Main's latest CI run of the scenario fails with this line too.
    pub on_main: bool,
    /// A scenario whose paths the task's change does not touch: a finding on main, not the
    /// task's failure. ref: docs/tasks.md "Scenarios"
    pub outside: bool,
}

impl SharedFailure {
    /// The key of a post once per failure.
    pub fn key(&self) -> String {
        format!("check:{}\n{}", clip(&self.command), self.line)
    }

    /// The event line for the manager.
    pub fn event(&self) -> String {
        if self.outside && self.tasks.len() < 2 {
            return format!(
                "a scenario fails in {} outside its changed paths, likely on main: `{}` ends \
                 with `{}`; one fix task",
                self.tasks(),
                clip(&self.command),
                self.shown
            );
        }
        format!(
            "checks fail the same way in {}, likely on main: `{}` ends with `{}`; one fix task, \
             not one per task",
            self.tasks(),
            clip(&self.command),
            self.shown
        )
    }

    /// The sentence for the worker whose check-run just failed: it is not the worker's to fix.
    pub fn sentence(&self, own: u32) -> String {
        let others: Vec<String> = self
            .tasks
            .iter()
            .filter(|task| **task != own)
            .map(|task| format!("#{task}"))
            .collect();
        let main = self.on_main.then(|| "on main".to_string());
        let outside = self
            .outside
            .then(|| "outside this task's changed paths".to_string());
        let place: Vec<String> = main
            .into_iter()
            .chain((!others.is_empty()).then(|| format!("in {}", others.join(", "))))
            .chain(outside)
            .collect();
        format!(
            "`{}`: fails {}{}: `{}`; not yours.",
            clip(&self.command),
            place.join(" and "),
            if self.outside && !self.on_main && others.is_empty() {
                ""
            } else {
                " too"
            },
            self.shown
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

/// A line of `gh run view --log-failed` (`<job>\t<step>\t<time> <text>`, scripts/ci-scenario.py
/// prints it as is) without the job, the step, the time and GitHub's `##[error]` mark: the same
/// assert in two runs reads the same.
fn log_text(line: &str) -> &str {
    let mut text = line.splitn(3, '\t').last().unwrap_or(line);
    if let Some((time, rest)) = text.split_once(' ') {
        let is_time = time.len() >= 20 && time.as_bytes()[4] == b'-' && time.ends_with('Z');
        if is_time {
            text = rest;
        }
    }
    text.trim().trim_start_matches("##[error]").trim_start()
}

/// `FAIL` or `FAILED` as a whole word, as nextest and pytest print it: not the `FAIL` in the
/// `CACHE_ON_FAILURE: false` that GitHub's env echo puts in every log.
fn has_fail_word(line: &str) -> bool {
    line.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "FAIL" || word == "FAILED")
}

/// The last line of a failed result's own output that tells its cause, clipped, as printed and
/// with digits as `0`: a `FAIL`/`FAILED`/`AssertionError` line, else an `error` line that is no
/// generic summary. What follows `MAIN_MARK` is main's, not the result's.
fn failure_line(result: &CheckResult) -> Option<(String, String)> {
    if result.exit == Some(0) || result.timed_out || result.output.contains(NO_SPACE) {
        return None;
    }
    line_of(result.output.split(MAIN_MARK).next().unwrap_or_default())
}

/// The line of main's latest CI run that failed the result's command, when the log has it.
fn main_line(result: &CheckResult) -> Option<(String, String)> {
    let (_, main) = result.output.split_once(MAIN_MARK)?;
    line_of(main)
}

fn line_of(output: &str) -> Option<(String, String)> {
    let lines = || output.lines().rev().map(log_text);
    let line = lines()
        .find(|line| has_fail_word(line) || line.contains("AssertionError"))
        .or_else(|| {
            lines().find(|line| {
                let lower = line.to_lowercase();
                lower.contains("error") && !GENERIC.iter().any(|g| lower.contains(g))
            })
        })?;
    let shown: String = line.chars().take(LINE_CHARS).collect();
    let mut out = String::new();
    for c in shown.chars() {
        match (c.is_ascii_digit(), out.ends_with('0')) {
            (true, true) => {}
            (true, false) => out.push('0'),
            _ => out.push(c),
        }
    }
    Some((shown, out))
}

/// What the latest result of each command of `results` says: its failure line (as printed, and
/// with digits as `0`), or none.
fn lines_of<'a>(
    results: impl IntoIterator<Item = &'a CheckResult>,
) -> BTreeMap<String, Option<(String, String)>> {
    results
        .into_iter()
        .map(|result| (result.command.clone(), failure_line(result)))
        .collect()
}

/// The failures of `results` (task `task`'s newest) that the latest check results of the
/// project's other tasks, logged in the last 24 h, also have: bounded reads of the newest
/// tasks' log ends. Those with the same command and line in two or more tasks, this one included,
/// and those main's latest CI run (the result's `MAIN_MARK` block) fails with too.
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
    let on_main = |command: &str, line: &str| {
        results
            .iter()
            .filter(|result| result.command == command)
            .any(|result| main_line(result).is_some_and(|(_, main)| main == line))
    };
    let mut found: BTreeMap<(String, String), (String, Vec<u32>, bool)> = own
        .into_iter()
        .filter_map(|(command, line)| {
            let (shown, line) = line?;
            let main = on_main(&command, &line);
            Some(((command, line), (shown, vec![task], main)))
        })
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
            if let Some((_, tasks, _)) = line.and_then(|(_, line)| found.get_mut(&(command, line)))
            {
                tasks.push(other);
            }
        }
    }
    found
        .into_iter()
        .filter(|(_, (_, tasks, on_main))| tasks.len() > 1 || *on_main)
        .map(|((command, line), (shown, mut tasks, on_main))| {
            tasks.sort_unstable();
            SharedFailure {
                command,
                line,
                shown,
                tasks,
                on_main,
                outside: false,
            }
        })
        .collect()
}

/// `result`'s failure as a finding outside task `task`'s change: its scenario's paths name no
/// changed path. None when the result passed or its output names no cause.
pub fn outside(task: u32, result: &CheckResult) -> Option<SharedFailure> {
    let (shown, line) = failure_line(result)?;
    Some(SharedFailure {
        command: result.command.clone(),
        line,
        shown,
        tasks: vec![task],
        on_main: false,
        outside: true,
    })
}
