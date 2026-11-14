//! The chats: `say`, `chat`, `search` (docs/tasks.md "Manager", "Chats").

use super::approve::owner_only;
use crate::cli::load_config;
use crate::cli::PromptSource;
use crate::task::args::ChatCmd;
use crate::task::broker;
use crate::task::manager;
use crate::task::{acting_as_manager, actor, launch_manager, open, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::chat::{self, Reading, Window};
use wsx_core::task::chats;
use wsx_core::task::manager::{pending_approvals, Author, ManagerRequest, ManagerResponse, Party};
use wsx_core::task::owner::{self};
use wsx_core::task::store::ManagerDir;
use wsx_core::task::MAX_MESSAGE_BYTES;
use wsx_core::task::{search, text};
use wsx_core::task::{
    store::{self, TaskDir},
    Actor,
};

pub(crate) fn ensure_manager(project: Option<&str>) -> Result<(bool, ManagerDir)> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_manager(&name, &main, &exe, None, launch_manager(&main))
}

/// `wsx task say`: to the manager, or to the open chat `to`, from the owner or a chat.
/// `from_chat`: this host's agent sends it (`reply`: as a reply); the sender is the acting chat
/// (the first as `manager`), and for another project (`-p`) its first chat is the one reached.
pub(crate) fn say_command(
    project: Option<&str>,
    to: Option<&str>,
    text: PromptSource,
    (from_chat, reply): (bool, bool),
    json: bool,
) -> Result<()> {
    if reply && !from_chat {
        bail!("--reply answers with --from-chat");
    }
    let text = text.read(MAX_MESSAGE_BYTES)?;
    let (name, main) = project_entry(project)?;
    let from = match from_chat.then_some(reply) {
        None => None,
        Some(reply) => {
            // ^ Only a manager host sets the sender's project (broker.rs).
            let own = std::env::var(broker::FROM_ENV)
                .ok()
                .filter(|own| acting_as_manager() && !own.is_empty())
                .context("--from-chat is for a chat's agent")?;
            let party = if own == name {
                let sender = match actor() {
                    Actor::Chat(sender) => sender,
                    _ => "manager".to_string(),
                };
                if sender == to.unwrap_or("manager") {
                    bail!("--from-chat messages another chat, not itself");
                }
                Party::Chat(sender)
            } else if actor() != Actor::Manager || to.is_some() {
                bail!("--from-chat reaches another project's first chat, as the first chat, with no --to");
            } else {
                Party::Project(own)
            };
            Some(Author::Peer { from: party, reply })
        }
    };
    // ^ `--to manager` is the first chat, as another chat names it in its reply.
    let chat = to.filter(|to| *to != "manager");
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let sent = from.is_some().then(|| text.clone());
    let response = owner::say(
        &name,
        &main,
        &exe,
        chat,
        launch_manager(&main),
        text,
        from,
        Vec::new(),
    )?;
    // ^ A chat's message always answers in JSON: its host logs `sent` from
    // the `text` (broker.rs). ref: docs/tasks.md "Managers talk"
    if json || sent.is_some() {
        let mut value = serde_json::to_value(&response)?;
        if let Some(text) = sent {
            value["text"] = json!(text);
        }
        print_json(&value)?;
    } else {
        println!(
            "message {}: {}",
            response.id.map_or("-".into(), |id| id.to_string()),
            response.message
        );
    }
    if !response.ok {
        bail!("the manager refused the message: {}", response.message);
    }
    Ok(())
}

