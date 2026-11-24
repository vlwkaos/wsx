//! A task's checks at the host: one that runs past `tasks.checkTimeoutSeconds` is killed and fails
//! the task, a loud one keeps its end, a leftover process cannot hold the check, and a check-run
//! that exits 75 is never kept. Real host binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::{fs, process::Command};

/// A sleep whose command line names no folder: the digits carry this test's pid, so the sleeps of
/// a test beside this one are not matched.
fn sleep_marker(lead: u32) -> String {
    format!("{lead}.{:04}", std::process::id() % 10000)
}

/// The `ps` lines that hold `marker`.
fn leftovers(marker: &str) -> Vec<String> {
    let ps = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&ps.stdout)
        .lines()
        .filter(|line| line.contains(marker) && !line.contains("ps -axo"))
        .map(|line| line.trim_start().to_string())
        .collect()
}

fn settled(fixture: &Fixture, id: &str, states: &[&str]) -> Value {
    fixture.wait_task(id, "the task settles", |task| {
        states
            .iter()
            .any(|state| task["state"] == *state)
            .then(|| task.clone())
    })
}

fn new_noop(fixture: &Fixture) -> String {
    fixture.json(&["task", "new", "NOOP", "--json"])["id"]
        .as_u64()
        .expect("task id")
        .to_string()
}

// ref: docs/tasks.md "Checks, revisions and report"
#[test]
fn a_check_past_the_timeout_is_killed_and_fails_the_task() {
    let sleep = sleep_marker(37);
    let fixture = Fixture::with_tasks_config(&format!(
        "  checks: [\"sleep {sleep}\"]\n  checkTimeoutSeconds: 1\n"
    ));
    let id = new_noop(&fixture);
    let failed = settled(&fixture, &id, &["failed", "ready"]);
    let check = &failed["result"]["checks"][0];
    assert!(
        failed["state"] == "failed" && check["timed_out"] == true && check["exit"].is_null(),
        "{}",
        failed["result"]
    );
    let report = fs::read_to_string(failed["report"].as_str().unwrap()).expect("report");
    assert!(report.contains(&format!("`sleep {sleep}`: timed out")), "{report}");
    // ^ Only the check's own process counts: the worker's instructions name the check too.
    let alive: Vec<String> = leftovers(&format!("sleep {sleep}"))
        .into_iter()
        .filter(|line| {
            let command = line.split_once(' ').map_or("", |(_, command)| command.trim_start());
            command.starts_with("sleep") || command.starts_with("/bin/sh -c sleep")
        })
        .collect();
    assert!(alive.is_empty(), "the check's process is left: {alive:?}");
}

// ref: docs/tasks.md "Checks, revisions and report"
#[test]
fn a_loud_check_runs_to_its_end_and_keeps_its_tail() {
    // ^ Far past the kept output: its exit decides, and the output kept is the end, where a test
    // run puts its summary.
    let fixture = Fixture::with_tasks_config(
        "  checks: [\"seq 1 40000; seq 1 40000 >&2; echo tail end\"]\n",
    );
    let id = new_noop(&fixture);
    let task = settled(&fixture, &id, &["ready", "failed"]);
    let check = &task["result"]["checks"][0];
    let output = check["output"].as_str().expect("output");
    assert!(
        check["exit"] == 0 && output.contains("tail end") && output.len() < 20000,
        "{check}"
    );
}

// ref: docs/tasks.md "Checks, revisions and report"
#[test]
fn a_process_a_check_leaves_behind_does_not_hold_the_check() {
    let sleep = sleep_marker(47);
    let fixture = Fixture::with_tasks_config(&format!(
        "  checks: [\"(sleep {sleep} &); echo left running\"]\n"
    ));
    let id = new_noop(&fixture);
    let task = settled(&fixture, &id, &["ready", "failed"]);
    let check = &task["result"]["checks"][0];
    assert!(
        check["exit"] == 0 && check["output"].as_str().unwrap().contains("left running"),
        "{check}"
    );
    for line in leftovers(&format!("sleep {sleep}")) {
        let pid = line.split_whitespace().next().unwrap();
        let _ = Command::new("kill").args(["-9", pid]).status();
    }
}

fn trace_pair(fixture: &Fixture, name: &str) -> Vec<(i64, String)> {
    let key = format!("check_run_repeat_{name}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(trace) = fixture.traces().iter().find(|trace| trace.get(&key).is_some()) {
            return trace[&key]
                .as_array()
                .unwrap()
                .iter()
                .map(|answer| (answer[0].as_i64().unwrap(), answer[1].as_str().unwrap().to_string()))
                .collect();
        }
        assert!(std::time::Instant::now() < deadline, "the {name} answers timeout");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ref: docs/tasks.md "Check-run"
#[test]
fn a_check_run_that_exits_75_is_never_kept_and_one_that_exits_1_is() {
    let fixture = Fixture::with_tasks_config(
        "  scenarios:\n    tempfail: \"echo run >> {root}/tempfail.log; exit 75\"\n    hardfail: \"echo run >> {root}/hardfail.log; exit 1\"\n",
    );
    let runs = |name: &str| {
        fs::read_to_string(fixture.root.join(name)).map_or(0, |text| text.matches("run").count())
    };
    let flaky = fixture.json(&["task", "new", "CHECKRUN_REPEAT tempfail", "--json"])["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let hard = fixture.json(&["task", "new", "CHECKRUN_REPEAT hardfail", "--json"])["id"]
        .as_u64()
        .unwrap()
        .to_string();
    for id in [&flaky, &hard] {
        assert_eq!(settled(&fixture, id, &["ready", "failed"])["state"], "ready");
    }
    let pair = trace_pair(&fixture, "tempfail");
    assert!(pair[0].0 == 1 && pair[1].0 == 1, "{pair:?}");
    assert!(!pair[0].1.contains("stored") && !pair[1].1.contains("stored"), "{pair:?}");
    assert!(
        pair[1].1.contains("exit 75 (a temporary failure, not kept: the next check-run runs it again)"),
        "{pair:?}"
    );
    assert_eq!(runs("tempfail.log"), 2);
    let pair = trace_pair(&fixture, "hardfail");
    assert!(pair[0].0 == 1 && pair[0].1.contains("exit 1") && !pair[0].1.contains("stored"), "{pair:?}");
    assert!(
        pair[1].0 == 1 && pair[1].1.contains("(stored: it ran on this tree before)"),
        "{pair:?}"
    );
    assert_eq!(runs("hardfail.log"), 1);
}
