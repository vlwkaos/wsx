//! A host runs its agent's `wsx task` commands: the manager host every `task` command of the
//! manager agent, a task host the worker's `check-run`, whatever the command's shape (heredoc,
//! `;`, `&&`, a pipe), so the host keeps the actor and the check slot; under `WSX_TASK_BROKER` the CLI
//! sends a `task` command's argv, stdin and folder to the host's socket and prints the host's
//! answer. ref: docs/tasks.md "Permissions"

use super::args::TaskCmd;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wsx_core::git::output_with_timeout_limit;
use wsx_core::task::owner::ACTOR_ENV;
use wsx_core::task::policy::{Capability, Policy, Role, Trust, RULES_ENV, TRUST_ENV};

pub const BROKER_ENV: &str = "WSX_TASK_BROKER";
/// The manager's own project, for `say --from-manager`; set only by the manager host.
pub const FROM_ENV: &str = "WSX_TASK_FROM";
/// Stdin, and stdout and stderr each, of one command.
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_STREAM_BYTES as u64;
const MAX_RESPONSE_BYTES: u64 = 3 * MAX_STREAM_BYTES as u64;
/// A landing waits for its host up to 300 s. A deploy or release answers once it started:
/// its command runs detached, past this limit (docs/tasks.md "Deploy and release").
const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);
/// `task` subcommands that start hosts; never run for the agent.
const HIDDEN: [&str; 2] = ["host", "manager-host"];

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub argv: Vec<String>,
    pub stdin: Option<String>,
    cwd: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Response {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// The policy of the agent a host runs now, which `launch` sets at each agent start. Before
/// the first start it grants nothing: no command runs on a guess.
#[derive(Clone)]
pub struct SharedPolicy(std::sync::Arc<std::sync::Mutex<Policy>>);

impl Default for SharedPolicy {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Policy::of(
            Trust::Untrusted,
            Role::Manager,
            Some(&[]),
        ))))
    }
}

impl SharedPolicy {
    pub fn get(&self) -> Policy {
        *self.0.lock().expect("policy lock")
    }

    pub fn set(&self, policy: Policy) {
        *self.0.lock().expect("policy lock") = policy;
    }
}

/// Whether a host runs this argv for an agent of `policy`: `task <subcommand> ...`, not a
/// hidden host command, and a command whose capability the policy grants. An argv that
/// parses to no command runs nothing in the CLI either (its own parser refuses it, or prints
/// help), so it is passed on.
pub(super) fn allowed(argv: &[String], policy: Policy) -> Result<()> {
    match argv {
        [task, sub, ..] if task == "task" && !HIDDEN.contains(&sub.as_str()) => {}
        [task, sub, ..] if task == "task" => bail!("`wsx task {sub}` is not for the manager"),
        _ => bail!("the manager may run only `wsx task` commands"),
    }
    // ^ `result` is the worker host's own, not a CLI command.
    let (needs, what): (&[Capability], String) = match (argv[1].as_str(), TaskCmd::parse_argv(argv))
    {
        ("result", _) => (&[Capability::Result], "result".into()),
        (_, Some(command)) => (command.needs(), command_name(argv)),
        (_, None) => return Ok(()),
    };
    if policy.allows_any(needs) {
        return Ok(());
    }
    bail!("{}", policy.refusal(&what, needs))
}

/// The command as the refusal names it: `chat new`, `routine changes`, `close --discard`.
fn command_name(argv: &[String]) -> String {
    match (argv[1].as_str(), argv.get(2)) {
        ("chat", Some(action)) if !action.starts_with('-') => format!("chat {action}"),
        ("routine", _) => "routine changes".into(),
        ("checkin", _) => "a check-in".into(),
        ("close", _) if argv.iter().any(|arg| arg == "--discard") => "close --discard".into(),
        (sub, _) => sub.to_string(),
    }
}

/// The CLI under `WSX_TASK_BROKER`: send a `task` or `memory` command to the host and exit
/// with its code; refuse anything else. Called before the arguments are parsed.
pub fn forward_if_brokered() {
    let Some(socket) = std::env::var_os(BROKER_ENV).filter(|value| !value.is_empty()) else {
        return;
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match forward(Path::new(&socket), argv) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("wsx: {error:#}");
            2
        }
    };
    std::process::exit(code);
}

