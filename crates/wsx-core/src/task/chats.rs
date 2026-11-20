//! Chats: a manager with a name and a goal. Each is `tasks/chats/<name>/` (the name is its id,
//! unique on the machine) with its own log, `notes/`, an optional `goal.md` and `chat.json
//! {agent?, repos}`. A repo's `home.json` names its main chat, the owner's choice (`set_main`),
//! which hears what no open chat owns. The repo (files, tasks, rules, memory) is shared; a chat reaches another's context by
//! search or by `say`. ref: docs/tasks.md "Chats"

use super::chats_home;
use super::checks::CheckItem;
use super::manager::ApprovalLine;
use super::store::{self, ManagerDir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// The chat's purpose, in the manager's instructions (`wsx task chat goal`); a changed goal
/// reaches the agent at its next turn. Every chat has one.
pub const GOAL_FILE: &str = "goal.md";
/// Written by `chat close`; its host leaves. The notes and the log stay.
pub const CLOSED_FILE: &str = "closed";
/// `{agent?}`: the name of the `[[agents]]` entry the chat runs on; none runs the manager's.
pub const CHAT_FILE: &str = "chat.json";
pub const MAX_GOAL_BYTES: usize = 8 * 1024;
pub const MAX_NAME_CHARS: usize = 24;
/// A chat's shown title (`rename`), in characters.
pub const MAX_TITLE_CHARS: usize = 40;

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

/// The repo's name, from its store id, in a chat's name form.
pub(super) fn repo_base(project_id: &str) -> String {
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

/// The one ownership rule: a task (`TaskRecord::owner_chat`: the chat it moved to, else its
/// maker) or an approval request (`chat`) of an open chat belongs to that chat; every other one,
/// also one of a closed chat, belongs to the repo's main chat (`main`) when that is open, else to
/// no chat. The chat hears the task's events and the request's decision, and its view shows the
/// request and the task's To check items.
pub fn owner<'a>(
    made_by: Option<&'a str>,
    open: &[String],
    main: Option<&'a str>,
) -> Option<&'a str> {
    let is_open = |name: &&str| open.iter().any(|open| open == name);
    made_by.filter(is_open).or(main.filter(is_open))
}

/// The chat that looks at a list and the open chats and main chat it is told apart from:
/// `owns` is `owner` for that chat, the one rule every list a chat shows reads (tasks,
/// approvals, To check items, the overview, the ledger).
#[derive(Debug, Clone)]
pub struct Viewer {
    name: String,
    open: Vec<String>,
    main: Option<String>,
}

impl Viewer {
    /// Chat `chat` of the repo, or its main chat for `None` (with none, a viewer with no name
    /// that owns nothing).
    pub fn new(project_id: &str, chat: Option<&str>) -> Self {
        let main = store::main_chat(project_id);
        Self::named(
            chat.or(main.as_deref()).unwrap_or_default(),
            open_names(),
            main.as_deref(),
        )
    }

