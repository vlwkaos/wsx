//! Read-side views of a project's tasks, shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Ledger", docs/web-client-tasks.md

use super::store::{self, TaskDir, RESULT_FILE};
use super::{DeployKind, TaskRecord, TaskState, WaitOn};
use crate::model::workspace::TaskConfig;
use anyhow::Result;
use serde_json::{json, Value};

pub const MAX_RESULT_READ_BYTES: u64 = 1024 * 1024;
/// Paths shown per overlapping task in `show`.
const OVERLAP_PATHS_SHOWN: usize = 20;
const TITLE_CHARS: usize = 120;

/// Attention order (knowledge "Manager Mode Attention UX"): what needs the owner first.
pub fn attention(state: TaskState) -> u8 {
    match state {
        TaskState::Waiting => 0,
        TaskState::Failed => 1,
        TaskState::Ready => 2,
        TaskState::Checking | TaskState::Working => 3,
        TaskState::Landed | TaskState::Closed => 4,
    }
}

/// `#4 Fix the login: <what>`, the title cut at 60 characters and the line at 160.
pub fn task_line(task: u32, title: &str, what: &str) -> String {
    let title = clip(title.trim(), MAX_TITLE_CHARS);
    let line = if title.is_empty() {
        format!("#{task} {what}")
    } else {
        format!("#{task} {title}: {what}")
    };
    clip(&line, NOTICE_CHARS)
}
const NOTICE_CHARS: usize = 160;

/// The states a task is reported in: the routine digest and the web's "Since you left" line
/// list each task that entered one. `Landed` is the last.
pub const REPORTED_STATES: [TaskState; 3] =
    [TaskState::Ready, TaskState::Failed, TaskState::Landed];

/// Task states the manager hears about: the reported ones, a closed task and a task that waits
/// for anyone but the owner's hand (not a task the owner works on by hand).
pub fn is_event(record: &TaskRecord) -> bool {
    match record.state {
        TaskState::Waiting => !record.waits_on(WaitOn::Hand),
        TaskState::Closed => true,
        state => REPORTED_STATES.contains(&state),
    }
}

/// What a project's digest says since `after_ms`: each task that became ready, failed or
/// landed, in its latest state (`failed` names the first check that did not pass), and the
/// last deploy and release (docs/tasks.md "Routines").
pub fn digest_lines(project_id: &str, after_ms: u64) -> Vec<String> {
    let mut lines = Vec::new();
    for (dir, record) in project_tasks(project_id).unwrap_or_default() {
        let Ok(record) = record else { continue };
        if record.updated_unix_ms <= after_ms || !REPORTED_STATES.contains(&record.state) {
            continue;
        }
        let what = match record.state {
            TaskState::Failed => read_result(&dir)
                .and_then(|result| {
                    check_outcomes(&result)
                        .into_iter()
                        .find(|(_, outcome)| *outcome != "passed")
                        .map(|(command, outcome)| format!("failed: {command} {outcome}"))
                })
                .unwrap_or_else(|| "failed".into()),
            TaskState::Ready => "ready for review".into(),
            _ => "landed".into(),
        };
        let title = readable_title(&dir, &record).unwrap_or_default();
        lines.push(task_line(record.id, &title, &what));
    }
    let deploys = store::read_deploys(project_id).unwrap_or_default();
    for (kind, word) in [
        (DeployKind::Deploy, "deployed"),
        (DeployKind::Release, "released"),
    ] {
        if let Some(deploy) = deploys.iter().rev().find(|deploy| {
            deploy.kind == kind && deploy.exit == Some(0) && deploy.unix_ms > after_ms
        }) {
            lines.push(format!("{word} {}", short_commit(&deploy.commit)));
        }
    }
    lines
}

/// A commit's first 12 characters, as every answer and row names it.
pub fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The first line of `text`, at most `chars` characters in all: a longer line keeps
/// `chars - 1` of them, without trailing space, and an ellipsis.
pub fn clip(text: &str, chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.chars().count() <= chars {
        true => line.to_string(),
        false if chars == 0 => String::new(),
        false => {
            let cut: String = line.chars().take(chars - 1).collect();
            format!("{}…", cut.trim_end())
        }
    }
}

