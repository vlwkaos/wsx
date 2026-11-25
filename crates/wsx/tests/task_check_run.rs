//! A worker's check-run through the host's broker: a check that prints more than the broker's
//! answer holds keeps its end and names the log that has all of it, and a scenario's `--part`
//! follows its rules (docs/tasks.md "Checks, revisions and report", "Scenarios"). Real host
//! binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::{fs, path::Path};

const CHECK: &str = "  checks:\n    - \"test ! -e broken.txt\"\n";

fn ready(fixture: &Fixture, prompt: &str) -> Value {
    let id = fixture.new_task(prompt);
    let task = fixture.settled(&id);
    assert_eq!(task["state"], "ready", "{}", fixture.log(&task));
    task
}

fn trace(fixture: &Fixture, key: &str) -> Value {
    fixture
        .traces()
        .iter()
        .rev()
        .find_map(|trace| trace.get(key).cloned())
        .unwrap_or_else(|| panic!("no {key} trace"))
}

// ref: docs/tasks.md "Checks, revisions and report"
#[test]
fn a_check_run_that_prints_more_than_the_broker_holds_keeps_the_end_and_names_the_log() {
    // ^ The scenario is check 2: the project's own check is check 1.
    let fixture = Fixture::with_tasks_config(&format!(
        "{CHECK}  scenarios:\n    loud: yes check-line | head -c 4000000; echo loud-end; exit 3\n"
    ));
    let loud = ready(&fixture, "CHECKRUN_WITH loud WORK loud.txt");
    let answers = trace(&fixture, "check_run_with");
    let code = answers[0][0].as_i64().expect("exit code");
    let said = answers[0][1].as_str().expect("answer");
    assert_eq!(code, 1, "{said}");
    assert!(
        said.contains("check 2 `yes check-line") && said.contains(": exit 3"),
        "{said}"
    );
    // ^ The first 4 MB end mid-line: the end of the output is kept, and it is short.
    assert!(said.trim_end().ends_with("check-line\nchecloud-end") && said.len() < 16 * 1024);
    let at = said.find("[the first ").expect("the cut note");
    let note = &said[at..];
    let whole_in = note
        .split("the whole output is in ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .expect("the log path");
    let whole = Path::new(loud["log"].as_str().unwrap()).with_file_name("check-run-2.log");
    assert_eq!(
        fs::canonicalize(whole_in).unwrap(),
        fs::canonicalize(&whole).unwrap()
    );
    assert_eq!(
        fs::metadata(&whole).unwrap().len() as usize,
        4_000_000 + "loud-end\n".len()
    );
}

// ref: docs/tasks.md "Scenarios"
#[test]
fn a_scenario_part_is_one_name_and_its_pass_is_no_full_pass() {
    let fixture = Fixture::with_tasks_config(&format!(
        "{CHECK}  scenarios:\n    parted: \"echo part=[$WSX_SCENARIO_PART]\"\n"
    ));
    let parted = ready(&fixture, "CHECKRUN_PART parted tasks WORK part.txt");
    let answers = trace(&fixture, "check_run_part");
    let at = |n: usize| {
        (
            answers[n][0].as_i64().unwrap(),
            answers[n][1].as_str().unwrap().to_string(),
        )
    };
    let (first, again, full, bad, plain) = (at(0), at(1), at(2), at(3), at(4));
    // ^ The command gets WSX_SCENARIO_PART, and a repeat of the same part runs again.
    for run in [&first, &again] {
        assert!(
            run.0 == 0 && run.1.contains("WSX_SCENARIO_PART='tasks'"),
            "{run:?}"
        );
    }
    assert!(first.1.contains("not a full pass"), "{first:?}");
    // ^ The full run has no part variable and no such note.
    assert!(
        full.0 == 0
            && !full.1.contains("export WSX_SCENARIO_PART")
            && !full.1.contains("not a full pass"),
        "{full:?}"
    );
    assert!(
        bad.0 != 0 && bad.1.contains("--part takes one name"),
        "{bad:?}"
    );
    assert!(
        plain.0 != 0 && plain.1.contains("--part needs exactly one scenario"),
        "{plain:?}"
    );
    let log: Vec<Value> = fixture
        .log(&parted)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let run = log
        .iter()
        .find(|entry| entry["kind"] == "check_run")
        .expect("a check_run entry");
    let results = run["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{run}");
    assert!(
        results[0]["command"]
            .as_str()
            .unwrap()
            .starts_with("export WSX_SCENARIO_PART='tasks'; ")
            && results[0]["output"]
                .as_str()
                .unwrap()
                .contains("part=[tasks]"),
        "{run}"
    );
    // ^ A part's pass is no full pass: it adds no evidence of its own.
    let evidence = parted["result"]["result"]["evidence"].as_array().unwrap();
    assert!(
        !evidence.iter().any(|e| e["check"]
            .as_str()
            .is_some_and(|c| c.contains("recorded by the host"))),
        "{evidence:?}"
    );
}
