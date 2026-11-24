//! What the owner does through a project's manager: start it, talk to it, and decide
//! its approval requests. Shared by `wsx task` and the web client.
//! ref: docs/tasks.md "Manager", "Approvals"

use super::manager::{
    pending_approvals, waiting_approvals, ApprovalAction, ApprovalLine, ManagerRequest,
    ManagerResponse,
};
use super::store::{self, ManagerDir, TaskDir};
use super::{call_socket, now_unix_ms, MAX_MESSAGE_BYTES};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Set to `manager` for the manager's agent; its commands then ask instead of acting.
pub const ACTOR_ENV: &str = "WSX_TASK_ACTOR";
pub const HOST_TIMEOUT: Duration = Duration::from_secs(15);
/// A landing rebases the branch onto the default branch before the host answers.
pub const LAND_TIMEOUT: Duration = Duration::from_secs(300);
const MANAGER_START_WAIT: Duration = Duration::from_secs(10);
/// Most characters of an action's answer kept in its decision.
const MAX_OUTCOME_CHARS: usize = 1000;

/// Start the open chat's host (the repo's main chat when `chat` is none), and the repo's
/// service when that is not running either, unless they answer. True when the chat's host was
/// started. `launch` creates the hidden session (its label, `task-<chat>` or
/// `task-repo-<repo-id>`) in the main checkout with the given argv; the CLI may start wsxd for
/// it, the web client never does.
pub fn ensure_manager(
    name: &str,
    main: &Path,
    wsx: &Path,
    chat: Option<&str>,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
) -> Result<(bool, ManagerDir)> {
    let project_id = store::project_id(name, main);
    let dir = match chat {
        None => super::chats::open_main(&project_id)?,
        Some(chat) => super::chats::open_chat(chat)?,
    };
    let status = || {
        call_socket::<_, ManagerResponse>(&dir.socket_path(), &ManagerRequest::Status, HOST_TIMEOUT)
    };
    // ^ The service of a chat's own repo; another repo's runs on its own.
    if dir.project_id == project_id {
        ensure_repo(name, main, wsx, &launch)?;
    }
    // ^ A host hung in a turn under an older wsx is closed and started again here.
    if let Ok(answer) = status() {
        if !super::replace::close_hung_manager(&dir, main, &answer)? {
            return Ok((false, dir));
        }
    }
    let mut argv = vec![wsx.to_string_lossy().into_owned()];
    argv.extend(dir.host_args());
    let label = format!("task-{}", dir.name());
    super::close_left_session(&label, main);
    launch(label.clone(), argv)
        .context("the manager did not start; no session was left running")?;
    let deadline = Instant::now() + MANAGER_START_WAIT;
    while Instant::now() < deadline {
        if status().is_ok() {
            return Ok((true, dir));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "the session started but its host does not answer; look at session {label} in {}",
        main.display()
    )
}

/// The session label of a repo's service.
pub fn repo_label(project_id: &str) -> String {
    format!("task-repo-{project_id}")
}

/// Start the repo's service, `wsx task repo-host <repo-id>`, unless it answers. One that is
/// running but answers nothing is closed and started again when `replace_hung` is set (the
/// sweep after a deploy); `ensure_manager` leaves it to that sweep. True when it started one.
pub fn ensure_repo(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
) -> Result<bool> {
    ensure_repo_with(name, main, wsx, launch, false)
}

pub fn ensure_repo_with(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    replace_hung: bool,
) -> Result<bool> {
    let project_id = store::project_id(name, main);
    let socket = store::repo_socket(&project_id)?;
    let status = || {
        call_socket::<_, ManagerResponse>(&socket, &ManagerRequest::Status, HOST_TIMEOUT).is_ok()
    };
    if status() {
        return Ok(false);
    }
    let args = ["task", "repo-host", project_id.as_str()];
    let label = repo_label(&project_id);
    if let Some(pid) = super::replace::host_pid(&args) {
        // ^ The service's own sweep never closes the service it runs in.
        if !replace_hung || pid as u32 == std::process::id() {
            return Ok(false);
        }
        let _ = super::close_session(&label, main);
        super::replace::stop_tree(Some(pid));
        // SAFETY: a signal to a process of this user; the host ends on it.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let mut argv = vec![wsx.to_string_lossy().into_owned()];
    argv.extend(args.map(String::from));
    super::close_left_session(&label, main);
    launch(label.clone(), argv)
        .context("the repo service did not start; no session was left running")?;
    let deadline = Instant::now() + MANAGER_START_WAIT;
    while Instant::now() < deadline {
        if status() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "the session started but the repo service does not answer; look at session {label} in {}",
        main.display()
    )
}

/// Give the manager, or the open chat `chat`, a message, starting it first when needed.
/// `from` is another manager or chat; none is the owner. `short`: an events message's lines
/// as the chat shows them.
#[allow(clippy::too_many_arguments)]
pub fn say(
    name: &str,
    main: &Path,
    wsx: &Path,
    chat: Option<&str>,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    text: String,
    from: Option<super::manager::Author>,
    short: Vec<super::manager::Short>,
) -> Result<ManagerResponse> {
    // ^ The owner's message with no prefix may be routed by a small model first; it only picks
    // a prefix, which the chat's host acts on (docs/tasks.md "Router").
    let text = match from {
        None => routed(name, main, chat, text),
        Some(_) => text,
    };
    let (_, dir) = ensure_manager(name, main, wsx, chat, launch)?;
    call_socket(
        &dir.socket_path(),
        &ManagerRequest::Say { text, from, short },
        HOST_TIMEOUT,
    )
}

/// How a chat closed: its hand-back reached the main chat, there is no main chat to take its
/// open tasks, or it was a fork that owned no task and the fork is undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    Handed,
    NoMain,
    ForkUndone,
}

/// Close the open chat `name` of the repo (`wsx task chat close`, the web's and the TUI's Close
/// chat) and hand it back: its goal, open tasks and last decisions go to the repo's main chat
/// as one message of wsx, which the main chat reads with no turn. The open tasks are the main
/// chat's from then on (`chats::owner`). Closing the main chat leaves the repo with none, and
/// its open tasks have no chat (`NoMain`). A fork that owned no task is undone instead
/// (`ForkUndone`). ref: docs/tasks.md "Chats", "Lead and forks"
pub fn close_chat(
    project: (&str, &Path),
    wsx: &Path,
    name: &str,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
) -> Result<Closed> {
    // ^ A fork closed before it owned a task is undone: the chat it came from is told in a note
    // that no turn reads, and nothing is handed back (its goal would be forked again).
    let dir = super::chats::open_chat(name)?;
    let fork = super::chats::unstarted_fork(&dir)?;
    let text = fork.is_none().then(|| super::chats::hand_back(&dir));
    let main = super::chats::close(name)?;
    if let Some(from) = fork {
        // ^ Both logs note it: the fork's own, so a reopen of the name closes as any chat.
        let undone = super::manager::ManagerEvent::ForkUndone {
            to: name.to_string(),
        };
        dir.append(undone.clone())?;
        if let Some(origin) = ManagerDir::existing(&from)? {
            origin.append(undone)?;
        }
        return Ok(Closed::ForkUndone);
    }
    let (Some(main), Some(text)) = (main, text) else {
        return Ok(Closed::NoMain);
    };
    let short = super::manager::Short::event(
        format!("Chat {name} closed; its open tasks are yours"),
        false,
    );
    say(
        project.0,
        project.1,
        wsx,
        Some(&main),
        launch,
        text,
        Some(super::manager::Author::Events),
        vec![short],
    )
    .map(|_| Closed::Handed)
    .context("the chat is closed, but its hand-back did not reach the main chat")
}

/// How long the router's model may take before the message stays where it was written.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(20);

/// The owner's message `text` to chat `chat` (none: the main chat) of the project, with the
/// router's prefix when `tasks.router.model` is set and the model sends it on: `@<chat>` for
/// an open chat whose goal it belongs to, `/new <name>` for a goal no open chat has. The model
/// reads only the message and each open chat's name and goal line. A message with a prefix of
/// its own, no model, a model that fails, is slow, or names a chat that is not open or a name
/// in use: the text as it was, so it stays. Only `from: None` (the owner typing in a chat) is
/// routed; mail and chats' messages name their chat. ref: docs/tasks.md "Router"
fn routed(name: &str, main: &Path, chat: Option<&str>, text: String) -> String {
    let Some(model) = crate::config::project::load_task_config(main)
        .ok()
        .and_then(|settings| settings.router_model)
    else {
        return text;
    };
    // ^ A prefix of its own, or a quote it answers: the rules or the quote decide.
    let start = text.trim_start();
    if start.starts_with(['/', '@', '>']) {
        return text;
    }
    let project_id = store::project_id(name, main);
    let open = super::chats::open_chats(&project_id);
    let here = match chat {
        Some(chat) => chat.to_string(),
        None => match super::chats::open_main(&project_id) {
            Ok(dir) => dir.chat,
            Err(_) => return text,
        },
    };
    let line = |dir: &ManagerDir| {
        let goal = fork_goal(&super::chats::goal(dir));
        format!(
            "- {}: {}",
            dir.name(),
            if goal.is_empty() {
                "(the lead: no goal of its own)"
            } else {
                &goal
            }
        )
    };
    let chats: Vec<String> = open.iter().map(line).collect();
    let prompt = format!(
        "Route one message of the owner. It was written in chat {here}. The project's open chats, each with its goal:\n{}\n\nThe message:\n{}\n\nAnswer stay when it continues the goal of {here}, asks about it, or you are not sure. Answer chat with that chat's name when it plainly belongs to another open chat's goal. Answer new with a short new name (lowercase, dashes) only when it asks for work whose goal no open chat has.",
        chats.join("\n"),
        text.chars().take(4000).collect::<String>()
    );
    let verdict = ask_router(&model, &prompt);
    let Some(verdict) = verdict else {
        return text;
    };
    let open_name = |chat: &str| open.iter().any(|dir| dir.name() == chat);
    match verdict["route"].as_str() {
        Some("chat") => match verdict["chat"].as_str() {
            Some(target) if target != here && open_name(target) => format!("@{target} {text}"),
            _ => text,
        },
        Some("new") => match verdict["name"].as_str() {
            Some(new)
                if super::chats::checked_name(new).is_ok()
                    && !store::chats_root().join(new).exists() =>
            {
                format!("/new {new} {text}")
            }
            _ => text,
        },
        _ => text,
    }
}

/// One call of the router's model with no tools, no project and no session: its structured
/// answer, or none on any failure.
fn ask_router(model: &str, prompt: &str) -> Option<serde_json::Value> {
    use crate::process::{run_bounded, Bound, Options, Stderr};
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "route": { "enum": ["stay", "chat", "new"] },
            "chat": { "type": "string" },
            "name": { "type": "string" }
        },
        "required": ["route"]
    });
    let mut command = std::process::Command::new("claude");
    command.current_dir(store::state_dir()).env_clear();
    for name in super::AGENT_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        .args([
            "--model",
            model,
            "--setting-sources",
            "",
            "--no-session-persistence",
        ])
        // ^ No tool and no MCP server: the message may be pasted text that asks for one.
        .args(["--tools", "", "--strict-mcp-config"])
        .args(["--json-schema", &schema.to_string()])
        .arg(prompt)
        .stdin(std::process::Stdio::null());
    let run = run_bounded(
        &mut command,
        &Options {
            timeout: ROUTE_TIMEOUT,
            stdout: Bound::Tail(256 * 1024),
            stderr: Stderr::Null,
            reap: true,
        },
    )
    .ok()?;
    run.stdout.split(|byte| *byte == b'\n').find_map(|line| {
        let value: serde_json::Value = serde_json::from_slice(line).ok()?;
        (value["type"] == "result" && value["is_error"] == false)
            .then(|| value["structured_output"].clone())
            .filter(serde_json::Value::is_object)
    })
}

