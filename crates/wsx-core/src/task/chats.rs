//! Chats: a manager with a name and a goal. Each is `tasks/chats/<name>/` (the name is its id,
//! unique on the machine) with its own log, `notes/`, an optional `goal.md` and `chat.json
//! {agent?, repos, from?}`. A repo's `home.json` names its home chat, which hears what no chat
//! owns. The repo (files, tasks, rules, memory) is shared; a chat reaches another's context by
//! search or by `say`. ref: docs/tasks.md "Chats"

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
pub const MAX_NAME_CHARS: usize = 24;
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
    if name.starts_with(REPO_PREFIX) {
        bail!("a chat's name does not start with {REPO_PREFIX}, which names a repo's service: {name:?}");
    }
    Ok(name)
}

/// A repo service's session is `task-repo-<repo-id>`: no chat's `task-<name>` may equal it.
const REPO_PREFIX: &str = "repo-";

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

/// Every chat folder of the machine, open or closed, by name.
fn machine_chats() -> Vec<ManagerDir> {
    let Ok(entries) = std::fs::read_dir(store::chats_root()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| checked_name(name).is_ok())
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|name| ManagerDir::existing(name).ok()?)
        .collect()
}

/// The chats that run in the repo `project_id`, open or closed, by name.
pub fn all(project_id: &str) -> Vec<ManagerDir> {
    machine_chats()
        .into_iter()
        .filter(|dir| repos(dir).iter().any(|repo| repo == project_id))
        .collect()
}

pub fn is_open(dir: &ManagerDir) -> bool {
    !dir.path.join(CLOSED_FILE).exists()
}

/// The open chats of the repo, by name.
pub fn open_chats(project_id: &str) -> Vec<ManagerDir> {
    all(project_id).into_iter().filter(is_open).collect()
}

/// The open chat of that name, or an error that says why there is none.
pub fn open_chat(name: &str) -> Result<ManagerDir> {
    match ManagerDir::existing(name)? {
        Some(dir) if is_open(&dir) => Ok(dir),
        _ => bail!("there is no open chat {name}; `wsx task chat list` names them"),
    }
}

/// The names of the open chats of the machine: a task made in one repo by a chat of another
/// belongs to that chat (`owner`).
pub fn open_names() -> Vec<String> {
    machine_chats()
        .iter()
        .filter(|dir| is_open(dir))
        .map(|dir| dir.name().to_string())
        .collect()
}

/// The name of the repo's own chat before its home is set: the repo's name, in a chat's name
/// form, and one that no chat has yet.
pub fn home_name(project_id: &str) -> String {
    free_name(&home_base(project_id))
}

/// The repo's name, from its store id, in a chat's name form.
pub(super) fn home_base(project_id: &str) -> String {
    let slug = project_id
        .rsplit_once('-')
        .filter(|(_, hash)| hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .map_or(project_id, |(slug, _)| slug);
    slug_name(slug)
}

/// `name` as a chat's name: lowercase letters, digits and `-`, from a letter, at most
/// `MAX_NAME_CHARS`.
pub(super) fn slug_name(name: &str) -> String {
    let mut slug: String = name
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c if c.is_ascii_lowercase() || c.is_ascii_digit() => c,
            _ => '-',
        })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if !slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        slug.insert(0, 'r');
    }
    slug.truncate(MAX_NAME_CHARS - 3);
    let slug = slug.trim_end_matches('-');
    if slug == "manager" {
        "manager-repo".into()
    } else if let Some(rest) = slug.strip_prefix(REPO_PREFIX) {
        format!("rp-{rest}")
    } else {
        slug.to_string()
    }
}

/// `base`, or `base-<n>` for the first n that is not the name of a chat folder.
fn free_name(base: &str) -> String {
    (1..)
        .map(|n| match n {
            1 => base.to_string(),
            n => format!("{base}-{n}"),
        })
        .find(|name| !store::chats_root().join(name).exists())
        .expect("a free chat name")
}

/// The one ownership rule: a task (`created_by`) or an approval request (`chat`) made by an
/// open chat belongs to that chat; every other one, also one of a closed chat, belongs to the
/// repo's home chat (`home`). The chat hears the task's events and the request's decision, and
/// its view shows the request and the task's To check items.
pub fn owner<'a>(made_by: Option<&'a str>, open: &[String], home: &'a str) -> &'a str {
    made_by
        .filter(|chat| open.iter().any(|name| name == chat))
        .unwrap_or(home)
}

