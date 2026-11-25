//! A dirty main checkout (docs/tasks.md "Landing"): a changed file the land does not touch stays
//! dirty and the land goes on; one the land changes too refuses it, naming only that file, and the
//! ledger named it before the press. Real host binary, fake Claude
//! (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::{fs, path::Path};

fn new_ready(fixture: &Fixture, prompt: &str) -> String {
    let id = fixture.new_task(prompt);
    fixture.wait_task(&id, "ready", |task| {
        (task["state"] == "ready").then_some(())
    });
    id
}

fn blocked_by(fixture: &Fixture, id: &str) -> Value {
    let ledger = fixture.json(&["chat", "ledger", "--json"]);
    let row = ledger["open"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"].to_string() == id)
        .unwrap_or_else(|| panic!("task {id} is not in the ledger: {ledger}"))
        .clone();
    row["land_blocked_by"].clone()
}

// ref: docs/tasks.md "Landing"
#[test]
fn a_land_refuses_only_when_a_dirty_main_file_overlaps_it_and_names_that_file() {
    let fixture = Fixture::new();
    let project = fixture.root.join("project");
    fs::write(project.join(".gitignore"), "target/\n").unwrap();
    fs::write(project.join("README"), "fixture\n").unwrap();
    fixture.git(&["add", "."]);
    fixture.git(&["commit", "-qm", "files"]);
    fs::write(project.join("scratch.txt"), "untracked").unwrap();
    // ^ Dirty, and not a file the land changes.
    fs::write(project.join(".gitignore"), "target/\nunrelated/\n").unwrap();

    let free = new_ready(&fixture, "WORK free.txt");
    assert_eq!(blocked_by(&fixture, &free), serde_json::json!([]));
    let landed = fixture.json(&["task", "land", &free, "--json"]);
    assert_eq!(landed["state"], "landed", "{landed}");
    assert!(!fixture.git(&["show", "main:free.txt"]).is_empty());
    assert_eq!(
        fixture.git(&["status", "--porcelain", "--", ".gitignore"]),
        "M .gitignore"
    );

    // ^ A dirty file the land changes too refuses it.
    fs::write(project.join("README"), "dirty\n").unwrap();
    let overlap = new_ready(&fixture, "WORK README");
    assert_eq!(
        blocked_by(&fixture, &overlap),
        serde_json::json!(["README"])
    );
    let output = fixture.wsx(&["task", "land", &overlap, "--json"]);
    assert!(!output.status.success(), "{output:?}");
    let answer: Value = serde_json::from_slice(&output.stdout).expect("the refusal as JSON");
    let said = answer["message"].as_str().expect("message");
    assert!(
        said.contains("uncommitted changes in README that this land changes too"),
        "{said}"
    );
    assert!(
        !said.contains(".gitignore") && !said.contains("scratch.txt"),
        "{said}"
    );
    // ^ It says where to stash them: the main checkout.
    let stash = said
        .split("stash them in ")
        .nth(1)
        .and_then(|rest| rest.split(", then land again").next())
        .unwrap_or_else(|| panic!("no stash folder in: {said}"));
    assert!(Path::new(stash).is_absolute());
    assert_eq!(
        fs::canonicalize(stash).unwrap(),
        fs::canonicalize(&project).unwrap()
    );
    assert_eq!(
        fixture.json(&["task", "show", &overlap, "--json"])["state"],
        "ready"
    );
    assert_eq!(
        fs::read_to_string(project.join("README")).unwrap(),
        "dirty\n"
    );
}
