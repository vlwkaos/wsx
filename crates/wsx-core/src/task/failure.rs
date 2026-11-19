//! A check that fails the same way in two tasks of a project fails on main, not in the tasks. ref: docs/tasks.md "Checks, revisions and report" ("Shared failure")

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
/// The most of a section's label that the key and the shown line keep: the label says where, the
/// error says why.
const LABEL_CHARS: usize = 60;
/// The summaries that follow every failure and tell nothing of its cause: cargo's. Lower case.
const GENERIC: [&str; 3] = ["could not compile", "previous error", "test run failed"];

/// One command that fails with one line in `tasks` (ascending, this task among them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedFailure {
    pub command: String,
    /// The line with digits as `0`: the compare and the key.
    pub line: String,
    /// The newest task's line as printed (clipped): the event and the worker sentence.
    pub shown: String,
    pub tasks: Vec<u32>,
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
        let outside = self
            .outside
            .then(|| "outside this task's changed paths".to_string());
        let place: Vec<String> = (!others.is_empty())
            .then(|| format!("in {}", others.join(", ")))
            .into_iter()
            .chain(outside)
            .collect();
        format!(
            "`{}`: fails {}{}: `{}`; not yours.",
            clip(&self.command),
            place.join(" and "),
            if self.outside && others.is_empty() {
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

/// `FAIL` or `FAILED` as a whole word, as nextest and pytest print it: not the `FAIL` in
/// `CACHE_ON_FAILURE: false`.
fn has_fail_word(line: &str) -> bool {
    line.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "FAIL" || word == "FAILED")
}

/// The last line of a failed result's own output that tells its cause, clipped, as printed and
/// with digits as `0`: a `FAIL`/`FAILED`/`AssertionError` line, else an `error` line that is no
/// generic summary.
fn failure_line(result: &CheckResult) -> Option<(String, String)> {
    if result.exit == Some(0) || result.timed_out || result.output.contains(NO_SPACE) {
        return None;
    }
    line_of(&result.output)
}

fn line_of(output: &str) -> Option<(String, String)> {
    let lines = || output.lines().rev().map(str::trim);
    let line = lines()
        .find(|line| has_fail_word(line) || line.contains("AssertionError"))
        .or_else(|| {
            lines().find(|line| {
                let lower = line.to_lowercase();
                lower.contains("error") && !GENERIC.iter().any(|g| lower.contains(g))
            })
        })?;
    Some(match section_line(line) {
        Some((label, place, error)) => {
            let error: String = error.chars().take(LINE_CHARS).collect();
            let cut: String = label.chars().take(LABEL_CHARS).collect();
            let shown_label = if cut.len() < label.len() {
                format!("{cut}…")
            } else {
                cut.clone()
            };
            (
                format!("FAILED section {shown_label} at {place}: {error}"),
                zeros(&format!("{cut} | {error}")),
            )
        }
        None => {
            let shown: String = line.chars().take(LINE_CHARS).collect();
            let key = zeros(&shown);
            (shown, key)
        }
    })
}

/// `s` with each run of digits as one `0`.
fn zeros(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match (c.is_ascii_digit(), out.ends_with('0')) {
            (true, true) => {}
            (true, false) => out.push('0'),
            _ => out.push(c),
        }
    }
    out
}

/// A `FAILED section <label> at <file>:<n>[ (in <fn>, <file>:<n>)]: <error>` line of
/// scripts/sections.py as (label, where, error). The label is the section's first sentence and
/// can be long: the cause is the error.
fn section_line(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix("FAILED section ")?;
    rest.match_indices(" at ").find_map(|(i, mark)| {
        let after = &rest[i + mark.len()..];
        let colon = after.find(':')?;
        let digits = after[colon + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        if digits == 0 || after[..colon].contains(char::is_whitespace) {
            return None;
        }
        let number = colon + 1 + digits;
        let tail = &after[number..];
        let end = if tail.starts_with(": ") {
            number
        } else if tail.starts_with(" (in ") {
            number + tail.find("): ")? + 1
        } else {
            return None;
        };
        Some((&rest[..i], &after[..end], &after[end + 2..]))
    })
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
    let mut found: BTreeMap<(String, String), (String, Vec<u32>)> = own
        .into_iter()
        .filter_map(|(command, line)| {
            let (shown, line) = line?;
            Some(((command, line), (shown, vec![task])))
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
            if let Some((_, tasks)) = line.and_then(|(_, line)| found.get_mut(&(command, line))) {
                tasks.push(other);
            }
        }
    }
    found
        .into_iter()
        .filter(|(_, (_, tasks))| tasks.len() > 1)
        .map(|((command, line), (shown, mut tasks))| {
            tasks.sort_unstable();
            SharedFailure {
                command,
                line,
                shown,
                tasks,
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
        outside: true,
    })
}

#[cfg(test)]
mod tests {
    use super::line_of;

    const LABEL: &str = "FAILED section task_journey post levels: a warn or bad `wsx task post` from a LaunchAgent-like environment (";

    fn key(line: &str) -> String {
        line_of(line).unwrap().1
    }

    #[test]
    fn section_lines_with_different_causes_do_not_match() {
        let a = format!("{LABEL} at test-web-client.py:4919: AssertionError: FAIL: 390 px: the reply was not sent with its quote; page says: ui");
        let b = format!("{LABEL} at test-web-client.py:4968: AssertionError: FAIL: 390 px: Jump to latest did not return to the end; page says: ui");
        assert_ne!(key(&a), key(&b));
        let (shown, _) = line_of(&b).unwrap();
        assert!(shown.contains("Jump to latest did not return"), "{shown}");
        assert!(shown.contains("…"), "{shown}");
    }

    #[test]
    fn same_cause_on_another_line_and_size_matches() {
        let a = format!("{LABEL} at test-web-client.py:4919: AssertionError: FAIL: 390 px: the reply was not sent with its quote; page says: ui");
        let b = format!("{LABEL} at test-web-client.py:4950 (in reply, helpers.py:12): AssertionError: FAIL: 1280 px: the reply was not sent with its quote; page says: ui");
        assert_eq!(key(&a), key(&b));
    }

    #[test]
    fn other_lines_keep_their_key() {
        assert_eq!(
            key("FAIL [ 12.345s] wsx-core task::x::y"),
            "FAIL [ 0.0s] wsx-core task::x::y"
        );
        let (shown, key) = line_of("FAILED section broken at nowhere").unwrap();
        assert_eq!(shown, "FAILED section broken at nowhere");
        assert_eq!(key, "FAILED section broken at nowhere");
    }
}
