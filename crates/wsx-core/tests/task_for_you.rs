//! The For you list and the chat's pill counts are one answer (`needs::for_you`, `needs::chat_counts`):
//! a chat with one task that waits for an answer, one approval, one open question and one To check
//! item has four items, three of them needs and one check, and the counts say the same. Real files
//! in a temp state folder, read through the store. ref: docs/web-client-tasks.md "Home"

use serde_json::json;
use std::fs;
use wsx_core::task::chats;
use wsx_core::task::checks::{self, CheckItem};
use wsx_core::task::manager::{ApprovalLine, ManagerEvent};
use wsx_core::task::needs::{self, YouSource};
use wsx_core::task::store::{self, TaskDir, RECORD_FILE};

const PROJECT: &str = "demo-0a1b2c3d";

fn record(id: u32, state: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut record = json!({
        "id": id, "project": "demo", "project_id": PROJECT, "project_path": "/p",
        "default_branch": "main", "base": "abc", "branch": format!("wsx/t{id}"),
        "working_copy": "/p-t", "pane_id": null, "adapter": "claude", "model": "m",
        "checks": [], "agent_session": null, "state": state, "revision": 0,
        "created_by": "owner", "created_unix_ms": 1, "updated_unix_ms": 2,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    record
}

#[test]
fn the_list_and_the_counts_of_a_chat_agree() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.work")
        .join(format!("for-you-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    // SAFETY: this file has one test; nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    // ^ The project's first chat is its main chat, which owns what no other chat does.
    let home = chats::create(PROJECT, "demo", None, None).unwrap();
    // ^ One task that waits for the owner's answer, one landed task whose To check item is open.
    let waits = TaskDir::allocate(PROJECT).unwrap();
    let body = record(
        waits.id,
        "waiting",
        json!({ "waiting": { "on": "owner", "reason": "which one?" } }),
    );
    fs::write(waits.file(RECORD_FILE), serde_json::to_vec(&body).unwrap()).unwrap();
    let landed = TaskDir::allocate(PROJECT).unwrap();
    let body = record(landed.id, "landed", json!({}));
    fs::write(landed.file(RECORD_FILE), serde_json::to_vec(&body).unwrap()).unwrap();
    let item: CheckItem = serde_json::from_value(json!({
        "id": "", "task": landed.id, "revision": 0, "commit": "abc", "check": "look at it",
        "pass_if": "it looks right", "state": "open", "unix_ms": 5,
    }))
    .unwrap();
    checks::change(PROJECT, |_| Ok(vec![item])).unwrap();
    // ^ One request nobody decided, one reply that asks and nobody answered.
    let request: ApprovalLine = serde_json::from_value(
        json!({ "kind": "request", "id": "", "action": "deploy", "unix_ms": 6 }),
    )
    .unwrap();
    store::append_approval(PROJECT, request).unwrap();
    let ask: ManagerEvent = serde_json::from_value(
        json!({ "kind": "reply", "id": 1, "to": [], "text": "Which one?", "asks": true }),
    )
    .unwrap();
    home.append(ask).unwrap();

    let you = needs::for_you(PROJECT, false);
    let mut kinds: Vec<&str> = you.iter().map(|item| item.kind).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["approval", "ask", "check", "waiting"], "{you:?}");
    assert!(you.iter().all(|item| item.chat == home.name()), "{you:?}");
    assert!(you
        .iter()
        .any(|item| item.source == YouSource::Task(waits.id)));

    let counts = needs::chat_counts(PROJECT, false);
    let own = counts[home.name()];
    assert_eq!(
        (own.needs, own.check),
        (
            you.iter().filter(|item| item.kind != "check").count(),
            you.iter().filter(|item| item.kind == "check").count()
        ),
        "{own:?} {you:?}"
    );
    assert_eq!((own.needs, own.check, own.open), (3, 1, 1), "{own:?}");
    let _ = fs::remove_dir_all(&state);
}