fn forward(socket: &Path, argv: Vec<String>) -> Result<i32> {
    if !matches!(argv.first().map(String::as_str), Some("task" | "memory")) {
        bail!("only `wsx task` and `wsx memory` commands run here");
    }
    // ^ Only `-` reads standard input (docs/tasks.md); an agent's stdin may never close.
    let stdin = if argv.iter().any(|arg| arg == "-") {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(MAX_STREAM_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .context("cannot read standard input")?;
        if bytes.len() > MAX_STREAM_BYTES {
            bail!("standard input is larger than {MAX_STREAM_BYTES} bytes");
        }
        Some(String::from_utf8(bytes).context("standard input is not UTF-8 text")?)
    } else {
        None
    };
    let cwd = std::env::current_dir().context("cannot read the current folder")?;
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("the host does not answer on {}", socket.display()))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    // ^ A worker's check-run waits for the machine's check slot as long as the queue is, then
    // for its checks' own timeouts; its host always answers or closes the socket.
    let check_run = argv.get(1).is_some_and(|sub| sub == "check-run");
    stream.set_read_timeout((!check_run).then_some(RUN_LIMIT + Duration::from_secs(30)))?;
    stream.write_all(&serde_json::to_vec(&Request { argv, stdin, cwd })?)?;
    // ^ The host reads one JSON value and needs no end mark. One that refuses the command at once has
    // answered and closed before this: macOS then fails the shutdown with ENOTCONN (os error 57),
    // and its answer is still readable.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let response: Response =
        read_one(&stream, MAX_RESPONSE_BYTES).context("the host did not answer")?;
    print!("{}", response.stdout);
    eprint!("{}", response.stderr);
    let _ = std::io::stdout().flush();
    Ok(response.code)
}

/// What the manager host's broker needs besides its socket.
pub struct ManagerBroker {
    pub exe: PathBuf,
    pub fallback_cwd: PathBuf,
    /// The manager's own project, and its task ID (its memory scope).
    pub own: String,
    pub project_id: String,
    pub actor: String,
    /// Answers the host owes, by sender (`chat:<name>` or a project).
    pub owed: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, u32>>>,
    pub policy: SharedPolicy,
}

/// The manager host's side: every `task` command runs as its `actor` (`manager` or
/// `chat:<name>`). Another project is reached only by `ledger` and `say --from-manager`, a
/// chat of this one by `say --from-chat`; a `--reply` spends an answer the
/// host owes (`owed`), and `sent` hears each message that went out.
/// ref: docs/tasks.md "Managers talk", "Chats"
pub fn serve_manager(
    listener: UnixListener,
    broker: ManagerBroker,
    sent: impl Fn(String, String, bool) + Send + Sync + 'static,
) {
    let ManagerBroker {
        exe,
        fallback_cwd,
        own,
        project_id,
        actor,
        owed,
        policy,
    } = broker;
    serve(listener, move |request: Request| {
        let policy = policy.get();
        if request.argv.first().is_some_and(|first| first == "memory") {
            return run_memory(&exe, request, &fallback_cwd, &project_id, policy);
        }
        let target = match allowed(&request.argv, policy)
            .and_then(|()| reach(&request.argv, &own, &actor))
        {
            Ok(target) => target,
            Err(error) => return refused(format!("{error:#}")),
        };
        let reply = request.argv.iter().any(|arg| arg == "--reply");
        let owes = |to: &String| {
            owed.lock()
                .expect("owed lock")
                .get(to)
                .is_some_and(|count| *count > 0)
        };
        if let (Some(to), true) = (&target, reply) {
            if !owes(to) {
                return refused(format!(
                    "no message from {} waits for an answer, and a reply is not answered",
                    match to.strip_prefix("chat:") {
                        Some(chat) => chat.to_string(),
                        None => format!("the manager of {to}"),
                    }
                ));
            }
        }
        let response = run(&exe, request, &fallback_cwd, &own, &actor, policy)
            .unwrap_or_else(|error| refused(format!("{error:#}")));
        if let (Some(to), 0) = (target, response.code) {
            // ^ Spent only by a reply that went out.
            if reply {
                if let Some(count) = owed.lock().expect("owed lock").get_mut(&to) {
                    *count = count.saturating_sub(1);
                }
            }
            let text = serde_json::from_str::<serde_json::Value>(&response.stdout)
                .ok()
                .and_then(|value| value["text"].as_str().map(str::to_string))
                .unwrap_or_default();
            sent(to, text, reply);
        }
        response
    });
}

