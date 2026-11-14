//! A host whose own code cannot end its turn: a deploy reaches a host only before its next
//! turn, so one that hangs inside a turn under an older wsx never follows. Callers replace it:
//! a deploy, a starting manager host, `call_task_host` and `ensure_manager` find a host that runs an older wsx and whose
//! turn shows no agent output past `stall::turn_quiet_ms`, stop it and its agent's tree, and
//! start one on this wsx. ref: docs/tasks.md "Host" (A host hung in a turn)

use super::manager::{ManagerEvent, ManagerResponse};
use super::status::span;
use super::store::{self, ManagerDir, TaskDir};
use super::{stall, HostRequest, LogEvent, TaskRecord, TaskState};
use crate::config::project::load_task_config;
use crate::model::workspace::TaskConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The turn a host hands over to the next one, in its task folder.
pub const HANDOFF_FILE: &str = "handoff.txt";
/// Set on every agent, `<host pid>-<n>`: the processes of its tree that left its process
/// group still carry it, so a host's agents are found by its pid.
pub const TREE_ENV: &str = "WSX_AGENT_TREE";

/// The wsx file a host was started as. A deploy that replaces it makes a running host
/// exec the new file before its next agent start, so the agent gets the new launch. A host
/// sends it in its status answer, so a caller can tell a host that runs an older wsx.
/// ref: docs/tasks.md "Host"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binary {
    path: PathBuf,
    identity: String,
}

impl Binary {
    /// The path the host was started with (its argv[0], which `task new` and `say` make
    /// absolute), else the running file.
    pub fn capture() -> Option<Self> {
        let path = std::env::args_os()
            .next()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::current_exe().ok())?;
        let identity = crate::runtime::binary_identity(&path).ok()?;
        Some(Self { path, identity })
    }

    /// True when another file is now at the path (`behind`).
    pub fn replaced(&self) -> bool {
        behind(Some(self))
    }

    /// Replace this process with the file now at the path and the same arguments; the
    /// pane and its pid stay. Returns only when that file could not start.
    pub fn exec(&self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(&self.path)
            .args(std::env::args_os().skip(1))
            .exec()
    }
}

/// True when a host that answered with `binary` runs an older wsx than the file now at its
/// path; a host from before this answer names none, and is older. A missing file is not
/// followed, and neither is one put there while a trial runs: it may be the trial's build.
pub fn behind(binary: Option<&Binary>) -> bool {
    !store::any_trial()
        && binary.is_none_or(|binary| {
            crate::runtime::binary_identity(&binary.path).is_ok_and(|now| now != binary.identity)
        })
}

/// A turn with no agent output past the quiet bound: when it started and how long it is quiet.
pub struct Hung {
    started_ms: u64,
    quiet_ms: u64,
}

impl Hung {
    /// The one log entry of a replacement.
    fn reason(&self, now_ms: u64) -> String {
        format!(
            "the host ran an older wsx and its turn, started {} ago, had no agent output for {}: replaced by a host on this wsx",
            span(now_ms.saturating_sub(self.started_ms)),
            span(self.quiet_ms)
        )
    }
}

/// The quiet bound of a host's own turn in the project at `main`; `fallback_secs` is the check
/// timeout when its settings cannot be read.
fn bound_ms(main: &Path, fallback_secs: Option<u64>) -> u64 {
    let timeout = load_task_config(main).map_or_else(
        |_| fallback_secs.unwrap_or_else(|| TaskConfig::default().check_timeout_secs),
        |config| config.check_timeout_secs,
    );
    stall::turn_quiet_ms(timeout)
}

/// The task's turn when it is hung in a host that runs an older wsx: `working`, with no output
/// of its worker (`stall::progress_ms`) past the bound, and a host that answers as older.
fn task_hung(dir: &TaskDir, record: &TaskRecord, now_ms: u64) -> Option<Hung> {
    if record.state != TaskState::Working {
        return None;
    }
    let quiet_ms = now_ms.saturating_sub(stall::progress_ms(dir, record));
    if quiet_ms <= bound_ms(&record.project_path, Some(record.check_timeout_secs)) {
        return None;
    }
    let answer = super::call_host(&dir.socket_path(), &HostRequest::Status, stall::PROBE).ok()?;
    behind(answer.binary.as_ref()).then_some(Hung {
        started_ms: record.state_unix_ms,
        quiet_ms,
    })
}

/// Replace the task's host when its turn hangs in an older wsx (`task_hung`): close its
/// session, stop its agent's tree, log one `stopped` entry, hand the turn over and start a
/// host on this wsx, which resumes the conversation as a host that followed a deploy does.
/// True when it did.
pub(super) fn replace_hung_task(dir: &TaskDir) -> anyhow::Result<bool> {
    let Ok(record) = dir.read_record() else {
        return Ok(false);
    };
    let now_ms = super::now_unix_ms();
    let Some(hung) = task_hung(dir, &record, now_ms) else {
        return Ok(false);
    };
    let id = record.id.to_string();
    let pid = host_pid(&["task", "host", &dir.project_id, &id]);
    super::restart_host(dir, &record, || {
        stop_tree(pid);
        let reason = hung.reason(now_ms);
        let _ = dir.append(LogEvent::Stopped {
            reason: reason.clone(),
        });
        let text = format!(
            "{reason}. Your last turn was stopped. Continue the task from where it stopped."
        );
        let _ = dir.write_file(HANDOFF_FILE, text.as_bytes());
    })?;
    Ok(true)
}

