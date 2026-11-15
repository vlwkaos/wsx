//! The move of a repo's chats out of its store: `tasks/<repo>/manager/` becomes
//! `tasks/chats/<repo name>/` (the repo's home chat, `home.json`) and `tasks/<repo>/chats/<n>/`
//! becomes `tasks/chats/<n>/`. Idempotent; nothing stays under the old paths. A host that runs
//! on an old folder is stopped first, so nothing writes the old path after the move, and the
//! caller starts it again on the new one: its restore queues the messages it had not answered.
//! ref: docs/tasks.md "Chats", docs/design/orchestration-model.md section 8

use super::chats::{self, CHAT_FILE};
use super::manager::{ManagerEvent, ManagerRequest, ManagerResponse};
use super::store;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_FILE: &str = "chats.lock";
const MANAGER_DIR: &str = "manager";
/// A host answers its status, or its pid is found, until it is stopped; this long at most.
const STOP_WAIT: Duration = Duration::from_secs(8);

/// A chat whose host the move stopped: the caller starts it on the new folder.
pub(super) struct Stopped {
    /// The chat's new name; the repo's home chat starts with no `--chat`.
    pub chat: String,
    pub home: bool,
}

/// One old chat folder: the name it had (none: the first chat) and its path.
struct Old {
    name: Option<String>,
    path: PathBuf,
}

pub(super) fn run(project_id: &str, project: &Path) -> Result<Vec<Stopped>> {
    let old = project.join(MANAGER_DIR);
    let old_chats = project.join("chats");
    if !old.is_dir() && !old_chats.is_dir() {
        return Ok(Vec::new());
    }
    let root = store::chats_root();
    let tasks = root.parent().context("the chats folder has no parent")?;
    store::create_private_dir(&root)?;
    // ^ Names are taken one machine wide, so one lock holds every repo's move.
    let _lock = store::open_locked(
        &tasks.join(LOCK_FILE),
        std::fs::OpenOptions::new().write(true).create(true),
    )?;
    let mut folders = Vec::new();
    if old.is_dir() {
        folders.push(Old {
            name: None,
            path: old,
        });
    }
    let mut named: Vec<String> = std::fs::read_dir(&old_chats)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| chats::checked_name(name).is_ok() && old_chats.join(name).is_dir())
        .collect();
    named.sort();
    folders.extend(named.into_iter().map(|name| Old {
        path: old_chats.join(&name),
        name: Some(name),
    }));
    let base = chats::home_name(project_id);
    let home = store::home_chat(project_id).unwrap_or(base);
    // ^ The new names, taken now so no other repo's move takes one in between.
    let mut names: BTreeMap<String, String> = BTreeMap::from([("manager".into(), home.clone())]);
    let mut taken = vec![home.clone()];
    for folder in folders.iter().filter_map(|folder| folder.name.as_ref()) {
        let new = if !root.join(folder).exists() && !taken.contains(folder) {
            folder.clone()
        } else {
            free(
                &format!("{}-{folder}", chats::home_base(project_id)),
                &taken,
            )
        };
        taken.push(new.clone());
        names.insert(folder.clone(), new);
    }
    let stopped: Vec<Stopped> = folders
        .iter()
        .filter_map(|folder| stop_host(project_id, folder, &names, &home))
        .collect();
    rewrite(project_id, project, &folders, &names, &home)?;
    for folder in &folders {
        let new = &names[folder.name.as_deref().unwrap_or("manager")];
        let target = root.join(new);
        std::fs::rename(&folder.path, &target).with_context(|| {
            format!(
                "cannot move {} to {}",
                folder.path.display(),
                target.display()
            )
        })?;
        let mut file: Value = store::read_bounded_if_exists(&target.join(CHAT_FILE), 4096)?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        file["repos"] = serde_json::json!([project_id]);
        store::write_atomic(&target.join(CHAT_FILE), &serde_json::to_vec(&file)?)?;
        if folder.name.is_none() {
            store::set_home_chat(project_id, new)?;
        }
    }
    // ^ The search digest names the chats of before; it is rebuilt on the next search.
    let _ = std::fs::remove_file(project.join(crate::memory::index::DIGEST_FILE));
    let _ = std::fs::remove_dir(&old_chats);
    for stop in &stopped {
        if let Ok(Some(dir)) = store::ManagerDir::existing(&stop.chat) {
            let _ = dir.append(ManagerEvent::Stopped {
                reason: format!("the chat moved to tasks/chats/{}", stop.chat),
            });
        }
    }
    Ok(stopped)
}

