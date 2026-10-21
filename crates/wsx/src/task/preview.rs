//! `wsx task preview`: serve one task for a person to try, on its own tailnet port.
//! ref: docs/tasks.md "Preview"
//!
//! A detached `wsx task preview-host` owns the preview: it starts `tasks.preview.command`
//! with `$PORT` in the task's working copy (or a temporary copy of a landed commit), waits
//! for `tasks.preview.ready`, maps the port with `tailscale serve` and writes
//! `preview.json`. It stops after an hour, on `preview --stop` (`preview.stop`), when the
//! task closes or the command exits, and then removes the mapping, the process group and
//! the temporary copy.

use super::hosting;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::model::workspace::TaskPreview;
use wsx_core::task::{copy, now_unix_ms, store::TaskDir, LogEvent, TaskState};

const STATE_FILE: &str = "preview.json";
const STOP_FILE: &str = "preview.stop";
const ERROR_FILE: &str = "preview.error";
const LOG_FILE: &str = "preview.log";
const LIFETIME: Duration = Duration::from_secs(60 * 60);
const READY_WAIT: Duration = Duration::from_secs(180);
const START_WAIT: Duration = Duration::from_secs(200);
const STOP_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(500);

/// `preview.json`: a running preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Running {
    pub url: String,
    pub port: u16,
    /// The preview host's pid; the command runs in a process group of its own.
    pub pid: u32,
    pub until_unix_ms: u64,
}

pub fn running(dir: &TaskDir) -> Option<Running> {
    let bytes = dir.read_file(STATE_FILE, 4096).ok().flatten()?;
    let running: Running = serde_json::from_slice(&bytes).ok()?;
    // SAFETY: signal 0 only checks that the pid exists.
    (unsafe { libc::kill(running.pid as i32, 0) } == 0).then_some(running)
}

/// Start the preview, or answer the running one.
pub fn start(dir: &TaskDir) -> Result<Running> {
    let record = dir.read_record()?;
    let settings =
        load_task_config(&record.project_path).map_err(|error| anyhow::anyhow!(error))?;
    if settings.preview.is_none() {
        bail!(
            "{} configures no tasks.preview; nothing to try",
            record.project
        );
    }
    // ^ A landed task keeps `landed`; a closed one never landed and has nothing to serve.
    if record.state == TaskState::Closed {
        bail!(
            "task t{} was closed without landing; nothing to try",
            record.id
        );
    }
    if let Some(running) = running(dir) {
        return Ok(running);
    }
    for name in [STATE_FILE, STOP_FILE, ERROR_FILE] {
        let _ = std::fs::remove_file(dir.file(name));
    }
    let log = log_file(dir)?;
    hosting::spawn_wsx(
        &[
            "task",
            "preview-host",
            &record.project_id,
            &record.id.to_string(),
        ],
        log,
    )?;
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if let Some(running) = running(dir) {
            return Ok(running);
        }
        if let Some(error) = dir.read_file(ERROR_FILE, 8192).ok().flatten() {
            bail!(
                "the preview did not start: {}",
                String::from_utf8_lossy(&error).trim()
            );
        }
        std::thread::sleep(POLL);
    }
    bail!(
        "the preview did not answer within {} s; see {}",
        START_WAIT.as_secs(),
        dir.file(LOG_FILE).display()
    )
}

/// Stop a running preview and wait for its cleanup. False when none ran.
pub fn stop(dir: &TaskDir) -> Result<bool> {
    let Some(running) = running(dir) else {
        let _ = std::fs::remove_file(dir.file(STATE_FILE));
        return Ok(false);
    };
    dir.write_file(STOP_FILE, b"stop")?;
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if self::running(dir).is_none() {
            return Ok(true);
        }
        std::thread::sleep(POLL);
    }
    bail!(
        "the preview on port {} did not stop within {} s",
        running.port,
        STOP_WAIT.as_secs()
    )
}

fn log_file(dir: &TaskDir) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.file(LOG_FILE))
        .context("cannot open the preview log")
}

/// The detached preview host. Its failure before the preview runs is in `preview.error`.
pub fn host(project_id: &str, id: u32) -> Result<()> {
    let dir = TaskDir::open(project_id, id)?;
    let mut preview = Preview::default();
    let result = preview.run(&dir);
    preview.clean(&dir);
    if let Err(error) = &result {
        let _ = dir.write_file(ERROR_FILE, format!("{error:#}").as_bytes());
    }
    result
}

#[derive(Default)]
struct Preview {
    child: Option<std::process::Child>,
    served: Option<u16>,
    /// The temporary copy of a landed commit, and the main checkout it belongs to.
    temporary: Option<(PathBuf, PathBuf)>,
}