/// The lines of `approvals.jsonl` that chat `viewer` shows: its own requests and their
/// decisions.
pub fn approvals_of(
    lines: Vec<ApprovalLine>,
    viewer: &str,
    open: &[String],
    home: &str,
) -> Vec<ApprovalLine> {
    let mine = |line: &ApprovalLine| match line {
        ApprovalLine::Request { chat, .. } => owner(chat.as_deref(), open, home) == viewer,
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

/// The To check items of the tasks that chat `viewer` owns.
pub fn checks_of(
    project_id: &str,
    items: Vec<CheckItem>,
    viewer: &str,
    open: &[String],
    home: &str,
) -> Vec<CheckItem> {
    items
        .into_iter()
        .filter(|item| {
            let made_by = store::TaskDir::open(project_id, item.task)
                .and_then(|dir| dir.read_record())
                .map(|record| record.created_by);
            owner(made_by.as_ref().ok().and_then(|by| by.chat()), open, home) == viewer
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

/// The chat and the message that asked for a chat (`chat new --owner-said`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatFrom {
    pub chat: String,
    pub message: u64,
}

/// `chat.json`: the chat's own agent, the repos it runs in (the first is its own, where its
/// host runs) and the message that asked for it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<ChatFrom>,
}

fn chat_file(dir: &ManagerDir) -> ChatFile {
    store::read_bounded_if_exists(&dir.path.join(CHAT_FILE), 4096)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_chat_file(dir: &ManagerDir, file: &ChatFile) -> Result<()> {
    store::write_atomic(&dir.path.join(CHAT_FILE), &serde_json::to_vec(file)?)
}

/// The repos the chat runs in; the first is its own.
pub fn repos(dir: &ManagerDir) -> Vec<String> {
    chat_file(dir).repos
}

/// A new chat folder gets its `chat.json` with its repo.
pub(super) fn ensure_file(dir: &ManagerDir) -> Result<()> {
    if dir.path.join(CHAT_FILE).exists() {
        return Ok(());
    }
    let file = ChatFile {
        repos: vec![dir.project_id.clone()],
        ..ChatFile::default()
    };
    write_chat_file(dir, &file)
}

/// The message that asked for the chat, if a chat's agent did.
pub fn from(dir: &ManagerDir) -> Option<ChatFrom> {
    chat_file(dir).from
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

/// Open the chat in the repo `project_id`: a new folder, or a closed one again with its notes.
/// A new `goal` replaces the old; `from` is the message of a chat's agent that asked for it.
pub fn create(
    project_id: &str,
    name: &str,
    goal: Option<&str>,
    agent: Option<&str>,
    from: Option<ChatFrom>,
) -> Result<ManagerDir> {
    let name = checked_name(name)?;
    let agent = agent.map(checked_agent).transpose()?;
    if let Some(goal) = goal {
        checked_goal(goal)?;
    }
    if let Some(dir) = ManagerDir::existing(name)? {
        if is_open(&dir) {
            bail!("chat {name} is already open");
        }
        if repos(&dir).first().map(String::as_str) != Some(project_id) {
            bail!("chat {name} is a closed chat of another repo; choose another name");
        }
    }
    let dir = ManagerDir::open_as(project_id, name)?;
    if let Some(goal) = goal {
        store::write_atomic(&dir.path.join(GOAL_FILE), goal.as_bytes())?;
    }
    let mut file = chat_file(&dir);
    file.agent = agent.map(str::to_string);
    file.from = from;
    write_chat_file(&dir, &file)?;
    match std::fs::remove_file(dir.path.join(CLOSED_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("cannot open the closed chat again")
        }
        _ => {}
    }
    Ok(dir)
}

/// Close an open chat (`wsx task chat close`, the web's and the TUI's Close chat): its host sees
/// the file and leaves. The notes and the log stay; `create` of the same name opens it again.
/// A repo's home chat, an unknown or a closed name is refused: repoint the home first. Returns
/// the hand-back for the home chat (`hand_back`); the caller says it there.
pub fn close(name: &str) -> Result<(String, String)> {
    let dir = open_chat(name)?;
    let project_id = dir.project_id.clone();
    let home = store::home_chat(&project_id).unwrap_or_default();
    if home == name {
        bail!("{name} is the home chat of its repo; repoint the home first");
    }
    let text = hand_back(&dir);
    store::write_atomic(&dir.path.join(CLOSED_FILE), b"closed by the owner")?;
    Ok((home, text))
}

/// How many of the closed chat's last decisions lines the hand-back carries.
const HAND_BACK_DECISIONS: usize = 5;

/// What a closing chat hands to the home chat: its goal, its open tasks, its last decisions.
/// The open tasks belong to the home chat from then on (`owner`).
fn hand_back(dir: &ManagerDir) -> String {
    let goal = goal(dir);
    let goal = goal.lines().map(str::trim).find(|line| !line.is_empty());
    let tasks: Vec<String> = store::task_numbers(&dir.project_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| {
            store::TaskDir::open(&dir.project_id, id)
                .ok()?
                .read_record()
                .ok()
        })
        .filter(|record| record.created_by.chat() == Some(dir.name()) && record.state.is_open())
        .map(|record| {
            format!(
                "#{} {} ({})",
                record.id,
                record.title.as_deref().unwrap_or_default(),
                record.state.as_str()
            )
        })
        .collect();
    let notes = dir.path.join("notes/decisions.md");
    let decisions: Vec<String> = store::read_bounded_if_exists(&notes, 256 * 1024)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(HAND_BACK_DECISIONS)
        .rev()
        .collect();
    let list = |items: &[String]| match items.is_empty() {
        true => "none".to_string(),
        false => format!(
            "\n{}",
            items
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    format!(
        "Chat {name} is closed; its open tasks are yours now. Its notes stay readable in its folder.\nGoal: {goal}\nOpen tasks: {tasks}\nLast decisions: {decisions}",
        name = dir.name(),
        goal = goal.unwrap_or("none"),
        tasks = list(&tasks),
        decisions = list(&decisions),
    )
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
    let home = store::home_chat(project_id);
    all(project_id)
        .into_iter()
        .map(|dir| {
            let goal = goal(&dir);
            let mut row = serde_json::json!({
                "name": dir.name(),
                "state": if is_open(&dir) { "open" } else { "closed" },
                "goal": goal.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default(),
                "last_unix_ms": last_active_ms(&dir),
                "home": home.as_deref() == Some(dir.name()),
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

/// Run `migrate` on `project` the first time this process opens its store, then start again
/// the hosts the move stopped.
pub(super) fn migrate_once(project_id: &str, project: &Path) {
    let first = CHECKED
        .lock()
        .expect("migration lock")
        .get_or_insert_default()
        .insert(project.display().to_string());
    if !first {
        return;
    }
    if let Err(error) = migrate(project_id, project) {
        eprintln!("wsx: cannot move the chats of {project_id}: {error:#}");
    }
}

/// The two moves of a store from before: roles to chats, then the chats out of the repo's
/// folder (`chats_move`). Idempotent; a step that fails leaves the rest for the next run.
fn migrate(project_id: &str, project: &Path) -> Result<()> {
    migrate_roles(project_id, project)?;
    for stopped in super::chats_move::run(project_id, project)? {
        start_again(project_id, &stopped);
    }
    Ok(())
}

/// Start the host the move stopped, on the new folder, when this is a `wsx`: the other
/// binaries leave it to the next `ensure_manager`.
fn start_again(project_id: &str, stopped: &super::chats_move::Stopped) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let is_wsx = exe.file_stem().is_some_and(|stem| stem == "wsx");
    let Some(main) = super::chats_move::main_of(project_id).filter(|_| is_wsx) else {
        return;
    };
    let Ok((config, _)) = crate::config::global::GlobalConfig::load() else {
        return;
    };
    let name = config
        .projects
        .iter()
        .find(|project| project.path == main)
        .map_or_else(String::new, |project| project.name.clone());
    let launch = |label, argv| {
        crate::ops::create_session_launch(
            &crate::runtime::Client::local(),
            &config,
            &main,
            label,
            argv,
            None,
        )
        .map(drop)
    };
    let chat = (!stopped.home).then_some(stopped.chat.as_str());
    if let Err(error) = super::owner::ensure_manager(&name, &main, &exe, chat, launch) {
        eprintln!("wsx: cannot start chat {} again: {error:#}", stopped.chat);
    }
}

/// Before chats a project had roles. Idempotent: it moves `roles/<n>/` to `chats/<n>/` and
/// `brief.md` to `goal.md`, keeps a role's adapter and model as the `[[agents]]` entry that has
/// both (else drops them with a warning), rewrites the values `role:<n>` and `role-reply:<n>`
/// in task records and logs to `chat:<n>` and `chat-reply:<n>`, and drops the approval lines of
/// the removed `role` action, and deletes the search digest if it has a `role` field (it is
/// rebuilt on the next search). A step that fails leaves the rest for the next run.
fn migrate_roles(project_id: &str, project: &Path) -> Result<()> {
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
    let rename = |value: &mut Value| rename_peers(value, &names);
    for dir in &dirs {
        rewrite_dir(dir, &[b"role:", b"role-reply:"], &rename)?;
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

/// Rewrite the `.json` and `.jsonl` files of `dir` that hold one of `needles` (a quick look
/// before the parse): `rename` changes a value and tells whether it did.
pub(super) fn rewrite_dir(
    dir: &Path,
    needles: &[&[u8]],
    rename: &dyn Fn(&mut Value) -> bool,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    // ^ A repo without a first chat has no old `manager/` to rewrite.
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let jsonl = path.extension().is_some_and(|ext| ext == "jsonl");
        if !(jsonl || path.extension().is_some_and(|ext| ext == "json")) || !path.is_file() {
            continue;
        }
        let Some(bytes) = store::read_bounded_if_exists(&path, 64 << 20)? else {
            continue;
        };
        if !needles
            .iter()
            .any(|needle| store::contains_bytes(&bytes, needle))
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
                    rename(&mut value).then(|| serde_json::to_vec(&value).ok())?
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
