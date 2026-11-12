//! Chats: a manager with a name and a goal. The project's first chat is `manager/`; each other
//! is `tasks/<project-id>/chats/<name>/` with its own log, `notes/`, an optional `goal.md` and
//! `chat.json {agent?}`. The project (files, tasks, rules, memory) is shared; a chat reaches
//! another's context by search or by `say`. ref: docs/tasks.md "Chats"

use super::checks::CheckItem;
use super::manager::ApprovalLine;
use super::store::{self, ManagerDir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

/// The chat's goal, besides the manager's instructions; a new goal starts a fresh conversation.
pub const GOAL_FILE: &str = "goal.md";
/// Written by `chat close`; its host leaves. The notes and the log stay.
pub const CLOSED_FILE: &str = "closed";
/// `{agent?}`: the name of the `[[agents]]` entry the chat runs on; none runs the manager's.
pub const CHAT_FILE: &str = "chat.json";
pub const MAX_GOAL_BYTES: usize = 8 * 1024;
const MAX_NAME_CHARS: usize = 24;
/// How much of a chat's log is read for its last turn.
const TAIL_BYTES: u64 = 64 * 1024;

/// A chat's name: a lowercase letter, then lowercase letters, digits and `-`, as the end of its
/// session label `task-<name>`; never `manager`.
pub fn checked_name(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.len() <= MAX_NAME_CHARS;
    if !valid {
        bail!("a chat's name is a lowercase letter, then letters, digits or -, at most {MAX_NAME_CHARS}: {name:?}");
    }
    if name == "manager" {
        bail!("`manager` is the project's first chat, not a name to create");
    }
    Ok(name)
}

/// Why `name` cannot be the name of a new chat, in a phrase, or none: the New chat forms of the
/// web and the TUI say it before they ask, with `open` the project's open chats. An empty name
/// has no problem yet.
pub fn name_problem(name: &str, open: &[String]) -> Option<String> {
    if name.is_empty() {
        None
    } else if name == "manager" {
        Some("manager: the project's first chat".into())
    } else if checked_name(name).is_err() {
        Some(format!(
            "{name}: a-z, 0-9 and -, up to {MAX_NAME_CHARS}, from a letter"
        ))
    } else {
        open.iter()
            .any(|each| each == name)
            .then(|| format!("{name}: already open"))
    }
}

/// The project's chat folders besides `manager/`, open or closed, by name.
pub fn all(project_id: &str) -> Vec<ManagerDir> {
    let Ok(entries) = store::chats_dir(project_id).and_then(|dir| Ok(std::fs::read_dir(dir)?))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| checked_name(name).is_ok())
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|name| ManagerDir::existing_as(project_id, Some(name)).ok()?)
        .collect()
}

pub fn is_open(dir: &ManagerDir) -> bool {
    dir.chat.is_some() && !dir.path.join(CLOSED_FILE).exists()
}

/// The open chats' folders, by name.
pub fn open_chats(project_id: &str) -> Vec<ManagerDir> {
    all(project_id).into_iter().filter(is_open).collect()
}

/// The open chat of that name, or an error that says why there is none.
pub fn open_chat(project_id: &str, name: &str) -> Result<ManagerDir> {
    match ManagerDir::existing_as(project_id, Some(name))? {
        Some(dir) if is_open(&dir) => Ok(dir),
        _ => bail!("the project has no open chat {name}; `wsx task chat list` names them"),
    }
}

/// The names of the open chats.
pub fn open_names(project_id: &str) -> Vec<String> {
    open_chats(project_id)
        .iter()
        .map(|dir| dir.name().to_string())
        .collect()
}

/// The one ownership rule: a task (`created_by`) or an approval request (`chat`) made by an
/// open chat belongs to that chat; every other one, also one of a closed chat, belongs to the
/// first chat (`None`). The chat hears the task's events and the request's decision, and its
/// view shows the request and the task's To check items.
pub fn owner<'a>(made_by: Option<&'a str>, open: &[String]) -> Option<&'a str> {
    made_by.filter(|chat| open.iter().any(|name| name == chat))
}

/// The lines of `approvals.jsonl` that chat `viewer` (`None`: the first) shows: its own
/// requests and their decisions.
pub fn approvals_of(
    lines: Vec<ApprovalLine>,
    viewer: Option<&str>,
    open: &[String],
) -> Vec<ApprovalLine> {
    let mine = |line: &ApprovalLine| match line {
        ApprovalLine::Request { chat, .. } => owner(chat.as_deref(), open) == viewer,
        ApprovalLine::Decision { .. } => false,
    };
    let ids: HashSet<String> = lines
        .iter()
        .filter(|line| mine(line))
        .map(|line| line.id().to_string())
        .collect();
    lines
        .into_iter()
        .filter(|line| ids.contains(line.id()))
        .collect()
}