/// The goal of a chat forked from the owner's message `text`: its first line, at most 200
/// characters, so the new chat's purpose is the owner's own words.
pub fn fork_goal(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// Fork the owner's message `message` of chat `origin` (its `text`, from that chat's log) into
/// the new chat `name` of the project: the new chat's goal is `fork_goal`, and it starts on the
/// message as the owner's own words. The name is taken by making its folder, so a used name is
/// refused and two forks at once cannot share one; `origin` notes the fork before the message
/// goes. When the message does not reach the new chat, its host stops, it closes and both logs
/// say the fork is undone. Returns the new chat's goal and the message's id there.
/// The one fork of `chat new --brief` and the router's `/new`. ref: docs/tasks.md "Lead and forks"
#[allow(clippy::too_many_arguments)]
pub fn fork(
    project: (&str, &Path),
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    origin: &ManagerDir,
    message: u64,
    text: String,
    name: &str,
    agent: Option<&str>,
) -> Result<(String, Option<u64>)> {
    use super::manager::{Author, ManagerEvent};
    let name = super::chats::checked_name(name)?;
    let goal = fork_goal(&text);
    if goal.is_empty() {
        bail!("the message has no words to fork; nothing was done");
    }
    // ^ Private as every chat folder (`store::create_private_dir`).
    let made = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(store::chats_root().join(name))
    };
    if let Err(error) = made {
        match error.kind() {
            std::io::ErrorKind::AlreadyExists => {
                bail!("chat {name} was opened before: a fork needs a new name; nothing was done")
            }
            _ => bail!("cannot make the folder of chat {name}: {error}; nothing was done"),
        }
    }
    origin.append(ManagerEvent::Forked {
        to: name.to_string(),
        message,
    })?;
    let project_id = store::project_id(project.0, project.1);
    let dir = super::chats::create(&project_id, name, Some(&goal), agent)?;
    let sent = dir
        .append(ManagerEvent::ForkedFrom {
            from: origin.name().to_string(),
            message,
        })
        .and_then(|()| {
            say(
                project.0,
                project.1,
                wsx,
                Some(name),
                launch,
                text,
                // ^ The owner's words, already routed: `Owner`, so no router moves them again.
                Some(super::manager::Author::Owner),
                Vec::new(),
            )
        });
    // ^ The fork stands once the message is in the new chat's log, also when the answer did not
    // come back in time (a host that took it, then was slow).
    let arrived = || {
        dir.read_log().ok().and_then(|log| {
            log.into_iter().find_map(|entry| match entry.event {
                ManagerEvent::Message {
                    id,
                    from: Author::Owner,
                    ..
                } => Some(id),
                _ => None,
            })
        })
    };
    let id = match sent {
        Ok(response) if response.ok => response.id,
        failed => {
            let Some(id) = arrived() else {
                let why = match failed {
                    Ok(response) => response.message,
                    Err(error) => format!("{error:#}"),
                };
                // ^ No half fork runs: its host stops, the chat closes (its name stays used) and
                // both logs say the fork is undone.
                let _ = super::replace::stop_host(&dir, project.1);
                let _ = super::chats::close(name);
                let undone = ManagerEvent::ForkUndone {
                    to: name.to_string(),
                };
                let _ = dir.append(undone.clone());
                let _ = origin.append(undone);
                bail!("the fork to {name} did not start: {why}; chat {name} is closed");
            };
            Some(id)
        }
    };
    Ok((goal, id))
}

