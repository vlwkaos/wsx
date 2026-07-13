//! Supported application boundary for routine daemon requests and startup.

use super::ipc::{self, Action, Request, Response};
use super::RoutineError;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const STARTUP_FD_ENV: &str = "WSX_ROUTINE_STARTUP_FD";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Client for a routine daemon rooted at one machine-local state directory.
///
/// [`request`](Self::request) never starts a daemon. [`request_with_start`](Self::request_with_start)
/// first probes daemon availability with a status request, then starts the
/// caller-provided command only when the daemon is unavailable. The caller's
/// request is sent exactly once. The command must run a routine daemon that
/// writes `ready` or `error:<message>` to the descriptor supplied in the
/// `WSX_ROUTINE_STARTUP_FD` environment variable. The client adds that
/// handshake and a detached process group; executable, arguments, standard
/// streams, and other environment are controlled by the caller.
#[derive(Debug, Clone)]
pub struct RoutineClient {
    root: PathBuf,
    startup_timeout: Duration,
}

impl RoutineClient {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
    }

    /// Override the bounded startup timeout, primarily for hosts and tests.
    pub fn with_startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn socket_path(&self) -> PathBuf {
        self.root.join("daemon-v1.sock")
    }

    /// Send to an already-running daemon without changing daemon lifecycle.
    pub fn request(&self, request: &Request) -> Result<Response, RoutineError> {
        ipc::send(&self.socket_path(), request)?.into_result()
    }

    /// Send a request once, starting the daemon with `command` if it is unavailable.
    pub fn request_with_start(
        &self,
        request: &Request,
        mut command: Command,
    ) -> Result<Response, RoutineError> {
        // ^ Probe with an observation so a mutation is never retried after an ambiguous disconnect.
        let status = Request::new(request.project.clone(), Action::Status);
        let availability = if matches!(&request.action, Action::Status) {
            self.request(request)
        } else {
            self.request(&status)
        };
        match availability {
            Ok(response) if matches!(&request.action, Action::Status) => return Ok(response),
            Ok(_) => return self.request(request),
            Err(RoutineError::Unavailable(_)) => {}
            Err(error) => return Err(error),
        }

        self.start(&request.project, &mut command)?;
        self.request(request)
    }

    fn start(&self, project: &Path, command: &mut Command) -> Result<(), RoutineError> {
        let deadline = Instant::now()
            .checked_add(self.startup_timeout)
            .ok_or_else(|| {
                RoutineError::Validation("routine daemon startup timeout is too large".into())
            })?;
        let (read_end, write_end) = startup_pipe()?;
        let read_fd = read_end.as_raw_fd();
        let write_fd = write_end.as_raw_fd();
        command.env(STARTUP_FD_ENV, write_fd.to_string());
        unsafe {
            command.pre_exec(move || {
                libc::close(read_fd);
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(write_end);

        let status = Request::new(project.to_path_buf(), Action::Status);
        let mut read_end = std::fs::File::from(read_end);
        let result = self.await_startup(&status, &mut child, &mut read_end, deadline);
        if result.is_err() {
            stop_startup_child(&mut child);
        }
        result
    }

    fn await_startup(
        &self,
        status: &Request,
        child: &mut Child,
        read_end: &mut std::fs::File,
        deadline: Instant,
    ) -> Result<(), RoutineError> {
        while Instant::now() < deadline {
            if fd_readable(read_end.as_raw_fd())? {
                // ^ Concurrent spawns may inherit peer pipe writers, so handshake reads cannot wait for EOF.
                let mut startup = [0_u8; 4096];
                let bytes = read_end.read(&mut startup)?;
                let startup = std::str::from_utf8(&startup[..bytes]).map_err(|_| {
                    RoutineError::Corrupt(
                        "routine daemon returned non-UTF-8 startup response".into(),
                    )
                })?;
                if startup == "ready" {
                    return self
                        .poll_ready(status, deadline)
                        .map(|_| ())
                        .ok_or_else(|| {
                            RoutineError::Unavailable(
                                "routine daemon reported ready but did not accept status requests"
                                    .into(),
                            )
                        });
                }
                if let Some(error) = startup.strip_prefix("error:") {
                    if self.poll_ready(status, deadline).is_some() {
                        // ^ A singleton-race loser has failed even though its peer is ready.
                        stop_startup_child(child);
                        return Ok(());
                    }
                    return Err(RoutineError::Unavailable(format!(
                        "routine daemon failed to start: {error}"
                    )));
                }
                if startup.is_empty() {
                    if let Some(exit) = child.try_wait()? {
                        return Err(RoutineError::Unavailable(format!(
                            "routine daemon exited during startup: {exit}"
                        )));
                    }
                }
                return Err(RoutineError::Corrupt(
                    "routine daemon returned an invalid startup response".into(),
                ));
            }
            if let Some(exit) = child.try_wait()? {
                return Err(RoutineError::Unavailable(format!(
                    "routine daemon exited during startup: {exit}"
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Err(RoutineError::Unavailable(format!(
            "routine daemon did not become ready within {} ms",
            self.startup_timeout.as_millis()
        )))
    }

    fn poll_ready(&self, status: &Request, deadline: Instant) -> Option<Response> {
        while Instant::now() < deadline {
            if let Ok(response) = self.request(status) {
                return Some(response);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        None
    }
}

fn startup_pipe() -> Result<(OwnedFd, OwnedFd), RoutineError> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}

fn fd_readable(fd: i32) -> Result<bool, RoutineError> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(result > 0)
}

fn stop_startup_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn test_root(label: &str) -> PathBuf {
        PathBuf::from(".tmp").join(format!(
            "routine-client-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn helper_command(root: &Path, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("routine::client::tests::daemon_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env("WSX_ROUTINE_TEST_ROOT", root)
            .env("WSX_ROUTINE_TEST_MODE", mode);
        command
    }

    fn status(root: &Path) -> Request {
        Request::new(root.to_path_buf(), Action::Status)
    }

    #[test]
    fn direct_request_does_not_start_an_absent_daemon() {
        let root = test_root("direct");
        let result = RoutineClient::new(root.clone()).request(&status(&root));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        assert!(!root.exists());
    }

    #[test]
    fn auto_start_serves_request_and_direct_shutdown_stops_daemon() {
        let root = test_root("start");
        let client = RoutineClient::new(root.clone());
        let response = client
            .request_with_start(&status(&root), helper_command(&root, "daemon"))
            .unwrap();
        assert!(matches!(response, Response::Daemon { .. }));
        client
            .request(&Request::new(root.clone(), Action::Shutdown))
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_auto_starts_converge_on_one_daemon() {
        let root = test_root("race");
        let barrier = Arc::new(Barrier::new(3));
        let mut callers = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = barrier.clone();
            callers.push(std::thread::spawn(move || {
                barrier.wait();
                RoutineClient::new(root.clone())
                    .request_with_start(&status(&root), helper_command(&root, "daemon"))
            }));
        }
        barrier.wait();
        for caller in callers {
            assert!(matches!(
                caller.join().unwrap(),
                Ok(Response::Daemon { .. })
            ));
        }
        RoutineClient::new(root.clone())
            .request(&Request::new(root.clone(), Action::Shutdown))
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn singleton_race_reaps_the_losing_startup_child() {
        let root = test_root("race-reap");
        let barrier = Arc::new(Barrier::new(3));
        let mut callers = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = barrier.clone();
            callers.push(std::thread::spawn(move || {
                let client = RoutineClient::new(root.clone());
                let mut command = helper_command(&root, "daemon_with_pid");
                barrier.wait();
                client.start(&root, &mut command)
            }));
        }
        barrier.wait();
        for caller in callers {
            caller.join().unwrap().unwrap();
        }

        let winner_pid = match RoutineClient::new(root.clone())
            .request(&status(&root))
            .unwrap()
        {
            Response::Daemon { pid, .. } => pid,
            response => panic!("expected daemon response, got {response:?}"),
        };
        let helper_pids: Vec<u32> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                name.to_str()?
                    .strip_prefix("helper-")?
                    .strip_suffix(".pid")?
                    .parse()
                    .ok()
            })
            .collect();
        assert_eq!(helper_pids.len(), 2);
        let loser_pid = *helper_pids.iter().find(|&&pid| pid != winner_pid).unwrap();
        assert_eq!(unsafe { libc::kill(loser_pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );

        RoutineClient::new(root.clone())
            .request(&Request::new(root.clone(), Action::Shutdown))
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_timeout_kills_and_reaps_the_spawned_process() {
        let root = test_root("timeout");
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("helper.pid");
        let result = RoutineClient::new(root.clone())
            .with_startup_timeout(Duration::from_millis(300))
            .request_with_start(&status(&root), helper_command(&root, "hang"));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unrepresentable_startup_timeout_is_rejected_without_spawning() {
        let root = test_root("unrepresentable-timeout");
        let result = RoutineClient::new(root.clone())
            .with_startup_timeout(Duration::MAX)
            .request_with_start(&status(&root), helper_command(&root, "must_not_start"));
        assert!(matches!(result, Err(RoutineError::Validation(_))));
        assert!(!root.join("helper.pid").exists());
    }

    #[test]
    fn invalid_transport_response_does_not_trigger_a_start() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let root = test_root("transport");
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("daemon-v1.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            stream.write_all(b"not-json\n").unwrap();
        });
        let result = RoutineClient::new(root.clone())
            .request_with_start(&status(&root), helper_command(&root, "must_not_start"));
        assert!(matches!(result, Err(RoutineError::Corrupt(_))));
        server.join().unwrap();
        assert!(!root.join("helper.pid").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ambiguous_mutation_disconnect_is_not_retried_or_auto_started() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        let root = test_root("ambiguous-mutation");
        std::fs::create_dir_all(&root).unwrap();
        let listener = UnixListener::bind(root.join("daemon-v1.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let (mut probe, _) = listener.accept().unwrap();
            let mut probe_request = String::new();
            BufReader::new(probe.try_clone().unwrap())
                .read_line(&mut probe_request)
                .unwrap();
            let probe_request: Request = serde_json::from_str(&probe_request).unwrap();
            assert!(matches!(probe_request.action, Action::Status));
            probe
                .write_all(b"{\"result\":\"daemon\",\"protocol\":1,\"pid\":1}\n")
                .unwrap();

            let (mutation, _) = listener.accept().unwrap();
            let mut mutation_request = String::new();
            BufReader::new(mutation)
                .read_line(&mut mutation_request)
                .unwrap();
            let mutation_request: Request = serde_json::from_str(&mutation_request).unwrap();
            assert!(matches!(mutation_request.action, Action::Delete { .. }));
        });
        let request = Request::new(
            root.clone(),
            Action::Delete {
                revision: 1,
                name: "daily".into(),
            },
        );
        let result = RoutineClient::new(root.clone())
            .request_with_start(&request, helper_command(&root, "must_not_start"));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        server.join().unwrap();
        assert!(!root.join("helper.pid").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn startup_error_handshake_respects_timeout_and_reaps_child() {
        let root = test_root("error-timeout");
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("helper.pid");
        let started = Instant::now();
        let result = RoutineClient::new(root.clone())
            .with_startup_timeout(Duration::from_millis(300))
            .request_with_start(&status(&root), helper_command(&root, "error_hang"));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_startup_notification_is_corrupt_and_reaps_child() {
        let root = test_root("invalid-notification");
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("helper.pid");
        let result = RoutineClient::new(root.clone())
            .request_with_start(&status(&root), helper_command(&root, "invalid_hang"));
        assert!(matches!(result, Err(RoutineError::Corrupt(_))));
        assert_helper_reaped(&pid_path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn early_child_exit_is_unavailable_and_reaped() {
        let root = test_root("early-exit");
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("helper.pid");
        let result = RoutineClient::new(root.clone())
            .request_with_start(&status(&root), helper_command(&root, "exit"));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        assert_helper_reaped(&pid_path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ready_but_unreachable_is_unavailable_and_reaps_child() {
        let root = test_root("ready-unreachable");
        std::fs::create_dir_all(&root).unwrap();
        let pid_path = root.join("helper.pid");
        let result = RoutineClient::new(root.clone())
            .with_startup_timeout(Duration::from_millis(300))
            .request_with_start(&status(&root), helper_command(&root, "ready_hang"));
        assert!(matches!(result, Err(RoutineError::Unavailable(_))));
        assert_helper_reaped(&pid_path);
        let _ = std::fs::remove_dir_all(root);
    }

    fn assert_helper_reaped(pid_path: &Path) {
        let pid: i32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[test]
    #[ignore = "spawned by RoutineClient lifecycle tests"]
    fn daemon_helper() {
        let root = PathBuf::from(std::env::var_os("WSX_ROUTINE_TEST_ROOT").unwrap());
        let mode = std::env::var("WSX_ROUTINE_TEST_MODE").unwrap();
        if mode == "hang" || mode == "must_not_start" {
            std::fs::write(root.join("helper.pid"), std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        let fd = std::env::var(STARTUP_FD_ENV)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let mut startup = unsafe { std::fs::File::from_raw_fd(fd) };
        if matches!(mode.as_str(), "invalid_hang" | "exit" | "ready_hang") {
            std::fs::write(root.join("helper.pid"), std::process::id().to_string()).unwrap();
            let notification = match mode.as_str() {
                "invalid_hang" => Some("invalid"),
                "ready_hang" => Some("ready"),
                "exit" => None,
                _ => unreachable!(),
            };
            if let Some(notification) = notification {
                startup.write_all(notification.as_bytes()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            return;
        }
        if mode == "error_hang" {
            std::fs::write(root.join("helper.pid"), std::process::id().to_string()).unwrap();
            startup.write_all(b"error:startup failed").unwrap();
            std::thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "daemon_with_pid" {
            // ^ Startup helpers run before daemon root initialization.
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join(format!("helper-{}.pid", std::process::id())),
                std::process::id().to_string(),
            )
            .unwrap();
        }
        let _ = super::super::daemon::serve_with_startup(root, move |result| {
            let message = match result {
                Ok(()) => "ready".to_string(),
                Err(error) => format!("error:{error}"),
            };
            startup.write_all(message.as_bytes()).unwrap();
        });
    }
}