/// A task's `result.json`, or none when it is missing or unreadable.
pub fn read_result(dir: &TaskDir) -> Option<Value> {
    dir.read_file(RESULT_FILE, MAX_RESULT_READ_BYTES)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// What wsx ran on the task's latest revision, each check as `passed`, `failed` or
/// `timed out`: the manager states these, not the worker's words (docs/tasks.md "Ledger").
/// A task whose work resolves a landing conflict: its last conflict came after its last
/// revision. ref: docs/tasks.md "Landing"
pub fn resolving_conflict(dir: &TaskDir) -> bool {
    dir.last_event(|event| match event {
        super::LogEvent::Conflict { .. } => Some(true),
        super::LogEvent::Revision { .. } => Some(false),
        _ => None,
    })
    .ok()
    .flatten()
    .unwrap_or(false)
}

pub fn check_outcomes(result: &Value) -> Vec<(String, &'static str)> {
    result["checks"]
        .as_array()
        .map(|checks| {
            checks
                .iter()
                .map(|check| {
                    let outcome = if check["timed_out"].as_bool() == Some(true) {
                        "timed out"
                    } else if check["exit"].as_i64() == Some(0) {
                        "passed"
                    } else {
                        "failed"
                    };
                    (
                        clip(check["command"].as_str().unwrap_or_default(), 120),
                        outcome,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The longest `task new --title`. ref: docs/tasks.md "Store"
pub const MAX_TITLE_CHARS: usize = 60;
const MAX_MODEL_BYTES: usize = 256;

/// A `--title` as given, trimmed; refused when empty, too long or with control characters.
const MAX_HOLD_CHARS: usize = 240;

/// Why a task is held: one line in plain words. ref: docs/tasks.md "Hold and trial"
pub fn checked_hold(reason: &str) -> Result<String> {
    let reason = reason.trim();
    if reason.is_empty()
        || reason.chars().count() > MAX_HOLD_CHARS
        || reason.chars().any(char::is_control)
    {
        anyhow::bail!("a hold reason is one line of 1 to {MAX_HOLD_CHARS} characters");
    }
    Ok(reason.to_string())
}

/// At most this many expected path globs per task, each at most this many bytes.
pub const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: usize = 256;

/// `globs` added to `kept`: each relative to the checkout, without `..`, a valid glob.
/// ref: docs/tasks.md "Store"
pub fn checked_files(kept: &[String], globs: &[String]) -> Result<Vec<String>> {
    let mut files = kept.to_vec();
    for glob in globs {
        let glob = glob.trim().trim_start_matches("./");
        if glob.is_empty() || glob.len() > MAX_FILE_BYTES {
            anyhow::bail!("--files '{glob}' is empty or longer than {MAX_FILE_BYTES} bytes");
        }
        if glob.starts_with('/') || glob.split('/').any(|part| part == "..") {
            anyhow::bail!("--files '{glob}' is not relative to the checkout or has '..'");
        }
        if glob.chars().any(char::is_control) {
            anyhow::bail!("--files '{glob}' has control characters");
        }
        glob::Pattern::new(glob).map_err(|error| anyhow::anyhow!("--files '{glob}': {error}"))?;
        if !files.iter().any(|kept| kept == glob) {
            files.push(glob.to_string());
        }
    }
    if files.len() > MAX_FILES {
        anyhow::bail!("a task has at most {MAX_FILES} --files globs");
    }
    Ok(files)
}

pub fn checked_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        anyhow::bail!("the title is empty");
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        anyhow::bail!("the title is longer than {MAX_TITLE_CHARS} characters");
    }
    if title.chars().any(char::is_control) {
        anyhow::bail!("the title has control characters");
    }
    Ok(title.to_string())
}

/// The name the web shows: the `--title`, else the first sentence of the worker's latest
/// summary, else `title` (docs/web-client-tasks.md). Events, the ledger and a pull request
/// keep `title`, which a worker's words never change.
pub fn readable_title(dir: &TaskDir, record: &TaskRecord) -> Option<String> {
    if record.title.is_some() {
        return title(dir, record);
    }
    summary_sentence(dir).or_else(|| title(dir, record))
}

/// The first sentence of the worker's latest summary, at most 120 characters.
pub fn summary_sentence(dir: &TaskDir) -> Option<String> {
    let summary = read_result(dir)
        .and_then(|file| Some(file.get("result")?.get("summary")?.as_str()?.to_string()))?;
    Some(first_sentence(&summary))
        .filter(|sentence| !sentence.is_empty())
        .map(|sentence| clip(&sentence, TITLE_CHARS))
}

/// A worker model named by `task new --model`: one word of at most 256 bytes, as
/// `tasks.agent.model` allows. ref: docs/tasks.md "Config"
pub fn checked_model(model: &str) -> Result<String> {
    if model.is_empty() {
        anyhow::bail!("the model is empty");
    }
    if model.len() > MAX_MODEL_BYTES {
        anyhow::bail!("the model is longer than {MAX_MODEL_BYTES} bytes");
    }
    if model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        anyhow::bail!("the model must be one word");
    }
    // ^ It becomes the value of claude's `--model`; a leading dash would read as an option.
    if model.starts_with('-') {
        anyhow::bail!("the model must not start with '-'");
    }
    Ok(model.to_string())
}

/// The task's title: its `--title`, else the first line of its prompt (at most 120 characters),
/// shown without a leading `Owner <words>:`, which is how the manager's prompts open. Only the
/// shown title changes; the record and the prompt keep what was written. ref: docs/tasks.md "Store"
pub fn title(dir: &TaskDir, record: &TaskRecord) -> Option<String> {
    if let Some(title) = &record.title {
        return Some(match owner_lead(title) {
            Some((_, rest)) if !rest.is_empty() => rest.to_string(),
            _ => title.clone(),
        });
    }
    dir.prompt().ok().flatten().map(|prompt| {
        let mut lines = prompt
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let first = lines.next().unwrap_or_default();
        let line = match owner_lead(first) {
            Some((_, rest)) if !rest.is_empty() => rest,
            // ^ A bare `Owner report (what it is about):` names its subject in the brackets.
            Some((lead, _)) => bracketed(lead).or_else(|| lines.next()).unwrap_or(first),
            None => first,
        };
        line.chars().take(TITLE_CHARS).collect()
    })
}

/// The first sentence of the first line of `text`: up to a `.`, `!` or `?` followed by a
/// space or the end ("Did x. Then y" → "Did x."), as the web's `firstSentence`.
fn first_sentence(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    let end = line.char_indices().find(|&(index, mark)| {
        matches!(mark, '.' | '!' | '?')
            && line[index + mark.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
    });
    end.map_or(line, |(index, mark)| &line[..index + mark.len_utf8()])
        .to_string()
}

/// The longest lead-in before the colon, as in `Owner report (UX failures in how a task asks):`.
const OWNER_LEAD_CHARS: usize = 80;

/// A line opening `Owner <words>:` as its words and the text after the colon (maybe empty);
/// `None` for anything else, such as `Owner: Alice's tool`. A colon inside brackets, or one
/// without a space after it (`http://`), is part of the words.
fn owner_lead(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("Owner ")?;
    let mut depth = 0i32;
    let colon = rest.char_indices().find_map(|(at, char)| {
        match char {
            '(' => depth += 1,
            ')' => depth -= 1,
            ':' if depth <= 0
                && rest[at + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace) =>
            {
                return Some(at)
            }
            _ => {}
        }
        None
    })?;
    let lead = rest[..colon].trim();
    if !lead.starts_with(char::is_alphanumeric) || lead.chars().count() > OWNER_LEAD_CHARS {
        return None;
    }
    Some((lead, rest[colon + 1..].trim()))
}

/// `report (UX failures)` → `UX failures`.
fn bracketed(lead: &str) -> Option<&str> {
    let inner = lead[lead.find('(')? + 1..].strip_suffix(')')?.trim();
    (!inner.is_empty()).then_some(inner)
}

/// The agent a task runs on now, not the configured one: its record follows a switch to a
/// fallback.
/// ref: docs/tasks.md "Agents"
pub fn task_agent(record: &TaskRecord) -> Value {
    json!({
        "adapter": record.adapter, "model": record.model, "effort": record.effort,
        "fallback": record.fallback,
    })
}

/// The agent the manager's turns run on now: a fallback in its state, else `tasks.manager`,
/// or the chat's own. ref: docs/tasks.md "Agents"
pub fn manager_agent(dir: &store::ManagerDir, settings: &TaskConfig) -> Value {
    let own = super::chats::agent(dir);
    match dir.read_state().ok().and_then(|state| state.agent) {
        Some(agent) => json!({
            "adapter": agent.adapter, "model": agent.model, "effort": settings.manager_effort,
            "fallback": true,
        }),
        None => json!({
            "adapter": own.as_ref().map_or(&settings.manager_adapter, |own| &own.adapter),
            "model": own.as_ref().map_or(&settings.manager_model, |own| &own.model),
            "effort": settings.manager_effort, "fallback": false,
        }),
    }
}

/// An `agent` field in few words: `sonnet high`, `pi/qwen high`, `fallback pi/qwen`. The
/// web client writes the same (`agentWords` in `web/tasks.js`).
pub fn agent_label(agent: &Value) -> Option<String> {
    let model = agent["model"].as_str().filter(|model| !model.is_empty())?;
    let fallback = agent["fallback"].as_bool().unwrap_or(false);
    let adapter = agent["adapter"].as_str().unwrap_or_default();
    let label = match (fallback, adapter) {
        (false, "claude") => model.to_string(),
        (false, _) => format!("{adapter}/{model}"),
        (true, _) => format!("fallback {adapter}/{model}"),
    };
    Some(
        match agent["effort"].as_str().filter(|effort| !effort.is_empty()) {
            Some(effort) => format!("{label} {effort}"),
            None => label,
        },
    )
}

/// The owner's actions on a task that its state allows; the web's buttons and the TUI's keys
/// read this and no client makes its own rule. The host still decides; this only picks what
/// is offered. `status_kind` is the status's `kind`, `trial_stage` the stage of the project's
/// trial when it is this task's, `preview` whether the project sets `tasks.preview`.
/// - `land`: a ready task with no trial; `pass`: lands the revision on a live trial.
/// - `hand`, `handback`: take a build over in its working copy and give it back
///   (docs/tasks.md "By hand"); `trial_end` frees the live site (docs/tasks.md "Hold and trial").
/// - `answer`: the owner talks to the manager: a wait for the owner or a failure.
/// - `try`: start the task's preview (docs/tasks.md "Preview").
///
/// Hold and trial stay commands for the manager: the owner's message to the worker goes
/// through the manager (docs/web-client-tasks.md "Task sheet").
pub fn owner_actions(
    state: TaskState,
    waiting: Option<WaitOn>,
    status_kind: Option<&str>,
    trial_stage: Option<&str>,
    preview: bool,
) -> Vec<&'static str> {
    let mut actions = Vec::new();
    if (waiting == Some(WaitOn::Owner) && status_kind != Some("after"))
        || state == TaskState::Failed
    {
        actions.push("answer");
    }
    match (state, waiting) {
        (TaskState::Waiting, Some(WaitOn::Hand)) => actions.push("handback"),
        (TaskState::Ready, _) => {
            match trial_stage {
                None => actions.push("land"),
                Some("live") => actions.push("pass"),
                Some(_) => {}
            }
            actions.push("hand");
        }
        (TaskState::Working | TaskState::Waiting | TaskState::Failed, _) => actions.push("hand"),
        (TaskState::Checking | TaskState::Landed | TaskState::Closed, _) => {}
    }
    if trial_stage.is_some() {
        actions.push("trial_end");
    }
    if preview && state != TaskState::Closed {
        actions.push("try");
    }
    actions
}

pub fn task_summary(dir: &TaskDir, record: &TaskRecord) -> Value {
    json!({
        "id": record.id,
        "state": record.state,
        "title": title(dir, record),
        "branch": record.branch,
        "revision": record.revision,
        "parent": record.parent,
        "updated_unix_ms": record.updated_unix_ms,
    })
}

/// Every task of a project with its record, or the error that hid it. A task whose folder
/// is removed while the list is read is left out.
pub fn project_tasks(project_id: &str) -> Result<Vec<(TaskDir, Result<TaskRecord>)>> {
    let root = store::project_dir(project_id)?;
    let mut tasks = Vec::new();
    for id in store::task_numbers(project_id)? {
        let dir = match TaskDir::open(project_id, id) {
            Ok(dir) => dir,
            Err(_) if !root.join(id.to_string()).exists() => continue,
            Err(error) => return Err(error),
        };
        let record = dir.read_record();
        if record.is_err() && !dir.path.exists() {
            continue;
        }
        tasks.push((dir, record));
    }
    Ok(tasks)
}

/// The tasks that name `parent` with `task new --into`, in number order.
/// ref: docs/tasks.md "Parent tasks"
pub fn children(project_id: &str, parent: u32) -> Result<Vec<(TaskDir, TaskRecord)>> {
    Ok(project_tasks(project_id)?
        .into_iter()
        .filter_map(|(dir, record)| record.ok().map(|record| (dir, record)))
        .filter(|(_, record)| record.parent == Some(parent))
        .collect())
}

/// Refuse to `act` on a task while it has open children, naming them.
pub fn refuse_open_children(project_id: &str, parent: u32, act: &str) -> Result<()> {
    let open: Vec<u32> = children(project_id, parent)?
        .into_iter()
        .filter(|(_, record)| record.state.is_open())
        .map(|(_, record)| record.id)
        .collect();
    if !open.is_empty() {
        anyhow::bail!(
            "task t{parent} has open child tasks {}; land or close them before you {act} it",
            task_list(open)
        );
    }
    Ok(())
}

/// `#a, #b` for an error that names tasks.
pub fn task_list(ids: impl IntoIterator<Item = u32>) -> String {
    ids.into_iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The other open tasks whose paths meet this task's. A task's paths are the ones its
/// revisions changed plus its declared `files` globs; `declared` marks a meeting found only
/// through a glob. Changed paths are capped per record, so `complete` is false when either
/// side kept fewer paths than it changed. ref: docs/tasks.md "Store"
pub fn overlaps(record: &TaskRecord, others: &[&TaskRecord], with_paths: bool) -> Value {
    let mine: std::collections::HashSet<&str> = record.paths.iter().map(String::as_str).collect();
    let rows: Vec<_> = others
        .iter()
        // ^ A parent and its children always share paths: not an overlap.
        .filter(|other| {
            other.id != record.id
                && other.state.is_open()
                && record.parent != Some(other.id)
                && other.parent != Some(record.id)
        })
        .filter_map(|other| {
            let mut common: Vec<&str> = other
                .paths
                .iter()
                .map(String::as_str)
                .filter(|path| mine.contains(path))
                .collect();
            let declared = common.is_empty();
            if declared {
                common = declared_meetings(record, other);
            }
            if common.is_empty() {
                return None;
            }
            let complete = record.paths.len() as u32 == record.paths_total
                && other.paths.len() as u32 == other.paths_total;
            let mut row = json!({ "id": other.id, "count": common.len(), "complete": complete });
            if declared {
                row["declared"] = json!(true);
            }
            if with_paths {
                row["paths"] = json!(common.iter().take(OVERLAP_PATHS_SHOWN).collect::<Vec<_>>());
            }
            Some(row)
        })
        .collect();
    json!(rows)
}

/// This task's paths and globs that meet one of `other`'s through a declared glob.
fn declared_meetings<'a>(record: &'a TaskRecord, other: &TaskRecord) -> Vec<&'a str> {
    fn entries(task: &TaskRecord) -> impl Iterator<Item = (&str, bool)> {
        let globs = task.files.iter().map(|glob| (glob.as_str(), true));
        globs.chain(task.paths.iter().map(|path| (path.as_str(), false)))
    }
    entries(record)
        .filter(|&(mine, my_glob)| {
            entries(other).any(|(their, their_glob)| (my_glob || their_glob) && meet(mine, their))
        })
        .map(|(mine, _)| mine)
        .collect()
}

/// Whether two path globs can name a common path. A literal also covers what is under it
/// (`docs` meets `docs/tasks.md`); two wildcard globs meet when their literal heads and
/// tails agree, which can report a meeting that no real path makes.
pub(crate) fn meet(a: &str, b: &str) -> bool {
    const WILD: [char; 3] = ['*', '?', '['];
    let under = |path: &str, dir: &str| {
        path == dir
            || path
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let matches = |glob: &str, path: &str| {
        let options = glob::MatchOptions {
            require_literal_separator: true,
            ..Default::default()
        };
        let head = &glob[..glob.find(WILD).unwrap_or(glob.len())];
        glob::Pattern::new(glob).is_ok_and(|pattern| {
            std::iter::successors(Some(path), |path| path.rsplit_once('/').map(|(dir, _)| dir))
                .any(|path| pattern.matches_with(path, options))
        }) || under(head.trim_end_matches('/'), path)
    };
    match (a.contains(WILD), b.contains(WILD)) {
        (false, false) => under(a, b) || under(b, a),
        (true, false) => matches(a, b),
        (false, true) => matches(b, a),
        (true, true) => {
            let (head_a, head_b) = (&a[..a.find(WILD).unwrap()], &b[..b.find(WILD).unwrap()]);
            let (tail_a, tail_b) = (
                &a[a.rfind(WILD).unwrap() + 1..],
                &b[b.rfind(WILD).unwrap() + 1..],
            );
            (head_a.starts_with(head_b) || head_b.starts_with(head_a))
                && (tail_a.ends_with(tail_b) || tail_b.ends_with(tail_a))
        }
    }
}