/// An agent's `wsx memory` call, run for it by its host: refused unless its policy grants the
/// command, and pinned to its own project's store, so a write names no other scope (a read
/// may). Which commands each role has is `policy`'s; a `rule` record is judged by the store
/// (`RULES_ENV`). ref: docs/memory.md "Agents"
pub fn run_memory(
    exe: &Path,
    mut request: Request,
    fallback: &Path,
    own_id: &str,
    policy: Policy,
) -> Response {
    let outcome = memory_pin(&request.argv, own_id, policy).and_then(|pin| {
        if pin {
            request.argv.extend(["--scope".into(), own_id.into()]);
        }
        // ^ A memory command reads no actor, and the owner's is the one an empty mark gives.
        run(exe, request, fallback, own_id, "", policy)
    });
    outcome.unwrap_or_else(|error| refused(format!("{error:#}")))
}

/// Whether `policy` grants the `memory` argv, and a write stays in `own_id`'s scope; true when
/// the agent named no scope, so the caller adds its own.
fn memory_pin(argv: &[String], own_id: &str, policy: Policy) -> Result<bool> {
    use clap::Parser;
    // ^ An argv that does not parse runs nothing: the CLI's own parser refuses it.
    let Ok(args) = crate::cli::Args::try_parse_from(
        std::iter::once("wsx").chain(argv.iter().map(String::as_str)),
    ) else {
        return Ok(false);
    };
    let Some(crate::cli::Command::Memory {
        subcommand, scope, ..
    }) = args.command
    else {
        bail!("only `wsx task` and `wsx memory` commands run here");
    };
    let needs = subcommand.needs();
    if !policy.allows_any(needs) {
        bail!(
            "{}",
            policy.refusal(&format!("memory {}", subcommand.name()), needs)
        );
    }
    if needs != [Capability::Recall] && scope.iter().any(|s| s != own_id) {
        bail!("an agent writes memory only in its own project's scope ({own_id})");
    }
    Ok(scope.is_empty())
}

/// The commands a manager may run in another registered project: that project's tasks, in its
/// store, with its numbers, checks and land target.
const MANAGER_REACH: [&str; 9] = [
    "new", "show", "list", "add", "steer", "stop", "hold", "land", "close",
];

/// The other project a command reaches. A manager may run `MANAGER_REACH` there (the task
/// lives in that project's store, as the owner's `-p` makes it) and `say --from-manager`; a
/// chat only reads `ledger`. Or `chat:<name>` for `say --from-chat` to a chat of its own
/// (`manager` without `--to`); `None` for anything else of its own. A `--from-manager`
/// message must go to another project. ref: docs/tasks.md "Managers talk"
fn reach(argv: &[String], own: &str, actor: &str) -> Result<Option<String>> {
    let project = argv
        .iter()
        .enumerate()
        .find_map(|(index, arg)| match arg.as_str() {
            "-p" | "--project" => argv.get(index + 1).cloned(),
            _ => arg
                .strip_prefix("--project=")
                .or_else(|| arg.strip_prefix("-p").filter(|rest| !rest.is_empty()))
                .map(str::to_string),
        });
    let sub = argv.get(1).map(String::as_str).unwrap_or_default();
    let from_manager = argv.iter().any(|arg| arg == "--from-manager");
    let from_chat = argv.iter().any(|arg| arg == "--from-chat");
    let discard = argv.iter().any(|arg| arg == "--discard");
    let to = argv
        .iter()
        .position(|arg| arg == "--to")
        .and_then(|at| argv.get(at + 1))
        .cloned()
        .or_else(|| {
            argv.iter()
                .find_map(|arg| arg.strip_prefix("--to=").map(str::to_string))
        });
    match project.filter(|project| project != own) {
        None if from_manager => bail!("--from-manager messages another project's manager; name it with -p"),
        None if sub == "say" && from_chat => Ok(Some(format!(
            "chat:{}",
            to.as_deref().unwrap_or("manager")
        ))),
        None => Ok(None),
        // ^ Reading another ledger is not a message; only a sent message is logged.
        Some(_) if sub == "ledger" => Ok(None),
        Some(other) if actor == "manager" && sub == "say" && from_manager => Ok(Some(other)),
        Some(_) if actor == "manager" && MANAGER_REACH.contains(&sub) && !discard => Ok(None),
        Some(other) if actor == "manager" => bail!(
            "a manager acts in another project only with {}, never --discard or an approved action; for {other} it may also read `wsx task ledger -p {other}` and `wsx task say -p {other} --from-manager`",
            MANAGER_REACH.join(", ")
        ),
        Some(other) => bail!(
            "only a project's manager acts in another project; a chat stays in its own (for {other} it may read `wsx task ledger -p {other}`)"
        ),
    }
}

