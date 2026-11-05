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
    /// Of `turns`, the keep-warm pings (docs/tasks.md "Context rotation").
    pub pings: u64,
}

impl Usage {
    fn add(&mut self, other: &Usage) {
        self.cost_micro_usd += other.cost_micro_usd;
        self.input_tokens += other.input_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.output_tokens += other.output_tokens;
        self.turns += other.turns;
        self.pings += other.pings;
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
    pub fn since(turns: &[Turn], since: u64) -> Usage {
        let mut sum = Usage::default();
        for turn in turns.iter().filter(|turn| turn.unix_ms >= since) {
            sum.add(&turn.usage);
        }
        sum
    }
}

/// What a project's spend is split into per day: the usual workflow steps, any other worker
/// turn, and the manager. The web client gives each one fixed color. ref: docs/tasks.md "Usage"
const PARTS: [&str; 7] = [
    "build", "plan", "context", "review", "memo", "other", "manager",
];
const PART_OTHER: usize = 5;
const PART_MANAGER: usize = 6;

/// One model turn: when, of which part, and what it cost.
#[derive(Debug, Clone, Copy)]
pub struct Turn {
    pub unix_ms: u64,
    part: usize,
    pub usage: Usage,
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
fn turns<'a>(lines: impl Iterator<Item = (u64, usize, Option<&'a TurnUsage>, bool)>) -> Vec<Turn> {
    let mut running = 0;
    lines
        .map(|(unix_ms, part, usage, ping)| {
            let legacy = usage.is_some_and(|usage| usage.session_cost_micro_usd == 0);
            let usage = usage.cloned().unwrap_or_default();
            let mut cost = usage.cost_micro_usd;
            if legacy {
                cost = cost.checked_sub(running).unwrap_or(cost);
                running = usage.cost_micro_usd;
            }
            Turn {
                unix_ms,
                part,
                usage: Usage {
                    cost_micro_usd: cost,
                    input_tokens: usage.input_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_creation_tokens: usage.cache_creation_tokens,
                    output_tokens: usage.output_tokens,
                    turns: 1,
                    pings: u64::from(ping),
                },
            }
        })
        .collect()
}

type Stamp = Vec<Option<(u64, SystemTime)>>;
type Turns = Arc<Vec<Turn>>;

/// ^ Each log's turns, read again only when one of its files changed size or mtime: the web
/// server and the TUI ask for them on every poll.
static CACHE: Mutex<Option<HashMap<PathBuf, (Stamp, Turns)>>> = Mutex::new(None);