/// The manager's or a chat's turn when it is hung in a host that runs an older wsx: it
/// answered `busy` and as older, and its log has no agent line, delivery or reply past the
/// bound.
fn manager_hung(dir: &ManagerDir, main: &Path, answer: &ManagerResponse) -> Option<Hung> {
    if !answer.busy || !behind(answer.binary.as_ref()) {
        return None;
    }
    let log = dir.read_log().ok()?;
    let started_ms = log
        .iter()
        .rev()
        .find(|entry| matches!(entry.event, ManagerEvent::Delivered { .. }))?
        .unix_ms;
    let output_ms = log
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                entry.event,
                ManagerEvent::Delivered { .. }
                    | ManagerEvent::Agent { .. }
                    | ManagerEvent::Reply { usage: Some(_), .. }
            )
        })?
        .unix_ms;
    let quiet_ms = super::now_unix_ms().saturating_sub(output_ms);
    (quiet_ms > bound_ms(main, None)).then_some(Hung {
        started_ms,
        quiet_ms,
    })
}

/// Close the manager's or the chat's host when its turn hangs in an older wsx
/// (`manager_hung`), stop its agent's tree and log one `stopped` entry; the caller starts the
/// new host, whose restore queues the turn's messages again. True when it did.
pub(super) fn close_hung_manager(
    dir: &ManagerDir,
    main: &Path,
    answer: &ManagerResponse,
) -> anyhow::Result<bool> {
    let Some(hung) = manager_hung(dir, main, answer) else {
        return Ok(false);
    };
    let mut args = vec!["task", "manager-host", dir.project_id.as_str()];
    if let Some(chat) = &dir.chat {
        args.extend(["--chat", chat.as_str()]);
    }
    let pid = host_pid(&args);
    super::close_session(&format!("task-{}", dir.name()), main)?;
    stop_tree(pid);
    dir.append(ManagerEvent::Stopped {
        reason: hung.reason(super::now_unix_ms()),
    })?;
    Ok(true)
}

/// After a deploy and when a project's manager host starts: replace each hung host that runs
/// an older wsx, of every registered project.
/// A host that answers no status is left to `call_task_host` and `ensure_manager`.
pub fn sweep() -> Vec<String> {
    let mut errors = Vec::new();
    let Ok((config, _)) = crate::config::global::GlobalConfig::load() else {
        return errors;
    };
    let Ok(exe) = std::env::current_exe() else {
        return errors;
    };
    for project in &config.projects {
        let project_id = store::project_id(&project.name, &project.path);
        for (dir, record) in super::view::project_tasks(&project_id).unwrap_or_default() {
            if record.is_ok_and(|record| record.state == TaskState::Working) {
                if let Err(error) = replace_hung_task(&dir) {
                    errors.push(format!("task t{}: {error:#}", dir.id));
                }
            }
        }
        let managers = ManagerDir::existing(&project_id).ok().flatten();
        for dir in managers
            .into_iter()
            .chain(super::chats::open_chats(&project_id))
        {
            let answer = super::call_socket::<_, ManagerResponse>(
                &dir.socket_path(),
                &super::manager::ManagerRequest::Status,
                stall::PROBE,
            );
            if answer.is_ok_and(|answer| manager_hung(&dir, &project.path, &answer).is_some()) {
                let launch = |label, argv| {
                    crate::ops::create_session_launch(
                        &crate::runtime::Client::local(),
                        &config,
                        &project.path,
                        label,
                        argv,
                        None,
                    )
                    .map(drop)
                };
                let chat = dir.chat.as_deref();
                if let Err(error) =
                    super::owner::ensure_manager(&project.name, &project.path, &exe, chat, launch)
                {
                    errors.push(format!("{} of {}: {error:#}", dir.name(), project.name));
                }
            }
        }
    }
    errors
}

/// The pid of the running host whose arguments after the program are `args`.
fn host_pid(args: &[&str]) -> Option<libc::pid_t> {
    crate::process::find(|argv, _| {
        argv.len() == args.len() + 1
            && argv[1..]
                .iter()
                .zip(args)
                .all(|(have, want)| *have == want.as_bytes())
    })
    .into_iter()
    .next()
}

/// Stop every process that carries the agent mark of the host `pid` (`TREE_ENV`): its agents
/// and their tool processes, which a closed host session leaves running.
fn stop_tree(pid: Option<libc::pid_t>) {
    let Some(pid) = pid else {
        return;
    };
    let prefix = format!("{TREE_ENV}={pid}-");
    crate::process::stop_found(
        |_, env| {
            env.iter()
                .any(|variable| variable.starts_with(prefix.as_bytes()))
        },
        crate::process::STOP_GRACE,
    );
}
