//! A task's workflow steps (docs/tasks.md "Workflow"): the owner's own steps apply to a project that
//! names none, the host sends each step as its own turn and runs the checks after the last;
//! `--skip` and `--size` leave steps out; the first turn opens with the start set of the task's
//! files (docs/memory.md "Start set"). Real host binary, fake Claude
//! (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;

fn owner_steps(fixture: &Fixture, steps: &[(&str, &str)]) {
    let dir = fixture.root.join("config/wsx");
    fs::create_dir_all(&dir).expect("owner config folder");
    let mut text = String::from("tasks:\n  workflow:\n    steps:\n");
    for (name, prompt) in steps {
        text.push_str(&format!("      - name: {name}\n        prompt: {prompt}\n"));
    }
    fs::write(dir.join("wsx.config.yml"), text).expect("write the owner config");
}

/// The summaries of the task's turns, one per `result` entry.
fn turns(fixture: &Fixture, task: &Value) -> Vec<String> {
    fixture
        .entries(task)
        .iter()
        .filter(|entry| entry["kind"] == "result")
        .map(|entry| entry["summary"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn step_names(task: &Value) -> Vec<String> {
    task["record"]["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["name"].as_str().unwrap().to_string())
        .collect()
}

fn new_id(fixture: &Fixture, args: &[&str]) -> String {
    let mut all = vec!["task", "new"];
    all.extend(args);
    all.push("--json");
    fixture.json(&all)["id"]
        .as_u64()
        .expect("task id")
        .to_string()
}

fn ready(fixture: &Fixture, id: &str) -> Value {
    let task = fixture.wait_task(id, "the task settles", |task| {
        matches!(task["state"].as_str(), Some("ready" | "failed")).then(|| task.clone())
    });
    assert_eq!(task["state"], "ready", "{}", fixture.log(&task));
    task
}

fn launch_effort(fixture: &Fixture) -> Value {
    fixture
        .traces()
        .iter()
        .rev()
        .find(|trace| !trace["launch"].is_null())
        .map(|trace| trace["effort"].clone())
        .expect("a launch")
}

fn ledger_effort(fixture: &Fixture, id: &str) -> Value {
    let ledger = fixture.json(&["chat", "ledger", "--json"]);
    ledger["open"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"].as_u64() == id.parse().ok())
        .map(|row| row["agent"]["effort"].clone())
        .expect("the task in the ledger")
}

// ref: docs/tasks.md "Workflow"
#[test]
fn each_step_is_its_own_turn_and_skip_leaves_a_step_out() {
    let fixture = Fixture::with_tasks_config("  checks: [\"test -f s.txt\"]\n");
    owner_steps(&fixture, &[("plan", "PLAN only"), ("build", "WORK s.txt")]);
    let stepped = new_id(&fixture, &["BGRESULT NOOP"]);
    let done = ready(&fixture, &stepped);
    assert_eq!(done["record"]["step"], 1);
    assert_eq!(step_names(&done), ["plan", "build"]);
    let said = turns(&fixture, &done);
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said[0].contains("Step 1 of 2, plan: PLAN only"),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("Step 2 of 2, build: WORK s.txt"),
        "{}",
        said[1]
    );
    // ^ Each step asks to end its background jobs first; the last asks for the memo too, so no
    // step of its own writes it.
    assert!(said
        .iter()
        .all(|turn| turn.contains("wait for or stop each background job")));
    assert!(said[1].contains("return your final result, with its memo"));
    // ^ A result while a job still runs is logged once: its notice wakes another turn.
    let early = fixture
        .entries(&done)
        .iter()
        .filter(|e| {
            e["kind"] == "agent"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("the result came while 1 background job(s) run"))
        })
        .count();
    assert_eq!(early, 1);

    // ^ `--skip` leaves a step out for a prompt that carries its plan; the model goes with it.
    let skipped = new_id(&fixture, &["--skip", "plan", "--model", "sonnet", "NOOP"]);
    let done = ready(&fixture, &skipped);
    assert_eq!(done["record"]["model"], "sonnet");
    assert!(
        fixture.traces().iter().any(|t| t["model"] == "sonnet"),
        "the worker did not start on sonnet"
    );
    assert_eq!(step_names(&done), ["build"]);
    let said = turns(&fixture, &done);
    assert!(
        said.len() == 1 && said[0].contains("Step 1 of 1, build: WORK s.txt"),
        "{said:?}"
    );
    assert!(fixture
        .refusal(&["task", "new", "--skip", "review", "NOOP"])
        .contains("has no such step (steps: plan, build)"));
    assert!(fixture
        .refusal(&["task", "new", "--skip", "plan", "--skip", "build", "NOOP"])
        .contains("--skip leaves no step"));
}

// ref: docs/memory.md "Start set"
#[test]
fn context_and_memo_steps_fold_and_the_first_turn_opens_with_the_start_set() {
    let fixture = Fixture::with_tasks_config("  checks: [\"test -f s.txt\"]\n");
    owner_steps(
        &fixture,
        &[
            ("context", "CTX recall"),
            ("plan", "PLAN only"),
            ("build", "WORK s.txt"),
            ("memo", "MEMO words"),
        ],
    );
    // ^ A `context` step folds into the first other step and a `memo` step into the one before it:
    // no turn of their own. The first turn opens with the host's task context: the files and the
    // open tasks that share them.
    let wait_file = fixture.root.join("release-13b");
    let sharer = new_id(
        &fixture,
        &[
            "--files",
            "s.txt",
            "--title",
            "Sharer",
            &format!("WAIT {}", wait_file.display()),
        ],
    );
    // ^ The area brief whose glob meets s.txt reaches the prompt; an area brief of another glob and
    // every fact record (here about s.txt too) do not.
    let project = fixture.root.join("project");
    let config = project.join("wsx.config.yml");
    let plain = fs::read_to_string(&config).unwrap();
    fs::create_dir_all(project.join("briefs")).unwrap();
    fs::write(
        project.join("briefs/s.md"),
        "Area brief: s.txt keeps the flag; do not rename it.\n",
    )
    .unwrap();
    fs::write(
        &config,
        format!(
            "{plain}memory:\n  areas:\n    \"s.*\": briefs/s.md\n    \"zz/**\": briefs/zz.md\n"
        ),
    )
    .unwrap();
    let memory = |kind: &str, slug: &str, path: &str, text: &str, more: &[&str]| {
        let file = fixture.root.join(format!("{slug}.txt"));
        fs::write(&file, text).unwrap();
        let mut args = vec![
            "memory",
            "add",
            "--kind",
            kind,
            "--slug",
            slug,
            "--text-file",
            file.to_str().unwrap(),
            "--paths",
            path,
        ];
        args.extend(more);
        let output = fixture.wsx(&args);
        assert!(output.status.success(), "{output:?}");
    };
    // ^ A verified fact (its commit is HEAD): one that still never reaches the first prompt.
    let head = fixture.git(&["rev-parse", "HEAD"]);
    for (slug, path, text) in [
        ("start-s", "s.txt", "The fixture flag lives in s.txt."),
        ("start-other", "other.rs", "other.rs is unrelated."),
    ] {
        memory(
            "fact",
            slug,
            path,
            text,
            &[
                "--source",
                "task:1",
                "--verified-by",
                "wsx.config.yml",
                "--commit",
                &head,
            ],
        );
    }
    for (slug, path, text) in [
        ("gate-s", "s.txt", "Keep the s.txt flag as it is."),
        ("gate-other", "other.rs", "Rename other.rs freely."),
    ] {
        memory(
            "rule",
            slug,
            path,
            text,
            &["--source", "msg:1", "--verified-by", "owner:1"],
        );
    }

    let folded = new_id(&fixture, &["--files", "s.txt", "NOOP"]);
    let done = ready(&fixture, &folded);
    assert_eq!(step_names(&done), ["plan", "build"]);
    assert_eq!(done["record"]["effort"], "high");
    assert_eq!(ledger_effort(&fixture, &folded), "high");
    assert_eq!(launch_effort(&fixture), "high");
    let said = turns(&fixture, &done);
    assert_eq!(said.len(), 2, "{said:?}");
    let first = &said[0];
    assert!(
        first.contains("Task context from the host:\nFiles: s.txt"),
        "{first}"
    );
    assert!(
        first.contains(&format!(
            "Open task #{sharer} \"Sharer\" (working) shares: s.txt"
        )),
        "{first}"
    );
    assert!(
        !first.contains("start-s") && !first.contains("The fixture flag lives in s.txt."),
        "{first}"
    );
    assert!(
        !first.contains("start-other") && !first.contains("zz.md"),
        "{first}"
    );
    assert!(
        first.contains("Area brief briefs/s.md (files s.*):\nArea brief: s.txt keeps the flag; do not rename it."),
        "{first}"
    );
    // ^ A rule whose paths meet the files reaches the prompt; one for other files does not.
    assert!(
        first.contains("Rule gate-s (paths s.txt)\nKeep the s.txt flag as it is.")
            && !first.contains("gate-other"),
        "{first}"
    );
    assert!(!first.contains("data from earlier work"), "{first}");
    assert!(
        first.contains("PLAN only\n\nIf the task context above is not enough: CTX recall"),
        "{first}"
    );
    assert!(
        said[1].contains("WORK s.txt\n\nFor the memo: MEMO words"),
        "{}",
        said[1]
    );

    // ^ `--size small` runs only the build step, with the folded words, on the small model; the
    // effort goes with the size; `--model` still wins, and `--skip` does not go with it.
    let small = new_id(&fixture, &["--size", "small", "NOOP"]);
    let done = ready(&fixture, &small);
    assert_eq!(done["record"]["model"], "sonnet");
    assert_eq!(step_names(&done), ["build"]);
    assert_eq!(done["record"]["effort"], "high");
    assert_eq!(ledger_effort(&fixture, &small), "high");
    assert_eq!(launch_effort(&fixture), "high");
    let said = turns(&fixture, &done);
    assert!(
        said.len() == 1 && said[0].contains("If the task context above is not enough: CTX recall"),
        "{said:?}"
    );
    assert!(said[0].contains("For the memo: MEMO words"), "{}", said[0]);
    let chosen = new_id(&fixture, &["--size", "small", "--model", "haiku", "NOOP"]);
    assert_eq!(
        fixture.json(&["task", "show", &chosen, "--json"])["record"]["model"],
        "haiku"
    );
    assert!(fixture
        .refusal(&["task", "new", "--size", "small", "--skip", "plan", "NOOP"])
        .contains("cannot be used with"));
    // ^ Neither task may outlive the test: the sharer still waits on its file.
    for id in [&sharer, &chosen] {
        fixture.json(&["task", "close", id, "--discard", "--json"]);
    }
}