/// Give the owner's message `message` of chat `origin` (its `text`) to the open chat `target`
/// of the project `project` (the router's `@<chat>`): `target` takes it as the owner's own
/// words, then both logs note the move. A failure is noted in `origin`, where the owner says it
/// again; words that reached `target` though its answer did not come back are moved.
/// ref: docs/tasks.md "Router"
#[allow(clippy::too_many_arguments)]
pub fn move_message(
    project: (&str, &Path),
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    origin: &ManagerDir,
    message: u64,
    text: String,
    target: &str,
) -> Result<()> {
    use super::manager::{Author, ManagerEvent};
    let moved = super::chats::open_chat(target).and_then(|dir| {
        let before = dir.read_log().map(|log| log.len()).unwrap_or(0);
        let text_sent = text.clone();
        let sent = say(
            project.0,
            project.1,
            wsx,
            Some(target),
            launch,
            text,
            // ^ The owner's words, already routed: `Owner`, so no router moves them again.
            Some(Author::Owner),
            Vec::new(),
        )
        .and_then(|sent| {
            anyhow::ensure!(sent.ok, "{}", sent.message);
            Ok(())
        });
        let arrived = || {
            dir.read_log().is_ok_and(|log| {
                log.iter().skip(before).any(|entry| {
                    matches!(&entry.event, ManagerEvent::Message { from: Author::Owner, text, .. } if *text == text_sent)
                })
            })
        };
        if sent.is_err() && !arrived() {
            return sent;
        }
        dir.append(ManagerEvent::MovedFrom {
            from: origin.name().to_string(),
            message,
        })?;
        origin.append(ManagerEvent::Moved {
            to: target.to_string(),
            message,
        })
    });
    if let Err(error) = &moved {
        origin.append(ManagerEvent::MoveFailed {
            to: target.to_string(),
            message,
            reason: format!("{error:#}"),
        })?;
    }
    moved
}

