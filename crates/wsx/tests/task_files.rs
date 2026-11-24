//! What a task records about the files it touches: the paths of each revision and the `--files`
//! globs it declares meet other open tasks in list, show and the ledger; a bad glob, model or
//! title creates or sends nothing. Real host binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::{json, Value};

fn new_task(fixture: &Fixture, args: &[&str]) -> u64 {
    let mut argv = vec!["task", "new"];
    argv.extend(args);
    argv.push("--json");
    fixture.json(&argv)["id"].as_u64().expect("task id")
}

fn show(fixture: &Fixture, id: u64) -> Value {
    fixture.json(&["task", "show", &id.to_string(), "--json"])
}

fn ready(fixture: &Fixture, id: u64) -> Value {
    fixture.wait_task(&id.to_string(), "the task settles", |task| {
        match task["state"].as_str()? {
            "ready" | "failed" => Some(task.clone()),
            _ => None,
        }
    })
}

fn rows(fixture: &Fixture, list: &[&str]) -> Vec<Value> {
    let answer = fixture.json(list);
    answer["tasks"]
        .as_array()
        .or_else(|| answer["open"].as_array())
        .cloned()
        .unwrap_or_default()
}

fn row(rows: &[Value], id: u64) -> Value {
    rows.iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("no row #{id} in {rows:?}"))
        .clone()
}

fn count(fixture: &Fixture) -> usize {
    rows(fixture, &["task", "list", "--all", "--json"]).len()
}

/// Two tasks write `x.txt`, one writes `y.txt`.
fn three_tasks(fixture: &Fixture) -> (u64, u64, u64) {
    let x1 = new_task(fixture, &["WORK x.txt alpha"]);
    let y1 = new_task(fixture, &["WORK y.txt"]);
    let x2 = new_task(fixture, &["WORK x.txt beta"]);
    for id in [x1, y1, x2] {
        assert_eq!(ready(fixture, id)["state"], "ready");
    }
    (x1, y1, x2)
}

// ref: docs/tasks.md "Store"
#[test]
fn each_revision_records_its_paths_and_open_tasks_overlap() {
    let fixture = Fixture::new();
    let (x1, y1, x2) = three_tasks(&fixture);
    // ^ The fixture names `agent.model: fake-model`; a task takes it unless `--model` says otherwise.
    assert_eq!(show(&fixture, x1)["record"]["model"], "fake-model");
    let before = count(&fixture);
    // `new --model` picks the worker's model; a bad one creates no task.
    for bad in ["two words", "-x", ""] {
        let refused = fixture.wsx(&["task", "new", "WORK no.txt", &format!("--model={bad}")]);
        assert!(
            !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("model"),
            "{bad:?}: {refused:?}"
        );
    }
    assert_eq!(count(&fixture), before);
    let memo = new_task(
        &fixture,
        &[
            "WORK memo.txt MEMO fixtures stay small",
            "--model",
            "sonnet",
        ],
    );
    assert_eq!(ready(&fixture, memo)["state"], "ready");
    let task = show(&fixture, memo);
    assert_eq!(task["record"]["model"], "sonnet");
    assert_eq!(
        fixture.traces().last().expect("a launch")["model"],
        "sonnet"
    );
    // The memo stays in result.json and reaches the ledger's recent tasks once the task is closed.
    assert_eq!(task["result"]["result"]["memo"], "fixtures stay small");
    let closed = fixture.wsx(&["task", "close", &memo.to_string(), "--discard"]);
    assert!(closed.status.success(), "{closed:?}");
    let ledger = fixture.json(&["chat", "ledger", "--json"]);
    let recent = row(ledger["recent"].as_array().expect("recent"), memo);
    assert_eq!(recent["memo"], "fixtures stay small", "{recent}");

    let record = &show(&fixture, x1)["record"];
    assert_eq!(record["paths"], json!(["x.txt"]));
    assert_eq!(record["paths_total"], 1);
    let listed = rows(&fixture, &["task", "list", "--json"]);
    assert_eq!(
        row(&listed, x1)["overlaps"],
        json!([{"id": x2, "count": 1, "complete": true}])
    );
    assert_eq!(row(&listed, y1)["overlaps"], json!([]));
    assert_eq!(row(&listed, x2)["overlaps"][0]["id"], x1);
    assert_eq!(
        show(&fixture, x2)["overlaps"],
        json!([{"id": x1, "count": 1, "complete": true, "paths": ["x.txt"]}])
    );
}

