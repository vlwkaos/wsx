//! A task never loses work at the host: an add to a task that waits on `after` joins its first
//! turn after the prompt; a check-run never holds the worker's result, and its answer reaches the
//! owner and the worker after a stop or a host restart (docs/tasks.md "After another task", "Check-run"). Real host binary, fake
//! Claude (`scripts/fake-claude-stream.py`); the boundary is Seatbelt, so run it as a check
//! (`wsx task check-run`), outside a worker's boundary.

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

/// Poll `test` for up to 40 s.
fn until<T>(label: &str, mut test: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(value) = test() {
            return value;
        }
        assert!(Instant::now() < deadline, "{label} timeout");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The texts of the messages the fake Claude received: (a chat's, text).
fn messages(fixture: &Fixture) -> Vec<(bool, String)> {
    fixture
        .traces()
        .iter()
        .filter_map(|trace| {
            Some((
                trace["manager"] == true,
                trace["message"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// What a worker's agent received (a chat's event lines name the titles too).
fn received(fixture: &Fixture, needle: &str) -> Vec<String> {
    messages(fixture)
        .into_iter()
        .filter(|(manager, text)| !manager && text.contains(needle))
        .map(|(_, text)| text)
        .collect()
}

/// The repo's service and the lead run: the service ends a wait on a closed task and tells
/// the lead what its sweep finds. `say` starts both.
fn with_a_lead(fixture: &Fixture) {
    let said = fixture.wsx(&["chat", "say", "hello"]);
    assert!(said.status.success(), "{said:?}");
}

/// #1 works until `hold` exists; #2 was created `--after 1` and has had an add. Nothing of #2
/// has reached an agent: the add said it is queued behind the prompt.
fn waiting_task_with_an_add(fixture: &Fixture) {
    with_a_lead(fixture);
    let hold = fixture.root.join("hold");
    fixture.json(&["task", "new", &format!("WAIT {}", hold.display()), "--json"]);
    fixture.json(&[
        "task",
        "new",
        "--after",
        "1",
        "--because",
        "needs",
        "NOOP the prompt",
        "--json",
    ]);
    fixture.wait_task("2", "the new task waits", |task| {
        (task["state"] == "waiting").then_some(())
    });
    let added = fixture.wsx(&["task", "add", "2", "NOOP the add"]);
    let said = String::from_utf8_lossy(&added.stdout).into_owned();
    assert!(
        added.status.success() && said.contains("queued"),
        "{added:?}"
    );
    let task = fixture.json(&["task", "show", "2", "--json"]);
    assert_eq!(task["state"], "waiting", "{task}");
    assert!(
        received(fixture, "the prompt").is_empty() && received(fixture, "the add").is_empty(),
        "an add started a turn: {:?}",
        messages(fixture)
    );
}

/// The first turn's input holds the prompt, then the add, once.
fn first_turn_has_prompt_then_add(fixture: &Fixture) {
    let first = until("the first turn of #2", || {
        received(fixture, "the prompt").into_iter().next()
    });
    let (prompt, add) = (first.find("the prompt"), first.find("the add"));
    assert!(
        prompt.is_some() && add.is_some() && prompt < add,
        "prompt then add expected: {first}"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        received(fixture, "the add").len(),
        1,
        "{:?}",
        messages(fixture)
    );
}

#[test]
fn an_add_waits_for_the_prompt_until_the_wait_is_turned_off() {
    let fixture = Fixture::new();
    waiting_task_with_an_add(&fixture);
    let off = fixture.wsx(&["task", "after", "2", "--off"]);
    assert!(off.status.success(), "{off:?}");
    first_turn_has_prompt_then_add(&fixture);
}

#[test]
fn an_add_waits_for_the_prompt_until_the_task_waited_for_ends() {
    let fixture = Fixture::new();
    waiting_task_with_an_add(&fixture);
    let closed = fixture.wsx(&["task", "close", "1", "--discard"]);
    assert!(closed.status.success(), "{closed:?}");
    first_turn_has_prompt_then_add(&fixture);
}

/// A fixture whose scenario `gate` is a command that waits for the file `gate`, then fails when
/// the file `fail` exists. The first call per commit starts a run (a line in `starts`); a later
/// call joins it. The script exits when the fixture is gone.
fn gated_ci() -> Fixture {
    let fixture = Fixture::with_tasks_config("  scenarios:\n    gate: \"sh {root}/fake-ci.sh\"\n");
    let root = fixture.root.display();
    fs::write(
        fixture.root.join("fake-ci.sh"),
        format!(
            "#!/bin/sh\n\
             R={root}\n\
             echo call >> $R/calls\n\
             key=$(git rev-parse HEAD)\n\
             mkdir -p $R/runs\n\
             if [ ! -e $R/runs/$key ]; then touch $R/runs/$key; echo $key >> $R/starts; fi\n\
             while [ ! -e $R/gate ]; do [ -d $R ] || exit 1; sleep 0.2; done\n\
             if [ -e $R/fail ]; then echo 'FAILED section gate'; exit 1; fi\n\
             echo ci-passed\n"
        ),
    )
    .expect("write the fake ci script");
    fixture
}

fn lines(fixture: &Fixture, name: &str) -> usize {
    fs::read_to_string(fixture.root.join(name)).map_or(0, |text| text.lines().count())
}

/// A worker whose result settled while its scenario check-run is out: the task is `ready`, the
/// run started and not ended.
fn worker_with_a_run_out(fixture: &Fixture) {
    fixture.json(&["task", "new", "WORK a.txt CHECKRUN_LATER gate", "--json"]);
    until("the run started", || {
        (lines(fixture, "starts") == 1).then_some(())
    });
    fixture.wait("the result settles without the answer", |task| {
        (task["state"] == "ready").then_some(())
    });
    let task = fixture.json(&["task", "show", "1", "--json"]);
    assert!(
        !fixture.log(&task).contains("\"kind\":\"check_run\""),
        "the run ended early: {task}"
    );
}

#[test]
fn a_late_check_run_that_passes_is_evidence_and_starts_no_turn() {
    let fixture = gated_ci();
    worker_with_a_run_out(&fixture);
    fs::write(fixture.root.join("gate"), "").expect("open the gate");
    fixture.wait("the answer is logged", |task| {
        fixture
            .log(task)
            .contains("\"kind\":\"check_run\"")
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));
    assert!(received(&fixture, "Your check-run ended").is_empty());
    // ^ The pass is recorded as evidence in the settled result (docs/tasks.md "Check-run").
    let task = fixture.json(&["task", "show", "1", "--json"]);
    assert_eq!(task["state"], "ready", "{task}");
    let evidence = task["result"]["result"]["evidence"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let proof: Vec<_> = evidence
        .iter()
        .filter(|line| {
            line["check"]
                .as_str()
                .is_some_and(|check| check.contains("check-run"))
        })
        .collect();
    assert!(
        proof.len() == 1
            && proof[0]["proof"]
                .as_str()
                .is_some_and(|text| text.contains(": exit 0")),
        "{task}"
    );
}

#[test]
fn a_failed_check_run_reopens_the_settled_task() {
    let fixture = gated_ci();
    worker_with_a_run_out(&fixture);
    fs::write(fixture.root.join("fail"), "").expect("mark the run failed");
    fs::write(fixture.root.join("gate"), "").expect("open the gate");
    let turn = until("the failure's turn", || {
        received(&fixture, "Your check-run ended")
            .into_iter()
            .next()
    });
    assert!(turn.contains("FAILED section gate"), "{turn}");
    fixture.wait("the task settles again", |task| {
        (task["state"] == "ready").then_some(())
    });
}

#[test]
fn a_check_run_that_ends_after_a_stop_reaches_the_chat_and_the_next_turn() {
    let fixture = gated_ci();
    with_a_lead(&fixture);
    worker_with_a_run_out(&fixture);
    let stopped = fixture.wsx(&["task", "stop", "1"]);
    assert!(stopped.status.success(), "{stopped:?}");
    fs::write(fixture.root.join("gate"), "").expect("open the gate");
    fixture.wait("the run ends", |task| {
        fixture
            .log(task)
            .contains("\"kind\":\"check_run\"")
            .then_some(())
    });

    // ^ The stopped worker gets no turn; the chat's event line says the answer waits for it
    // (a task's event starts no lead turn, docs/tasks.md "Chat host").
    assert!(received(&fixture, "Your check-run ended").is_empty());
    let chat_log = fixture.root.join("state/wsx/tasks/chats/demo/log.jsonl");
    until("the chat's event line", || {
        fs::read_to_string(&chat_log)
            .ok()?
            .contains("check-run ended after the stop")
            .then_some(())
    });
    // ^ The answer joins the turn the next add starts.
    let added = fixture.wsx(&["task", "add", "1", "NOOP go on"]);
    assert!(added.status.success(), "{added:?}");
    let turn = until("the add's turn", || {
        received(&fixture, "go on").into_iter().next()
    });
    assert!(turn.contains("Your check-run ended"), "{turn}");
}

#[test]
fn a_host_that_restarts_while_a_check_run_is_out_runs_it_once() {
    let fixture = gated_ci();
    worker_with_a_run_out(&fixture);
    let task = fixture.json(&["task", "show", "1", "--json"]);
    let project_id = task["record"]["project_id"].as_str().expect("project id");
    let host = format!("task host {project_id} 1");
    let killed = Command::new("pkill")
        .args(["-9", "-f", &format!("{host}$")])
        .status()
        .expect("run pkill");
    assert!(killed.success(), "no host to kill");
    until("the host is gone", || {
        (fixture.json(&["task", "show", "1", "--json"])["host_running"] == false).then_some(())
    });

    let restarted = fixture.wsx(&["session", "restart", "task-1"]);
    assert!(restarted.status.success(), "{restarted:?}");
    until("the run is out again", || {
        (lines(&fixture, "calls") >= 2).then_some(())
    });
    fs::write(fixture.root.join("gate"), "").expect("open the gate");
    fixture.wait("the answer is logged", |task| {
        fixture
            .log(task)
            .contains("\"kind\":\"check_run\"")
            .then_some(())
    });
    // ^ A pass after the settled result starts no turn, and one run of the same commit ran: the
    // script joined it.
    std::thread::sleep(Duration::from_secs(2));
    assert!(received(&fixture, "Your check-run ended").is_empty());
    assert_eq!(lines(&fixture, "starts"), 1);
    let task = fixture.json(&["task", "show", "1", "--json"]);
    assert_eq!(task["state"], "ready", "{task}");
}

/// A task parked `--because files` plans at once; only its build waits for the other task's
/// end, then starts on the default branch (docs/tasks.md "After another task").
#[test]
fn a_task_parked_for_its_files_plans_now_and_builds_after_the_wait() {
    let fixture = Fixture::with_tasks_config(
        "  workflow:\n    steps:\n      - {name: plan, prompt: plan it}\n      - {name: build, prompt: build it}\n",
    );
    with_a_lead(&fixture);
    let hold = fixture.root.join("hold");
    fixture.json(&["task", "new", &format!("WAIT {}", hold.display()), "--json"]);
    fixture.json(&[
        "task",
        "new",
        "--after",
        "1",
        "--because",
        "files",
        "NOOP parked",
        "--json",
    ]);
    let plan = until("the plan turn of #2", || {
        received(&fixture, "NOOP parked").into_iter().next()
    });
    assert!(plan.contains("plan it"), "{plan}");
    fixture.wait_task("2", "the build waits", |task| {
        let reason = task["record"]["waiting"]["reason"]
            .as_str()
            .unwrap_or_default();
        (task["state"] == "waiting" && reason.contains("its build waits for #1")).then_some(())
    });
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        received(&fixture, "build it").is_empty(),
        "{:?}",
        messages(&fixture)
    );
    let closed = fixture.wsx(&["task", "close", "1", "--discard"]);
    assert!(closed.status.success(), "{closed:?}");
    until("the build turn of #2", || {
        received(&fixture, "build it").into_iter().next()
    });
    fixture.wait_task("2", "the build ends", |task| {
        (task["state"] == "ready").then_some(())
    });
}

/// A background failure that arrives while the checks of the result run waits for them, then
/// reopens the task once (docs/tasks.md "Check-run").
#[test]
fn a_failure_during_the_checks_reopens_the_task_after_them() {
    let fixture = gated_ci();
    let checks_gate = fixture.root.join("checks-gate");
    fs::write(
        fixture.root.join("project/wsx.config.yml"),
        fs::read_to_string(fixture.root.join("project/wsx.config.yml"))
            .expect("read the config")
            .replace(
                "tasks:\n",
                &format!(
                    "tasks:\n  checks:\n    - \"while [ ! -e {} ]; do sleep 0.2; done\"\n",
                    checks_gate.display()
                ),
            ),
    )
    .expect("write the config");
    fixture.json(&["task", "new", "WORK a.txt CHECKRUN_LATER gate", "--json"]);
    until("the run started", || {
        (lines(&fixture, "starts") == 1).then_some(())
    });
    fixture.wait("the checks of the result run", |task| {
        (task["state"] == "checking").then_some(())
    });
    fs::write(fixture.root.join("fail"), "").expect("mark the run failed");
    fs::write(fixture.root.join("gate"), "").expect("open the gate");
    fixture.wait("the failure is logged", |task| {
        fixture
            .log(task)
            .contains("\"kind\":\"check_run\"")
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(1));
    assert!(received(&fixture, "Your check-run ended").is_empty());
    fs::write(&checks_gate, "").expect("open the checks");
    until("the failure's turn", || {
        received(&fixture, "Your check-run ended")
            .into_iter()
            .next()
    });
    fixture.wait("the task settles again", |task| {
        (task["state"] == "ready").then_some(())
    });
    assert_eq!(received(&fixture, "Your check-run ended").len(), 1);
}

/// The report names the `assert` lines a revision removed or changed, and only those: a comment
/// that says assert and an added assert stay out (docs/tasks.md "Checks, revisions and report").
#[test]
fn the_report_names_the_test_checks_a_revision_edited() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("project/checks.py"),
        "x = 1\nassert x == 1, 'the decided value'\n# assert in a comment\n",
    )
    .expect("write the checks");
    fixture.git(&["add", "checks.py"]);
    fixture.git(&["commit", "-q", "-m", "checks"]);
    // ^ The owner's own git config may name paths `c/` and `w/`; the report shows them as written.
    fixture.git(&["config", "diff.mnemonicPrefix", "true"]);
    fixture.json(&["task", "new", "LINE checks.py 2", "--json"]);
    // ^ Replaces `x = 1` with a line that adds an assert: an added check is not an edited one.
    fixture.json(&["task", "new", "LINE checks.py 1 assert y", "--json"]);
    let report = |id: &str| -> String {
        let path = fixture.wait_task(id, "the task is ready with a report", |task| {
            (task["state"] == "ready")
                .then(|| task["report"].as_str().map(str::to_string))
                .flatten()
        });
        fs::read_to_string(&path).expect("read report.md")
    };
    let edited = report("1");
    assert!(
        edited.contains("Edited test checks: 1 `assert` line removed or changed"),
        "{edited}"
    );
    assert!(
        edited.contains("- `checks.py`: `assert x == 1, 'the decided value'`"),
        "{edited}"
    );
    assert!(!edited.contains("in a comment"), "{edited}");
    let untouched = report("2");
    assert!(!untouched.contains("Edited test checks"), "{untouched}");
}

/// Code owns a task's events (docs/design/orchestration.md step 3): with `tasks.land.auto` a
/// ready task's review job, a fresh read-only agent, decides. Round 1 asks for changes, which
/// go to the worker as a turn; round 2 lands the new revision. The lead, running for the
/// owner's message, takes no turn of task events through it all. ref: docs/tasks.md "Review"
#[test]
fn a_review_job_lands_a_ready_task_and_no_event_wakes_the_lead() {
    let fixture = Fixture::with_tasks_config("  checks:\n    - \"true\"\n");
    let config = fixture.root.join("project/wsx.config.yml");
    let text = fs::read_to_string(&config).expect("read the project config");
    fs::write(
        &config,
        text.replace("land: { auto: false }", "land: { auto: true }"),
    )
    .expect("turn auto land on");
    fixture.git(&["commit", "-qam", "auto land"]);
    with_a_lead(&fixture);
    until("the manager's first turn", || {
        messages(&fixture)
            .iter()
            .any(|(manager, text)| *manager && text.contains("hello"))
            .then_some(())
    });
    fixture.json(&["task", "new", "REVIEWNO REVIEWWRITE WORK a.txt", "--json"]);
    let landed = fixture.wait_task("1", "the review lands the task", |task| {
        (task["state"] == "landed").then(|| task.clone())
    });
    let log = fixture.log(&landed);
    let reviews: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["kind"] == "review")
        .collect();
    let verdicts: Vec<(u64, &str)> = reviews
        .iter()
        .map(|review| {
            (
                review["round"].as_u64().unwrap_or_default(),
                review["verdict"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(verdicts, [(1, "changes"), (2, "land")], "{log}");
    assert_eq!(reviews[0]["findings"][0], "WORK reviewfix.txt", "{log}");
    // ^ Each verdict carries the reviewer's spend, which the task's usage counts as `review`.
    assert!(
        reviews.iter().all(|review| review["usage"].is_object()),
        "{log}"
    );
    assert_ne!(reviews[0]["revision"], reviews[1]["revision"], "{log}");
    // ^ The worker did the finding: both files are on main.
    for file in ["a.txt", "reviewfix.txt"] {
        let shown = fixture.git(&["show", &format!("main:{file}")]);
        assert!(!shown.is_empty(), "{file} is not on main");
    }
    // ^ The reviewer reads only: its write to the working copy was refused by its boundary.
    let writes: Vec<String> = fixture
        .traces()
        .iter()
        .filter_map(|trace| trace["review_write"].as_str().map(str::to_string))
        .collect();
    assert_eq!(writes, ["refused", "refused"], "{writes:?}");
    // ^ The task's events reach the main chat after the repo service's quiet window; a chat
    // they woke would take its turn at once.
    let chat_log = fixture.root.join("state/wsx/tasks/chats/demo/log.jsonl");
    until("the land event reaches the manager's chat", || {
        fs::read_to_string(&chat_log)
            .ok()?
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|entry| {
                entry["from"] == "events"
                    && entry["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("#1 landed"))
            })
            .then_some(())
    });
    std::thread::sleep(Duration::from_secs(3));
    // ^ The lead's only message is the owner's: no turn of task events.
    let manager: Vec<String> = messages(&fixture)
        .into_iter()
        .filter(|(manager, _)| *manager)
        .map(|(_, text)| text)
        .collect();
    assert!(
        manager.iter().all(|text| !text.starts_with("Events")),
        "the manager took a turn of events: {manager:?}"
    );
}
