//! `wsx project run`: an agent asks the owner to run one command outside its boundary; `wsx task
//! ran`: the session pane's command once approved. ref: docs/tasks.md "Approved runs"

use super::approve::{ask_covering, request_of};
use crate::task::{acting_as_chat, project_entry};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use wsx_core::task::manager::{ApprovalAction, ApprovalLine, RunSpec};
use wsx_core::task::owner::command_line;
use wsx_core::task::store::{self, TaskDir};

/// Set by the task host for its worker's `run`: the task that asks.
pub(crate) const WORKER_ENV: &str = "WSX_TASK_WORKER";
const MAX_WORDS: usize = 64;
const MAX_WORD_BYTES: usize = 4096;
const MAX_WHY_CHARS: usize = 500;
/// The lines of the run's end that go back to the asker.
const TAIL_LINES: usize = 40;
const TAIL_BYTES: usize = 4096;

/// Record a run request. Only an agent asks: the owner runs a command in a terminal.
pub(crate) fn ask(
    project: Option<&str>,
    cwd: Option<PathBuf>,
    why: Option<String>,
    argv: Vec<String>,
    json: bool,
) -> Result<()> {
    let worker: Option<u32> = std::env::var(WORKER_ENV)
        .ok()
        .and_then(|text| text.parse().ok());
    if !acting_as_chat() && worker.is_none() {
        bail!(
            "wsx project run is how an agent asks the owner; the owner runs the command in a terminal"
        );
    }
    if argv.is_empty() || argv.len() > MAX_WORDS || argv[0].is_empty() {
        bail!("a run is one command of 1-{MAX_WORDS} words; nothing was asked");
    }
    // ^ The owner approves what the card shows: no word may hold a character that changes how
    // it reads (a control, a line break, a bidi or zero-width mark).
    if let Some(word) = argv.iter().find(|word| {
        word.len() > MAX_WORD_BYTES || word.chars().any(wsx_core::task::owner::misleading_char)
    }) {
        bail!(
            "a word of the command is over {MAX_WORD_BYTES} bytes or holds a control, line break or bidi character ({}); nothing was asked",
            wsx_core::task::owner::shown_word(&word.chars().take(40).collect::<String>())
        );
    }
    let why = why.unwrap_or_default();
    // ^ One line: a second line could pose as the owner's or wsx's words on the card.
    if why.chars().count() > MAX_WHY_CHARS
        || why.chars().any(wsx_core::task::owner::misleading_char)
    {
        bail!("--why is one line of at most {MAX_WHY_CHARS} characters; nothing was asked");
    }
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("no current folder")?,
    };
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("--cwd {} is not a folder here", cwd.display()))?;
    if !cwd.is_dir() {
        bail!("--cwd {} is not a folder", cwd.display());
    }
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let mut request = request_of(ApprovalAction::Run, None, None, None);
    if let ApprovalLine::Request {
        task, chat, run, ..
    } = &mut request
    {
        *run = Some(RunSpec {
            argv,
            cwd: cwd.display().to_string(),
            why,
        });
        if let Some(worker) = worker {
            // ^ The task's chat hears the decision and the end; the worker gets the end as an add.
            *task = Some(worker);
            *chat = TaskDir::open(&project_id, worker)?
                .read_record()?
                .owner_chat()
                .map(str::to_string);
        }
    }
    // ^ Never approved by an owner's chat message: the owner sees the command before it runs.
    ask_covering(&main, &project_id, request, None, json).map(drop)
}

/// The pane's command: run the approved request `id` on this terminal, then record its exit
/// and last lines (a `Ran` line). Once per request; the argv is the request's, never this
/// command line's.
pub(crate) fn ran(project: Option<&str>, id: &str) -> Result<()> {
    if acting_as_chat() || std::env::var_os(WORKER_ENV).is_some() {
        bail!("wsx task ran is the session's own command");
    }
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    // ^ One run at a time: a second approval or a `ran` by hand finds the lock held.
    let log = store::run_log(&project_id, id)?;
    let claim = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log.with_extension("lock"))?;
    if claim.try_lock().is_err() {
        bail!("{id} is running");
    }
    let lines = store::read_approvals(&project_id)?;
    let run = lines
        .iter()
        .find_map(|line| match line {
            ApprovalLine::Request {
                id: asked,
                run: Some(run),
                ..
            } if asked == id => Some(run.clone()),
            _ => None,
        })
        .with_context(|| format!("no run request {id}"))?;
    let approved = lines.iter().any(|line| {
        matches!(line, ApprovalLine::Decision { id: decided, approved: true, .. } if decided == id)
    });
    if !approved {
        bail!("{id} is not approved");
    }
    if lines
        .iter()
        .any(|line| matches!(line, ApprovalLine::Ran { id: ran, .. } if ran == id))
    {
        bail!("{id} already ran");
    }
    println!("wsx: approved run {id}, in {}", run.cwd);
    println!("$ {}\n", command_line(&run.argv));
    // ^ `script` gives the command this pane's terminal (a `sudo` prompt works) and keeps a
    // copy of what it printed; its exit status is the command's (macOS `script`).
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/script")
            .arg("-q")
            .arg(&log)
            .args(&run.argv)
            .current_dir(&run.cwd)
            .status()
    } else {
        std::process::Command::new(&run.argv[0])
            .args(&run.argv[1..])
            .current_dir(&run.cwd)
            .status()
    };
    let (exit, tail) = match status {
        Ok(status) => (status.code(), tail(&log)),
        Err(error) => (Some(127), format!("cannot start it: {error}")),
    };
    store::append_approval(
        &project_id,
        ApprovalLine::Ran {
            id: id.into(),
            exit,
            tail,
            unix_ms: wsx_core::task::now_unix_ms(),
        },
    )?;
    println!(
        "\nwsx: {id} ended ({}); the asker was told. This pane can be closed.",
        exit.map_or("by a signal".into(), |code| format!("exit {code}"))
    );
    Ok(())
}

/// The last lines of the run's output, without terminal escapes and carriage returns.
fn tail(log: &std::path::Path) -> String {
    let Ok(bytes) = std::fs::read(log) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(TAIL_BYTES * 4);
    let text = strip_escapes(&String::from_utf8_lossy(&bytes[start..]));
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.rsplit('\r').next().unwrap_or(line).trim_end())
        .filter(|line| !line.is_empty())
        .collect();
    let kept = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let cut = kept.len().saturating_sub(TAIL_BYTES);
    let cut = (cut..=kept.len())
        .find(|at| kept.is_char_boundary(*at))
        .unwrap_or(kept.len());
    kept[cut..].to_string()
}

/// Text without CSI and OSC sequences and other control bytes but newline, carriage return
/// and tab.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' | '\r' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}
