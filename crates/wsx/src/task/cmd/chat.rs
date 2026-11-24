//! The chats: `say`, `chat`, `search` (docs/tasks.md "Chat host", "Chats").

use super::approve::owner_only;
use crate::cli::load_config;
use crate::cli::PromptSource;
use crate::runtime_util::print_json;
use crate::task::args::ChatCmd;
use crate::task::chat_runner;
use crate::task::{acting_as_chat, actor, launch_chat, open, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::memory::{backend, index::Target};
use wsx_core::task::chat::{self, Reading, Window};
use wsx_core::task::chat_host::{
    pending_approvals, Author, ChatEvent, ChatRequest, ChatResponse, Party,
};
use wsx_core::task::chats;
use wsx_core::task::owner::{self};
use wsx_core::task::store::ChatDir;
use wsx_core::task::store::{self, TaskDir};
use wsx_core::task::text;
use wsx_core::task::MAX_MESSAGE_BYTES;

pub(crate) fn ensure_lead(project: Option<&str>) -> Result<(bool, ChatDir)> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_lead(&name, &main, &exe, None, launch_chat(&main))
}

/// Start the repo's service unless it runs.
pub(crate) fn ensure_repo(project: Option<&str>) -> Result<bool> {
    let (name, main) = project_entry(project)?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    owner::ensure_repo(&name, &main, &exe, launch_chat(&main))
}

/// `wsx chat say --uses <dep>`: one message to the main chat of every registered project whose
/// lockfile or manifest names `dep` (`deps::uses`), from the owner, or with `from_chat` from the
/// acting chat, which it does not reach itself. Each lead answers it or forks what it asks of its
/// project ("Lead and forks"). `-p` names none: every project is looked at.
/// ref: docs/tasks.md "Mail"
pub(crate) fn mail_command(
    project: Option<&str>,
    dep: &str,
    text: PromptSource,
    from_chat: bool,
    json: bool,
) -> Result<()> {
    let dep = wsx_core::task::deps::checked_dep(dep)?;
    if project.is_some() {
        bail!("--uses looks at every registered project; drop -p");
    }
    let dep = dep.trim();
    if dep.is_empty() {
        bail!("--uses needs a dependency name");
    }
    let text = text.read(MAX_MESSAGE_BYTES)?;
    let sender = match from_chat {
        false => None,
        true => Some(
            actor()
                .chat()
                .filter(|_| acting_as_chat())
                .context("--from-chat is for a chat's agent")?
                .to_string(),
        ),
    };
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let mut rows = Vec::new();
    for registered in crate::cli::load_config()?.projects {
        let Some(file) = wsx_core::task::deps::uses(&registered.path, dep) else {
            continue;
        };
        let project_id = store::project_id(&registered.name, &registered.path);
        let target = chats::open_main(&project_id).map(|dir| dir.chat);
        let sent = target.and_then(|target| {
            if sender.as_deref() == Some(target.as_str()) {
                bail!("it is the sending chat");
            }
            // ^ The owner's mail names its chat: `Owner`, not `None`, so no router moves it.
            let from = Some(sender.clone().map_or(Author::Owner, |chat| Author::Peer {
                from: Party::Chat(chat),
                reply: false,
            }));
            let response = owner::say(
                &registered.name,
                &registered.path,
                &exe,
                Some(&target),
                launch_chat(&registered.path),
                format!("To the projects that use {dep} ({file} here):\n{text}"),
                from,
                Vec::new(),
            )?;
            anyhow::ensure!(response.ok, "{}", response.message);
            Ok((target, response.id))
        });
        rows.push(match sent {
            Ok((chat, id)) => {
                json!({"project": registered.name, "file": file, "chat": chat, "id": id})
            }
            Err(error) => {
                json!({"project": registered.name, "file": file, "skipped": format!("{error:#}")})
            }
        });
    }
    if json || sender.is_some() {
        return print_json(&json!({"uses": dep, "projects": rows}));
    }
    if rows.is_empty() {
        println!("no registered project names {dep}");
    }
    for row in &rows {
        match row["skipped"].as_str() {
            Some(why) => println!(
                "{}: skipped: {why}",
                row["project"].as_str().unwrap_or_default()
            ),
            None => println!(
                "{}: message {} to {} ({})",
                row["project"].as_str().unwrap_or_default(),
                row["id"],
                row["chat"].as_str().unwrap_or_default(),
                row["file"].as_str().unwrap_or_default()
            ),
        }
    }
    Ok(())
}

