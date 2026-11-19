//! The one rule for "the owner is needed" (`needs::stage`): each state and wait of a task gives
//! one answer, and the counts, the push and the list band read that answer and no rule of their
//! own. Real records in a temp state folder, read through the store. ref: docs/tasks.md "Status"

use serde_json::{json, Value};
use std::fs;
use wsx_core::task::chains::{Band, Chains};
use wsx_core::task::needs::{self, NeedKind, Needs, Stage};
use wsx_core::task::stall;
use wsx_core::task::store::{TaskDir, RECORD_FILE};
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
    let held = json!({ "hold": "a trial" });
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
        (
            "a plan for the manager",
            "waiting",
            "owner",
            waiting("plan"),
            None,
            false,
        ),
        ("paused", "waiting", "owner", waiting("paused"), None, false),
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
        (
            "ready, the chat reviews",
            "ready",
            "chat:plans",
            json!({}),
            None,
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
    let counted = Needs::read(PROJECT, false).unwrap();
    assert_eq!(
        (counted.owner, counted.working),
        (owner, working),
        "{counted:?}"
    );
    assert_eq!(owner, cases.iter().filter(|case| case.4.is_some()).count());
    let _ = fs::remove_dir_all(&state);
}
