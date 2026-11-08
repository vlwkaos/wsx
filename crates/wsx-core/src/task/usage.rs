//! Token use and cost summed from the task and manager logs' `usage`, for `task show`, the
//! ledger, the TUI and the web client. ref: docs/tasks.md "Usage"

use super::manager::{ManagerEntry, ManagerEvent};
use super::store::{self, ManagerDir, TaskDir};
use super::view::project_tasks;
use super::{LogEvent, TurnUsage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

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

/// One model turn: when, of which part, on which agent, and what it cost.
#[derive(Debug, Clone)]
pub struct Turn {
    pub unix_ms: u64,
    part: usize,
    /// `adapter/model`, or `unknown` for a turn whose adapter no record names.
    agent: Arc<str>,
    pub usage: Usage,
}

/// Who ran a turn: a task's worker or the project's manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Who {
    Workers,
    Manager,
}

/// The turns a spend's days and total count: since this machine's midnight, the 7 days, or
/// since the start of Claude's current limit window of that kind (`limits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Today,
    Week,
    FiveHour,
    SevenDay,
}

/// Which turns a spend counts (docs/web-client-tasks.md "Usage"). The default counts all of
/// the 7 days.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    pub period: Option<Period>,
    pub who: Option<Who>,
    /// An adapter (`claude`) or one model of it (`claude/claude-opus-5-5`).
    pub agent: Option<String>,
}

impl Filter {
    /// A turn of this agent and of these hands; the period is `start`'s.
    fn takes(&self, turn: &Turn) -> bool {
        let who = match self.who {
            Some(Who::Workers) => turn.part != PART_MANAGER,
            Some(Who::Manager) => turn.part == PART_MANAGER,
            None => true,
        };
        who && self.agent.as_deref().is_none_or(|agent| {
            *turn.agent == *agent
                || turn
                    .agent
                    .strip_prefix(agent)
                    .is_some_and(|model| model.starts_with('/'))
        })
    }
}

