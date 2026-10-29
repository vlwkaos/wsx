//! A task host starts its worker inside wsx's Seatbelt boundary, and a host that cannot apply
//! the boundary fails at once with the reason (docs/tasks.md "Permissions"). Drives the public
//! CLI on an isolated HOME, state and wsxd with the stream-json fake Claude
//! (`scripts/fake-claude-stream.py`). Seatbelt does not start inside another sandbox: run it
//! as a check (`wsx task check-run`), outside a worker's boundary.

// ^ The boundary is Seatbelt only: off macOS an agent start fails by design (docs/tasks.md
// "Permissions").
#![cfg(target_os = "macos")]

use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("resolve the repository root");
        let wsx = Path::new(env!("CARGO_BIN_EXE_wsx"));
        let wsxd = wsx.with_file_name("wsxd");
        assert!(
            wsxd.is_file(),
            "missing adjacent wsxd: {}; build with cargo build -p wsx -p wsx-daemon first",
            wsxd.display()
        );
        // ^ Short: the daemon socket must stay below macOS SUN_LEN.
        let root = repository.join("target").join(format!(
            "launch-it-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let config = root.join("home/Library/Application Support/wsx");
        let project = root.join("project");
        for dir in [&config, &project, &root.join("bin"), &root.join("t")] {
            fs::create_dir_all(dir).expect("create a fixture folder");
        }
        // ^ The daemon socket's folder must be the owner's alone.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("chmod the root");
        fs::write(
            config.join("config-v2.toml"),
            format!(
                "resume_agents_on_restore = false\n[[projects]]\nname = \"demo\"\npath = {:?}\n",
                project.to_str().expect("UTF-8 path")
            ),
        )
        .expect("write the wsx config");
        fs::write(
            project.join("wsx.config.yml"),
            "tasks:\n  minFreeGb: 0\n  land: { auto: false }\n  agent:\n    model: fake-model\n",
        )
        .expect("write the project config");
        let fake = root.join("bin/claude");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nexec python3 {:?} \"$@\"\n",
                repository.join("scripts/fake-claude-stream.py")
            ),
        )
        .expect("write the fake claude");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod the fake");
        let fixture = Self { root };
        fixture.git(&["init", "-q", "--template=", "-b", "main"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-q", "-m", "start"]);
        fixture
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .current_dir(self.root.join("project"))
            .env("HOME", self.root.join("home"))
            .env("WSX_EXIT_WITH_PID", std::process::id().to_string())
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("TMPDIR", self.root.join("t"))
            .env("PATH", path)
            .env("SHELL", "/bin/sh")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        // ^ A worker's broker or actor would route or refuse the commands.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("WSX_") {
                command.env_remove(name);
            }
        }
        command
            .env(
                "WSX_DAEMON_BIN",
                Path::new(env!("CARGO_BIN_EXE_wsx")).with_file_name("wsxd"),
            )
            .env("WSX_SOCKET", self.root.join("d.sock"));
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .command(Path::new("git"))
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn wsx(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run wsx {args:?}: {error}"))
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.wsx(args);
        assert!(output.status.success(), "wsx {args:?}: {output:?}");
        serde_json::from_slice(&output.stdout).expect("wsx JSON")
    }

    /// The fake's trace lines (it writes beside the task copies).
    fn traces(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("fake-claude.trace"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn log(&self, task: &Value) -> String {
        task["log"]
            .as_str()
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_default()
    }

    /// Poll `test` for up to 30 s; on a timeout fail with what the task host logged.
    fn wait<T>(&self, label: &str, mut test: impl FnMut(&Value) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let task = self.json(&["task", "show", "1", "--json"]);
            if let Some(value) = test(&task) {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "{label} timeout\ntask: {task}\nlog.jsonl:\n{}",
                self.log(&task)
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.wsx(&["daemon", "stop", "--force"]);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_task_worker_starts_inside_the_boundary() {
    let fixture = Fixture::new();
    let created = fixture.json(&["task", "new", "NOOP", "--json"]);
    assert_eq!(created["state"], "working", "{created}");

    let launch = fixture.wait("fake launch", |_| {
        fixture
            .traces()
            .into_iter()
            .find(|trace| trace["launch"] == true)
    });
    assert_eq!(launch["mode"], "bypassPermissions", "{launch}");
    assert_eq!(launch["wsx_env"], serde_json::json!(["WSX_TASK_BROKER"]));
}

#[test]
fn a_host_inside_another_sandbox_fails_with_the_reason() {
    let fixture = Fixture::new();
    // ^ wsx starts the fixture's wsxd, and so the task host, inside this outer sandbox.
    let output = fixture
        .command(Path::new("/usr/bin/sandbox-exec"))
        .args(["-p", "(version 1)(allow default)"])
        .arg(env!("CARGO_BIN_EXE_wsx"))
        .args(["task", "new", "NOOP", "--json"])
        .output()
        .expect("run sandbox-exec");
    assert!(output.status.success(), "{output:?}");

    let task = fixture.wait("task failed", |task| {
        (task["state"] == "failed").then(|| task.clone())
    });
    let log = fixture.log(&task);
    assert!(log.contains("another sandbox"), "{log}");
    assert!(fixture.traces().is_empty(), "the fake started");
    let ledger = fixture.json(&["task", "ledger", "--json"]);
    assert!(ledger.to_string().contains("another sandbox"), "{ledger}");
}

/// `show --json` and the ledger sum a task's turns, also lines from before
/// `session_cost_micro_usd`, whose `cost_micro_usd` held the session's running total
/// (docs/tasks.md "Usage").
#[test]
fn usage_sums_a_tasks_turns_and_older_running_totals() {
    use std::io::Write;
    let fixture = Fixture::new();
    fixture.json(&["task", "new", "NOOP", "--json"]);
    let task = fixture.wait("first result", |task| {
        (task["usage"]["turns"].as_u64() >= Some(1) && task["state"] != "working")
            .then(|| task.clone())
    });
    let first = task["usage"].clone();
    // ^ The fake costs 0.001 USD a turn and reads 10 input tokens per model call.
    assert_eq!(
        first["cost_micro_usd"].as_u64(),
        first["turns"].as_u64().map(|turns| turns * 1000),
        "{first}"
    );
    assert!(first["input_tokens"].as_u64() > Some(0), "{first}");

    // ^ Two turns of one older session (running totals 3000, then 5000), then a new process
    // whose total starts lower again (1000): 3000 + 2000 + 1000 spent.
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(task["log"].as_str().expect("log path"))
        .expect("open the task log");
    for (total, unix_ms) in [(3000, 1), (5000, 2), (1000, 3)] {
        writeln!(
            log,
            r#"{{"unix_ms":{unix_ms},"kind":"result","status":"done","summary":"old","usage":{{"duration_ms":1,"input_tokens":100,"output_tokens":20,"cache_read_tokens":4000,"cache_creation_tokens":300,"cost_micro_usd":{total},"context_tokens":0}}}}"#
        )
        .expect("append an older result");
    }
    drop(log);
    let shown = fixture.json(&["task", "show", "1", "--json"])["usage"].clone();
    let sum = |key: &str, extra: u64| first[key].as_u64().unwrap() + extra;
    assert_eq!(
        shown["cost_micro_usd"].as_u64(),
        Some(sum("cost_micro_usd", 6000)),
        "{shown}"
    );
    assert_eq!(
        shown["input_tokens"].as_u64(),
        Some(sum("input_tokens", 300)),
        "{shown}"
    );
    assert_eq!(
        shown["cache_read_tokens"].as_u64(),
        Some(sum("cache_read_tokens", 12000)),
        "{shown}"
    );
    assert_eq!(
        shown["cache_creation_tokens"].as_u64(),
        Some(sum("cache_creation_tokens", 900)),
        "{shown}"
    );
    assert_eq!(
        shown["output_tokens"].as_u64(),
        Some(sum("output_tokens", 60)),
        "{shown}"
    );
    assert_eq!(shown["turns"].as_u64(), Some(sum("turns", 3)), "{shown}");

    let ledger = fixture.json(&["task", "ledger", "--json"]);
    let row = ledger["open"]
        .as_array()
        .into_iter()
        .chain(ledger["recent"].as_array())
        .flatten()
        .find(|row| row["id"] == 1)
        .unwrap_or_else(|| panic!("no ledger row for #1: {ledger}"));
    assert_eq!(row["usage"], shown, "{row}");
    // ^ The older lines are from 1970: only the fake's own turns are in today's and the week's.
    for window in ["today", "week"] {
        assert_eq!(ledger["usage"][window]["workers"], first, "{ledger}");
    }
    let text = fixture.wsx(&["task", "show", "1"]);
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("usage:        $0.0"), "{text}");
}