/// The To check items of the tasks that chat `viewer` (`None`: the first) owns.
pub fn checks_of(
    project_id: &str,
    items: Vec<CheckItem>,
    viewer: Option<&str>,
    open: &[String],
) -> Vec<CheckItem> {
    items
        .into_iter()
        .filter(|item| {
            let made_by = store::TaskDir::open(project_id, item.task)
                .and_then(|dir| dir.read_record())
                .map(|record| record.created_by);
            owner(made_by.as_ref().ok().and_then(|by| by.chat()), open) == viewer
        })
        .collect()
}

pub fn goal(dir: &ManagerDir) -> String {
    store::read_bounded_if_exists(&dir.path.join(GOAL_FILE), MAX_GOAL_BYTES as u64)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// A goal as given: UTF-8 text, not empty, at most 8 KiB.
pub fn checked_goal(goal: &str) -> Result<&str> {
    if goal.trim().is_empty() {
        bail!("the chat's goal is empty");
    }
    if goal.len() > MAX_GOAL_BYTES {
        bail!("the chat's goal is larger than {MAX_GOAL_BYTES} bytes");
    }
    Ok(goal)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
}

fn chat_file(dir: &ManagerDir) -> ChatFile {
    store::read_bounded_if_exists(&dir.path.join(CHAT_FILE), 4096)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// A chat's own agent: an `[[agents]]` entry's adapter and model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAgent {
    pub adapter: String,
    pub model: String,
}

fn entry_agent(entry: &str) -> Option<ChatAgent> {
    agent_entries()
        .into_iter()
        .find_map(|(name, agent)| (name == entry).then_some(agent))
}

/// `agent`: a name in `[[agents]]` that has a headless adapter and a model, as a task's
/// `--model` is checked before it is kept.
fn checked_agent(entry: &str) -> Result<&str> {
    if entry_agent(entry).is_none() {
        bail!("--agent {entry}: no [[agents]] entry of that name has an adapter and a model");
    }
    Ok(entry)
}

/// The `[[agents]]` entries a chat may run on: those with an adapter and a model, as
/// `(name, agent)` in config order.
pub fn agent_entries() -> Vec<(String, ChatAgent)> {
    let Ok((config, _)) = crate::config::global::GlobalConfig::load() else {
        return Vec::new();
    };
    config
        .agents
        .into_iter()
        .filter_map(|entry| {
            Some((
                entry.name,
                ChatAgent {
                    adapter: entry.adapter?,
                    model: entry.model?,
                },
            ))
        })
        .collect()
}

/// The chat's own agent; none runs the manager's (also when its entry is gone from the config).
pub fn agent(dir: &ManagerDir) -> Option<ChatAgent> {
    entry_agent(&chat_file(dir).agent?)
}

/// Open the chat: a new folder, or a closed one again with its notes. A new `goal` replaces
/// the old.
pub fn create(
    project_id: &str,
    name: &str,
    goal: Option<&str>,
    agent: Option<&str>,
) -> Result<ManagerDir> {
    let name = checked_name(name)?;
    let agent = agent.map(checked_agent).transpose()?;
    if let Some(goal) = goal {
        checked_goal(goal)?;
    }
    if ManagerDir::existing_as(project_id, Some(name))?.is_some_and(|dir| is_open(&dir)) {
        bail!("chat {name} is already open");
    }
    let dir = ManagerDir::open_as(project_id, Some(name))?;
    if let Some(goal) = goal {
        store::write_atomic(&dir.path.join(GOAL_FILE), goal.as_bytes())?;
    }
    let file = ChatFile {
        agent: agent.map(str::to_string),
    };
    store::write_atomic(&dir.path.join(CHAT_FILE), &serde_json::to_vec(&file)?)?;
    match std::fs::remove_file(dir.path.join(CLOSED_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("cannot open the closed chat again")
        }
        _ => {}
    }
    Ok(dir)
}

/// Close a chat; its host sees the file and leaves. The notes and the log stay.
pub fn close(dir: &ManagerDir, why: &str) -> Result<()> {
    store::write_atomic(&dir.path.join(CLOSED_FILE), why.as_bytes())
}

/// When the chat last had anything in its conversation, else when it was opened.
pub fn last_active_ms(dir: &ManagerDir) -> u64 {
    dir.read_tail(TAIL_BYTES)
        .ok()
        .and_then(|entries| entries.last().map(|entry| entry.unix_ms))
        .unwrap_or_else(|| {
            std::fs::metadata(dir.path.join(CHAT_FILE))
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_millis() as u64)
        })
}