/// `adapter/model` of a turn: its own adapter, else `adapter` (its task's, from task.json).
/// A turn with neither is `unknown`; one with no model is `<adapter>/unknown`.
fn agent_of(usage: Option<&TurnUsage>, adapter: &str) -> Arc<str> {
    let own = usage.map_or("", |usage| usage.adapter.as_str());
    let adapter = if own.is_empty() { adapter } else { own };
    let model = usage.map_or("", |usage| usage.model.as_str());
    match (adapter, model) {
        ("", _) => "unknown".into(),
        (adapter, "") => format!("{adapter}/unknown").into(),
        (adapter, model) => format!("{adapter}/{model}").into(),
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
fn turns<'a>(
    lines: impl Iterator<Item = (u64, usize, Option<&'a TurnUsage>, bool)>,
    adapter: &str,
) -> Vec<Turn> {
    let mut running = 0;
    lines
        .map(|(unix_ms, part, usage, ping)| {
            let agent = agent_of(usage, adapter);
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
                agent,
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
/// What one read of a log gives: its turns, and whether it holds an `agent_switch`.
type Logged = (Turns, bool);

/// A cached log not asked for in this long is dropped at the next read of a log.
const CACHE_IDLE: std::time::Duration = std::time::Duration::from_secs(600);

struct Entry {
    stamp: Stamp,
    logged: Logged,
    used: Instant,
}

/// ^ Each log's turns, read again only when one of its files changed size or mtime: the web
/// server and the TUI ask for them on every poll. A log that nobody asked for in
/// `CACHE_IDLE` (a task or project that is gone, a project the view left) leaves it, so a
/// long session over many projects does not keep every log it ever saw.
static CACHE: Mutex<Option<HashMap<PathBuf, Entry>>> = Mutex::new(None);

/// The cache, also after a panic while it was held: an entry is whole or absent.
fn cache() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, Entry>>> {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cached(files: &[PathBuf], read: impl FnOnce() -> (Vec<Turn>, bool)) -> Logged {
    let stamp: Stamp = files
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            Some((meta.len(), meta.modified().ok()?))
        })
        .collect();
    let key = files[files.len() - 1].clone();
    if let Some(entry) = cache().get_or_insert_default().get_mut(&key) {
        if entry.stamp == stamp {
            entry.used = Instant::now();
            return entry.logged.clone();
        }
    }
    let (turns, switched) = read();
    let logged = (Arc::new(turns), switched);
    let mut cache = cache();
    let map = cache.get_or_insert_default();
    map.retain(|_, entry| entry.used.elapsed() < CACHE_IDLE);
    map.insert(
        key,
        Entry {
            stamp,
            logged: logged.clone(),
            used: Instant::now(),
        },
    );
    logged
}

/// The worker turns of a task: its log's `result` and `ping` lines. A line from before
/// `adapter` counts under the task's adapter. ref: docs/tasks.md "Usage"
pub fn task_turns(dir: &TaskDir) -> Turns {
    task_log(dir).0
}

/// True once the task's log has an `agent_switch`: it runs on a fallback agent. Read with the
/// turns, so the whole log is parsed once per change. ref: docs/tasks.md "Agents"
pub fn task_switched(dir: &TaskDir) -> bool {
    task_log(dir).1
}

fn task_log(dir: &TaskDir) -> Logged {
    cached(&[dir.file(store::LOG_FILE)], || {
        let log = dir.read_log().unwrap_or_default();
        let adapter = dir
            .read_record()
            .map(|record| record.adapter)
            .unwrap_or_default();
        let switched = log
            .iter()
            .any(|entry| matches!(entry.event, LogEvent::AgentSwitch { .. }));
        let lines = log.iter().filter_map(|entry| match &entry.event {
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
        });
        (turns(lines, &adapter), switched)
    })
}

/// A task's whole usage.
pub fn task(dir: &TaskDir) -> Usage {
    Usage::since(&task_turns(dir), 0)
}

/// The manager's turns: its replies and pings in both log files. A line from before `adapter`
/// is `unknown`: no record tells which agent the manager ran then.
pub fn manager_turns(dir: &ManagerDir) -> Turns {
    cached(&dir.log_files(), || {
        (
            manager_log_turns(&dir.read_log().unwrap_or_default()),
            false,
        )
    })
    .0
}

pub fn manager_log_turns(log: &[ManagerEntry]) -> Vec<Turn> {
    let lines = log.iter().filter_map(|entry| match &entry.event {
        ManagerEvent::Reply { usage, .. } => {
            Some((entry.unix_ms, PART_MANAGER, usage.as_ref(), false))
        }
        ManagerEvent::Ping { usage } => Some((entry.unix_ms, PART_MANAGER, usage.as_ref(), true)),
        _ => None,
    });
    turns(lines, "")
}

/// How many days the project's usage covers, today included.
const DAYS: usize = 7;
/// The earliest a period reaches back: a 7-day limit window can start before the 7 days'
/// first midnight.
const SINCE_MAX_MS: u64 = 8 * DAY_MS;

/// The local midnight that starts the 7 days, today included.
fn week_start(now_ms: u64) -> u64 {
    let mut week = local_midnight_ms(now_ms);
    for _ in 1..DAYS {
        week = local_midnight_ms(week.saturating_sub(1));
    }
    week
}

/// When `filter`'s period starts. A limit window not in `limits` has reset and its next one
/// starts with Claude's next turn, so it holds none yet.
fn start(filter: &Filter, now_ms: u64, limits: &Value) -> u64 {
    let window = |kind: &str| {
        limits
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|limit| limit["windows"].as_array().into_iter().flatten())
            .find(|window| window["kind"] == kind)
            .and_then(|window| window["start_ms"].as_u64())
            .unwrap_or(now_ms)
    };
    let since = match filter.period {
        None | Some(Period::Week) => week_start(now_ms),
        Some(Period::Today) => local_midnight_ms(now_ms),
        Some(Period::FiveHour) => window("five_hour"),
        Some(Period::SevenDay) => window("seven_day"),
    };
    since.max(now_ms.saturating_sub(SINCE_MAX_MS))
}

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

/// What `project` and `all` sum: the windows, the days from the filter's start (newest
/// first), the filter's whole slice, and the agents of the 7 days.
#[derive(Debug, Clone, Default)]
struct Spend {
    today: Window,
    week: Window,
    days: Vec<Day>,
    total: Usage,
    agents: BTreeSet<String>,
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
        self.total.add(&other.total);
        self.agents.extend(other.agents.iter().cloned());
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

/// The project's spend: see `Spend`. The windows follow the filter's agent and hands; the
/// days and the total also `since`, the period's `start`. A task not updated since then had
/// no turn in them and is not read.
fn spend(project_id: &str, now_ms: u64, filter: &Filter, since: u64) -> Spend {
    let today = local_midnight_ms(now_ms);
    let week = week_start(now_ms);
    let mut starts = vec![today];
    while starts[starts.len() - 1] > since {
        starts.push(local_midnight_ms(
            starts[starts.len() - 1].saturating_sub(1),
        ));
    }
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
    let add = |sum: &mut Spend, turns: &[Turn]| {
        let (mut on_today, mut on_week) = (Usage::default(), Usage::default());
        for turn in turns {
            if turn.unix_ms >= week && !sum.agents.contains(&*turn.agent) {
                if let Some((adapter, _)) = turn.agent.split_once('/') {
                    sum.agents.insert(adapter.to_string());
                }
                sum.agents.insert(turn.agent.to_string());
            }
            if !filter.takes(turn) {
                continue;
            }
            if turn.unix_ms >= today {
                on_today.add(&turn.usage);
            }
            if turn.unix_ms >= week {
                on_week.add(&turn.usage);
            }
            if turn.unix_ms >= since {
                if let Some(day) = starts.iter().position(|start| turn.unix_ms >= *start) {
                    sum.days[day].costs[turn.part] += turn.usage.cost_micro_usd;
                    sum.days[day].usage.add(&turn.usage);
                }
                sum.total.add(&turn.usage);
            }
        }
        (on_today, on_week)
    };
    for (dir, record) in project_tasks(project_id).unwrap_or_default() {
        if record.is_ok_and(|record| record.updated_unix_ms >= since.min(week)) {
            let (today, week) = add(&mut sum, &task_turns(&dir));
            sum.today.workers.add(&today);
            sum.week.workers.add(&week);
        }
    }
    let manager = ManagerDir::existing(project_id)
        .ok()
        .flatten()
        .map(|dir| manager_turns(&dir))
        .unwrap_or_default();
    (sum.today.manager, sum.week.manager) = add(&mut sum, &manager);
    sum
}

/// The project's spend today (since local midnight) and in the last 7 days from a local
/// midnight, workers and manager apart, and per day (newest first) by `PARTS`.
pub fn project(project_id: &str, now_ms: u64) -> Value {
    spend(project_id, now_ms, &Filter::default(), week_start(now_ms)).json()
}

/// `project`'s spend of `filter` summed over `projects` (name, task store id), plus `total`
/// (the filter's slice), `agents` (each adapter and `adapter/model` of the 7 days, for the
/// agent choice), `projects` (each one's cost in the slice, the dearest first; one that spent
/// nothing has no row) and `limits` (see `limits`). ref: docs/web-client-tasks.md "Usage"
pub fn all(projects: &[(String, String)], now_ms: u64, filter: &Filter) -> Value {
    let mut sum = Spend::default();
    let mut rows: Vec<(String, u64)> = Vec::new();
    let limits = limits(now_ms);
    let since = start(filter, now_ms, &limits);
    for (name, store) in projects {
        let each = spend(store, now_ms, filter, since);
        sum.add(&each);
        if each.total.cost_micro_usd > 0 {
            rows.push((name.clone(), each.total.cost_micro_usd));
        }
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut value = sum.json();
    value["total"] = json!(sum.total);
    value["agents"] = json!(sum.agents);
    value["projects"] = rows
        .into_iter()
        .map(|(name, cost)| json!({ "name": name, "cost_micro_usd": cost }))
        .collect();
    value["limits"] = limits;
    value
}

/// How long each of Claude's limit windows lasts, by the kind `rate_limit_info` names.
const WINDOWS: [(&str, u64); 2] = [("five_hour", 5 * 3_600_000), ("seven_day", 7 * DAY_MS)];

/// Claude's newest limit state (`store::rate_limit` of the `claude` account; Pi reports none):
/// `[{agent, as_of_ms, status, windows}]`, each window `{kind, status, utilization,
/// resets_ms, start_ms}` that has not reset yet. `utilization` (0 to 1) and `start_ms`
/// (`resets_ms` less the window's length) only when known. Empty with no record or none left.
/// ref: docs/tasks.md "Usage"
pub fn limits(now_ms: u64) -> Value {
    let Some(record) = store::rate_limit("claude", now_ms) else {
        return json!([]);
    };
    let info = &record["info"];
    let window = |kind: &str, each: &Value| {
        let resets_ms = each["resetsAt"].as_u64()?.checked_mul(1000)?;
        (resets_ms > now_ms).then(|| {
            let length = WINDOWS.iter().find(|(name, _)| *name == kind);
            json!({
                "kind": kind,
                "status": each["status"].as_str().unwrap_or(""),
                "utilization": each["utilization"].as_f64(),
                "resets_ms": resets_ms,
                "start_ms": length.map(|(_, length)| resets_ms.saturating_sub(*length)),
            })
        })
    };
    // ^ Claude Code 2.1.316 names each window in `unifiedWindows`; an older line names one.
    let windows: Vec<Value> = match info["unifiedWindows"].as_object() {
        Some(each) => each
            .iter()
            .filter_map(|(kind, each)| window(kind, each))
            .collect(),
        None => info["rateLimitType"]
            .as_str()
            .and_then(|kind| window(kind, info))
            .into_iter()
            .collect(),
    };
    if windows.is_empty() {
        return json!([]);
    }
    json!([{
        "agent": "claude",
        "as_of_ms": record["unix_ms"],
        "status": info["status"].as_str().unwrap_or(""),
        "windows": windows,
    }])
}

/// The start of `now_ms`'s day in this machine's zone; 24 hours back when that cannot be told.
pub fn local_midnight_ms(now_ms: u64) -> u64 {
    let fallback = now_ms.saturating_sub(DAY_MS);
    let Some(mut tm) = i64::try_from(now_ms / 1000)
        .ok()
        .and_then(crate::routine::local::local_tm)
    else {
        return fallback;
    };
    (tm.tm_hour, tm.tm_min, tm.tm_sec) = (0, 0, 0);
    tm.tm_isdst = -1;
    // SAFETY: tm is a valid, filled struct.
    let midnight = unsafe { libc::mktime(&mut tm) };
    u64::try_from(midnight).map_or(fallback, |secs| secs * 1000)
}
