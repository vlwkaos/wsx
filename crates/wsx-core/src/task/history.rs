//! A task's history from its log: each event in words (`line`, which the search digest also
//! indexes), the log without agent progress (`timeline`, which the web sheet shows as it is),
//! the dated lines of one task (`read`, `wsx task search --task`) and where the task ended up
//! (`outcome`, on every task hit of a search). ref: docs/tasks.md "Search"

use super::search::{day_of, first_line};
use super::store::TaskDir;
use super::{view, LogEntry, LogEvent, MessageKind, TaskRecord, TaskState};
use anyhow::Result;
use serde::Serialize;

/// A run of agent progress lines, folded into one row.
#[derive(Debug, Serialize)]
pub struct Steps<'a> {
    kind: &'static str,
    pub count: u64,
    pub last: &'a str,
    pub unix_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Row<'a> {
    Steps(Steps<'a>),
    Event(&'a LogEntry),
}

/// The newest rows of a log and how many older ones were left out.
#[derive(Debug, Serialize)]
pub struct Timeline<'a> {
    pub entries: Vec<Row<'a>>,
    pub skipped: usize,
}

/// The log without agent progress: each run of progress lines becomes one `Steps` row with its
/// count and last line. At most the newest `cap` rows. ref: docs/web-client-tasks.md "Task sheet"
pub fn timeline(log: &[LogEntry], cap: usize) -> Timeline<'_> {
    let mut rows: Vec<Row> = Vec::new();
    for entry in log {
        if let LogEvent::Agent { text, .. } = &entry.event {
            match rows.last_mut() {
                Some(Row::Steps(last)) => {
                    last.count += 1;
                    last.last = text;
                    last.unix_ms = entry.unix_ms;
                }
                _ => rows.push(Row::Steps(Steps {
                    kind: "steps",
                    count: 1,
                    last: text,
                    unix_ms: entry.unix_ms,
                })),
            }
            continue;
        }
        rows.push(Row::Event(entry));
    }
    let skipped = rows.len().saturating_sub(cap);
    Timeline {
        entries: rows.split_off(skipped),
        skipped,
    }
}

/// An event in words, with its label; none for the events that are no step of the task's story
/// (progress, waits, pings, previews). The one wording of the digest and of `read`.
pub fn line(event: &LogEvent) -> Option<(&'static str, String)> {
    let short = |commit: &str| commit.get(..7).unwrap_or(commit).to_string();
    Some(match event {
        LogEvent::Created { base, branch } => ("start", format!("branch {branch} from {base}")),
        LogEvent::Message { mode, text } => (
            match mode {
                MessageKind::Prompt => "prompt",
                MessageKind::Add => "add",
                MessageKind::Steer => "steer",
            },
            text.clone(),
        ),
        LogEvent::Result {
            status, summary, ..
        } => (
            "result",
            match status.as_str() {
                "" | "done" => summary.clone(),
                other => format!("{other}: {summary}"),
            },
        ),
        LogEvent::Revision {
            revision, commit, ..
        } => ("revision", format!("r{revision} {}", short(commit))),
        LogEvent::Checks { results } if !results.is_empty() => {
            let failed: Vec<&str> = results
                .iter()
                .filter(|check| check.timed_out || check.exit != Some(0))
                .map(|check| check.command.as_str())
                .collect();
            let text = match failed.is_empty() {
                true => format!("{} checks passed", results.len()),
                false => format!("{} failed: {}", failed.len(), failed.join(", ")),
            };
            ("checks", text)
        }
        LogEvent::Landed { commit } => ("landed", short(commit)),
        LogEvent::Closed { discarded } => (
            "closed",
            match discarded {
                true => "discarded".into(),
                false => "kept".into(),
            },
        ),
        LogEvent::Stopped { reason } => ("stopped", format!("stopped: {reason}")),
        LogEvent::LandStopped { reason } => ("stopped", format!("land stopped: {reason}")),
        _ => return None,
    })
}

/// One dated line of a task's history.
#[derive(Debug, Serialize)]
pub struct Entry {
    pub unix_ms: u64,
    pub label: &'static str,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct History {
    pub task: u32,
    pub title: String,
    pub state: &'static str,
    pub outcome: String,
    /// Older lines left out of `entries`.
    pub skipped: usize,
    pub entries: Vec<Entry>,
}

/// The newest `limit` lines of the task's story, oldest first: its start, every add and steer,
/// each turn's result, its revisions, checks, stops and its end. A run of progress is one line.
pub fn read(dir: &TaskDir, limit: usize) -> Result<History> {
    let record = dir.read_record()?;
    let log = dir.read_log()?;
    let mut entries: Vec<Entry> = timeline(&log, usize::MAX)
        .entries
        .into_iter()
        .filter_map(|row| match row {
            Row::Steps(steps) => Some(Entry {
                unix_ms: steps.unix_ms,
                label: "steps",
                text: format!("{} steps, last: {}", steps.count, first_line(steps.last)),
            }),
            Row::Event(entry) => line(&entry.event).map(|(label, text)| Entry {
                unix_ms: entry.unix_ms,
                label,
                text: first_line(&text),
            }),
        })
        .collect();
    let skipped = entries.len().saturating_sub(limit);
    entries.drain(..skipped);
    Ok(History {
        task: record.id,
        title: view::title(dir, &record).unwrap_or_default(),
        state: record.state.as_str(),
        outcome: outcome(dir, &record),
        skipped,
        entries,
    })
}

/// Where the task is in a few words: "landed 11-07 cd67238", "closed: discarded", or an open
/// task's state and revision ("ready r2"). Not stored anywhere: it is read when asked.
pub fn outcome(dir: &TaskDir, record: &TaskRecord) -> String {
    match record.state {
        TaskState::Landed => dir
            .last_entry(|entry| match &entry.event {
                LogEvent::Landed { commit } => Some((entry.unix_ms, commit.clone())),
                _ => None,
            })
            .ok()
            .flatten()
            .map_or_else(
                || "landed".into(),
                |(at, commit)| {
                    let commit = commit.get(..7).unwrap_or(&commit);
                    format!("landed {} {commit}", &day_of(at)[5..])
                },
            ),
        TaskState::Closed => match dir
            .last_event(|event| match event {
                LogEvent::Closed { discarded } => Some(*discarded),
                _ => None,
            })
            .ok()
            .flatten()
        {
            Some(true) => "closed: discarded".into(),
            _ => "closed".into(),
        },
        state if record.revision > 0 => format!("{} r{}", state.as_str(), record.revision),
        state => state.as_str().into(),
    }
}

/// `YYYY-MM-DD hh:mm` (UTC) of a unix time in ms.
pub fn stamp(unix_ms: u64) -> String {
    let minutes = unix_ms / 60_000 % 1440;
    format!(
        "{} {:02}:{:02}",
        day_of(unix_ms),
        minutes / 60,
        minutes % 60
    )
}
