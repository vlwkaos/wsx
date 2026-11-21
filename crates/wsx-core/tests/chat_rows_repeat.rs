//! A task event line repeated with no bubble between shows once, at any time gap
//! (`chat_rows`). ref: docs/tasks.md "Manager"

use serde_json::json;
use wsx_core::task::chat::{chat_rows, Names};
use wsx_core::task::manager::ManagerEntry;

fn rows(log: Vec<serde_json::Value>) -> Vec<String> {
    let log: Vec<ManagerEntry> = log
        .into_iter()
        .map(|line| serde_json::from_value(line).unwrap())
        .collect();
    let refs: Vec<&ManagerEntry> = log.iter().collect();
    let names = Names {
        own: "manager",
        manager: "manager",
    };
    chat_rows(&refs, &[], &[], &[], &names)
        .into_iter()
        .map(|row| match row.text.is_empty() {
            true => format!("{:?}", row.kind),
            false => row.text,
        })
        .collect()
}

fn event(id: u64, at: u64, task: u32, state: &str) -> serde_json::Value {
    let text = format!("#{task} {state}");
    json!({ "unix_ms": at, "kind": "message", "id": id, "from": "events", "text": text,
        "short": [{ "text": text, "task": task, "state": state }] })
}

#[test]
fn minutes_apart_repeats_show_once() {
    // ^ Task 554's log: eight "waiting" events 4 to 26 minutes apart, a quiet reply between two.
    let mut log: Vec<_> = (0..8)
        .map(|n| event(n + 1, 1_000 + n * 900_000, 554, "waiting"))
        .collect();
    log.insert(
        4,
        json!({ "unix_ms": 3_600_000, "kind": "reply", "id": 20, "to": [4], "text": "Waiting.", "quiet": true }),
    );
    let shown = rows(log);
    assert_eq!(
        shown.iter().filter(|text| *text == "#554 waiting").count(),
        1,
        "{shown:?}"
    );
}

#[test]
fn a_different_state_between_is_no_repeat() {
    let shown = rows(vec![
        event(1, 100, 7, "waiting"),
        event(2, 200_000, 7, "working"),
        event(3, 400_000, 7, "waiting"),
    ]);
    assert_eq!(shown, ["#7 waiting", "#7 working", "#7 waiting"]);
}

#[test]
fn a_bubble_ends_the_run() {
    let shown = rows(vec![
        event(1, 100, 7, "waiting"),
        json!({ "unix_ms": 200, "kind": "message", "id": 2, "from": "owner", "text": "status?" }),
        event(3, 300, 7, "waiting"),
    ]);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert_eq!(
        shown.iter().filter(|text| *text == "#7 waiting").count(),
        2,
        "{shown:?}"
    );
}

#[test]
fn two_tasks_interleaved_count_apart() {
    let shown = rows(vec![
        event(1, 100, 1, "waiting"),
        event(2, 200, 2, "working"),
        event(3, 300, 1, "waiting"),
    ]);
    assert_eq!(shown, ["#1 waiting", "#2 working"]);
}