/// Ask the repo's main chat to write to the owner with no turn, starting its host first. An
/// error when its host does not start, `ok: false` with `busy` while a turn runs.
#[allow(clippy::too_many_arguments)]
pub fn post(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    event: String,
    text: String,
    level: Option<super::manager::Level>,
) -> Result<ManagerResponse> {
    let (_, dir) = ensure_manager(name, main, wsx, None, launch)?;
    call_socket(
        &dir.socket_path(),
        &ManagerRequest::Post { event, text, level },
        HOST_TIMEOUT,
    )
}

/// Ask the repo's service to send `why` as an event to `chat` (the main chat when none) at
/// `at_ms`, in place of that chat's last check-in. It starts the service when needed.
pub fn checkin(
    name: &str,
    main: &Path,
    wsx: &Path,
    launch: impl Fn(String, Vec<String>) -> Result<()>,
    chat: Option<String>,
    at_ms: u64,
    why: String,
) -> Result<ManagerResponse> {
    ensure_repo(name, main, wsx, launch)?;
    call_socket(
        &store::repo_socket(&store::project_id(name, main))?,
        &ManagerRequest::Checkin { at_ms, why, chat },
        HOST_TIMEOUT,
    )
}

pub fn action_name(action: ApprovalAction) -> &'static str {
    match action {
        ApprovalAction::Land => "land",
        ApprovalAction::Deploy => "deploy",
        ApprovalAction::Release => "release",
        ApprovalAction::Discard => "close --discard",
        ApprovalAction::Push => "push",
        ApprovalAction::Project => "project add",
        ApprovalAction::Run => "run",
    }
}

