//! The manager and role chats: `say`, `chat`, `search`, `role` (docs/tasks.md "Manager", "Roles").

use super::approve::{ask_owner, owner_only, request_of};
use crate::cli::load_config;
use crate::cli::PromptSource;
use crate::task::args::RoleCmd;
use crate::task::broker;
use crate::task::manager;
use crate::task::{
    acting_as_manager, actor, launch_manager, not_for_roles, open, print_json, project_entry,
};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::chat::{self, chat_rows, Names, Window};
use wsx_core::task::manager::{ApprovalAction, ManagerEvent, ManagerRequest, ManagerResponse};
use wsx_core::task::owner::{self};
use wsx_core::task::roles;
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

/// Who sends a message with `say`, besides the owner; each with whether it is a reply.
pub(crate) enum From {
    /// `--from-manager`: this manager to another project's.
    Manager(bool),
    /// `--from-role`: the manager or a role to a role or the manager of its own project.
    Role(bool),
}

/// Send to the manager, or to the open role `role`.
pub(crate) fn say(
    project: Option<&str>,
    role: Option<&str>,
    text: String,
    from: Option<From>,
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    // ^ Only a manager host sets the sender's project (broker.rs).
    let own = || {
        std::env::var(broker::FROM_ENV)
            .ok()
            .filter(|own| acting_as_manager() && !own.is_empty())
    };
    let from = match from {
        None => None,
        Some(From::Manager(reply)) => {
            let own = own()
                .filter(|_| actor() == Actor::Manager)
                .context("--from-manager is for a project's manager")?;
            if own == name {
                bail!("--from-manager messages another project's manager");
            }
            Some(wsx_core::task::manager::Author::Manager {
                project: own,
                reply,
            })
        }
        Some(From::Role(reply)) => {
            let own = own().context("--from-role is for the manager or a role")?;
            if own != name {
                bail!("--from-role messages a role or the manager of its own project");
            }
            let sender = match actor() {
                Actor::Role(sender) => sender,
                _ => "manager".to_string(),
            };
            if sender == role.unwrap_or("manager") {
                bail!("--from-role messages another role or the manager, not itself");
            }
            Some(wsx_core::task::manager::Author::Role {
                name: sender,
                reply,
            })
        }
    };
    // ^ `--role manager` is the manager, as a role names it in its reply.
    let role = role.filter(|role| *role != "manager");
    let exe = std::env::current_exe().context("cannot find the wsx binary")?;
    let sent = from.is_some().then(|| text.clone());
    let response = owner::say(
        &name,
        &main,
        &exe,
        role,
        launch_manager(&main),
        text,
        from,
        Vec::new(),
    )?;
    // ^ A manager's or a role's message always answers in JSON: its host logs `sent` from
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

/// The conversation after message `after`, around message `around`, or from the oldest entry that
/// shows `find`: messages, replies, rotations and stops; `asks`, the open questions (docs/tasks.md "Log window").
pub(crate) fn chat(
    project: Option<&str>,
    role: Option<&str>,
    (after, find, around): (Option<u64>, Option<&str>, Option<u64>),
    json: bool,
) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let dir = match role {
        None => ManagerDir::open(&project_id)?,
        Some(role) => ManagerDir::existing_as(&project_id, Some(role))?
            .with_context(|| format!("the project has no role {role}"))?,
    };
    let window = Window::new(find, after, around);
    let now = wsx_core::task::now_unix_ms();
    let shown = load_task_config(&main)
        .unwrap_or_default()
        .manager_shown_name(&name)
        .to_string();
    let own = role.unwrap_or(&shown);
    let names = Names {
        own,
        manager: &shown,
        peer: &|other: &str| other.to_string(),
    };
    let page = chat::page(&dir, &window, &names, &[], now, |_| {})?;
    let asks: Vec<u64> = page.asks.iter().map(|ask| ask.id).collect();
    if json {
        return print_json(
            &json!({ "entries": page.entries, "skipped": page.skipped, "asks": asks }),
        );
    }
    for entry in &page.entries {
        match &entry.event {
            ManagerEvent::Message { id, from, text, .. } => println!("[{id}] {from:?}: {text}"),
            ManagerEvent::Reply { id, text, .. } if asks.contains(id) => {
                println!("[{id}] manager (asks): {text}")
            }
            ManagerEvent::Reply { id, text, .. } => println!("[{id}] manager: {text}"),
            // ^ A pause, a new conversation and a stop read as the other chats say them.
            ManagerEvent::Limit { .. }
            | ManagerEvent::Rotated { .. }
            | ManagerEvent::Stopped { .. } => {
                for row in chat_rows(&[entry], &[], &[], &[], &names) {
                    println!("({})", row.text);
                }
            }
            _ => {}
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

/// `wsx task role new|list|close`. ref: docs/tasks.md "Roles"
pub(crate) fn role(action: RoleCmd) -> Result<()> {
    let open_project = |project: Option<&str>| -> Result<(String, String, PathBuf)> {
        let (name, main) = project_entry(project)?;
        let project_id = store::project_id(&name, &main);
        // ^ A role idle past tasks.roles.closeDays is closed before it is counted or listed.
        if let Ok(settings) = load_task_config(&main) {
            roles::close_idle(&project_id, settings.roles_close_secs);
        }
        Ok((name, project_id, main))
    };
    match action {
        RoleCmd::New {
            name,
            brief,
            said,
            project,
            json,
        } => {
            not_for_roles("role new")?;
            let (_, project_id, main) = open_project(project.as_deref())?;
            let settings = load_task_config(&main)
                .map_err(|error| anyhow::anyhow!("{error}; no role was opened"))?;
            roles::may_create(&project_id, &name, settings.roles_max)?;
            let brief = read_brief(&brief)?;
            // ^ The manager asks the owner once: a Yes/No card, or the owner's own word.
            if actor() == Actor::Manager
                && ask_owner(
                    &project_id,
                    request_of(
                        ApprovalAction::Role,
                        None,
                        None,
                        Some((&name, &brief)),
                        None,
                    ),
                    said.owner_said,
                    json,
                )?
            {
                return Ok(());
            }
            roles::create(&project_id, &name, &brief)?;
            let message = format!("role {name} is open; `wsx task say --role {name}` talks to it");
            if json {
                return print_json(&json!({ "role": name, "state": "open", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
        RoleCmd::List { project, json } => {
            let (_, project_id, _) = open_project(project.as_deref())?;
            let lines = store::read_approvals(&project_id)?;
            let mut rows = roles::rows(&project_id, &lines);
            for row in rows.iter_mut().filter(|row| row["state"] != "asked") {
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
                return print_json(&json!({ "roles": rows }));
            }
            for row in &rows {
                println!(
                    "{} {}",
                    row["name"].as_str().unwrap_or_default(),
                    row["state"].as_str().unwrap_or_default()
                );
            }
            Ok(())
        }
        RoleCmd::Close {
            name,
            project,
            json,
        } => {
            not_for_roles("role close")?;
            let (_, project_id, _) = open_project(project.as_deref())?;
            let dir = roles::open_role(&project_id, &name)?;
            roles::close(&dir, &format!("closed by the {}", String::from(actor())))?;
            let message = format!("role {name} is closed; its notes stay");
            if json {
                return print_json(&json!({ "role": name, "state": "closed", "message": message }));
            }
            println!("{message}");
            Ok(())
        }
    }
}

/// A role's brief from a file, or `-` for standard input.
pub(crate) fn read_brief(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let limit = roles::MAX_BRIEF_BYTES as u64 + 1;
    match path.as_os_str() == "-" {
        true => std::io::stdin().lock().take(limit).read_to_end(&mut bytes),
        false => std::fs::File::open(path)
            .with_context(|| format!("cannot read the brief {}", path.display()))?
            .take(limit)
            .read_to_end(&mut bytes),
    }
    .context("cannot read the brief")?;
    let brief = String::from_utf8(bytes).context("the brief is not UTF-8 text")?;
    roles::checked_brief(&brief)?;
    Ok(brief)
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

/// `wsx task say`: to the manager or a role, from the owner, a manager or a role.
pub(crate) fn say_command(
    project: Option<&str>,
    role: Option<&str>,
    text: PromptSource,
    (from_manager, from_role, reply): (bool, bool, bool),
    json: bool,
) -> Result<()> {
    if reply && !from_manager && !from_role {
        bail!("--reply answers with --from-manager or --from-role");
    }
    let from = match (from_manager, from_role) {
        (true, _) => Some(From::Manager(reply)),
        (_, true) => Some(From::Role(reply)),
        _ => None,
    };
    say(project, role, text.read(MAX_MESSAGE_BYTES)?, from, json)
}

/// `wsx task boundary-profile`: the manager's Seatbelt profile (hidden; the boundary scenario).
pub(crate) fn boundary_profile(project: Option<&str>) -> Result<()> {
    let (name, main) = project_entry(project)?;
    manager::print_profile(&name, &main)
}