fn cached(files: &[PathBuf], read: impl FnOnce() -> Vec<Turn>) -> Turns {
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

/// The worker turns of a task: its log's `result` and `ping` lines.
pub fn task_turns(dir: &TaskDir) -> Turns {
    cached(&[dir.file(store::LOG_FILE)], || {
        let log = dir.read_log().unwrap_or_default();
        turns(log.iter().filter_map(|entry| match &entry.event {
            LogEvent::Result { usage, step, .. } => {
                let part = step
                    .as_deref()
                    .and_then(|step| PARTS[..PART_OTHER].iter().position(|part| *part == step))
                    .unwrap_or(PART_OTHER);
                Some((entry.unix_ms, part, usage.as_ref(), false))
            }
            // ^ A keep-warm ping is no step's work. ref: docs/tasks.md "Context rotation"
            LogEvent::Ping { usage } => Some((entry.unix_ms, PART_OTHER, usage.as_ref(), true)),
            _ => None,
        }))
    })
}

/// A task's whole usage.
pub fn task(dir: &TaskDir) -> Usage {
    Usage::since(&task_turns(dir), 0)
}

/// The manager's turns: its replies and pings in both log files.
pub fn manager_turns(dir: &ManagerDir) -> Turns {
    cached(&dir.log_files(), || {
        manager_log_turns(&dir.read_log().unwrap_or_default())
    })
}

pub fn manager_log_turns(log: &[ManagerEntry]) -> Vec<Turn> {
    turns(log.iter().filter_map(|entry| match &entry.event {
        ManagerEvent::Reply { usage, .. } => {
            Some((entry.unix_ms, PART_MANAGER, usage.as_ref(), false))
        }
        ManagerEvent::Ping { usage } => Some((entry.unix_ms, PART_MANAGER, usage.as_ref(), true)),
        _ => None,
    }))
}

/// How many days the project's usage covers, today included.
const DAYS: usize = 7;

/// Workers and manager apart, over one window.
#[derive(Debug, Clone, Copy, Default, Serialize)]
struct Window {
    workers: Usage,
    manager: Usage,
}

/// One day: its cost per `PARTS` and its token use.
#[derive(Debug, Clone, Copy, Default)]
struct Day {
    day_ms: u64,
    costs: [u64; PARTS.len()],
    usage: Usage,
}

/// What `project` and `all` sum: the windows and the days, newest first.
#[derive(Debug, Clone, Default)]
struct Spend {
    today: Window,
    week: Window,
    days: Vec<Day>,
}

impl Spend {
    fn add(&mut self, other: &Spend) {
        for (mine, theirs) in [
            (&mut self.today, &other.today),
            (&mut self.week, &other.week),
        ] {
            mine.workers.add(&theirs.workers);
            mine.manager.add(&theirs.manager);
        }
        self.days.resize(other.days.len(), Day::default());
        for (mine, theirs) in self.days.iter_mut().zip(&other.days) {
            mine.day_ms = theirs.day_ms;
            for (cost, other) in mine.costs.iter_mut().zip(theirs.costs) {
                *cost += other;
            }
            mine.usage.add(&theirs.usage);
        }
    }

    fn cost(window: &Window) -> u64 {
        window.workers.cost_micro_usd + window.manager.cost_micro_usd
    }

    fn json(&self) -> Value {
        let days: Vec<Value> = self
            .days
            .iter()
            .map(|day| {
                let parts: serde_json::Map<String, Value> = PARTS
                    .iter()
                    .zip(day.costs)
                    .filter(|(_, cost)| *cost > 0)
                    .map(|(part, cost)| (part.to_string(), json!(cost)))
                    .collect();
                json!({ "day_ms": day.day_ms, "parts": parts, "usage": day.usage })
            })
            .collect();
        json!({ "today": self.today, "week": self.week, "days": days })
    }
}

/// The project's spend: see `Spend`. A task not updated in the 7 days had no turn in them and
/// is not read.
fn spend(project_id: &str, now_ms: u64) -> Spend {
    let today = local_midnight_ms(now_ms);
    let mut starts = vec![today];
    for _ in 1..DAYS {
        starts.push(local_midnight_ms(
            starts[starts.len() - 1].saturating_sub(1),
        ));
    }
    let week = starts[DAYS - 1];
    let mut sum = Spend {
        days: starts
            .iter()
            .map(|day_ms| Day {
                day_ms: *day_ms,
                ..Day::default()
            })
            .collect(),
        ..Spend::default()
    };
    let add_days = |days: &mut [Day], turns: &[Turn]| {
        for turn in turns.iter().filter(|turn| turn.unix_ms >= week) {
            if let Some(day) = starts.iter().position(|start| turn.unix_ms >= *start) {
                days[day].costs[turn.part] += turn.usage.cost_micro_usd;
                days[day].usage.add(&turn.usage);
            }
        }
    };
    for (dir, record) in project_tasks(project_id).unwrap_or_default() {
        if record.is_ok_and(|record| record.updated_unix_ms >= week) {
            let turns = task_turns(&dir);
            add_days(&mut sum.days, &turns);
            sum.today.workers.add(&Usage::since(&turns, today));
            sum.week.workers.add(&Usage::since(&turns, week));
        }
    }
    let manager = ManagerDir::existing(project_id)
        .ok()
        .flatten()
        .map(|dir| manager_turns(&dir))
        .unwrap_or_default();
    add_days(&mut sum.days, &manager);
    sum.today.manager = Usage::since(&manager, today);
    sum.week.manager = Usage::since(&manager, week);
    sum
}

/// The project's spend today (since local midnight) and in the last 7 days from a local
/// midnight, workers and manager apart, and per day (newest first) by `PARTS`.
pub fn project(project_id: &str, now_ms: u64) -> Value {
    spend(project_id, now_ms).json()
}

/// `project` summed over `projects` (name, task store id), plus `projects`: each one's
/// cost today and in 7 days, the dearest 7 days first. A project that spent nothing in 7 days
/// has no row. ref: docs/web-client-tasks.md "All projects"
pub fn all(projects: &[(String, String)], now_ms: u64) -> Value {
    let mut sum = Spend::default();
    let mut rows: Vec<(String, u64, u64)> = Vec::new();
    for (name, store) in projects {
        let each = spend(store, now_ms);
        sum.add(&each);
        let week = Spend::cost(&each.week);
        if week > 0 {
            rows.push((name.clone(), Spend::cost(&each.today), week));
        }
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    let mut value = sum.json();
    value["projects"] = rows
        .into_iter()
        .map(|(name, today, week)| json!({ "name": name, "today_micro_usd": today, "week_micro_usd": week }))
        .collect();
    value
}

/// The start of `now_ms`'s day in this machine's zone; 24 hours back when that cannot be told.
pub fn local_midnight_ms(now_ms: u64) -> u64 {
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