/// `base` as a chat's name that is not a folder and not in `taken`.
fn free(base: &str, taken: &[String]) -> String {
    let base = chats::slug_name(base);
    (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .find(|name| !store::chats_root().join(name).exists() && !taken.contains(name))
        .expect("a free chat name")
}

/// Stop the host that runs on the old folder, if one does: close its session, stop its agent's
/// tree and wait until it is gone. The chat's new name tells the caller what to start.
fn stop_host(
    project_id: &str,
    folder: &Old,
    names: &BTreeMap<String, String>,
    home: &str,
) -> Option<Stopped> {
    let mut args = vec!["task", "manager-host", project_id];
    if let Some(name) = &folder.name {
        args.extend(["--chat", name.as_str()]);
    }
    let (socket, broker) = store::sockets_of(&folder.path);
    let answers = || {
        super::call_socket::<_, ManagerResponse>(
            &socket,
            &ManagerRequest::Status,
            super::stall::PROBE,
        )
        .is_ok()
    };
    let pid = super::replace::host_pid(&args);
    if pid.is_none() && !answers() {
        return None;
    }
    let main = main_of(project_id)?;
    let label = format!("task-{}", folder.name.as_deref().unwrap_or("manager"));
    let _ = super::close_session(&label, &main);
    super::replace::stop_tree(pid);
    if let Some(pid) = pid {
        // SAFETY: a signal to a process of this user; the host ends on it.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline && (super::replace::host_pid(&args).is_some() || answers()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(broker);
    let chat = names[folder.name.as_deref().unwrap_or("manager")].clone();
    Some(Stopped {
        home: chat == home,
        chat,
    })
}

/// The registered repo's main checkout.
pub(super) fn main_of(project_id: &str) -> Option<PathBuf> {
    let (config, _) = crate::config::global::GlobalConfig::load().ok()?;
    config
        .projects
        .into_iter()
        .find(|project| store::project_id(&project.name, &project.path) == project_id)
        .map(|project| project.path)
}

/// Rewrite, in the old places, the names that change: the peers of the chats' logs and states
/// and of the tasks, the creators of the tasks and the chats of the approval requests.
fn rewrite(
    project_id: &str,
    project: &Path,
    folders: &[Old],
    names: &BTreeMap<String, String>,
    home: &str,
) -> Result<()> {
    let home_by = format!("chat:{home}");
    let rename = |value: &mut Value| rename_value(value, names, &home_by);
    let mut dirs: Vec<PathBuf> = folders.iter().map(|folder| folder.path.clone()).collect();
    dirs.extend(
        store::task_numbers(project_id)?
            .iter()
            .map(|id| project.join(id.to_string())),
    );
    for dir in &dirs {
        if dir.is_dir() {
            chats::rewrite_dir(dir, &[b"chat", b"manager"], &rename)?;
        }
    }
    let approvals = project.join(store::APPROVALS_FILE);
    let Some(bytes) = store::read_bounded_if_exists(&approvals, 64 << 20)? else {
        return Ok(());
    };
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        match serde_json::from_slice::<Value>(line) {
            Ok(mut value) if value["kind"] == "request" => {
                value["chat"] = match value["chat"].as_str() {
                    Some(chat) => names.get(chat).map_or(chat, String::as_str).into(),
                    None => home.into(),
                };
                out.extend_from_slice(&serde_json::to_vec(&value)?);
            }
            _ => out.extend_from_slice(line),
        }
        out.push(b'\n');
    }
    if out != bytes {
        store::write_atomic(&approvals, &out)?;
    }
    Ok(())
}

/// A creator or a peer key that names a chat of before, as it is named now. A peer is
/// `chat:<n>`, an answer to it `chat-reply:<n>`. A creator is `manager` (the first chat),
/// `manager@<repo>` (another repo's first chat; its chat is named for the repo) or `chat:<n>`.
fn rename_text(text: &str, names: &BTreeMap<String, String>) -> Option<String> {
    ["chat:", "chat-reply:"].iter().find_map(|prefix| {
        let name = text.strip_prefix(prefix)?;
        let new = names.get(name).filter(|new| *new != name)?;
        Some(format!("{prefix}{new}"))
    })
}

fn rename_value(value: &mut Value, names: &BTreeMap<String, String>, home_by: &str) -> bool {
    match value {
        Value::String(text) => rename_text(text, names)
            .map(|renamed| *text = renamed)
            .is_some(),
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |any, item| rename_value(item, names, home_by) | any),
        Value::Object(map) => {
            let mut changed = false;
            for (key, item) in map.iter_mut() {
                let creator = key == "created_by" && item.is_string();
                changed |= match (creator, item.as_str()) {
                    (true, Some("manager")) => {
                        *item = home_by.into();
                        true
                    }
                    (true, Some(by)) if by.starts_with("manager@") => {
                        *item =
                            format!("chat:{}", chats::slug_name(&by["manager@".len()..])).into();
                        true
                    }
                    _ => rename_value(item, names, home_by),
                };
            }
            let keys: Vec<(String, String)> = map
                .keys()
                .filter_map(|key| Some((key.clone(), rename_text(key, names)?)))
                .collect();
            for (old, new) in keys {
                if let Some(item) = map.remove(&old) {
                    map.insert(new, item);
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}