/// The conversation after message `after` or around message `around`, as the other chats show
/// it (`chat::read_for`); `asks`, the open questions (docs/tasks.md "Log window"); `approvals`
/// and `to_check`, the ids this chat owns and shows (`chats::owner`).
pub(crate) fn chat(
    project: Option<&str>,
    chat: Option<&str>,
    (after, around): (Option<u64>, Option<u64>),
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let dir = match chat {
        None => ManagerDir::existing(&project_id)?,
        Some(chat) => Some(
            ManagerDir::existing_as(&project_id, Some(chat))?
                .with_context(|| format!("the project has no chat {chat}"))?,
        ),
    };
    let window = Window::new(None, after, around);
    let settings = load_task_config(&main).unwrap_or_default();
    let Reading {
        page,
        approvals,
        items,
    } = chat::read_for(
        dir.as_ref(),
        (&name, &project_id),
        &settings,
        &window,
        &|other: &str| other.to_string(),
        |_| {},
    )?;
    let asks: Vec<u64> = page.asks.iter().map(|ask| ask.id).collect();
    // ^ The owner messages a held manager has not taken into a turn, as the chats show them.
    let waiting = chat::waiting_ids(&page.log);
    if json {
        return print_json(&json!({
            "entries": page.entries, "skipped": page.skipped, "asks": asks, "waiting": waiting,
            "approvals": pending_approvals(&approvals).iter().map(|line| line.id()).collect::<Vec<_>>(),
            "to_check": items.iter().map(|item| &item.id).collect::<Vec<_>>(),
        }));
    }
    for row in &page.rows {
        match (&row.name, row.id) {
            (Some(name), Some(id)) => {
                let flag = if row.asks {
                    " (asks)"
                } else if row.queued {
                    " (waiting)"
                } else {
                    ""
                };
                println!("[{id}] {name}{flag}: {}", row.text)
            }
            _ => println!("({})", row.text),
        }
    }
    Ok(())
}

/// `wsx task search <words>`: ranked hits in one project or every registered one, each with
/// the reference to quote it by. ref: docs/tasks.md "Search"
pub(crate) fn search(
    project: Option<&str>,
    words: &str,
    task: Option<&str>,
    all: bool,
    limit: Option<usize>,
    reindex: bool,
    json: bool,
) -> Result<()> {
    let mut only = None;
    if let Some(task) = task {
        let dir = open(project, task)?;
        if words.trim().is_empty() {
            return history(&dir, limit.unwrap_or(HISTORY_LINES), json);
        }
        only = Some(dir.id);
    }
    let current = project_entry(project).ok().map(|(name, _)| name);
    let targets: Vec<search::Target> = match all {
        true => load_config()?
            .projects
            .iter()
            .map(|project| search::Target {
                name: project.name.clone(),
                project_id: store::project_id(&project.name, &project.path),
            })
            .collect(),
        false => {
            let (name, main) = project_entry(project)?;
            vec![search::Target {
                project_id: store::project_id(&name, &main),
                name,
            }]
        }
    };
    let started = Instant::now();
    let found = search::search(
        &targets,
        current.as_deref(),
        words,
        limit.unwrap_or(SEARCH_HITS),
        reindex,
        only,
    )?;
    if json {
        let mut value = serde_json::to_value(&found)?;
        value["ms"] = json!(started.elapsed().as_millis() as u64);
        return print_json(&value);
    }
    for hit in &found.hits {
        let reference = hit.quote.as_deref().map_or(hit.reference.clone(), |quote| {
            format!("{}  {quote}", hit.reference)
        });
        let outcome = hit
            .outcome
            .as_deref()
            .map_or(String::new(), |o| format!(" [{o}]"));
        println!(
            "{reference}{outcome}  {} {}  {}",
            text::day_of(hit.unix_ms),
            hit.by,
            hit.line
        );
    }
    for problem in &found.problems {
        eprintln!("skipped {problem}");
    }
    if found.hits.is_empty() {
        println!("nothing found in {} records", found.docs);
    }
    Ok(())
}

/// The lines of `wsx task search --task <n>` shown without `--limit`, and the hits of a search.
pub(crate) const HISTORY_LINES: usize = 40;

pub(crate) const SEARCH_HITS: usize = 10;

/// `wsx task search --task <n>`: the task's story as dated lines, newest last.
/// ref: docs/tasks.md "Search"
pub(crate) fn history(dir: &TaskDir, limit: usize, json: bool) -> Result<()> {
    let history = wsx_core::task::history::read(dir, limit)?;
    if json {
        return print_json(&serde_json::to_value(&history)?);
    }
    println!("t{} [{}]  {}", history.task, history.outcome, history.title);
    if history.skipped > 0 {
        println!(
            "({} earlier lines left out; --limit shows more)",
            history.skipped
        );
    }
    for entry in &history.entries {
        println!(
            "{} {:<8} {}",
            wsx_core::task::history::stamp(entry.unix_ms),
            entry.label,
            entry.text
        );
    }
    Ok(())
}

