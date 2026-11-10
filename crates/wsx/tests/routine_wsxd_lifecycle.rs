//! Routines end to end through the shipped binaries: `wsx routine` against the wsxd it starts,
//! a run that outlives a live handoff, and the one-time import of a real asched store
//! (`tests/fixtures/asched-store`, written by asched 0.2.0's daemon). ref: docs/tasks.md "Routines"

use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            net::UnixListener,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
const POLL_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
// ^ Runs start only by hand: February 29 comes once in four years.
const NEVER_DUE: &str = "0 0 29 2 *";

struct Install {
    root: PathBuf,
    project: PathBuf,
    asched: PathBuf,
}

impl Install {
    fn new() -> Self {
        // ^ Socket paths under this root stay below macOS SUN_LEN, so drop the `..` parts.
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let root = repository.join("target").join(format!(
            "routine-it-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let wsxd = Path::new(env!("CARGO_BIN_EXE_wsx")).with_file_name("wsxd");
        assert!(
            wsxd.is_file(),
            "build wsxd beside wsx first: cargo build -p wsx -p wsx-daemon"
        );
        fs::create_dir_all(root.join("project")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("state")).unwrap();
        // ^ wsxd refuses a socket folder that others may write.
        fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
        let project = root.join("project").canonicalize().unwrap();
        Self {
            asched: root.join("a"),
            root,
            project,
        }
    }

    fn wsx(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_wsx"))
            .args(args)
            .current_dir(&self.project)
            .env_remove("WSX_TASK_BROKER")
            .env_remove("WSX_TASK_ACTOR")
            .env("ASCHED_ROOT", &self.asched)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/config"))
            .env("WSX_SOCKET", self.root.join("state/w.sock"))
            .env("WSX_EXIT_WITH_PID", std::process::id().to_string())
            .output()
            .unwrap_or_else(|error| panic!("run wsx {args:?}: {error}"))
    }

    /// `wsx routine <args> --project <project> --json`, which must succeed.
    fn routine(&self, args: &[&str]) -> Value {
        let project = self.project.to_str().unwrap();
        let mut all = vec!["routine"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--project", project, "--json"]);
        let output = self.wsx(&all);
        assert!(
            output.status.success(),
            "wsx {all:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn view(&self, name: &str) -> Value {
        let list = self.routine(&["list"]);
        list["routines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["routine"]["name"] == name)
            .unwrap_or_else(|| panic!("no routine {name}: {list}"))
            .clone()
    }

    /// The newest run of `name` once it is no longer running.
    fn finished_run(&self, name: &str) -> Value {
        let deadline = Instant::now() + POLL_TIMEOUT;
        loop {
            let runs = self.routine(&["logs", name]);
            let last = runs["runs"].as_array().unwrap().last().cloned();
            if let Some(run) = last.filter(|run| run["status"] != "running") {
                return run;
            }
            assert!(Instant::now() < deadline, "{name} still runs: {runs}");
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = self.wsx(&["daemon", "stop", "--force"]);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn routine_cli_runs_commands_and_events_through_wsxd() {
    let install = Install::new();
    assert_eq!(install.routine(&["list"])["routines"], Value::Array(vec![]));
    install.routine(&[
        "add",
        "report",
        "--cron",
        NEVER_DUE,
        "--arg=sh",
        "--arg=-c",
        "--arg=echo out; echo err >&2; exit 3",
    ]);
    install.routine(&["run", "report"]);
    let run = install.finished_run("report");
    assert_eq!(
        (&run["status"], &run["exit_code"]),
        (&"failed".into(), &3.into())
    );
    assert_eq!(run["final_output"], "out");
    let report_err = PathBuf::from(run["stderr_path"].as_str().unwrap());
    assert_eq!(fs::read_to_string(&report_err).unwrap(), "err\n");

    // An event runs its routine once per id, with the payload in the environment.
    install.routine(&[
        "add",
        "closed",
        "--event",
        "task.closed",
        "--arg=sh",
        "--arg=-c",
        "--arg=printf %s \"$WSX_EVENT_PAYLOAD\"",
    ]);
    let fire = [
        "fire",
        "--kind",
        "task.closed",
        "--event-id",
        "t1",
        "--payload",
        r#"{"task":7}"#,
    ];
    assert_eq!(
        install.routine(&fire),
        serde_json::json!({"handled": {"routines": [{"started": {"name": "closed"}}]}})
    );
    let run = install.finished_run("closed");
    assert_eq!(run["final_output"], r#"{"task":7}"#);
    assert_eq!(
        run["cause"],
        serde_json::json!({"event": {"kind": "task.closed", "event_id": "t1"}})
    );
    assert_eq!(install.routine(&fire), Value::from("deduplicated"));

    // A stale revision is refused; a delete removes the routine's runs and their files.
    let list = install.routine(&["list"]);
    let stale = (list["revision"].as_u64().unwrap() - 1).to_string();
    let refused = install.wsx(&[
        "routine",
        "delete",
        "report",
        "--revision",
        &stale,
        "--project",
        install.project.to_str().unwrap(),
    ]);
    assert!(
        !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("stale")
    );
    install.routine(&["delete", "report"]);
    assert!(
        !report_err.parent().unwrap().exists(),
        "the deleted routine's output stayed"
    );
    let names: Vec<Value> = install.routine(&["list"])["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|view| view["routine"]["name"].clone())
        .collect();
    assert_eq!(names, vec![Value::from("closed")]);
}

#[test]
fn findings_exit_codes_make_a_finished_run_findings_not_failed() {
    let install = Install::new();
    // ^ docs/tasks.md "Routines": exit 1 is listed, 3 is not, 0 succeeds.
    for (name, code) in [("found", 1), ("broke", 3), ("clean", 0)] {
        install.routine(&[
            "add",
            name,
            "--cron",
            NEVER_DUE,
            "--findings-exit",
            "2,1",
            "--arg=sh",
            "--arg=-c",
            &format!("--arg=echo {name}; exit {code}"),
        ]);
        install.routine(&["run", name]);
    }
    let seen: Vec<(Value, Value)> = ["found", "broke", "clean"]
        .into_iter()
        .map(|name| {
            let run = install.finished_run(name);
            (run["status"].clone(), run["exit_code"].clone())
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("findings".into(), 1.into()),
            ("failed".into(), 3.into()),
            ("succeeded".into(), 0.into())
        ]
    );
    // The codes are stored sorted (a routine without them stores none, so older stores read as before).
    assert_eq!(
        install.view("found")["routine"]["findings_exit"],
        serde_json::json!([1, 2])
    );
    for bad in ["0", "256", "1,x"] {
        let out = install.wsx(&[
            "routine",
            "add",
            "bad",
            "--cron",
            NEVER_DUE,
            "--findings-exit",
            bad,
            "--arg=true",
            "--project",
            install.project.to_str().unwrap(),
        ]);
        assert!(!out.status.success(), "{bad} was accepted");
    }
}

#[test]
fn a_run_outlives_a_live_handoff_and_records_its_exit() {
    let install = Install::new();
    // ^ The run ends when the test creates `gate`, not after a fixed time: a slow runner takes
    // longer than any sleep to restart the daemon, and the run would end by itself first.
    let gate = install.root.join("gate");
    install.routine(&[
        "add",
        "slow",
        "--cron",
        NEVER_DUE,
        "--arg=sh",
        "--arg=-c",
        &format!(
            "--arg=while [ ! -e '{}' ]; do sleep 0.05; done; echo done; exit 7",
            gate.display()
        ),
    ]);
    install.routine(&["run", "slow"]);
    assert_eq!(install.view("slow")["running"], true);
    let restarted = install.wsx(&["daemon", "restart", "--json"]);
    let answer: Value = serde_json::from_slice(&restarted.stdout).unwrap_or_default();
    assert_eq!(answer["state"], "restarted", "{restarted:?}");
    let after = install.view("slow");
    assert_eq!(after["running"], true, "the handoff ended the run: {after}");
    fs::write(&gate, "").unwrap();
    let run = install.finished_run("slow");
    assert_eq!(
        (&run["status"], &run["exit_code"]),
        (&"failed".into(), &7.into())
    );
    assert_eq!(run["final_output"], "done");
}

/// asched's daemon as far as the import meets it: it holds its lock and answers `shutdown`.
fn old_scheduler(asched: &Path) -> thread::JoinHandle<String> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(asched.join("daemon-v1.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let listener = UnixListener::bind(asched.join("daemon-v1.sock")).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        (&stream)
            .write_all(b"{\"result\":\"ok\",\"revision\":null}\n")
            .unwrap();
        drop(lock);
        request
    })
}

#[test]
fn the_first_start_imports_asched_routines_and_history() {
    let install = Install::new();
    // ^ The fixture names its root, project and project key with placeholders.
    let key = wsx_core::routine::store::project_key(&install.project);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/asched-store");
    for file in ["projects/@KEY@.toml", "runtime/@KEY@.toml"] {
        let text = fs::read_to_string(fixture.join(file))
            .unwrap()
            .replace("@ASCHED@", install.asched.to_str().unwrap())
            .replace("@PROJECT@", install.project.to_str().unwrap())
            .replace("@KEY@", &key);
        let target = install.asched.join(file.replace("@KEY@", &key));
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
    }
    let logs = fixture.join("logs/@KEY@");
    for routine in fs::read_dir(&logs).unwrap().flatten() {
        for run in fs::read_dir(routine.path()).unwrap().flatten() {
            let target = install
                .asched
                .join("logs")
                .join(&key)
                .join(routine.file_name())
                .join(run.file_name());
            fs::create_dir_all(&target).unwrap();
            for file in fs::read_dir(run.path()).unwrap().flatten() {
                fs::copy(file.path(), target.join(file.file_name())).unwrap();
            }
        }
    }
    // ^ One project asched left unreadable: the import leaves it out and takes the rest.
    let broken = install
        .asched
        .join(format!("projects/{}.toml", "0".repeat(32)));
    fs::write(&broken, "project_path = [not toml").unwrap();
    let runtime_file = install.asched.join(format!("runtime/{key}.toml"));
    let before = fs::read(&runtime_file).unwrap();
    let old = old_scheduler(&install.asched);

    let list = install.routine(&["list"]);
    let request: Value = serde_json::from_str(&old.join().unwrap()).unwrap();
    assert_eq!(request["action"], serde_json::json!({"action": "shutdown"}));
    assert_eq!(list["revision"], 4, "{list}");
    let names: Vec<&str> = list["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|view| view["routine"]["name"].as_str().unwrap())
        .collect();
    // ^ asched's `later` has an `at` trigger, which routines no longer have: it is left out.
    assert_eq!(names, ["closed", "nightly", "slow"]);

    // Finished runs keep their result and output, now in wsxd's store.
    let nightly = install.view("nightly")["latest_run"].clone();
    assert_eq!(
        (&nightly["status"], &nightly["final_output"]),
        (&"succeeded".into(), &"imported-output".into())
    );
    let stdout = nightly["stdout_path"].as_str().unwrap();
    assert!(
        stdout.starts_with(install.root.join("state/routines").to_str().unwrap()),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(stdout).unwrap(), "imported-output\n");
    let closed = install.view("closed")["latest_run"].clone();
    assert_eq!(
        closed["cause"],
        serde_json::json!({"event": {"kind": "task.closed", "event_id": "e1"}})
    );
    // The receipt came along: the same event does not run again.
    let fire = [
        "fire",
        "--kind",
        "task.closed",
        "--event-id",
        "e1",
        "--payload",
        "{}",
    ];
    assert_eq!(install.routine(&fire), Value::from("deduplicated"));

    // The run asched had in flight is not lost: it is recorded as ended by the upgrade.
    let slow = install.view("slow");
    assert_eq!(
        (&slow["running"], &slow["latest_run"]["status"]),
        (&false.into(), &"interrupted".into())
    );
    assert!(slow["latest_run"]["final_output"]
        .as_str()
        .unwrap()
        .contains("interrupted by the upgrade from asched"));

    // asched's files stay as a backup, and the imported routines run.
    assert_eq!(fs::read(&runtime_file).unwrap(), before);
    assert_eq!(
        fs::read_dir(install.root.join("state/routines/projects"))
            .unwrap()
            .count(),
        1,
        "only the readable project is imported"
    );
    install.routine(&["run", "nightly"]);
    let run = install.finished_run("nightly");
    assert_eq!(
        (&run["status"], &run["final_output"]),
        (&"succeeded".into(), &"imported-output".into())
    );
    assert_eq!(
        install.routine(&["logs", "nightly"])["runs"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// A cancel can come at any moment of a run's start, also before its command has written
/// `pid`: the command never goes on without a record. A run starts only once its record is stored.
#[test]
fn a_cancel_right_after_the_start_stops_the_command() {
    let install = Install::new();
    let started = install.root.join("started");
    fs::create_dir_all(&started).unwrap();
    let script = format!("--arg=echo > {}/$$; exec sleep 30", started.display());
    install.routine(&[
        "add", "hold", "--cron", NEVER_DUE, "--arg=sh", "--arg=-c", &script,
    ]);
    for _ in 0..8 {
        install.routine(&["run", "hold"]);
        install.routine(&["cancel", "hold"]);
        assert_eq!(install.finished_run("hold")["status"], "cancelled");
    }
    // ^ A command that was about to start has had time to name itself.
    thread::sleep(Duration::from_secs(1));
    let deadline = Instant::now() + POLL_TIMEOUT;
    for entry in fs::read_dir(&started).unwrap().flatten() {
        let pid: i32 = entry.file_name().to_str().unwrap().parse().unwrap();
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "the cancelled command {pid} still runs"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    // A run whose record cannot be stored does not start, and the request says why.
    let projects = install.root.join("state/routines/projects");
    let commands = fs::read_dir(&started).unwrap().count();
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = install.wsx(&[
        "routine",
        "run",
        "hold",
        "--project",
        install.project.to_str().unwrap(),
    ]);
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("denied"),
        "{refused:?}"
    );
    thread::sleep(Duration::from_secs(1));
    assert_eq!(
        fs::read_dir(&started).unwrap().count(),
        commands,
        "a run started without its record"
    );
    let runs = install.routine(&["logs", "hold"]);
    assert_eq!(runs["runs"].as_array().unwrap().len(), 8, "{runs}");
}

#[test]
fn a_stored_one_shot_routine_is_dropped_and_no_new_one_is_taken() {
    let install = Install::new();
    install.routine(&["add", "keep", "--cron", NEVER_DUE, "--arg=true"]);
    // ^ Routines recur: `--at` is not a trigger any more (docs/tasks.md "Routines").
    let refused = install.wsx(&[
        "routine",
        "add",
        "once",
        "--at",
        "23:59",
        "--arg=true",
        "--project",
        install.project.to_str().unwrap(),
    ]);
    assert!(!refused.status.success(), "--at was accepted");
    // ^ A store an older wsxd wrote, with a one-shot routine and its claim.
    let projects = install.root.join("state/routines/projects");
    let file = fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut stored: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let revision = stored["revision"].as_u64().unwrap();
    stored["routines"].as_array_mut().unwrap().push(serde_json::json!({
        "name": "once", "trigger": {"at": {"epoch_minute": 1}}, "command": ["true"], "prompt": ""
    }));
    stored["claims"]["once"] = 1.into();
    fs::write(&file, serde_json::to_vec(&stored).unwrap()).unwrap();
    let names = |list: Value| -> Vec<String> {
        list["routines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|view| view["routine"]["name"].as_str().unwrap().to_string())
            .collect()
    };
    // ^ A reader sees no one-shot, also before wsxd takes the store again.
    assert_eq!(names(install.routine(&["list"])), ["keep"]);
    let restarted = install.wsx(&["daemon", "restart", "--json"]);
    let answer: Value = serde_json::from_slice(&restarted.stdout).unwrap_or_default();
    assert_eq!(answer["state"], "restarted", "{restarted:?}");
    assert_eq!(names(install.routine(&["list"])), ["keep"]);
    let cleaned = fs::read_to_string(&file).unwrap();
    assert!(
        !cleaned.contains("\"at\"") && !cleaned.contains("\"once\""),
        "the file still holds the one-shot: {cleaned}"
    );
    let after: Value = serde_json::from_str(&cleaned).unwrap();
    assert!(after["revision"].as_u64().unwrap() > revision);
}