impl Preview {
    fn run(&mut self, dir: &TaskDir) -> Result<()> {
        let record = dir.read_record()?;
        let settings =
            load_task_config(&record.project_path).map_err(|error| anyhow::anyhow!(error))?;
        let config = settings
            .preview
            .context("tasks.preview is not configured")?;
        let cwd = if record.working_copy.exists() {
            record.working_copy.clone()
        } else {
            let commit =
                landed_commit(dir).context("the task has no working copy and no landed commit")?;
            let path = copy::working_copy_path_named(
                &record.project_path,
                &format!("preview-{id}", id = record.id),
            )?;
            copy::create_detached(&record.project_path, &commit, &path)?;
            self.temporary = Some((path.clone(), record.project_path.clone()));
            path
        };
        let port = free_port(&config)?;
        let output = dir.file(LOG_FILE);
        let log = std::fs::OpenOptions::new().append(true).open(&output)?;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &config.command])
            .current_dir(&cwd)
            .env("PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        self.child = Some(
            command
                .spawn()
                .context("cannot start tasks.preview.command")?,
        );
        self.wait_ready(port, &config.ready)?;
        tailscale(&[
            "serve",
            "--bg",
            &format!("--https={port}"),
            &format!("http://127.0.0.1:{port}"),
        ])?;
        self.served = Some(port);
        let host = tailnet_name()?;
        let running = Running {
            url: format!("https://{host}:{port}/"),
            port,
            pid: std::process::id(),
            until_unix_ms: now_unix_ms() + LIFETIME.as_millis() as u64,
        };
        dir.write_file(STATE_FILE, &serde_json::to_vec(&running)?)?;
        let _ = dir.append(LogEvent::Preview {
            url: running.url.clone(),
            port,
        });
        let started = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let closed = dir.read_record().map_or(true, |record| {
                record.state == TaskState::Closed && record.working_copy == cwd
            });
            let exited = self
                .child
                .as_mut()
                .is_none_or(|child| !matches!(child.try_wait(), Ok(None)));
            if started.elapsed() >= LIFETIME || dir.file(STOP_FILE).exists() || closed || exited {
                return Ok(());
            }
        }
    }

    /// Wait until `ready` answers 200 on the port, or the command exits.
    fn wait_ready(&mut self, port: u16, ready: &str) -> Result<()> {
        let deadline = Instant::now() + READY_WAIT;
        while Instant::now() < deadline {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    bail!("tasks.preview.command exited ({status}) before {ready} answered");
                }
            }
            if answers_ok(port, ready) {
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
        bail!(
            "{ready} on port {port} did not answer 200 within {} s",
            READY_WAIT.as_secs()
        )
    }

    /// Remove the Serve mapping, the process group and the temporary copy.
    fn clean(&mut self, dir: &TaskDir) {
        if let Some(port) = self.served.take() {
            let _ = tailscale(&["serve", "--bg", &format!("--https={port}"), "off"]);
        }
        if let Some(mut child) = self.child.take() {
            let group = child.id() as i32;
            // SAFETY: signals to the process group this host started.
            unsafe { libc::kill(-group, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(100));
            }
            // SAFETY: as above.
            unsafe { libc::kill(-group, libc::SIGKILL) };
            let _ = child.wait();
        }
        if let Some((path, main)) = self.temporary.take() {
            if path.join("Cargo.toml").exists() {
                super::cargo_clean(&path);
            }
            let _ = copy::remove(&main, &path, None);
        }
        for name in [STATE_FILE, STOP_FILE] {
            let _ = std::fs::remove_file(dir.file(name));
        }
    }
}

fn landed_commit(dir: &TaskDir) -> Option<String> {
    dir.read_log()
        .ok()?
        .iter()
        .rev()
        .find_map(|entry| match &entry.event {
            LogEvent::Landed { commit } => Some(commit.clone()),
            _ => None,
        })
}

/// The first port of the range that nothing listens on.
fn free_port(config: &TaskPreview) -> Result<u16> {
    let (first, last) = config.ports;
    (first..=last)
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .with_context(|| format!("no free port in tasks.preview.ports {first}-{last}"))
}

/// Whether `GET <path>` on the port answers with status 200.
fn answers_ok(port: u16, path: &str) -> bool {
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let request = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 16];
    let read = stream.read(&mut head).unwrap_or(0);
    let line = String::from_utf8_lossy(&head[..read]);
    line.starts_with("HTTP/1.") && line.get(9..12) == Some("200")
}

/// Run `tailscale <args>`: port-scoped only, never `serve reset`.
fn tailscale(args: &[&str]) -> Result<String> {
    let mut command = Command::new("tailscale");
    command.args(args).stdin(Stdio::null());
    let output = wsx_core::git::output_with_timeout_limit(
        &mut command,
        Duration::from_secs(30),
        1024 * 1024,
    )
    .with_context(|| format!("cannot run tailscale {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "tailscale {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// This machine's tailnet name, such as `host.tail1234.ts.net`.
fn tailnet_name() -> Result<String> {
    let status: serde_json::Value = serde_json::from_str(&tailscale(&["status", "--json"])?)
        .context("tailscale status --json is not JSON")?;
    status["Self"]["DNSName"]
        .as_str()
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| !name.is_empty())
        .context("tailscale status names no DNSName for this machine")
}