/// A character that could make a shown command read as another: a control, a format (zero
/// width, bidi) or a line break. `wsx task run` refuses words with one.
pub fn misleading_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{2028}' | '\u{2029}')
}

/// A word with each misleading character written as its escape, so the card shows what runs.
pub fn shown_word(word: &str) -> String {
    word.chars()
        .map(|c| match misleading_char(c) {
            true => c.escape_unicode().to_string(),
            false => c.to_string(),
        })
        .collect()
}

/// A run's command as one line the owner reads: each word as given, quoted when it holds a
/// space, a quote or nothing.
pub fn command_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shown_word(word))
        .map(|word| {
            if !word.is_empty()
                && !word
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | '$' | '`'))
            {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// An approval request as the action it asks for, never its id: "Approve land #3",
/// "Approve discard #4", "Approve deploy". A push names what it carries: "Approve push #319
/// #320 (9 commits)", else the commit. ref: docs/web-client-tasks.md
pub fn approval_label(request: &ApprovalLine) -> String {
    let ApprovalLine::Request {
        action,
        task,
        commit,
        lease,
        tasks,
        commits,
        path,
        run,
        ..
    } = request
    else {
        return String::new();
    };
    // ^ A run names its command; why it asks is the card's second line.
    if let Some(run) = run {
        return format!("Approve run: {}", command_line(&run.argv));
    }
    let verb = match action {
        ApprovalAction::Discard => "discard",
        ApprovalAction::Push if lease.is_some() => "force push",
        other => action_name(*other),
    };
    let mut label = match (task, action, commit) {
        (Some(task), ..) => format!("Approve {verb} #{task}"),
        // ^ A project add names the folder; the URL is the card's second line.
        _ if path.is_some() => format!("Approve {verb} {}", path.as_deref().unwrap_or_default()),
        // ^ A push with no landed task names the commit: "Approve force push 1a2b3c4d5e6f".
        (None, ApprovalAction::Push, Some(commit)) if tasks.is_empty() => {
            format!("Approve {verb} {}", super::view::short_commit(commit))
        }
        _ => format!("Approve {verb}"),
    };
    for task in tasks.iter().take(MAX_LABEL_TASKS) {
        label += &format!(" #{task}");
    }
    if tasks.len() > MAX_LABEL_TASKS {
        label += &format!(" +{} more", tasks.len() - MAX_LABEL_TASKS);
    }
    match commits {
        Some(1) => label += " (1 commit)",
        Some(count) => label += &format!(" ({count} commits)"),
        None => {}
    }
    label
}

/// A request the owner approved in the chat that waits for task `after` to land, in words:
/// "release after #382, approved by the owner". ref: docs/tasks.md "Approvals"
pub fn waiting_label(request: &ApprovalLine) -> String {
    let after = match request {
        ApprovalLine::Request {
            after: Some(after), ..
        } => *after,
        _ => return String::new(),
    };
    let label = approval_label(request);
    format!(
        "{} after #{after}, approved by the owner",
        label.strip_prefix("Approve ").unwrap_or(&label)
    )
}

/// Most task numbers a push label lists before "+n more".
const MAX_LABEL_TASKS: usize = 8;

/// What a push of `to` brings to origin, whose tip is `from` (none: a branch origin lacks):
/// the landed tasks (by number) whose landed commit lies in `from..to`, and the commits.
/// Commits carry no task ids, so a task is matched by the `Landed` entry of its log.
pub fn carried(
    project_id: &str,
    main: &Path,
    from: Option<&str>,
    to: &str,
) -> Result<(Vec<u32>, u32)> {
    let commits = super::copy::commits_after(main, from, to)?;
    let on = |commit: &str, tip: &str| super::copy::is_ancestor(main, commit, tip).unwrap_or(false);
    let mut tasks = Vec::new();
    for id in store::task_numbers(project_id)? {
        let Ok(dir) = TaskDir::open(project_id, id) else {
            continue;
        };
        if dir
            .read_record()
            .map_or(true, |record| record.state != super::TaskState::Landed)
        {
            continue;
        }
        let landed = dir.last_entry(|entry| match &entry.event {
            super::LogEvent::Landed { commit } => Some(commit.clone()),
            _ => None,
        });
        if let Ok(Some(commit)) = landed {
            if on(&commit, to) && !from.is_some_and(|from| on(&commit, from)) {
                tasks.push(id);
            }
        }
    }
    Ok((tasks, commits))
}

/// A decided request in words, from its label: "Approved land #3", "Denied discard #4",
/// "Dropped discard #4: #4 is closed".
pub fn decision_words(label: &str, approved: bool, outcome: &str) -> String {
    match (approved, outcome.strip_prefix(DROPPED)) {
        (true, _) => label.replacen("Approve", "Approved", 1),
        (false, Some(why)) => format!("{}: {why}", label.replacen("Approve", "Dropped", 1)),
        (false, None) => label.replacen("Approve", "Denied", 1),
    }
}

/// The outcome of a request wsx settled itself because its task moved on: "dropped; <why>".
pub const DROPPED: &str = "dropped; ";

/// Why a pending request no longer applies: its task left or moved past the revision it
/// names. ref: docs/tasks.md "Approvals"
pub fn stale_reason(request: &ApprovalLine, record: &super::TaskRecord) -> Option<String> {
    let ApprovalLine::Request { task, revision, .. } = request else {
        return None;
    };
    let task = (*task)?;
    if !record.state.is_open() {
        return Some(format!("#{task} is {}", record.state.as_str()));
    }
    revision
        .filter(|revision| *revision != record.revision)
        .map(|revision| format!("#{task} moved from r{revision} to r{}", record.revision))
}

/// The pending request `id`. A request that was decided answers with its outcome ("dropped;
/// replaced by a170"), and nothing runs.
pub fn find_request<'a>(lines: &'a [ApprovalLine], id: &str) -> Result<&'a ApprovalLine> {
    pending_approvals(lines)
        .into_iter()
        .find(|line| matches!(line, ApprovalLine::Request { id: pending, .. } if pending == id))
        .with_context(|| {
            let decided = lines.iter().rev().find_map(|line| match line {
                ApprovalLine::Decision {
                    id: decided,
                    outcome,
                    ..
                } if decided == id => Some(outcome),
                _ => None,
            });
            let waiting = waiting_approvals(lines)
                .into_iter()
                .find_map(|line| match line {
                    ApprovalLine::Request {
                        id: waiting,
                        after: Some(after),
                        ..
                    } if waiting == id => Some(*after),
                    _ => None,
                });
            match (decided, waiting) {
                (_, Some(after)) => format!(
                    "approval {id} was approved by the owner's message; it runs when #{after} lands"
                ),
                (Some(outcome), _) => format!("approval {id} was decided: {outcome}; nothing ran"),
                (None, _) => {
                    format!("no pending approval {id}; `wsx task ledger` lists the pending ones")
                }
            }
        })
}

