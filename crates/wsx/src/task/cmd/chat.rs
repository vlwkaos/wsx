//! The chats: `say`, `chat`, `search` (docs/tasks.md "Manager", "Chats").

use super::approve::owner_only;
use crate::cli::load_config;
use crate::cli::PromptSource;
use crate::task::args::ChatCmd;
use crate::task::manager;
use crate::task::{acting_as_manager, actor, launch_manager, open, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::chat::{self, Reading, Window};
use wsx_core::task::chats::{self, ChatFrom};
use wsx_core::task::manager::{
    pending_approvals, Author, ManagerRequest, ManagerResponse, Party, Short,
};
use wsx_core::task::owner::{self};
use wsx_core::task::store::ManagerDir;
use wsx_core::task::store::{self, TaskDir};
use wsx_core::task::MAX_MESSAGE_BYTES;
use wsx_core::task::{search, text};

pub(crate) fn ensure_manager(project: Option<&str>) -> Result<(bool, ManagerDir)> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_manager(&name, &main, &exe, None, launch_manager(&main))
}

/// Start the repo's service unless it runs.
pub(crate) fn ensure_repo(project: Option<&str>) -> Result<bool> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_repo(&name, &main, &exe, launch_manager(&main))
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
    let project_id = store::project_id(&name, &main);
    let from = match from_chat.then_some(reply) {
        None => None,
        Some(reply) => {
            let sender = actor()
                .chat()
                .filter(|_| acting_as_manager())
                .context("--from-chat is for a chat's agent")?
                .to_string();
            // ^ Only a home chat reaches another project's home chat (broker.rs).
            let own = ManagerDir::existing(&sender)?.map(|dir| dir.project_id);
            let own_repo = own.as_deref() == Some(project_id.as_str());
            let target = match (to.filter(|to| *to != "manager"), own_repo) {
                (Some(to), true) => to.to_string(),
                (None, _) => store::home_chat(&project_id).unwrap_or_default(),
                (Some(_), false) => {
                    bail!("--from-chat reaches another project's home chat, with no --to")
                }
            };
            if sender == target {
                bail!("--from-chat messages another chat, not itself");
            }
            Some(Author::Peer {
                from: Party::Chat(sender),
                reply,
            })
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
    let dir =
        ManagerDir::existing_in(&project_id, chat)?.filter(|dir| dir.project_id == project_id);
    if chat.is_some() && dir.is_none() {
        bail!("the project has no chat {}", chat.unwrap_or_default());
    }
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
    let dir = ManagerDir::open_home(&store::project_id(&name, &main))?;
    chat::dismiss(&dir, id, wsx_core::task::now_unix_ms())?;
    let message = format!("reply {id} asks no more");
    if json {
        return print_json(&json!({ "ok": true, "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// `wsx task chat new|list|close|home`: the owner's; a chat's agent opens and closes a chat when
/// the owner's message of its turn asks (`--owner-said`, as for `land`). ref: docs/tasks.md "Chats"
pub(crate) fn chat_command(action: ChatCmd, project: Option<&str>, json: bool) -> Result<()> {
    match action {
        ChatCmd::New {
            name,
            goal,
            agent,
            said,
        } => {
            let ((project, main), from) = lifecycle("chat new", project, said.owner_said)?;
            let project_id = store::project_id(&project, &main);
            let goal = goal.as_deref().map(read_goal).transpose()?;
            chats::create(
                &project_id,
                &name,
                goal.as_deref(),
                agent.as_deref(),
                from.clone(),
            )?;
            // ^ The first row of a chat that a chat's agent opened links to the message that asked.
            if let Some(from) = from {
                let short = Short::event(
                    format!("Opened from chat {}, message {}", from.chat, from.message),
                    false,
                );
                let text = format!("Opened from chat {}, message {}.", from.chat, from.message);
                let exe = std::env::current_exe().context("cannot find the wsx binary")?;
                owner::say(
                    &project,
                    &main,
                    &exe,
                    Some(&name),
                    launch_manager(&main),
                    text,
                    Some(Author::Events),
                    vec![short],
                )?;
            }
            let message = format!("chat {name} is open; `wsx task say --to {name}` talks to it");
            if json {
                return print_json(&json!({ "chat": name, "state": "open", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        ChatCmd::List => {
            owner_only("chat list")?;
            let (name, main) = project_entry(project)?;
            let mut rows = chats::rows(&store::project_id(&name, &main));
            for row in &mut rows {
                let socket = row["name"]
                    .as_str()
                    .map(ManagerDir::existing)
                    .transpose()?
                    .flatten()
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
                    "{} {}{}{}",
                    row["name"].as_str().unwrap_or_default(),
                    row["state"].as_str().unwrap_or_default(),
                    row["agent"]
                        .as_str()
                        .map_or(String::new(), |agent| format!(" {agent}")),
                    if row["home"].as_bool() == Some(true) {
                        " home"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        ChatCmd::Close { name, said } => {
            let ((project, main), _) = lifecycle("chat close", project, said.owner_said)?;
            chat_of_repo(&name, &store::project_id(&project, &main))?;
            let exe = std::env::current_exe().context("cannot find the wsx binary")?;
            owner::close_chat((&project, &main), &exe, &name, launch_manager(&main))?;
            let message = format!(
                "chat {name} is closed; its notes stay, its open tasks are the home chat's"
            );
            if json {
                return print_json(&json!({ "chat": name, "state": "closed", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        ChatCmd::Rename { name, title } => {
            owner_only("chat rename")?;
            let (project, main) = project_entry(project)?;
            chat_of_repo(&name, &store::project_id(&project, &main))?;
            let title = chats::rename(&name, &title)?;
            let message = match title.is_empty() {
                true => format!("chat {name} is shown by its id again"),
                false => format!("chat {name} is shown as {title:?}; its id stays {name}"),
            };
            if json {
                return print_json(&json!({ "chat": name, "title": title, "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        ChatCmd::Home { name } => {
            owner_only("chat home")?;
            let (project, main) = project_entry(project)?;
            let project_id = store::project_id(&project, &main);
            chat_of_repo(&name, &project_id)?;
            store::set_home_chat(&project_id, &name)?;
            let message = format!("chat {name} is the home chat of {project}; the hosts restart at their next message");
            if json {
                return print_json(&json!({ "chat": name, "home": true, "message": message }));
            }
            println!("{message}");
            Ok(())
        }
    }
}

/// The repo and the origin of a chat lifecycle command: the owner's own `-p` with no origin; a
/// chat's agent acts in its own repo on the owner's message of its turn, which is the origin of
/// a chat it opens. ref: docs/tasks.md "Chats"
fn lifecycle(
    command: &str,
    project: Option<&str>,
    said: Option<u64>,
) -> Result<((String, PathBuf), Option<ChatFrom>)> {
    if !acting_as_manager() {
        return Ok((project_entry(project)?, None));
    }
    let asker = actor().chat().map(str::to_string).unwrap_or_default();
    let said = crate::task::owner_said(said).with_context(|| {
        format!(
            "wsx task {command} is the owner's; a chat's agent runs it when the owner's message of this turn asks: pass --owner-said <id>; nothing was done"
        )
    })?;
    let own = ManagerDir::existing(&asker)?.context("the asking chat has no folder")?;
    owner::check_owner_said(&own.project_id, Some(&asker), said)?;
    let entry = crate::task::project_by_id(&own.project_id)?;
    Ok((
        entry,
        Some(ChatFrom {
            chat: asker,
            message: said,
        }),
    ))
}

/// The open chat `name`, which must run in the repo `project_id`.
fn chat_of_repo(name: &str, project_id: &str) -> Result<()> {
    match chats::open_chat(name)?.project_id == project_id {
        true => Ok(()),
        false => bail!("chat {name} runs in another project"),
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
