//! Contract files (`tasks.contracts`): a land that changes a line of one is the owner's. A chat's
//! agent gets an approval item instead of a land, the owner's own land goes through, and one that
//! only adds lines (a stricter contract) lands with a note to the main chat that starts no turn.
//! Through the real CLI and the fake agents. ref: docs/tasks.md "Contracts"
mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::time::{Duration, Instant};

fn state(fixture: &Fixture, id: &str) -> String {
    fixture.json(&["task", "show", id, "--json"])["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn wait_state(fixture: &Fixture, id: &str, want: &str) {
    fixture.wait_task(id, &format!("#{id} {want}"), |task| {
        (task["state"] == want).then_some(())
    });
}

/// The main chat's reply to the owner's message `text`, which runs a command in its turn.
fn chat_runs(fixture: &Fixture, text: &str) -> String {
    let id = fixture.json(&["task", "say", text, "--json"])["id"]
        .as_u64()
        .expect("message id");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = fixture.json(&["task", "chat", "--json"])["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(reply) = log.iter().find(|e| {
            e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
        }) {
            return reply["text"].as_str().unwrap_or_default().to_string();
        }
        assert!(Instant::now() < deadline, "no reply to {text}: {log:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_changed_contract_line_lands_only_by_the_owner() {
    let fixture = Fixture::new();
    let project = fixture.root.join("project");
    fs::create_dir_all(project.join("contracts")).unwrap();
    fs::write(
        project.join("contracts/계약.txt"),
        "assert the page has 20 rows\n",
    )
    .unwrap();
    let config = project.join("wsx.config.yml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("tasks:\n", "tasks:\n  contracts: [\"contracts/*.txt\"]\n"),
    )
    .unwrap();
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "-qm", "a contract"]);
    let main_before = fixture.git(&["rev-parse", "main"]);

    // ^ A worker rewrites the contract's line (a non-ASCII name, which git quotes unless asked
    // not to): the chat's land asks the owner and lands nothing.
    fixture.json(&["task", "new", "WORK contracts/계약.txt", "--json"]);
    wait_state(&fixture, "1", "ready");
    let asked = chat_runs(&fixture, "RUN wsx task land 1 --json");
    assert!(asked.contains("\"approval\""), "{asked}");
    assert_eq!(state(&fixture, "1"), "ready");
    assert_eq!(fixture.git(&["rev-parse", "main"]), main_before);

    // ^ The owner's own land goes through.
    let landed = fixture.wsx(&["task", "land", "1", "--json"]);
    assert!(landed.status.success(), "{landed:?}");
    wait_state(&fixture, "1", "landed");
    assert_ne!(fixture.git(&["rev-parse", "main"]), main_before);

    // ^ A new contract file only adds lines: the chat's land lands it, and the main chat gets a
    // note that starts no turn.
    fixture.json(&["task", "new", "WORK contracts/new.txt", "--json"]);
    wait_state(&fixture, "2", "ready");
    let said = chat_runs(&fixture, "RUN wsx task land 2 --json");
    assert!(!said.contains("\"approval\""), "{said}");
    wait_state(&fixture, "2", "landed");
    let deadline = Instant::now() + Duration::from_secs(30);
    let note: Value = loop {
        let log = fixture.json(&["task", "chat", "--json"])["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(note) = log.into_iter().find(|e| {
            e["from"] == "events"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("adds to the contract contracts/new.txt"))
        }) {
            break note;
        }
        assert!(
            Instant::now() < deadline,
            "no note of the stricter contract"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        !note["short"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|s| s["wakes"] == true),
        "{note}"
    );
}

#[test]
fn an_automatic_land_stops_at_a_changed_contract() {
    // ^ Auto land with a check: the review job lands a ready task by itself.
    let fixture = Fixture::with_tasks_config(
        "  checks:\n    - \"true\"\n  contracts: [\"contracts/*.txt\"]\n",
    );
    let project = fixture.root.join("project");
    fs::create_dir_all(project.join("contracts")).unwrap();
    fs::write(
        project.join("contracts/web.txt"),
        "assert the page has 20 rows\n",
    )
    .unwrap();
    let config = project.join("wsx.config.yml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("land: { auto: false }", "land: { auto: true }");
    assert!(text.contains("auto: true"), "{text}");
    fs::write(&config, text).unwrap();
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "-qm", "a contract, auto land"]);
    let main_before = fixture.git(&["rev-parse", "main"]);

    fixture.json(&["task", "new", "WORK contracts/web.txt", "--json"]);
    let task = fixture.wait("the land stops at the contract", |task| {
        fixture
            .log(task)
            .contains("it changes the contract contracts/web.txt")
            .then(|| task.clone())
    });
    assert_ne!(task["state"], "landed", "{task}");
    assert_eq!(fixture.git(&["rev-parse", "main"]), main_before);

    // ^ The owner lands it.
    let landed = fixture.wsx(&["task", "land", "1", "--json"]);
    assert!(landed.status.success(), "{landed:?}");
    wait_state(&fixture, "1", "landed");
}