/// The chats as rows for the ledger, `chat list` and the views, by name: `{name, state, goal,
/// last_unix_ms, agent?, adapter?, model?}`. `goal` is the goal's first line; `agent` is the
/// entry's name, `adapter` and `model` its own, left out for a chat on the manager's.
pub fn rows(project_id: &str) -> Vec<Value> {
    all(project_id)
        .into_iter()
        .map(|dir| {
            let goal = goal(&dir);
            let mut row = serde_json::json!({
                "name": dir.name(),
                "state": if is_open(&dir) { "open" } else { "closed" },
                "goal": goal.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default(),
                "last_unix_ms": last_active_ms(&dir),
            });
            if let (Some(entry), Some(own)) = (chat_file(&dir).agent, agent(&dir)) {
                row["agent"] = entry.into();
                row["adapter"] = own.adapter.into();
                row["model"] = own.model.into();
            }
            row
        })
        .collect()
}

/// A project not yet migrated in this process; `migrate` runs once for each.
static CHECKED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Run `migrate` on `project` the first time this process opens its store.
pub(super) fn migrate_once(project_id: &str, project: &Path) {
    let first = CHECKED
        .lock()
        .expect("migration lock")
        .get_or_insert_default()
        .insert(project.display().to_string());
    if first {
        if let Err(error) = migrate(project_id, project) {
            eprintln!("wsx: cannot move the roles of {project_id} to chats: {error:#}");
        }
    }
}

/// Before chats a project had roles. Idempotent: it moves `roles/<n>/` to `chats/<n>/` and
/// `brief.md` to `goal.md`, keeps a role's adapter and model as the `[[agents]]` entry that has
/// both (else drops them with a warning), rewrites the values `role:<n>` and `role-reply:<n>`
/// in task records and logs to `chat:<n>` and `chat-reply:<n>`, and drops the approval lines of
/// the removed `role` action, and deletes the search digest if it has a `role` field (it is
/// rebuilt on the next search). A step that fails leaves the rest for the next run.
fn migrate(project_id: &str, project: &Path) -> Result<()> {
    let approvals = project.join(store::APPROVALS_FILE);
    let old = project.join("roles");
    let digest = project.join(crate::memory::index::DIGEST_FILE);
    let has = |file: &Path, key: &[u8]| -> Result<bool> {
        Ok(store::read_bounded_if_exists(file, 64 << 20)?
            .is_some_and(|bytes| store::contains_bytes(&bytes, key)))
    };
    let (role_requests, role_digest) = (
        has(&approvals, br#""role""#)?,
        has(&digest, br#""role":"#).unwrap_or(false),
    );
    // ^ A digest too large to read is the search's to rebuild, not a reason to fail the move.
    if !old.is_dir() && !role_requests && !role_digest {
        return Ok(());
    }
    // ^ A log is rewritten whole here, while a host appends to it with no lock. Every process
    // migrates before its first append (`project_dir`), so this lock holds the others at that
    // door until the rewrite is done; only an older wsx, which never migrates, can still append.
    let _lock = store::open_locked(
        &project.join("migrate.lock"),
        std::fs::OpenOptions::new().write(true).create(true),
    )?;
    if let Some(bytes) = store::read_bounded_if_exists(&approvals, 64 << 20)? {
        drop_role_approvals(&approvals, &bytes)?;
    }
    if role_digest {
        let _ = std::fs::remove_file(&digest); // ^ a search may have rebuilt it first
    }
    if !old.is_dir() {
        return Ok(());
    }
    let mut roles: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&old)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .filter(|(name, path)| checked_name(name).is_ok() && path.is_dir())
        .collect();
    roles.sort();
    let names: Vec<&str> = roles
        .iter()
        .map(|(name, _)| name.as_str())
        .chain(["manager"])
        .collect();
    let mut dirs: Vec<std::path::PathBuf> = roles.iter().map(|(_, path)| path.clone()).collect();
    dirs.push(project.join("manager"));
    dirs.extend(
        store::task_numbers(project_id)?
            .iter()
            .map(|id| project.join(id.to_string())),
    );
    for dir in &dirs {
        rewrite_dir(dir, &names)?;
    }
    let agents = crate::config::global::GlobalConfig::load()
        .map(|(config, _)| config.agents)
        .unwrap_or_default();
    store::create_private_dir(&project.join("chats"))?;
    for (name, path) in &roles {
        if !path.is_dir() {
            continue; // ^ another process moved it first
        }
        move_role(name, path, &agents)?;
        let target = project.join("chats").join(name);
        if target.exists() {
            eprintln!("wsx: chat {name} exists already; role {name} stays in roles/");
        } else {
            std::fs::rename(path, target)?;
        }
    }
    let _ = std::fs::remove_dir(&old);
    Ok(())
}

/// `brief.md` to `goal.md`; `role.json {adapter, model}` to `chat.json {agent}`.
fn move_role(name: &str, path: &Path, agents: &[crate::config::global::AgentEntry]) -> Result<()> {
    let brief = path.join("brief.md");
    if brief.is_file() {
        std::fs::rename(&brief, path.join(GOAL_FILE))?;
    }
    let file = path.join("role.json");
    let Some(bytes) = store::read_bounded_if_exists(&file, 4096)? else {
        return Ok(());
    };
    let role: Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let mut chat = ChatFile::default();
    if let (Some(adapter), Some(model)) = (role["adapter"].as_str(), role["model"].as_str()) {
        chat.agent = agents
            .iter()
            .find(|agent| {
                agent.adapter.as_deref() == Some(adapter) && agent.model.as_deref() == Some(model)
            })
            .map(|agent| agent.name.clone());
        if chat.agent.is_none() {
            eprintln!("wsx: chat {name}: no [[agents]] entry runs {adapter}/{model}; it runs the manager's agent");
        }
    }
    store::write_atomic(&path.join(CHAT_FILE), &serde_json::to_vec(&chat)?)?;
    std::fs::remove_file(file)?;
    Ok(())
}

/// Rewrite the `.json` and `.jsonl` files of `dir` that hold a role's peer key.
fn rewrite_dir(dir: &Path, names: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let jsonl = path.extension().is_some_and(|ext| ext == "jsonl");
        if !(jsonl || path.extension().is_some_and(|ext| ext == "json")) || !path.is_file() {
            continue;
        }
        let Some(bytes) = store::read_bounded_if_exists(&path, 64 << 20)? else {
            continue;
        };
        if !store::contains_bytes(&bytes, b"role:")
            && !store::contains_bytes(&bytes, b"role-reply:")
        {
            continue;
        }
        let mut changed = false;
        let mut out = Vec::with_capacity(bytes.len());
        let lines: Vec<&[u8]> = match jsonl {
            true => bytes.split(|byte| *byte == b'\n').collect(),
            false => vec![&bytes],
        };
        for (at, line) in lines.iter().enumerate() {
            let rewritten = serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|mut value| {
                    rename_peers(&mut value, names).then(|| serde_json::to_vec(&value).ok())?
                });
            changed |= rewritten.is_some();
            out.extend_from_slice(rewritten.as_deref().unwrap_or(line));
            if at + 1 < lines.len() {
                out.push(b'\n');
            }
        }
        if changed {
            store::write_atomic(&path, &out)?;
        }
    }
    Ok(())
}

