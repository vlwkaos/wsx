//! The manager host runs the manager agent's `wsx task` commands. The agent's Bash stays in
//! its sandbox whatever the command's shape (heredoc, `;`, `&&`, a pipe); under
//! `WSX_TASK_BROKER` the CLI sends a `task` command's argv, stdin and folder to the host's
//! socket and prints the host's answer. ref: docs/tasks.md "Permissions"

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
/// Stdin, and stdout and stderr each, of one command.
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 2 * MAX_STREAM_BYTES as u64;
const MAX_RESPONSE_BYTES: u64 = 3 * MAX_STREAM_BYTES as u64;
/// A landing waits for its host up to 300 s; a deploy the owner allowed may run longer.
const RUN_LIMIT: Duration = Duration::from_secs(15 * 60);
/// `task` subcommands that start hosts; never run for the agent.
const HIDDEN: [&str; 2] = ["host", "manager-host"];

#[derive(Serialize, Deserialize)]
struct Request {
    argv: Vec<String>,
    stdin: Option<String>,
    cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Response {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Whether the host runs this argv: `task <subcommand> ...`, not a hidden host command.
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
    allowed(&argv)?;
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
        .with_context(|| format!("the manager host does not answer on {}", socket.display()))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    stream.set_read_timeout(Some(RUN_LIMIT + Duration::from_secs(30)))?;
    stream.write_all(&serde_json::to_vec(&Request { argv, stdin, cwd })?)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let response: Response =
        read_one(&stream, MAX_RESPONSE_BYTES).context("the manager host did not answer")?;
    print!("{}", response.stdout);
    eprint!("{}", response.stderr);
    let _ = std::io::stdout().flush();
    Ok(response.code)
}

/// The manager host's side: one thread per command, so a long landing does not hold the
/// others or the host's own loop.
pub fn serve(listener: UnixListener, exe: PathBuf, fallback_cwd: PathBuf) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (exe, fallback) = (exe.clone(), fallback_cwd.clone());
        std::thread::spawn(move || answer(stream, &exe, &fallback));
    }
}

// ^ Each side reads one JSON value, never up to the end of the stream: on macOS a socket's
// close-on-exec is set after it is opened, so a `wsx` child started meanwhile by another
// command may hold a copy, and the end would wait for that child. `shutdown` ends the
// socket whatever holds a copy.
fn answer(mut stream: UnixStream, exe: &Path, fallback: &Path) {
    let started = std::time::Instant::now();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let response = match read_one::<Request>(&stream, MAX_REQUEST_BYTES) {
        // ^ The time tells a slow client from a broken one.
        Err(error) => refused(format!(
            "cannot read the request after {} ms: {error:#}",
            started.elapsed().as_millis()
        )),
        Ok(request) => match allowed(&request.argv) {
            Err(error) => refused(format!("{error:#}")),
            Ok(()) => {
                run(exe, request, fallback).unwrap_or_else(|error| refused(format!("{error:#}")))
            }
        },
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

fn refused(message: String) -> Response {
    Response {
        stdout: String::new(),
        stderr: format!("wsx: {message}\n"),
        code: 2,
    }
}

/// Run the command as the manager actor, in the agent's folder when it is one.
fn run(exe: &Path, request: Request, fallback: &Path) -> Result<Response> {
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
