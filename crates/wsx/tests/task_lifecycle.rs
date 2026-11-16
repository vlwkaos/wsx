//! A task never loses work at the host: an add to a task that waits on `after` joins its first
//! turn after the prompt, and a check-run's answer reaches the owner and the worker after a stop
//! or a host restart (docs/tasks.md "After another task", "Check-run"). Real host binary, fake
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

/// The texts of the messages the fake Claude received: (a manager's, text).
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

/// What a worker's agent received (a manager's event lines name the titles too).
fn received(fixture: &Fixture, needle: &str) -> Vec<String> {
    messages(fixture)
        .into_iter()
        .filter(|(manager, text)| !manager && text.contains(needle))
        .map(|(_, text)| text)
        .collect()
}

/// The repo's service and the manager run: the service ends a wait on a closed task and tells
/// the manager what its sweep finds. `say` starts both.
fn with_a_manager(fixture: &Fixture) {
    let said = fixture.wsx(&["task", "say", "hello"]);
    assert!(said.status.success(), "{said:?}");
}

/// #1 works until `hold` exists; #2 was created `--after 1` and has had an add. Nothing of #2
/// has reached an agent: the add said it is queued behind the prompt.
fn waiting_task_with_an_add(fixture: &Fixture) {
    with_a_manager(fixture);
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

/// A fixture whose scenario `gate` is a `ci:` command that waits for the file `gate`. It acts as
/// scripts/ci-scenario.py does for one head: the first call per commit starts a run (a line in
/// `starts`), a later call joins it. The script exits when the fixture is gone.
fn gated_ci() -> Fixture {
    let fixture =
        Fixture::with_tasks_config("  scenarios:\n    gate: \"ci: sh {root}/fake-ci.sh\"\n");
    let root = fixture.root.display();
    fs::write(
        fixture.root.join("fake-ci.sh"),
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = --local ] && exit 69\n\
             R={root}\n\
             echo call >> $R/calls\n\
             key=$(git rev-parse HEAD)\n\
             mkdir -p $R/runs\n\
             if [ ! -e $R/runs/$key ]; then touch $R/runs/$key; echo $key >> $R/starts; fi\n\
             while [ ! -e $R/gate ]; do [ -d $R ] || exit 1; sleep 0.2; done\n\
             echo ci-passed\n"
        ),
    )
    .expect("write the fake ci script");
    fixture
}

fn lines(fixture: &Fixture, name: &str) -> usize {
    fs::read_to_string(fixture.root.join(name)).map_or(0, |text| text.lines().count())
}

/// A worker whose result waits for its `ci:` check-run, the run started and not ended.
fn worker_with_a_run_out(fixture: &Fixture) {
    fixture.json(&["task", "new", "WORK a.txt CHECKRUN_LATER gate", "--json"]);
    fixture.wait("the result waits for the check-run", |task| {
        (task["record"]["waiting"]["on"] == "check_run").then_some(())
    });
    until("the run started", || {
        (lines(fixture, "starts") == 1).then_some(())
    });
}

#[test]
fn a_check_run_that_ends_after_a_stop_wakes_the_manager() {
    let fixture = gated_ci();
    with_a_manager(&fixture);
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

    // ^ The stopped worker gets no turn; the manager hears of the answer.
    assert!(received(&fixture, "Your check-run ended").is_empty());
    until("the manager's wake", || {
        messages(&fixture)
            .into_iter()
            .find(|(manager, text)| *manager && text.contains("check-run ended after the stop"))
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
fn a_host_that_restarts_while_a_check_run_is_out_gets_its_answer_once() {
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
    until("the answer", || {
        (!received(&fixture, "Your check-run ended").is_empty()).then_some(())
    });
    // ^ One answer, and one run of the same commit: the script joined it as ci-scenario.py does.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(received(&fixture, "Your check-run ended").len(), 1);
    assert_eq!(lines(&fixture, "starts"), 1);
}