/// `wsx task dismiss <id>`: the owner's; the manager answers a question, it does not close it.
/// ref: docs/tasks.md "Manager"
pub(crate) fn dismiss(project: Option<&str>, id: u64, json: bool) -> Result<()> {
    owner_only("dismiss")?;
    let (name, main) = project_entry(project)?;
    let dir = ManagerDir::open(&store::project_id(&name, &main))?;
    chat::dismiss(&dir, id, wsx_core::task::now_unix_ms())?;
    let message = format!("reply {id} asks no more");
    if json {
        return print_json(&json!({ "ok": true, "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// `wsx task chat new|list|close`: the owner's. ref: docs/tasks.md "Chats"
pub(crate) fn chat_command(action: ChatCmd, project: Option<&str>, json: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    match action {
        ChatCmd::New { name, goal, agent } => {
            owner_only("chat new")?;
            let goal = goal.as_deref().map(read_goal).transpose()?;
            chats::create(&project_id, &name, goal.as_deref(), agent.as_deref())?;
            let message = format!("chat {name} is open; `wsx task say --to {name}` talks to it");
            if json {
                return print_json(&json!({ "chat": name, "state": "open", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        ChatCmd::List => {
            owner_only("chat list")?;
            let mut rows = chats::rows(&project_id);
            for row in &mut rows {
                let socket = ManagerDir::existing_as(&project_id, row["name"].as_str())?
                    .map(|dir| dir.socket_path());
                row["running"] = json!(socket.is_some_and(|socket| {
                    wsx_core::task::call_socket::<_, ManagerResponse>(
                        &socket,
                        &ManagerRequest::Status,
                        Duration::from_secs(2),
                    )
                    .is_ok()
                }));
            }
            if json {
                return print_json(&json!({ "chats": rows }));
            }
            for row in &rows {
                println!(
                    "{} {}{}",
                    row["name"].as_str().unwrap_or_default(),
                    row["state"].as_str().unwrap_or_default(),
                    row["agent"]
                        .as_str()
                        .map_or(String::new(), |agent| format!(" {agent}"))
                );
            }
            Ok(())
        }
        ChatCmd::Close { name } => {
            owner_only("chat close")?;
            chats::close(&project_id, &name)?;
            let message = format!("chat {name} is closed; its notes stay");
            if json {
                return print_json(&json!({ "chat": name, "state": "closed", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
    }
}

/// A chat's goal from a file, or `-` for standard input.
pub(crate) fn read_goal(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let limit = chats::MAX_GOAL_BYTES as u64 + 1;
    match path.as_os_str() == "-" {
        true => std::io::stdin().lock().take(limit).read_to_end(&mut bytes),
        false => std::fs::File::open(path)
            .with_context(|| format!("cannot read the goal {}", path.display()))?
            .take(limit)
            .read_to_end(&mut bytes),
    }
    .context("cannot read the goal")?;
    let goal = String::from_utf8(bytes).context("the goal is not UTF-8 text")?;
    chats::checked_goal(&goal)?;
    Ok(goal)
}

/// `wsx task manager`: start the project's manager, or say that it runs.
pub(crate) fn manager_command(project: Option<&str>, json: bool) -> Result<()> {
    let (started, dir) = ensure_manager(project)?;
    let state = if started { "started" } else { "running" };
    if json {
        let (project, main) = project_entry(project)?;
        let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
        let name = settings.manager_shown_name(&project);
        print_json(&json!({ "state": state, "socket": dir.socket_path(), "name": name }))
    } else {
        println!("manager {state}");
        Ok(())
    }
}

/// `wsx task boundary-profile`: the manager's Seatbelt profile (hidden; the boundary scenario).
pub(crate) fn boundary_profile(project: Option<&str>, untrusted: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    manager::print_profile(&name, &main, untrusted)
}