/// Whether any string value of `value` was a role's peer key, now the chat's.
fn rename_peers(value: &mut Value, names: &[&str]) -> bool {
    match value {
        Value::String(text) => {
            let renamed = [("role:", "chat:"), ("role-reply:", "chat-reply:")]
                .iter()
                .find_map(|(old, new)| {
                    let name = text.strip_prefix(old).filter(|name| names.contains(name))?;
                    Some(format!("{new}{name}"))
                });
            renamed.map(|renamed| *text = renamed).is_some()
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |any, item| rename_peers(item, names) | any),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |any, item| rename_peers(item, names) | any),
        _ => false,
    }
}

/// Drop the requests of the removed `role` action and the decisions of their ids.
fn drop_role_approvals(path: &Path, bytes: &[u8]) -> Result<()> {
    if !store::contains_bytes(bytes, br#""role""#) {
        return Ok(());
    }
    let lines: Vec<(&[u8], Option<Value>)> = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| (line, serde_json::from_slice(line).ok()))
        .collect();
    let is_role = |value: &Value| value["kind"] == "request" && value["action"] == "role";
    let ids: HashSet<&str> = lines
        .iter()
        .filter_map(|(_, value)| value.as_ref().filter(|value| is_role(value)))
        .filter_map(|value| value["id"].as_str())
        .collect();
    let kept: Vec<&[u8]> = lines
        .iter()
        .filter(|(line, value)| {
            !line.is_empty()
                && !value.as_ref().is_some_and(|value| {
                    is_role(value) || value["id"].as_str().is_some_and(|id| ids.contains(id))
                })
        })
        .map(|(line, _)| *line)
        .collect();
    let mut out = kept.join(&b'\n');
    if !out.is_empty() {
        out.push(b'\n');
    }
    store::write_atomic(path, &out)
}