/// `wsx chat say`: to the lead, or to the open chat `to`, from the owner or a chat.
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
                .filter(|_| acting_as_chat())
                .context("--from-chat is for a chat's agent")?
                .to_string();
            // ^ A chat reaches another project's main chat (broker.rs, policy.rs).
            let own = ChatDir::existing(&sender)?.map(|dir| dir.project_id);
            let own_repo = own.as_deref() == Some(project_id.as_str());
            let target = match (to.filter(|to| *to != "manager"), own_repo) {
                (Some(to), true) => to.to_string(),
                (None, _) => chats::open_main(&project_id)?.chat,
                (Some(_), false) => {
                    bail!("--from-chat reaches another project's main chat, with no --to")
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
        launch_chat(&main),
        text,
        from,
        Vec::new(),
    )?;
    // ^ A chat's message always answers in JSON: its host logs `sent` from
    // the `text` (broker.rs). ref: docs/tasks.md "Mail"
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
        bail!("the chat refused the message: {}", response.message);
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
    let dir = ChatDir::existing_in(&project_id, chat)?.filter(|dir| dir.project_id == project_id);
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
    // ^ The owner messages a held lead has not taken into a turn, as the chats show them.
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

/// `wsx chat search <words>`: ranked hits in one project or every registered one, each with
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
    let targets: Vec<Target> = match all {
        true => load_config()?
            .projects
            .iter()
            .map(|project| Target {
                name: project.name.clone(),
                project_id: store::project_id(&project.name, &project.path),
            })
            .collect(),
        false => {
            let (name, main) = project_entry(project)?;
            vec![Target {
                project_id: store::project_id(&name, &main),
                name,
            }]
        }
    };
    let started = Instant::now();
    let found = backend::lines(
        &backend::Engines::configured(),
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
        let said = |key: &str| hit.extra.get(key).and_then(Value::as_str);
        let reference =
            said("quote").map_or(hit.slug.clone(), |quote| format!("{}  {quote}", hit.slug));
        let outcome = said("outcome").map_or(String::new(), |o| format!(" [{o}]"));
        println!(
            "{reference}{outcome}  {} {}  {}",
            text::day_of(
                hit.extra
                    .get("unix_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            said("by").unwrap_or_default(),
            said("line").unwrap_or_default()
        );
    }
    for problem in &found.problems {
        eprintln!("skipped {problem}");
    }
    for notice in &found.notices {
        eprintln!("wsx: {notice}");
    }
    if found.hits.is_empty() {
        println!("nothing found in {} records", found.docs);
    }
    Ok(())
}

/// The lines of `wsx chat search --task <n>` shown without `--limit`, and the hits of a search.
pub(crate) const HISTORY_LINES: usize = 40;

pub(crate) const SEARCH_HITS: usize = 10;

/// `wsx chat search --task <n>`: the task's story as dated lines, newest last.
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

/// `wsx chat dismiss <id>`: the owner's; the lead answers a question, it does not close it.
/// ref: docs/tasks.md "Chat host"
pub(crate) fn dismiss(project: Option<&str>, id: u64, json: bool) -> Result<()> {
    owner_only("chat dismiss")?;
    let (name, main) = project_entry(project)?;
    let dir = chats::open_main(&store::project_id(&name, &main))?;
    chat::dismiss(&dir, id, wsx_core::task::now_unix_ms())?;
    let message = format!("reply {id} asks no more");
    if json {
        return print_json(&json!({ "ok": true, "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// `wsx chat new|list|close|goal|main`: the owner's; a chat's agent opens and closes a chat when
/// the owner's message of its turn asks (`--owner-said`, as for `land`). ref: docs/tasks.md "Chats"
pub(crate) fn chat_command(action: ChatCmd, project: Option<&str>, json: bool) -> Result<()> {
    match action {
        ChatCmd::New {
            name,
            goal,
            agent,
            brief,
            role,
            said,
        } => {
            // ^ Only a chat forks: the owner's own --brief is refused before anything is read.
            if brief && !acting_as_chat() {
                bail!("--brief forks the owner's message of the lead's turn: the lead's agent passes it with --owner-said; nothing was done");
            }
            let (project, main) = lifecycle("chat new", project, said.owner_said)?;
            let project_id = store::project_id(&project, &main);
            let goal = goal.as_deref().map(read_goal).transpose()?;
            if brief {
                return fork(
                    &project,
                    &main,
                    (&name, agent.as_deref()),
                    said.owner_said,
                    json,
                );
            }
            // ^ A role is a profile: its purpose and paths open the chat's goal, its agent is the
            // chat's unless --agent names one.
            let (goal, agent) = match role {
                None => (goal, agent),
                Some(role) => {
                    let settings = load_task_config(&main)
                        .map_err(|error| anyhow::anyhow!("{error}; nothing was done"))?;
                    let found = settings.roles.get(&role).with_context(|| {
                        let known: Vec<&str> = settings.roles.keys().map(String::as_str).collect();
                        format!(
                            "no role {role} in tasks.roles (it has: {}); nothing was done",
                            known.join(", ")
                        )
                    })?;
                    let mut text = format!(
                        "Role {role}: you own {}. Pass them as `--files` to the tasks you open; send work outside them to the project's lead.",
                        found.paths.join(", ")
                    );
                    if !found.purpose.is_empty() {
                        text = format!("{text}\n{}", found.purpose);
                    }
                    if let Some(goal) = goal.filter(|goal| !goal.trim().is_empty()) {
                        text = format!("{text}\n\n{goal}");
                    }
                    (Some(text), agent.or_else(|| found.agent.clone()))
                }
            };
            chats::create(&project_id, &name, goal.as_deref(), agent.as_deref())?;
            let message = format!("chat {name} is open; `wsx chat say --to {name}` talks to it");
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
                    .map(ChatDir::existing)
                    .transpose()?
                    .flatten()
                    .map(|dir| dir.socket_path());
                row["running"] = json!(socket.is_some_and(|socket| {
                    wsx_core::task::call_socket::<_, ChatResponse>(
                        &socket,
                        &ChatRequest::Status,
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
                    if row["main"].as_bool() == Some(true) {
                        " main"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        ChatCmd::Close { name, said } => {
            let (project, main) = lifecycle("chat close", project, said.owner_said)?;
            chat_of_repo(&name, &store::project_id(&project, &main))?;
            let exe = std::env::current_exe().context("cannot find the wsx binary")?;
            let closed = owner::close_chat((&project, &main), &exe, &name, launch_chat(&main))?;
            let message = match closed {
                owner::Closed::Handed => format!("chat {name} is closed; its notes stay, its open tasks are the main chat's"),
                owner::Closed::NoMain => format!("chat {name} is closed; its notes stay. {project} has no main chat now: its open tasks have no chat"),
                owner::Closed::ForkUndone => format!("chat {name} is closed before it owned a task: the fork is undone"),
            };
            if json {
                return print_json(&json!({ "chat": name, "state": "closed", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        ChatCmd::Goal { name, goal, said } => {
            let Some(path) = goal else {
                owner_only("chat goal")?;
                let (project, main) = project_entry(project)?;
                let goal = chats::goal(&chat_of_repo(&name, &store::project_id(&project, &main))?);
                if json {
                    return print_json(&json!({ "chat": name, "goal": goal }));
                }
                println!("{goal}");
                return Ok(());
            };
            let (project, main) = lifecycle("chat goal", project, said.owner_said)?;
            chat_of_repo(&name, &store::project_id(&project, &main))?;
            let goal = read_goal(&path)?;
            chats::set_goal(&name, &goal)?;
            let message = match goal.trim().is_empty() {
                true => {
                    format!("chat {name} has no purpose now; its agent reads that at its next turn")
                }
                false => {
                    format!("chat {name} has a new purpose; its agent reads it at its next turn")
                }
            };
            if json {
                return print_json(&json!({ "chat": name, "goal": goal, "message": message }));
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
        ChatCmd::Main { name, said } => {
            let (project, main) = lifecycle("chat main", project, said.owner_said)?;
            chats::set_main(&name, &store::project_id(&project, &main))?;
            let message = format!("chat {name} is the main chat of {project}");
            if json {
                return print_json(&json!({ "chat": name, "main": true, "message": message }));
            }
            println!("{message}");
            Ok(())
        }
    }
}

/// `chat new --brief`: the lead (the main chat with no goal) forks the owner's message `said` of
/// its turn into the new chat `name`, which starts on it as the owner's own message (its first
/// turn may act on it). The text and the goal (its first line) come from the lead's log, never
/// from the agent. A name used before is refused: it would reopen that chat's log. A message
/// forks once. ref: docs/tasks.md "Lead and forks"
fn fork(
    project: &str,
    main: &Path,
    (name, agent): (&str, Option<&str>),
    said: Option<u64>,
    json: bool,
) -> Result<()> {
    let asker = actor().chat().map(str::to_string);
    let (Some(asker), Some(said)) = (asker, said) else {
        bail!("--brief forks the owner's message of the lead's turn; nothing was done");
    };
    let origin = ChatDir::existing(&asker)?.context("the asking chat has no folder")?;
    if !chats::is_lead(&origin) {
        bail!("only the lead forks: the project's main chat with no goal; nothing was done");
    }
    let name = chats::checked_name(name)?;
    let log = origin.read_log()?;
    if log
        .iter()
        .any(|entry| matches!(&entry.event, ChatEvent::Forked { message, .. } if *message == said))
    {
        bail!("message {said} was forked already; nothing was done");
    }
    let text = log
        .into_iter()
        .find_map(|entry| match entry.event {
            ChatEvent::Message {
                id,
                from: Author::Owner,
                text,
                ..
            } if id == said => Some(text),
            _ => None,
        })
        .with_context(|| format!("there is no owner message {said}; nothing was done"))?;
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let (goal, brief_id) = owner::fork(
        (project, main),
        &exe,
        launch_chat(main),
        &origin,
        said,
        text,
        name,
        agent,
    )?;
    let message = format!("forked message {said} to chat {name}; it starts on it");
    if json {
        return print_json(&json!({
            "chat": name, "state": "open", "forked_from": asker, "message_id": said,
            "brief_id": brief_id, "goal": goal, "message": message,
        }));
    }
    println!("{message}");
    Ok(())
}

/// The repo of a chat lifecycle command: the owner's own `-p`; a chat's agent acts in its own
/// repo on the owner's message of its turn. ref: docs/tasks.md "Chats"
fn lifecycle(command: &str, project: Option<&str>, said: Option<u64>) -> Result<(String, PathBuf)> {
    if !acting_as_chat() {
        return project_entry(project);
    }
    let asker = actor().chat().map(str::to_string).unwrap_or_default();
    let said = crate::task::owner_said(said).with_context(|| {
        format!(
            "wsx {command} is the owner's; a chat's agent runs it when the owner's message of this turn asks: pass --owner-said <id>; nothing was done"
        )
    })?;
    let own = ChatDir::existing(&asker)?.context("the asking chat has no folder")?;
    owner::check_owner_said(&own.project_id, Some(&asker), said)?;
    crate::task::project_by_id(&own.project_id)
}

/// The open chat `name`, which must run in the repo `project_id`.
pub(super) fn chat_of_repo(name: &str, project_id: &str) -> Result<ChatDir> {
    match chats::open_chat(name)? {
        dir if dir.project_id == project_id => Ok(dir),
        _ => bail!("chat {name} runs in another project"),
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

/// `wsx chat lead`: start the project's lead, or say that it runs.
pub(crate) fn lead_command(project: Option<&str>, json: bool) -> Result<()> {
    let (started, dir) = ensure_lead(project)?;
    let state = if started { "started" } else { "running" };
    if json {
        let (project, main) = project_entry(project)?;
        let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
        let name = settings.lead_shown_name(&project);
        print_json(&json!({ "state": state, "socket": dir.socket_path(), "name": name }))
    } else {
        println!("lead {state}");
        Ok(())
    }
}

/// `wsx task boundary-profile`: the lead's Seatbelt profile (hidden; the boundary scenario).
pub(crate) fn boundary_profile(project: Option<&str>, untrusted: bool) -> Result<()> {
    let (name, main) = project_entry(project)?;
    chat_runner::print_profile(&name, &main, untrusted)
}
