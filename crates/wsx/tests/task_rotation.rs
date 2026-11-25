//! Context rotation (docs/tasks.md "Context rotation"): a worker conversation past
//! `rotateTokens` rotates between turns. Before the next turn the worker writes handoff notes in a
//! turn of its own, whose answer is no result, and the waiting turn opens a fresh conversation
//! with those notes, or none, never an earlier rotation's. A turn that passes the limit keeps its
//! own result. Real wsxd and task host, the fake Claude (`scripts/fake-claude-stream.py`: BIG makes
//! a 400k-token call, a `no-notes` file in the task folder makes it answer without notes).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::path::Path;

const ROTATE: &str = "    rotateTokens: 100000\n";
const OPENING: &str = "You continue a task whose earlier conversation";

/// The first messages of the fresh conversations, in order.
fn openings(fixture: &Fixture) -> Vec<String> {
    fixture
        .traces()
        .into_iter()
        .filter_map(|trace| trace["message"].as_str().map(str::to_string))
        .filter(|message| message.starts_with(OPENING))
        .collect()
}

/// The log as `kind: words` lines, for a failure message.
fn story(fixture: &Fixture, task: &Value) -> String {
    fixture
        .entries(task)
        .iter()
        .map(|entry| {
            let words = ["text", "summary", "reason", "status"]
                .iter()
                .find_map(|key| entry[*key].as_str())
                .unwrap_or("");
            format!(
                "{}: {}",
                entry["kind"].as_str().unwrap_or("?"),
                words.chars().take(120).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn count(entries: &[Value], kind: &str) -> usize {
    entries.iter().filter(|entry| entry["kind"] == kind).count()
}

/// Wait until the task is ready with `results` results and `rotations` rotations in its log.
fn ready(fixture: &Fixture, id: &str, results: usize, rotations: usize) -> Value {
    let ended = fixture.wait_task(id, "the task ends its turn", |task| {
        let entries = fixture.entries(task);
        (matches!(task["state"].as_str(), Some("ready" | "failed" | "waiting"))
            && count(&entries, "result") >= results
            && count(&entries, "rotated") >= rotations)
            .then(|| task.clone())
    });
    assert_eq!(ended["state"], "ready", "{}", story(fixture, &ended));
    ended
}

fn add(fixture: &Fixture, id: &str, text: &str) {
    fixture.json(&["task", "add", id, text, "--json"]);
}

// ref: docs/tasks.md "Context rotation"
#[test]
fn a_rotation_without_new_notes_never_hands_on_the_notes_of_an_earlier_one() {
    let fixture = Fixture::with_tasks_config(ROTATE);
    let id = fixture.new_task("BIG WORK a.txt");
    let task = ready(&fixture, &id, 1, 0);
    // ^ The next turn starts past the limit: the worker writes notes in a turn of its own first.
    add(&fixture, &id, "NOOP");
    ready(&fixture, &id, 2, 1);
    let first = openings(&fixture);
    assert_eq!(first.len(), 1, "{first:?}");
    assert!(first[0].contains("NOTES from the fixture"), "{}", first[0]);

    // ^ The second rotation's worker answers the request but writes no notes.
    let folder = Path::new(task["log"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    fs::write(folder.join("no-notes"), "").unwrap();
    add(&fixture, &id, "BIG");
    ready(&fixture, &id, 3, 1);
    add(&fixture, &id, "NOOP");
    let task = ready(&fixture, &id, 4, 2);
    let all = openings(&fixture);
    assert_eq!(all.len(), 2, "{all:?}\n{}", story(&fixture, &task));
    assert!(
        all[1].contains("wrote no handoff notes") && !all[1].contains("NOTES from the fixture"),
        "the fresh conversation got: {}",
        all[1]
    );
}

// ref: docs/tasks.md "Context rotation"
#[test]
fn a_turn_past_the_limit_keeps_its_result_and_the_next_turn_rotates_first() {
    let fixture = Fixture::with_tasks_config(ROTATE);
    let id = fixture.new_task("BIG WORK b.txt");
    let task = ready(&fixture, &id, 1, 0);
    // ^ No request reached the running turn: its own result is the task's.
    let summaries: Vec<String> = fixture
        .entries(&task)
        .iter()
        .filter(|entry| entry["kind"] == "result")
        .filter_map(|entry| entry["summary"].as_str().map(str::to_string))
        .collect();
    assert!(summaries[0].contains("BIG WORK b.txt"), "{summaries:?}");
    add(&fixture, &id, "NOOP");
    let task = ready(&fixture, &id, 2, 1);
    // ^ The request was a message of its own in the old conversation; its answer is no result.
    let sent: Vec<Value> = fixture
        .traces()
        .into_iter()
        .filter(|trace| trace["message"].is_string())
        .collect();
    let asked: Vec<&Value> = sent
        .iter()
        .filter(|trace| {
            trace["message"]
                .as_str()
                .unwrap()
                .contains("past this task's limit")
        })
        .collect();
    let fresh: Vec<&Value> = sent
        .iter()
        .filter(|trace| trace["message"].as_str().unwrap().starts_with(OPENING))
        .collect();
    assert!(
        asked.len() == 1 && fresh.len() == 1,
        "{}",
        story(&fixture, &task)
    );
    assert_ne!(asked[0]["session"], fresh[0]["session"]);
    assert!(fresh[0]["message"].as_str().unwrap().contains("NOOP"));
    assert_eq!(
        count(&fixture.entries(&task), "result"),
        2,
        "{}",
        story(&fixture, &task)
    );
}
