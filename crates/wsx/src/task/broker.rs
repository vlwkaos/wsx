//! A host runs its agent's `wsx task` commands: the manager host every `task` command of the
//! manager agent, a task host the worker's `check-run`. The agent's Bash stays in its sandbox
//! whatever the command's shape (heredoc, `;`, `&&`, a pipe); under `WSX_TASK_BROKER` the CLI
//! sends a `task` command's argv, stdin and folder to the host's socket and prints the host's
//! answer. ref: docs/tasks.md "Permissions"

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wsx_core::git::output_with_timeout_limit;
use wsx_core::task::owner::ACTOR_ENV;

pub const BROKER_ENV: &str = "WSX_TASK_BROKER";
/// The manager's own project, for `say --from-manager`; set only by the manager host.
pub const FROM_ENV: &str = "WSX_TASK_FROM";
/// Stdin, and stdout and stderr each, of one command.
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_STREAM_BYTES as u64;
const MAX_RESPONSE_BYTES: u64 = 3 * MAX_STREAM_BYTES as u64;
/// A landing waits for its host up to 300 s; a deploy the owner allowed may run longer.
const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);
/// `task` subcommands that start hosts; never run for the agent.
const HIDDEN: [&str; 2] = ["host", "manager-host"];

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub argv: Vec<String>,
    stdin: Option<String>,
    cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Response {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Whether the manager host runs this argv: `task <subcommand> ...`, not a hidden host command.
fn allowed(argv: &[String]) -> Result<()> {
    match argv {
        [task, sub, ..] if task == "task" && !HIDDEN.contains(&sub.as_str()) => Ok(()),
        [task, sub, ..] if task == "task" => bail!("`wsx task {sub}` is not for the manager"),
        _ => bail!("the manager may run only `wsx task` commands"),
    }
}

/// The CLI under `WSX_TASK_BROKER`: send a `task` command to the manager host and exit with
/// its code; refuse anything else. Called before the arguments are parsed.
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
    if argv.first().map(String::as_str) != Some("task") {
        bail!("only `wsx task` commands run here");
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
    stream.shutdown(std::net::Shutdown::Write)?;
    let response: Response =
        read_one(&stream, MAX_RESPONSE_BYTES).context("the host did not answer")?;
    print!("{}", response.stdout);
    eprint!("{}", response.stderr);
    let _ = std::io::stdout().flush();
    Ok(response.code)
}

/// The manager host's side: every `task` command runs as the manager actor. Another project
/// is reached only by `ledger` and `say --from-manager`; a `--reply` spends an answer the
/// manager owes (`owed`), and `sent` hears each message that went out.
/// ref: docs/tasks.md "Managers talk"
pub fn serve_manager(
    listener: UnixListener,
    exe: PathBuf,
    fallback_cwd: PathBuf,
    own: String,
    owed: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, u32>>>,
    sent: impl Fn(String, String, bool) + Send + Sync + 'static,
) {
    serve(listener, move |request: Request| {
        let target = match allowed(&request.argv).and_then(|()| reach(&request.argv, &own)) {
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
                    "no message from the manager of {to} waits for an answer, and a reply is not answered"
                ));
            }
        }
        let response = run(&exe, request, &fallback_cwd, &own)
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

/// The other project a command reaches, which only `ledger` and `say --from-manager` may;
/// `None` for the manager's own. A `--from-manager` message must go to another project.
fn reach(argv: &[String], own: &str) -> Result<Option<String>> {
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
    match project.filter(|project| project != own) {
        None if from_manager => bail!("--from-manager messages another project's manager; name it with -p"),
        None => Ok(None),
        // ^ Reading another ledger is not a message; only a sent message is logged.
        Some(_) if sub == "ledger" => Ok(None),
        Some(other) if sub == "say" && from_manager => Ok(Some(other)),
        Some(other) => bail!(
            "a manager acts only in its own project; for {other} it may read `wsx task ledger -p {other}` and `wsx task say -p {other} --from-manager`"
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

/// One JSON value from the stream, at most `limit` bytes, without waiting for its end.
fn read_one<T: serde::de::DeserializeOwned>(stream: &UnixStream, limit: u64) -> Result<T> {
    let reader = std::io::BufReader::new(stream.take(limit));
    let mut values = serde_json::Deserializer::from_reader(reader).into_iter::<T>();
    match values.next() {
        Some(value) => value.context("not a valid message"),
        None => bail!("the message is empty or larger than {limit} bytes"),
    }
}

pub fn refused(message: String) -> Response {
    Response {
        stdout: String::new(),
        stderr: format!("wsx: {message}\n"),
        code: 2,
    }
}

/// Run the command as the manager actor of `own`, in the agent's folder when it is one.
fn run(exe: &Path, request: Request, fallback: &Path, own: &str) -> Result<Response> {
    let cwd = if request.cwd.is_dir() {
        request.cwd
    } else {
        fallback.to_path_buf()
    };
    let mut command = Command::new(exe);
    command
        .args(&request.argv)
        .current_dir(cwd)
        .env(ACTOR_ENV, "manager")
        .env(FROM_ENV, own)
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
