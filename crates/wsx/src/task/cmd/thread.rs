//! `wsx task thread`: a chat's threads (docs/tasks.md "Threads").

use crate::task::args::ThreadCmd;
use crate::task::{actor, print_json, project_entry};
use anyhow::{bail, Context, Result};
use serde_json::json;
use wsx_core::task::store::{self, ManagerDir};
use wsx_core::task::{chats, now_unix_ms, thread};

/// The chat the command acts in: the acting chat, or the project's home chat for the owner.
fn chat_of(project: Option<&str>) -> Result<ManagerDir> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let home = store::home_chat(&project_id).unwrap_or_default();
    let chat = chats::owner(actor().chat(), &chats::open_names(), &home).to_string();
    ManagerDir::existing(&chat)?.with_context(|| format!("there is no chat {chat}"))
}

pub(crate) fn thread(action: ThreadCmd, project: Option<&str>, json: bool) -> Result<()> {
    let dir = chat_of(project)?;
    let message = match action {
        ThreadCmd::List => return list(&dir, json),
        ThreadCmd::Unplaced => return unplaced(&dir, json),
        ThreadCmd::Open { title } => format!("thread {} opened", thread::open(&dir, &title)?),
        ThreadCmd::Close { slug, why } => {
            format!(
                "thread {} closed",
                thread::close(&dir, &slug, &why, now_unix_ms())?
            )
        }
        ThreadCmd::Merge { from, into } => {
            thread::merge(&dir, &from, &into)?;
            format!("thread {from} merged into {into}")
        }
        ThreadCmd::Move { slug, tasks, asks } => {
            let tasks = tasks
                .iter()
                .map(|task| store::parse_task_number(task))
                .collect::<Result<Vec<u32>>>()?;
            if tasks.is_empty() && asks.is_empty() {
                bail!("name a task or --ask <reply id> to move");
            }
            let slug = thread::place(&dir, &slug, &tasks, &asks, now_unix_ms())?;
            format!(
                "{} task(s) and {} question(s) in thread {slug}",
                tasks.len(),
                asks.len()
            )
        }
    };
    if json {
        return print_json(&json!({ "chat": dir.name(), "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// The open threads first, then the closed ones, each with its title.
fn list(dir: &ManagerDir, json: bool) -> Result<()> {
    let threads = thread::Threads::read(dir)?;
    let rows = threads.listed();
    if json {
        return print_json(&json!({
            "chat": dir.name(),
            "open": threads.open_list().len(),
            "threads": rows.iter().map(|found| json!({
                "thread": found.slug,
                "title": found.title,
                "open": found.open,
                "merged_into": found.merged_into,
                "opened_ms": found.opened_ms,
            })).collect::<Vec<_>>(),
        }));
    }
    println!("{} open in chat {}", threads.open_list().len(), dir.name());
    for found in rows {
        let state = match (&found.merged_into, found.open) {
            (Some(into), _) => format!("merged into {into}"),
            (None, true) => "open".into(),
            (None, false) => "closed".into(),
        };
        println!("{}  \"{}\"  {state}", found.slug, found.title);
    }
    Ok(())
}

/// The open work of the chat that is in no thread, as the placing turn lists it.
fn unplaced(dir: &ManagerDir, json: bool) -> Result<()> {
    let found = thread::unplaced(dir, now_unix_ms())?;
    if json {
        return print_json(&json!({
            "chat": dir.name(),
            "tasks": found.tasks.iter().map(|(task, title)| json!({ "task": task, "title": title })).collect::<Vec<_>>(),
            "asks": found.asks.iter().map(|(id, quote)| json!({ "id": id, "quote": quote })).collect::<Vec<_>>(),
            "approvals": found.approvals.iter().map(|(id, task, label)| json!({ "id": id, "task": task, "label": label })).collect::<Vec<_>>(),
            "checks": found.checks.iter().map(|(id, task, check)| json!({ "id": id, "task": task, "check": check })).collect::<Vec<_>>(),
        }));
    }
    match found.is_empty() {
        true => println!("nothing open is outside a thread in chat {}", dir.name()),
        false => println!("{}", found.text()),
    }
    Ok(())
}