// ref: docs/tasks.md "Store"
#[test]
fn declared_files_overlap_before_any_revision() {
    let fixture = Fixture::new();
    let (x1, y1, x2) = three_tasks(&fixture);
    let gate = fixture.root.join("files-gate");
    let wait = format!("WAIT {}", gate.display());
    let d1 = new_task(
        &fixture,
        &[&wait, "--files", "src/*.rs", "--files", "docs/a.md"],
    );
    let d2 = new_task(&fixture, &[&wait, "--files", "src/**/*.rs"]);
    let d3 = new_task(&fixture, &[&wait, "--files", "web/*.js"]);
    let record = &show(&fixture, d1)["record"];
    assert_eq!(record["files"], json!(["src/*.rs", "docs/a.md"]));
    assert_eq!(record["revision"], 0);
    let listed = rows(&fixture, &["task", "list", "--json"]);
    let declared = |id: u64| json!({"id": id, "count": 1, "complete": true, "declared": true});
    assert_eq!(row(&listed, d1)["overlaps"], json!([declared(d2)]));
    assert_eq!(row(&listed, d3)["overlaps"], json!([]));
    assert_eq!(
        row(&listed, x1)["overlaps"],
        json!([{"id": x2, "count": 1, "complete": true}])
    );
    assert_eq!(
        show(&fixture, d2)["overlaps"],
        json!([{"id": d1, "count": 1, "complete": true, "declared": true, "paths": ["src/**/*.rs"]}])
    );

    // `add --files` extends; a declared glob meets another task's changed path.
    let added = fixture.wsx(&[
        "task",
        "add",
        &d3.to_string(),
        "--files",
        "*.txt",
        "also the text files",
    ]);
    assert!(added.status.success(), "{added:?}");
    assert_eq!(
        show(&fixture, d3)["record"]["files"],
        json!(["web/*.js", "*.txt"])
    );
    // `add --title` names what the add made the task carry; a bad title sends nothing.
    let added = fixture.wsx(&[
        "task",
        "add",
        &d3.to_string(),
        "--title",
        "Web and text files",
        "also the docs",
    ]);
    assert!(added.status.success(), "{added:?}");
    let title = "Web and text files";
    assert_eq!(show(&fixture, d3)["record"]["title"], title);
    let listed = rows(&fixture, &["task", "list", "--json"]);
    assert_eq!(row(&listed, d3)["title"], title);
    for bad in [String::new(), "x".repeat(61)] {
        let refused = fixture.wsx(&["task", "add", &d3.to_string(), "--title", &bad, "x"]);
        assert!(
            !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("title"),
            "{refused:?}"
        );
    }
    assert_eq!(show(&fixture, d3)["record"]["title"], title);
    let open = rows(&fixture, &["chat", "ledger", "--json"]);
    let d3_row = row(&open, d3);
    assert_eq!(
        d3_row["overlaps"],
        json!([declared(x1), declared(y1), declared(x2)])
    );
    assert!(row(&open, x1)["overlaps"]
        .as_array()
        .unwrap()
        .contains(&declared(d3)));
    // The ledger names the agent in use now (docs/tasks.md "Agents"); no effort.
    let agent = &d3_row["agent"];
    assert_eq!(
        (
            agent["adapter"].clone(),
            agent["effort"].clone(),
            agent["fallback"].clone()
        ),
        (json!("claude"), json!("high"), json!(false)),
        "{agent}"
    );
    std::fs::write(&gate, "open").expect("open the gate");
}

// ref: docs/tasks.md "Store"
#[test]
fn a_bad_files_glob_creates_no_task() {
    let fixture = Fixture::new();
    let gate = fixture.root.join("files-gate");
    let wait = format!("WAIT {}", gate.display());
    let d1 = new_task(&fixture, &[&wait, "--files", "src/*.rs"]);
    let before = count(&fixture);
    for bad in ["/abs/*.rs", "../x", "a/../b", &"x".repeat(257), "[", ""] {
        let refused = fixture.wsx(&["task", "new", "WORK no.txt", "--files", bad]);
        assert!(
            !refused.status.success()
                && String::from_utf8_lossy(&refused.stderr).contains("--files"),
            "{bad:?}: {refused:?}"
        );
    }
    let many: Vec<String> = (0..65)
        .flat_map(|n| ["--files".to_string(), format!("f{n}")])
        .collect();
    let mut argv = vec!["task", "new", "WORK no.txt"];
    argv.extend(many.iter().map(String::as_str));
    let refused = fixture.wsx(&argv);
    assert!(
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stderr).contains("at most 64"),
        "{refused:?}"
    );
    let refused = fixture.wsx(&["task", "add", &d1.to_string(), "--files", "../up", "x"]);
    assert!(
        !refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("--files"),
        "{refused:?}"
    );
    assert_eq!(count(&fixture), before);
    std::fs::write(&gate, "open").expect("open the gate");
}