    pub fn named(name: &str, open: Vec<String>, main: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            open,
            main: main.map(str::to_string),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn main(&self) -> Option<&str> {
        self.main.as_deref()
    }

    pub fn open(&self) -> &[String] {
        &self.open
    }

    /// Whether what `made_by` made belongs to this chat.
    pub fn owns(&self, made_by: Option<&str>) -> bool {
        owner(made_by, &self.open, self.main.as_deref()) == Some(self.name.as_str())
    }
}

/// The lines of `approvals.jsonl` that `viewer` shows: its own requests and their decisions.
pub fn approvals_of(lines: Vec<ApprovalLine>, viewer: &Viewer) -> Vec<ApprovalLine> {
    let mine = |line: &ApprovalLine| match line {
        ApprovalLine::Request { chat, .. } => viewer.owns(chat.as_deref()),
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

/// The To check items of the tasks that `viewer` owns.
pub fn checks_of(project_id: &str, items: Vec<CheckItem>, viewer: &Viewer) -> Vec<CheckItem> {
    items
        .into_iter()
        .filter(|item| {
            let record =
                store::TaskDir::open(project_id, item.task).and_then(|dir| dir.read_record());
            viewer.owns(record.as_ref().ok().and_then(|record| record.owner_chat()))
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

/// A goal as given: UTF-8 text, at most 8 KiB; an empty goal is no purpose.
pub fn checked_goal(goal: &str) -> Result<&str> {
    if goal.len() > MAX_GOAL_BYTES {
        bail!("the chat's goal is larger than {MAX_GOAL_BYTES} bytes");
    }
    Ok(goal)
}

/// `chat.json`: the chat's own agent, the repos it runs in (the first is its own, where its
/// host runs).
#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default)]
    repos: Vec<String>,
    /// The name the owner reads in place of the chat's id (`shown_name`); none shows the id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    title: String,
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

/// A chat's title as `rename` keeps it: trimmed, one line, at most `MAX_TITLE_CHARS`; empty
/// clears it. The reason a title is refused is in the error.
pub fn checked_title(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.chars().any(char::is_control) {
        bail!("a chat's title is one line");
    }
    let chars = title.chars().count();
    if chars > MAX_TITLE_CHARS {
        bail!("a chat's title is at most {MAX_TITLE_CHARS} characters, this is {chars}");
    }
    Ok(title)
}

/// The name the owner reads for a chat, the one rule of every surface: its title, else for the
/// repo's main chat `home` (the project's shown name), else the chat's id.
pub fn shown_name(dir: &ManagerDir, home: &str) -> String {
    shown_of(
        &chat_file(dir).title,
        dir.is_main().then_some(home),
        dir.name(),
    )
}

fn shown_of(title: &str, home: Option<&str>, id: &str) -> String {
    [title, home.unwrap_or_default(), id]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or(id)
        .to_string()
}

/// Set (or, empty, clear) an open chat's title; the id, its folder and every reference stay.
/// Returns the title kept. The repo's main chat is named by its project: refused.
pub fn rename(name: &str, title: &str) -> Result<String> {
    let title = checked_title(title)?;
    let dir = open_chat(name)?;
    if dir.is_main() {
        bail!("{name} is the project's first chat: it is named by its project");
    }
    let mut file = chat_file(&dir);
    file.title = title.to_string();
    write_chat_file(&dir, &file)?;
    Ok(file.title)
}

/// The names of the projects the chat runs in, its own first (`repos`, by the registered
/// project's name; one the config does not know shows its id without the hash).
pub fn project_names(dir: &ManagerDir) -> Vec<String> {
    let registered = Registered::load();
    repos(dir).iter().map(|id| registered.name(id)).collect()
}

/// Every registered project's store id and name.
pub struct Registered(Vec<(String, String)>);

impl Registered {
    pub fn load() -> Self {
        Self(
            crate::config::global::GlobalConfig::load()
                .map(|(config, _)| {
                    config
                        .projects
                        .into_iter()
                        .map(|project| {
                            (
                                store::project_id(&project.name, &project.path),
                                project.name,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    fn name(&self, id: &str) -> String {
        self.0
            .iter()
            .find(|(each, _)| each == id)
            .map_or_else(|| repo_base(id), |(_, name)| name.clone())
    }
}

/// The fields that name a chat in a row, one owner for the chats' rows of the ledger and of the
/// web overview: `shown` (`shown_name`, the project `project_id`'s name for its main chat),
/// `projects` (where it runs, its own first, by name) and `title` when it has one.
pub fn naming(
    dir: &ManagerDir,
    project_id: &str,
    is_main: bool,
    registered: &Registered,
) -> serde_json::Map<String, Value> {
    let file = chat_file(dir);
    let own = registered.name(project_id);
    let mut fields = serde_json::Map::new();
    fields.insert(
        "shown".into(),
        shown_of(&file.title, is_main.then_some(own.as_str()), dir.name()).into(),
    );
    fields.insert(
        "projects".into(),
        file.repos
            .iter()
            .map(|id| registered.name(id))
            .collect::<Vec<_>>()
            .into(),
    );
    if !file.title.is_empty() {
        fields.insert("title".into(), file.title.into());
    }
    fields
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
/// A new `goal` replaces the old (`set_goal`);
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
        write_goal(&dir, goal)?;
    }
    let mut file = chat_file(&dir);
    file.agent = agent.map(str::to_string);
    write_chat_file(&dir, &file)?;
    match std::fs::remove_file(dir.path.join(CLOSED_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("cannot open the closed chat again")
        }
        _ => {}
    }
    chats_home::clear(&dir)?;
    // ^ A project's first chat is its main chat.
    if store::main_chat(project_id).is_none() {
        store::set_main_chat(project_id, name)?;
    }
    Ok(dir)
}

/// Set an open chat's purpose; an empty goal clears it. The chat's agent
/// reads it at its next turn (`Manager::send_turn`).
pub fn set_goal(name: &str, goal: &str) -> Result<()> {
    checked_goal(goal)?;
    write_goal(&open_chat(name)?, goal)
}

fn write_goal(dir: &ManagerDir, goal: &str) -> Result<()> {
    let path = dir.path.join(GOAL_FILE);
    if !goal.trim().is_empty() {
        return store::write_atomic(&path, goal.as_bytes());
    }
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("cannot clear the chat's goal")
        }
        _ => Ok(()),
    }
}

/// Close an open chat (`wsx task chat close`, the web's and the TUI's Close chat): its host sees
/// the file and leaves. The notes and the log stay; `create` of the same name opens it again.
/// An unknown or a closed name is refused. Returns the repo's main chat that is left, which gets
/// the hand-back (`hand_back`, the caller says it there); closing the main chat clears the
/// pointer, so the repo has none and the text is not handed to anyone.
pub fn close(name: &str) -> Result<(Option<String>, String)> {
    let dir = open_chat(name)?;
    let text = hand_back(&dir);
    store::write_atomic(&dir.path.join(CLOSED_FILE), b"closed by the owner")?;
    let main = store::main_chat(&dir.project_id);
    if main.as_deref() == Some(name) {
        store::clear_main_chat(&dir.project_id)?;
        return Ok((None, text));
    }
    Ok((main, text))
}

/// The repo's main chat, which must be open: the one that hears what no chat owns and that
/// another project's `say` reaches. An error names `chat main` when there is none.
pub fn open_main(project_id: &str) -> Result<ManagerDir> {
    let name = store::main_chat(project_id).context(
        "the project has no main chat; `wsx task chat main <chat>` makes one of its chats the main chat",
    )?;
    open_chat(&name).with_context(|| {
        format!("its main chat {name} is closed; `wsx task chat main` sets another")
    })
}

/// Make the open chat `name` the main chat of `project_id`, which it must name in its `repos`.
pub fn set_main(name: &str, project_id: &str) -> Result<()> {
    let dir = open_chat(name)?;
    if !repos(&dir).iter().any(|id| id == project_id) {
        bail!("chat {name} does not name this project; it cannot be its main chat");
    }
    store::set_main_chat(project_id, name)
}

/// How many of the closed chat's last decisions lines the hand-back carries.
const HAND_BACK_DECISIONS: usize = 5;

/// What a closing chat hands to the main chat: its goal, its open tasks, its last decisions.
/// The open tasks belong to the main chat from then on (`owner`).
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
        .filter(|record| record.owner_chat() == Some(dir.name()) && record.state.is_open())
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

/// The chats as rows for the ledger, `chat list` and the views, by name: `{name, shown, projects,
/// state, goal, last_unix_ms, listed, main, title?, agent?, adapter?, model?}`. `shown` is the
/// name the owner reads (`shown_name`, the project's name for the main chat), `title` the chat's
/// own, `projects` the names of the projects it runs in, its own first; `goal` is the goal's
/// first line; `agent` is the entry's name, `adapter` and `model` its own, left out for a chat on
/// the manager's.
pub fn rows(project_id: &str) -> Vec<Value> {
    let (main, registered) = (store::main_chat(project_id), Registered::load());
    all(project_id)
        .iter()
        .map(|dir| row(dir, project_id, main.as_deref(), &registered))
        .collect()
}

/// The open chats with their row (`rows`) and pill counts (`counts`, `needs::chat_counts`): the
/// ledger and the web's Home rows read the same row. A closed chat is history.
pub fn open_rows(project_id: &str, land_auto: bool) -> Vec<(ManagerDir, Value)> {
    let counts = super::needs::chat_counts(project_id, land_auto);
    let (main, registered) = (store::main_chat(project_id), Registered::load());
    open_chats(project_id)
        .into_iter()
        .map(|dir| {
            let mut row = row(&dir, project_id, main.as_deref(), &registered);
            row["counts"] = serde_json::json!(counts.get(dir.name()).copied().unwrap_or_default());
            (dir, row)
        })
        .collect()
}

fn row(dir: &ManagerDir, project_id: &str, main: Option<&str>, registered: &Registered) -> Value {
    let last = chats_home::last_message_ms(dir);
    let is_main = main == Some(dir.name());
    let mut row = serde_json::json!({
        "name": dir.name(),
        "state": if is_open(dir) { "open" } else { "closed" },
        "goal": goal(dir).lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default(),
        "last_unix_ms": last,
        "listed": chats_home::listed_at(dir, || last),
        "folder": chats_home::folder(dir),
        "main": is_main,
    });
    row.as_object_mut()
        .expect("a row is an object")
        .extend(naming(dir, project_id, is_main, registered));
    if let (Some(entry), Some(own)) = (chat_file(dir).agent, agent(dir)) {
        row["agent"] = entry.into();
        row["adapter"] = own.adapter.into();
        row["model"] = own.model.into();
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shown_name_is_the_title_then_the_main_name_then_the_id() {
        assert_eq!(shown_of("Fee table", None, "fees"), "Fee table");
        assert_eq!(shown_of("Fee table", Some("wsx"), "fees"), "Fee table");
        assert_eq!(shown_of("", Some("wsx"), "manager-chat"), "wsx");
        assert_eq!(shown_of("", None, "fees"), "fees");
    }

    #[test]
    fn a_title_is_trimmed_one_line_and_at_most_forty_characters() {
        assert_eq!(checked_title("  Fee table \n").unwrap(), "Fee table");
        assert_eq!(checked_title("   ").unwrap(), "");
        assert!(checked_title(&"x".repeat(40)).is_ok());
        let long = checked_title(&"é".repeat(41)).unwrap_err().to_string();
        assert!(long.contains("at most 40") && long.contains("41"), "{long}");
        let lines = checked_title("two\nlines").unwrap_err().to_string();
        assert!(lines.contains("one line"), "{lines}");
    }
}
