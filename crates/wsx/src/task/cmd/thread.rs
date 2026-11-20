//! `wsx task thread`: a chat's threads (docs/tasks.md "Threads").

use crate::task::args::ThreadCmd;
use crate::task::{actor, print_json, project_entry};
use anyhow::{Context, Result};
use serde_json::json;
use wsx_core::config::project::load_task_config;
use wsx_core::task::store::{self, ManagerDir};
use wsx_core::task::{chats, now_unix_ms, thread};

/// The chat the command acts in: the acting chat, or the project's home chat for the owner.
fn chat_of(project: Option<&str>) -> Result<(ManagerDir, bool)> {
    let (name, main) = project_entry(project)?;
    let land_auto = load_task_config(&main).is_ok_and(|settings| settings.land_auto);
    let project_id = store::project_id(&name, &main);
    let home = store::home_chat(&project_id).unwrap_or_default();
    let chat = chats::owner(actor().chat(), &chats::open_names(), &home).to_string();
    let dir = ManagerDir::existing(&chat)?.with_context(|| format!("there is no chat {chat}"))?;
    Ok((dir, land_auto))
}

pub(crate) fn thread(action: ThreadCmd, project: Option<&str>, json: bool) -> Result<()> {
    let (dir, land_auto) = chat_of(project)?;
    let message = match action {
        ThreadCmd::List => return list(&dir, land_auto, json),
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
    };
    if json {
        return print_json(&json!({ "chat": dir.name(), "message": message }));
    }
    println!("{message}");
    Ok(())
}

/// The open threads in the order of `thread::list` (those that wait for you first, then by last
/// activity), each with what waits, then the closed ones.
fn list(dir: &ManagerDir, land_auto: bool, json: bool) -> Result<()> {
    let threads = thread::Threads::read(dir)?;
    let open = thread::list(dir, land_auto)?;
    let closed: Vec<_> = threads
        .listed()
        .into_iter()
        .filter(|found| !found.open)
        .collect();
    if json {
        let rows = open.iter().map(|found| {
            json!({
                "thread": found.slug,
                "title": found.title,
                "open": true,
                "merged_into": null,
                "opened_ms": found.opened_ms,
                "last_ms": found.last_ms,
                "counts": found.counts,
                "waits": found.waits,
            })
        });
        let rows = rows.chain(closed.iter().map(|found| {
            json!({
                "thread": found.slug,
                "title": found.title,
                "open": false,
                "merged_into": found.merged_into,
                "opened_ms": found.opened_ms,
            })
        }));
        return print_json(&json!({
            "chat": dir.name(),
            "open": open.len(),
            "threads": rows.collect::<Vec<_>>(),
        }));
    }
    println!("{} open in chat {}", open.len(), dir.name());
    for found in &open {
        match found.waits.is_empty() {
            true => println!("{}  \"{}\"  open", found.slug, found.title),
            false => println!(
                "{}  \"{}\"  open, waits for you: {}",
                found.slug, found.title, found.waits
            ),
        }
    }
    for found in closed {
        let state = match &found.merged_into {
            Some(into) => format!("merged into {into}"),
            None => "closed".into(),
        };
        println!("{}  \"{}\"  {state}", found.slug, found.title);
    }
    Ok(())
}
