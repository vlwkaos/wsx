//! Token use and cost summed from the task and manager logs' `usage`, for `task show`, the
//! ledger, the TUI and the web client. ref: docs/tasks.md "Usage"

use super::manager::{ManagerEntry, ManagerEvent};
use super::store::{self, ManagerDir, TaskDir};
use super::view::project_tasks;
use super::{LogEvent, TurnUsage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const DAY_MS: u64 = 24 * 3_600_000;

/// What some turns cost: the sum of their `usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub cost_micro_usd: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub turns: u64,
}

impl Usage {
    fn add(&mut self, other: &Usage) {
        self.cost_micro_usd += other.cost_micro_usd;
        self.input_tokens += other.input_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.output_tokens += other.output_tokens;
        self.turns += other.turns;
    }

    /// "$3.42 · in 12k · cache 1.2M read / 80k write · out 9k · 14 turns".
    pub fn line(&self) -> String {
        format!(
            "{} · in {} · cache {} read / {} write · out {} · {} turn{}",
            usd(self.cost_micro_usd),
            tokens(self.input_tokens),
            tokens(self.cache_read_tokens),
            tokens(self.cache_creation_tokens),
            tokens(self.output_tokens),
            self.turns,
            if self.turns == 1 { "" } else { "s" }
        )
    }

    /// The turns of `turns` at or after `since` (unix ms).
    pub fn since(turns: &[(u64, Usage)], since: u64) -> Usage {
        let mut sum = Usage::default();
        for (_, usage) in turns.iter().filter(|(unix_ms, _)| *unix_ms >= since) {
            sum.add(usage);
        }
        sum
    }
}

/// "$3.42".
pub fn usd(micro: u64) -> String {
    format!("${:.2}", micro as f64 / 1e6)
}

/// "950", "12k", "1.2M".
pub fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

/// Each turn with its time. A line written before `session_cost_micro_usd` existed (commit
/// 94f0b26) has it 0 and `cost_micro_usd` holding the session's running total, so its
/// turn's part is the difference from the previous such line, as `scripts/usage-report.py`
/// counts it.
fn turns<'a>(lines: impl Iterator<Item = (u64, Option<&'a TurnUsage>)>) -> Vec<(u64, Usage)> {
    let mut running = 0;
    lines
        .map(|(unix_ms, usage)| {
            let legacy = usage.is_some_and(|usage| usage.session_cost_micro_usd == 0);
            let usage = usage.cloned().unwrap_or_default();
            let mut cost = usage.cost_micro_usd;
            if legacy {
                cost = cost.checked_sub(running).unwrap_or(cost);
                running = usage.cost_micro_usd;
            }
            let turn = Usage {
                cost_micro_usd: cost,
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_creation_tokens: usage.cache_creation_tokens,
                output_tokens: usage.output_tokens,
                turns: 1,
            };
            (unix_ms, turn)
        })
        .collect()
}

type Stamp = Vec<Option<(u64, SystemTime)>>;
type Turns = Arc<Vec<(u64, Usage)>>;

/// ^ Each log's turns, read again only when one of its files changed size or mtime: the web
/// server and the TUI ask for them on every poll.
static CACHE: Mutex<Option<HashMap<PathBuf, (Stamp, Turns)>>> = Mutex::new(None);

fn cached(files: &[PathBuf], read: impl FnOnce() -> Vec<(u64, Usage)>) -> Turns {
    let stamp: Stamp = files
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            Some((meta.len(), meta.modified().ok()?))
        })
        .collect();
    let key = files[files.len() - 1].clone();
    if let Some((old, turns)) = CACHE.lock().unwrap().get_or_insert_default().get(&key) {
        if *old == stamp {
            return turns.clone();
        }
    }
    let turns = Arc::new(read());
    CACHE
        .lock()
        .unwrap()
        .get_or_insert_default()
        .insert(key, (stamp, turns.clone()));
    turns
}

/// The worker turns of a task: its log's `result` lines.
pub fn task_turns(dir: &TaskDir) -> Turns {
    cached(&[dir.file(store::LOG_FILE)], || {
        let log = dir.read_log().unwrap_or_default();
        turns(log.iter().filter_map(|entry| match &entry.event {
            LogEvent::Result { usage, .. } => Some((entry.unix_ms, usage.as_ref())),
            _ => None,
        }))
    })
}

/// A task's whole usage.
pub fn task(dir: &TaskDir) -> Usage {
    Usage::since(&task_turns(dir), 0)
}

/// The manager's turns: its replies in both log files.
pub fn manager_turns(dir: &ManagerDir) -> Turns {
    cached(&dir.log_files(), || {
        manager_log_turns(&dir.read_log().unwrap_or_default())
    })
}

pub fn manager_log_turns(log: &[ManagerEntry]) -> Vec<(u64, Usage)> {
    turns(log.iter().filter_map(|entry| match &entry.event {
        ManagerEvent::Reply { usage, .. } => Some((entry.unix_ms, usage.as_ref())),
        _ => None,
    }))
}

/// The project's spend today (since local midnight) and in the last 7 days, workers and
/// manager apart. A task not updated in the 7 days had no turn in them and is not read.
pub fn project(project_id: &str, now_ms: u64) -> Value {
    let today = local_midnight_ms(now_ms);
    let week = now_ms.saturating_sub(7 * DAY_MS);
    let (mut workers_today, mut workers_week) = (Usage::default(), Usage::default());
    for (dir, record) in project_tasks(project_id).unwrap_or_default() {
        if record.is_ok_and(|record| record.updated_unix_ms >= week) {
            let turns = task_turns(&dir);
            workers_today.add(&Usage::since(&turns, today));
            workers_week.add(&Usage::since(&turns, week));
        }
    }
    let manager = ManagerDir::existing(project_id)
        .ok()
        .flatten()
        .map(|dir| manager_turns(&dir))
        .unwrap_or_default();
    json!({
        "today": { "workers": workers_today, "manager": Usage::since(&manager, today) },
        "week": { "workers": workers_week, "manager": Usage::since(&manager, week) },
    })
}

/// The start of `now_ms`'s day in this machine's zone; 24 hours back when that cannot be told.
fn local_midnight_ms(now_ms: u64) -> u64 {
    let fallback = now_ms.saturating_sub(DAY_MS);
    let Ok(secs) = libc::time_t::try_from(now_ms / 1000) else {
        return fallback;
    };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for the call; localtime_r is thread-safe.
    if unsafe { libc::localtime_r(&secs, tm.as_mut_ptr()) }.is_null() {
        return fallback;
    }
    // SAFETY: localtime_r filled it.
    let mut tm = unsafe { tm.assume_init() };
    (tm.tm_hour, tm.tm_min, tm.tm_sec) = (0, 0, 0);
    tm.tm_isdst = -1;
    // SAFETY: tm is a valid, filled struct.
    let midnight = unsafe { libc::mktime(&mut tm) };
    u64::try_from(midnight).map_or(fallback, |secs| secs * 1000)
}
