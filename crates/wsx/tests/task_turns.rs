//! A worker turn that ends with no result: Claude's own 0-token turn for a background-task
//! notification is ignored, a model turn without a result is asked once, and a turn that ends while
//! a background job runs waits for the job's notice (docs/tasks.md "Claude adapter"). Real host
//! binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;

fn texts(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry["text"].as_str().map(str::to_string))
        .collect()
}

/// The turn's final state: `ready` or `failed`.
fn settled(fixture: &Fixture, prompt: &str) -> Value {
    let id = fixture.new_task(prompt);
    fixture.settled(&id)
}

/// How many times the host asked the task's session for a result.
fn result_retries(fixture: &Fixture, task: &Value) -> usize {
    let session = task["record"]["agent_session"].as_str().expect("session");
    fixture
        .traces()
        .iter()
        .filter(|trace| trace["session"] == session)
        .filter(|trace| {
            trace["message"]
                .as_str()
                .is_some_and(|text| text.starts_with("Your turn ended without a result."))
        })
        .count()
}

// ref: docs/tasks.md "Claude adapter"
#[test]
fn a_model_turn_without_a_result_is_asked_once_and_claudes_own_empty_turn_is_ignored() {
    let fixture = Fixture::new();
    let once = settled(&fixture, "NORESULT 1 WORK once.txt");
    assert_eq!(once["state"], "ready", "{}", once["result"]);
    assert_eq!(result_retries(&fixture, &once), 1);
    let log = fixture.entries(&once);
    let results: Vec<(Value, Value)> = log
        .iter()
        .filter(|entry| entry["kind"] == "result")
        .map(|entry| (entry["status"].clone(), entry["trigger"].clone()))
        .collect();
    assert_eq!(results, [("done".into(), "result_retry".into())]);
    assert!(texts(&log).contains(&"claude ended its own turn with no result: ignored".to_string()));

    // ^ Asked once only: a second turn without a result fails the task.
    let twice = settled(&fixture, "NORESULT 2 WORK twice.txt");
    assert_eq!(twice["state"], "failed");
    assert_eq!(result_retries(&fixture, &twice), 1);
    assert_eq!(
        twice["result"]["result"]["summary"],
        "the agent ended with success and no result"
    );
}

const WAITING: &str = "the turn ended while 1 background job(s) run: waiting for their notice";

// ref: docs/tasks.md "Claude adapter"
#[test]
fn a_turn_that_ends_while_a_background_job_runs_waits_for_its_notice() {
    let fixture = Fixture::new();
    let bg = settled(&fixture, "BACKGROUND 2 WORK background.txt");
    assert_eq!(bg["state"], "ready", "{}", bg["result"]);
    assert_eq!(result_retries(&fixture, &bg), 0);
    let log = fixture.entries(&bg);
    let statuses: Vec<&Value> = log
        .iter()
        .filter(|entry| entry["kind"] == "result")
        .map(|entry| &entry["status"])
        .collect();
    assert_eq!(statuses, ["done"]);
    assert_eq!(
        texts(&log).iter().filter(|text| *text == WAITING).count(),
        1
    );
}

// ref: docs/tasks.md "Claude adapter"
#[test]
fn a_background_job_that_never_reports_waits_at_most_the_check_timeout() {
    // ^ Then the result is asked for once.
    let fixture = Fixture::with_tasks_config("  checkTimeoutSeconds: 2\n");
    let stuck = settled(&fixture, "BACKGROUND never WORK stuck.txt");
    assert_eq!(stuck["state"], "ready", "{}", stuck["result"]);
    assert_eq!(result_retries(&fixture, &stuck), 1);
    let said = texts(&fixture.entries(&stuck));
    let gave_up = "no notice from 1 background job(s) in 2 s: the turn ended without a result";
    let at = |wanted: &str| said.iter().position(|text| text == wanted);
    assert!(at(WAITING).is_some() && at(gave_up).is_some(), "{said:?}");
    assert!(at(WAITING) < at(gave_up));
}