/// What an approved action did.
pub struct Approved {
    pub ok: bool,
    pub outcome: String,
    /// The action's own `--json` answer, or null.
    pub result: serde_json::Value,
}

/// Run an approved request's action as its own `wsx task` command, without the actor
/// mark, and record the decision. A task that moved past the requested revision is
/// recorded as not run and is an error.
pub fn approve(name: &str, main: &Path, id: &str, wsx: &Path) -> Result<Approved> {
    let project_id = store::project_id(name, main);
    let lines = store::read_approvals(&project_id)?;
    run_request(
        name,
        main,
        &project_id,
        find_request(&lines, id)?.clone(),
        wsx,
    )
}

/// Run a request that waited for its `after` task, once that task has landed: the same run
/// as `approve`, its decision naming the owner's message. Refused while the task is open.
/// ref: docs/tasks.md "Approvals"
pub fn approve_waiting(name: &str, main: &Path, id: &str, wsx: &Path) -> Result<Approved> {
    let project_id = store::project_id(name, main);
    let lines = store::read_approvals(&project_id)?;
    let request = waiting_approvals(&lines)
        .into_iter()
        .find(|line| line.id() == id)
        .with_context(|| format!("no waiting approval {id}"))?
        .clone();
    if let ApprovalLine::Request {
        after: Some(after), ..
    } = &request
    {
        let state = TaskDir::open(&project_id, *after)?.read_record()?.state;
        if state != super::TaskState::Landed {
            bail!("#{after} is {}, not landed; {id} waits", state.as_str());
        }
    }
    run_request(name, main, &project_id, request, wsx)
}

