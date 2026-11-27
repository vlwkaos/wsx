//! The one rule for "the owner is needed" (`needs::stage`): each state and wait of a task gives
//! one answer, and the counts, the push and the list band read that answer and no rule of their
//! own. Real records in a temp state folder, read through the store. ref: docs/tasks.md "Status"

use serde_json::{json, Value};
use std::fs;
use wsx_core::task::chains::{Band, Chains};
use wsx_core::task::needs::{self, NeedKind, Needs, Stage};
use wsx_core::task::stall;
use wsx_core::task::store::{self, TaskDir, RECORD_FILE};
use wsx_core::task::view;

/// What the case is, its state, its creator, extra record fields, its kind of need, whether it pushes.
type Case = (
    &'static str,
    &'static str,
    &'static str,
    Value,
    Option<NeedKind>,
    bool,
);

const PROJECT: &str = "demo-0a1b2c3d";

/// A record of `state` made by `by`, with `extra` fields (`waiting`, `after`, `hold`) merged in.
fn record(id: u32, state: &str, by: &str, extra: Value) -> Value {
    let mut record = json!({
        "id": id, "project": "demo", "project_id": PROJECT, "project_path": "/p",
        "default_branch": "main", "base": "abc", "branch": format!("wsx/t{id}"),
        "working_copy": "/p-t", "pane_id": null, "adapter": "claude", "model": "m",
        "checks": [], "agent_session": null, "state": state, "revision": 0,
        "created_by": by, "created_unix_ms": 1, "updated_unix_ms": 2,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    record
}

fn waiting(on: &str) -> Value {
    json!({ "waiting": { "on": on, "reason": "because" } })
}

#[test]
fn one_rule_decides_the_stage_the_counts_the_push_and_the_band() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.work")
        .join(format!("needs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    // SAFETY: this file has one test; nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    let after =
        json!({ "after": { "task": 9999, "text": "go", "since_unix_ms": 1, "because": "needs" } });
    let mut with_after = waiting("owner");
    with_after
        .as_object_mut()
        .unwrap()
        .extend(after.as_object().unwrap().clone());
    let held = json!({ "hold": "the owner tries it first" });
    use NeedKind::{Failed, Land, Waiting};
    let cases: Vec<Case> = vec![
        ("an agent turn", "working", "owner", json!({}), None, false),
        ("checks run", "checking", "owner", json!({}), None, false),
        (
            "waits for an answer",
            "waiting",
            "owner",
            waiting("owner"),
            Some(Waiting),
            true,
        ),
        (
            "waits for disk",
            "waiting",
            "owner",
            waiting("disk"),
            Some(Waiting),
            true,
        ),
        (
            "waits for the limit",
            "waiting",
            "owner",
            waiting("limit"),
            Some(Waiting),
            false,
        ),
        (
            "the owner works by hand",
            "waiting",
            "owner",
            waiting("hand"),
            None,
            false,
        ),
        // ^ A plan is the owner's to answer; a worker that waits on its check-run is not
        // (docs/tasks.md "Status", "Check-run").
        (
            "a plan",
            "waiting",
            "owner",
            waiting("plan"),
            Some(Waiting),
            true,
        ),
        (
            "waits on its check-run",
            "waiting",
            "chat:plans",
            waiting("check_run"),
            None,
            false,
        ),
        // ^ A record from before `check_run` reads as a wait on the owner.
        (
            "paused, an older record",
            "waiting",
            "owner",
            waiting("paused"),
            Some(Waiting),
            true,
        ),
        (
            "waits for another task",
            "waiting",
            "owner",
            with_after,
            None,
            false,
        ),
        (
            "ready, an owner task",
            "ready",
            "owner",
            json!({}),
            Some(Land),
            false,
        ),
        // ^ Without auto land no review job runs: a chat's ready task is the owner's to land too.
        (
            "ready, a chat's task",
            "ready",
            "chat:plans",
            json!({}),
            Some(Land),
            false,
        ),
        (
            "ready, the result asks",
            "ready",
            "chat:plans",
            waiting("asks"),
            Some(Land),
            false,
        ),
        (
            "ready, landing stopped",
            "ready",
            "chat:plans",
            waiting("land_stopped"),
            Some(Land),
            false,
        ),
        (
            "ready, held",
            "ready",
            "chat:plans",
            held,
            Some(Land),
            false,
        ),
        ("failed", "failed", "owner", json!({}), Some(Failed), false),
        ("landed", "landed", "owner", json!({}), None, false),
        ("closed", "closed", "owner", json!({}), None, false),
    ];
    let mut ids = Vec::new();
    for (_, task_state, by, extra, _, _) in &cases {
        let dir = TaskDir::allocate(PROJECT).unwrap();
        let body = record(dir.id, task_state, by, extra.clone());
        fs::write(
            dir.file(RECORD_FILE),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
        ids.push(dir.id);
    }

    let tasks = view::project_tasks(PROJECT).unwrap();
    let chains = Chains::of(&tasks, false);
    let (mut owner, mut working) = (0, 0);
    for ((name, _, _, _, kind, pushes), id) in cases.iter().zip(&ids) {
        let (dir, record) = tasks.iter().find(|(dir, _)| dir.id == *id).unwrap();
        let record = record.as_ref().unwrap();
        let stage = needs::stage(dir, record, false);
        assert_eq!(stage.need(), *kind, "{name}: {stage:?}");
        assert_eq!(needs::pushes(stage, record), *pushes, "{name}");
        match stage {
            Stage::Working => working += 1,
            Stage::NeedsYou(_) => owner += 1,
            Stage::Done | Stage::Closed => {}
        }
        if record.state.is_open() {
            let band = chains.place(*id).unwrap().band;
            // ^ A chain that waits for the owner is Waiting: the For you list holds it.
            assert!(
                kind.is_none() || band == Band::Waiting,
                "{name}: band {band:?}"
            );
        }
    }
    // ^ A ready task whose result asks the owner waits on the owner: the 4 h reminder (one push,
    // then one a day) reaches it as it reaches a task that waits for an answer.
    for (name, _, _, _, _, _) in cases
        .iter()
        .filter(|case| case.0.starts_with("ready, the result"))
    {
        let id = ids[cases.iter().position(|case| case.0 == *name).unwrap()];
        let (dir, record) = tasks.iter().find(|(dir, _)| dir.id == id).unwrap();
        let reminder = stall::owner_wait(dir, record.as_ref().unwrap(), 10 * 24 * 3_600_000)
            .unwrap_or_else(|| panic!("{name}: no reminder"));
        assert!(
            reminder.words.starts_with("waits on you for 9 d"),
            "{}",
            reminder.words
        );
    }
    // ^ A limit wait pushes while its account's failed sign-in is on record: only the owner can
    // end it. Between two tries (the retry time past, no success yet) it still does, and a failed
    // retry keeps the first failure's start, so a reader pushes the failure once.
    let limited = ids[cases
        .iter()
        .position(|case| case.0 == "waits for the limit")
        .unwrap()];
    let (dir, record) = tasks.iter().find(|(dir, _)| dir.id == limited).unwrap();
    let record = record.as_ref().unwrap();
    let pushes = || needs::pushes(needs::stage(dir, record, false), record);
    store::mark_sign_in_failed("claude", 1_000).unwrap();
    assert!(
        pushes(),
        "a failed sign-in pushes, also past its retry time"
    );
    store::mark_sign_in_failed("claude", 9_000).unwrap();
    assert_eq!(needs::sign_in_failed_since(record), Some(1_000));
    store::clear_sign_in_failed("claude");
    assert!(!pushes() && needs::sign_in_failed_since(record).is_none());
    let counted = Needs::read(PROJECT, false).unwrap();
    assert_eq!(
        (counted.owner, counted.working),
        (owner, working),
        "{counted:?}"
    );
    assert_eq!(owner, cases.iter().filter(|case| case.4.is_some()).count());
    let _ = fs::remove_dir_all(&state);
}
