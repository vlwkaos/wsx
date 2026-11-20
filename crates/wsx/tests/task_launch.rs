//! A task host starts its worker inside wsx's Seatbelt boundary, and a host that cannot apply
//! the boundary fails at once with the reason (docs/tasks.md "Permissions"). Drives the public
//! CLI on an isolated HOME, state and wsxd with the stream-json fake Claude
//! (`scripts/fake-claude-stream.py`). Seatbelt does not start inside another sandbox: run it
//! as a check (`wsx task check-run`), outside a worker's boundary.

// ^ The boundary is Seatbelt only: off macOS an agent start fails by design (docs/tasks.md
// "Permissions").
#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use std::{fs, path::Path};

#[test]
fn a_task_worker_starts_inside_the_boundary() {
    let fixture = Fixture::new();
    let created = fixture.json(&["task", "new", "NOOP", "--json"]);
    // ^ `task new` prints the state the host answers, and this fake worker may have finished
    // its turn by then: a test on a loaded machine read `checking`.
    assert_ne!(created["state"], "failed", "{created}");

    let launch = fixture.wait("fake launch", |_| {
        fixture
            .traces()
            .into_iter()
            .find(|trace| trace["launch"] == true)
    });
    assert_eq!(launch["mode"], "bypassPermissions", "{launch}");
    assert_eq!(
        launch["wsx_env"],
        serde_json::json!(["WSX_AGENT_TREE", "WSX_TASK_BROKER"])
    );
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
    // ^ The fake worker's results report no tokens and no cost.
    let first = task["usage"].clone();

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
    assert_eq!(shown["pings"].as_u64(), Some(0), "{shown}");
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
    // ^ Two keep-warm pings: turns of the task, counted apart (the TUI's Usage line).
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(task["log"].as_str().expect("log path"))
        .expect("open the task log");
    for unix_ms in [4, 5] {
        writeln!(
            log,
            r#"{{"unix_ms":{unix_ms},"kind":"ping","usage":{{"duration_ms":1,"input_tokens":1,"output_tokens":1,"cache_read_tokens":10,"cache_creation_tokens":0,"cost_micro_usd":10,"session_cost_micro_usd":10,"context_tokens":0}}}}"#
        )
        .expect("append a ping");
    }
    drop(log);
    let shown = fixture.json(&["task", "show", "1", "--json"])["usage"].clone();
    assert_eq!(shown["pings"].as_u64(), Some(2), "{shown}");
    assert_eq!(shown["turns"].as_u64(), Some(sum("turns", 5)), "{shown}");

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

/// A host whose record does not load posts one alert to the main chat and ends with status 0,
/// not silently; a second start inside the dedupe window posts none (docs/tasks.md "Routines").
#[test]
fn a_host_that_cannot_load_its_record_posts_one_alert() {
    let fixture = Fixture::new();
    let project = fixture.root.join("project");
    let project_id = wsx_core::task::store::project_id("demo", &project.canonicalize().unwrap());
    let dir = fixture
        .root
        .join("state/wsx/tasks")
        .join(&project_id)
        .join("7");
    fs::create_dir_all(&dir).expect("create the task folder");
    fs::write(dir.join("task.json"), "garbage").expect("write it");
    // ^ The manager host receives the post; it runs the project's main chat, its first chat.
    let made = fixture.wsx(&["task", "chat", "new", "demo"]);
    assert!(made.status.success(), "{made:?}");
    let started = fixture.wsx(&["task", "manager"]);
    assert!(started.status.success(), "{started:?}");

    let alerts = || -> Vec<String> {
        let chats = fixture.root.join("state/wsx/tasks/chats");
        let mut found = Vec::new();
        for chat in fs::read_dir(chats).into_iter().flatten().flatten() {
            let log = fs::read_to_string(chat.path().join("log.jsonl")).unwrap_or_default();
            found.extend(
                log.lines()
                    .filter(|line| line.contains("\"level\":\"bad\""))
                    .map(String::from),
            );
        }
        found
    };
    for _ in 0..2 {
        let ended = fixture.wsx(&["task", "host", &project_id, "7"]);
        assert!(ended.status.success(), "{ended:?}");
    }
    let alerts = alerts();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(
        alerts[0].contains("task t7 has no host")
            && alerts[0].contains("task.json does not load")
            && alerts[0].contains("expected value at line 1 column 1"),
        "{}",
        alerts[0]
    );
}