/// A host's side: one thread per command, so a long one does not hold the others or the
/// host's own loop.
pub fn serve(listener: UnixListener, handle: impl Fn(Request) -> Response + Send + Sync + 'static) {
    let handle = std::sync::Arc::new(handle);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let handle = handle.clone();
        std::thread::spawn(move || answer(stream, &*handle));
    }
}

// ^ Each side reads one JSON value, never up to the end of the stream: on macOS a socket's
// close-on-exec is set after it is opened, so a `wsx` child started meanwhile by another
// command may hold a copy, and the end would wait for that child. `shutdown` ends the
// socket whatever holds a copy.
fn answer(mut stream: UnixStream, handle: &dyn Fn(Request) -> Response) {
    let started = std::time::Instant::now();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let response = match read_one::<Request>(&stream, MAX_REQUEST_BYTES) {
        // ^ The time tells a slow client from a broken one.
        Err(error) => refused(format!(
            "cannot read the request after {} ms: {error:#}",
            started.elapsed().as_millis()
        )),
        Ok(request) => handle(request),
    };
    if let Ok(line) = serde_json::to_vec(&response) {
        let _ = stream.write_all(&line);
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// One JSON value from the stream, at most `limit` bytes, without waiting for its end. A
/// longer one is cut, so it is not valid; an empty one is a socket that closed unanswered.
fn read_one<T: serde::de::DeserializeOwned>(stream: &UnixStream, limit: u64) -> Result<T> {
    let reader = std::io::BufReader::new(stream.take(limit));
    let mut values = serde_json::Deserializer::from_reader(reader).into_iter::<T>();
    match values.next() {
        Some(value) => {
            value.with_context(|| format!("not a valid message of at most {limit} bytes"))
        }
        None => bail!("the connection closed without a message; run the command again"),
    }
}

pub fn refused(message: String) -> Response {
    Response {
        stdout: String::new(),
        stderr: format!("wsx: {message}\n"),
        code: 2,
    }
}

/// Run the command as `actor` of `own`, in the agent's folder when it is one.
fn run(
    exe: &Path,
    request: Request,
    fallback: &Path,
    own: &str,
    actor: &str,
    policy: Policy,
) -> Result<Response> {
    let cwd = if request.cwd.is_dir() {
        request.cwd
    } else {
        fallback.to_path_buf()
    };
    let mut command = Command::new(exe);
    command
        .args(&request.argv)
        .current_dir(cwd)
        .env(ACTOR_ENV, actor)
        .env(FROM_ENV, own)
        .env(TRUST_ENV, policy.trust.as_str())
        .env(
            RULES_ENV,
            if policy.allows(Capability::Decide) {
                "allow"
            } else {
                "deny"
            },
        )
        .env_remove(BROKER_ENV)
        .stdin(Stdio::null());
    if let Some(text) = request.stdin {
        // ^ The writer ends when the child has read it all or `command`, which holds the
        // read end, is dropped after the run.
        let (reader, mut writer) = std::io::pipe().context("cannot open a pipe")?;
        command.stdin(reader);
        std::thread::spawn(move || {
            let _ = writer.write_all(text.as_bytes());
        });
    }
    let output = match output_with_timeout_limit(&mut command, RUN_LIMIT, MAX_STREAM_BYTES) {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::TimedOut => {
            return Ok(Response {
                stdout: String::new(),
                stderr: format!("wsx: stopped after {} s\n", RUN_LIMIT.as_secs()),
                code: 124,
            })
        }
        Err(error) if error.kind() == ErrorKind::InvalidData => {
            bail!("the command wrote more than {MAX_STREAM_BYTES} bytes to stdout or stderr")
        }
        Err(error) => return Err(error).context("cannot run wsx"),
    };
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    let (stdout, stderr) = (text(output.stdout), text(output.stderr));
    let code = output.status.code().unwrap_or(1);
    Ok(Response {
        stdout,
        stderr,
        code,
    })
}