fn run_request(
    name: &str,
    main: &Path,
    project_id: &str,
    request: ApprovalLine,
    wsx: &Path,
) -> Result<Approved> {
    let ApprovalLine::Request {
        id,
        action,
        task,
        revision,
        commit,
        lease,
        path,
        url,
        said: owner_said,
        run,
        ..
    } = request
    else {
        unreachable!("only requests run");
    };
    let id = id.as_str();
    if action == ApprovalAction::Run {
        return start_run(name, main, project_id, id, run.as_ref(), wsx, owner_said);
    }
    // ^ The owner approves one revision: a task that moved on needs a new request.
    if let (Some(task), Some(revision)) = (task, revision) {
        let now = TaskDir::open(project_id, task)?.read_record()?.revision;
        if now != revision {
            let outcome = format!("not run: #{task} moved from r{revision} to r{now}; ask again");
            decide(project_id, id, true, None, &outcome, owner_said)?;
            bail!("{outcome}");
        }
    }
    let mut argv: Vec<String> = vec!["task".into()];
    match action {
        ApprovalAction::Land => {
            argv.extend(["land".into(), task.unwrap_or_default().to_string()]);
        }
        ApprovalAction::Discard => argv.extend([
            "close".into(),
            task.unwrap_or_default().to_string(),
            "--discard".into(),
        ]),
        ApprovalAction::Project => {
            argv.extend(["project".into(), "add".into(), path.unwrap_or_default()]);
            if let Some(url) = url {
                argv.extend(["--clone".into(), url]);
            }
        }
        ApprovalAction::Run => unreachable!("a run starts in a session (start_run)"),
        ApprovalAction::Push => {
            argv.push("push".into());
            // ^ No commit: the default branch tip at run time (a push that waited for a land).
            if let Some(commit) = commit {
                argv.extend(["--commit".into(), commit]);
            }
            // ^ The owner approved a rewrite over the origin tip the request saw, no other.
            if let Some(lease) = lease {
                argv.extend(["--force".into(), format!("--lease={lease}")]);
            }
        }
        ApprovalAction::Deploy | ApprovalAction::Release => {
            // ^ An approved run waits for a running deploy or release, as a land's deploy does.
            argv.extend([action_name(action).into(), "--approved".into()]);
            if let Some(commit) = commit {
                argv.extend(["--commit".into(), commit]);
            }
            // ^ The owner's message asked for it, as it would a release now: open To check
            // items stay on the release.
            if owner_said.is_some() && action == ApprovalAction::Release {
                argv.push("--checks-waived".into());
            }
        }
    }
    argv.extend(["-p".into(), name.into(), "--json".into()]);
    let output = std::process::Command::new(wsx)
        .args(&argv)
        .env_remove(ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run the approved action")?;
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    let said = result["message"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            result["state"]
                .as_str()
                .map(|state| format!("state {state}"))
        })
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string());
    let ok = output.status.success();
    let outcome = format!("{}: {said}", if ok { "done" } else { "failed" });
    decide(project_id, id, true, None, &outcome, owner_said)?;
    Ok(Approved {
        ok,
        outcome,
        result,
    })
}

/// Start an approved run in a session of the project's main checkout: the pane runs
/// `wsx task ran <id>`, which runs the request's own command on the pane's terminal, so the
/// owner watches it and answers a prompt (`sudo`) there. The decision says where it runs;
/// its end is a `Ran` line. ref: docs/tasks.md "Approved runs"
fn start_run(
    name: &str,
    main: &Path,
    project_id: &str,
    id: &str,
    run: Option<&super::manager::RunSpec>,
    wsx: &Path,
    owner_said: Option<u64>,
) -> Result<Approved> {
    let Some(run) = run else {
        let outcome = "not run: the request names no command".to_string();
        decide(project_id, id, true, None, &outcome, owner_said)?;
        bail!("{outcome}");
    };
    let (config, _) = crate::config::global::GlobalConfig::load()?;
    let argv = vec![
        wsx.display().to_string(),
        "task".into(),
        "ran".into(),
        id.into(),
        "-p".into(),
        name.into(),
    ];
    // ^ The decision comes first: the pane's `ran` refuses a request that is not approved.
    let outcome = format!("running in session run-{id}: {}", command_line(&run.argv));
    decide(project_id, id, true, None, &outcome, owner_said)?;
    let started = crate::ops::create_session_launch(
        &crate::runtime::Client::local(),
        &config,
        main,
        format!("run-{id}"),
        argv,
        None,
    );
    match started {
        Ok(_) => Ok(Approved {
            ok: true,
            outcome,
            result: serde_json::Value::Null,
        }),
        // ^ No pane, no run: its end says so, and the asker hears it as it hears any end.
        Err(error) => {
            let said = format!("not run: no session started ({error:#})");
            store::append_approval(
                project_id,
                ApprovalLine::Ran {
                    id: id.into(),
                    exit: None,
                    tail: said.clone(),
                    unix_ms: now_unix_ms(),
                },
            )?;
            Ok(Approved {
                ok: false,
                outcome: said,
                result: serde_json::Value::Null,
            })
        }
    }
}

