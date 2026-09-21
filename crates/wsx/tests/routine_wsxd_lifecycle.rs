//! End-to-end coverage for wsx's shipped routine scheduler integration.
//!
//! This deliberately drives the public CLI and the public asched-core boundary;
//! it does not use wsx implementation APIs.

use asched_core::{
    registry::{Project, RegistryStore},
    routine::{
        ipc::{Action, Request, Response},
        RoutineClient,
    },
};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
const POLL_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

struct IsolatedInstall {
    root: PathBuf,
    project: PathBuf,
    scheduler_root: PathBuf,
    wsx: PathBuf,
}

impl IsolatedInstall {
    fn new() -> Self {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = repository.join("target").join(format!(
            "routine-it-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("project");
        let scheduler_root = root.join("a");
        let empty_path = root.join("no-asched-on-path");
        fs::create_dir_all(&project).expect("create isolated project directory");
        fs::create_dir_all(&empty_path).expect("create isolated PATH directory");
        fs::create_dir_all(root.join("home")).expect("create isolated HOME");

        let wsx = PathBuf::from(env!("CARGO_BIN_EXE_wsx"));
        // Cargo exposes sibling package binaries in configurations that build them
        // as test artifacts. A normal installed layout has wsxd beside wsx, which
        // is also the layout wsx must support.
        let wsxd = option_env!("CARGO_BIN_EXE_wsxd")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                wsx.parent()
                    .expect("wsx has a parent directory")
                    .join("wsxd")
            });
        assert!(
            wsx.is_file(),
            "Cargo did not provide a wsx binary: {}",
            wsx.display()
        );
        assert!(
            wsxd.is_file(),
            "the wsx installation must ship an adjacent wsxd binary: {}",
            wsxd.display()
        );

        RegistryStore::new(scheduler_root.clone())
            .add(
                0,
                Project {
                    name: "project".into(),
                    working_dir: project.clone(),
                },
            )
            .expect("register the isolated project through the public registry API");

        Self {
            root,
            project,
            scheduler_root,
            wsx,
        }
    }

    fn client(&self) -> RoutineClient {
        RoutineClient::new(self.scheduler_root.clone())
    }

    fn socket(&self) -> PathBuf {
        self.client().socket_path()
    }

    fn project_arg(&self) -> &str {
        self.project.to_str().expect("project path must be UTF-8")
    }

    fn wsx(&self, args: &[&str]) -> Output {
        let path_without_asched = self.root.join("no-asched-on-path");
        Command::new(&self.wsx)
            .args(args)
            .current_dir(&self.project)
            .env("ASCHED_ROOT", &self.scheduler_root)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/config"))
            .env("XDG_DATA_HOME", self.root.join("home/data"))
            .env("PATH", path_without_asched)
            .output()
            .unwrap_or_else(|error| panic!("run wsx {args:?}: {error}"))
    }

    fn shutdown(&self) {
        let _ = self
            .client()
            .request(&Request::new(PathBuf::new(), Action::Shutdown));
    }
}

impl Drop for IsolatedInstall {
    fn drop(&mut self) {
        self.shutdown();
        let socket = self.socket();
        let deadline = Instant::now() + POLL_TIMEOUT;
        while socket.exists() && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn wait_for_logs(install: &IsolatedInstall, routine: &str, expected: &str) -> String {
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        let output = install.wsx(&[
            "routine",
            "logs",
            routine,
            "--project",
            install.project_arg(),
            "--json",
        ]);
        let text = format!(
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() && text.to_ascii_lowercase().contains(expected) {
            return text;
        }
        last = text;
        thread::sleep(POLL_INTERVAL);
    }
    panic!("timed out waiting for {routine} logs to contain {expected:?}; last output:\n{last}");
}

fn wait_for_socket_removal(socket: &Path) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    while socket.exists() && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    assert!(
        !socket.exists(),
        "scheduler socket was not removed: {}",
        socket.display()
    );
}

fn wait_for_pid_exit(pid: u32) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    while Instant::now() < deadline {
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    panic!("scheduler process {pid} remained after shutdown");
}

#[test]
fn routine_cli_starts_adjacent_wsxd_without_asched_and_cleans_up() {
    let install = IsolatedInstall::new();
    let socket = install.socket();
    let project = install.project_arg();
    assert!(!socket.exists(), "test requires an absent scheduler socket");

    // Listing is a routine operation, so it must start the scheduler despite the
    // deliberately empty PATH. Repeating it covers requests after startup.
    assert_success(
        &install.wsx(&["routine", "list", "--project", project, "--json"]),
        "initial routine list",
    );
    assert!(socket.exists(), "routine list did not start the scheduler");
    assert_success(
        &install.wsx(&["routine", "list", "--project", project, "--json"]),
        "repeated routine list",
    );

    let pid = match install
        .client()
        .request(&Request::new(PathBuf::new(), Action::Status))
        .expect("query scheduler through the public client API")
    {
        Response::Daemon { pid, .. } => pid,
        response => panic!("expected daemon status response, got {response:?}"),
    };

    let literal = "argv-literal;$(not-a-shell)";
    assert_success(
        &install.wsx(&[
            "routine",
            "add",
            "succeeds",
            "--project",
            project,
            "--cron",
            "* * * * *",
            "--arg",
            "/usr/bin/printf",
            "--arg",
            "%s",
            "--arg",
            literal,
        ]),
        "add successful routine",
    );
    assert_success(
        &install.wsx(&[
            "routine",
            "add",
            "fails",
            "--project",
            project,
            "--cron",
            "* * * * *",
            "--arg",
            "/usr/bin/false",
        ]),
        "add failing routine",
    );
    let listed = install.wsx(&["routine", "list", "--project", project, "--json"]);
    assert_success(&listed, "list created routines");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("succeeds") && listed.contains("fails"),
        "routine list omitted created routines:\n{listed}"
    );

    assert_success(
        &install.wsx(&["routine", "run", "succeeds", "--project", project]),
        "run successful routine",
    );
    let successful_logs = wait_for_logs(&install, "succeeds", "succeeded");
    assert!(
        successful_logs.contains(literal),
        "routine argv was not preserved as a literal direct argument:\n{successful_logs}"
    );

    // A failed child is a run result, not a scheduler failure. Some CLIs return a
    // nonzero status for this command; the durable run record is the contract.
    let _ = install.wsx(&["routine", "run", "fails", "--project", project]);
    wait_for_logs(&install, "fails", "failed");
    assert_success(
        &install.wsx(&["routine", "list", "--project", project, "--json"]),
        "routine list after failed run",
    );

    install.shutdown();
    wait_for_socket_removal(&socket);
    wait_for_pid_exit(pid);
}