/// Run `wsx <argv> -p <project> --json` as the owner (no task actor) and return its JSON
/// answer, or what it said when it failed. The web and the TUI both run an owner's task
/// action this way, the CLI's own path.
pub fn run_as_owner(
    wsx: &Path,
    project: &str,
    mut argv: Vec<String>,
) -> Result<serde_json::Value, String> {
    argv.extend(["-p".into(), project.into(), "--json".into()]);
    let output = std::process::Command::new(wsx)
        .args(&argv)
        .env_remove(ACTOR_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(said.trim().trim_start_matches("Error: ").to_string());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| format!("wsx {} did not print JSON", argv[1]))
}

/// Record that the owner denied a pending request.
pub fn deny(name: &str, main: &Path, id: &str, note: Option<String>) -> Result<()> {
    let project_id = store::project_id(name, main);
    find_request(&store::read_approvals(&project_id)?, id)?;
    let note = note.filter(|note| !note.trim().is_empty());
    if note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_MESSAGE_BYTES)
    {
        bail!("the note is larger than the limit of {MAX_MESSAGE_BYTES} bytes");
    }
    decide(&project_id, id, false, note, "denied", None)
}

/// Settle a pending request that no longer applies, with why; the manager host tells the
/// manager and the owner in the chat.
pub fn drop_request(project_id: &str, id: &str, why: &str) -> Result<()> {
    decide(
        project_id,
        id,
        false,
        None,
        &format!("{DROPPED}{why}"),
        None,
    )
}

/// Record a request the owner approved by asking for it in the chat: the manager names the
/// owner's message `said` of its running turn, and the action runs without a button.
/// The id of the recorded request is returned. ref: docs/tasks.md "Approvals"
pub fn approve_by_message(project_id: &str, request: ApprovalLine, said: u64) -> Result<String> {
    check_owner_said(project_id, request.chat(), said)?;
    let line = store::append_approval(project_id, request)?;
    let id = line.id().to_string();
    store::append_approval(
        project_id,
        ApprovalLine::Decision {
            id: id.clone(),
            approved: true,
            note: None,
            outcome: format!("approved by owner message {said}"),
            owner_said: Some(said),
            unix_ms: now_unix_ms(),
        },
    )?;
    Ok(id)
}

/// Record a request the owner approved in the chat that can run only once task `after` has
/// landed: `said` is checked now, in the owner's turn, and kept on the request; the manager
/// host runs it when `after` lands (`approve_waiting`). Nothing is recorded when a check fails.
/// ref: docs/tasks.md "Approvals"
pub fn approve_after(project_id: &str, mut request: ApprovalLine, said: u64) -> Result<String> {
    check_owner_said(project_id, request.chat(), said)?;
    let ApprovalLine::Request {
        after: Some(after),
        task,
        said: kept,
        ..
    } = &mut request
    else {
        bail!("only a request that waits is recorded");
    };
    if *task == Some(*after) {
        bail!("--after {after}: a task cannot wait for itself; nothing was recorded");
    }
    let state = TaskDir::open(project_id, *after)
        .and_then(|dir| dir.read_record())
        .with_context(|| format!("--after {after}: no such task; nothing was recorded"))?
        .state;
    if !state.is_open() {
        bail!(
            "--after {after}: #{after} is {}, not open; nothing was recorded",
            state.as_str()
        );
    }
    *kept = Some(said);
    Ok(store::append_approval(project_id, request)?
        .id()
        .to_string())
}

/// Refuse unless `said` is an owner message of the manager's running turn.
pub fn check_owner_said(project_id: &str, chat: Option<&str>, said: u64) -> Result<()> {
    let manager = ManagerDir::existing_in(project_id, chat)?
        .context("the chat has no log, so no owner message")?;
    super::manager::owner_said_in_turn(&manager.read_log()?, said)
        .map_err(|why| anyhow::anyhow!("--owner-said {said}: {why}; nothing was run"))
}

fn decide(
    project_id: &str,
    id: &str,
    approved: bool,
    note: Option<String>,
    outcome: &str,
    owner_said: Option<u64>,
) -> Result<()> {
    store::append_approval(
        project_id,
        ApprovalLine::Decision {
            id: id.into(),
            approved,
            note,
            outcome: outcome.chars().take(MAX_OUTCOME_CHARS).collect(),
            owner_said,
            unix_ms: now_unix_ms(),
        },
    )
    .map(drop)
}
