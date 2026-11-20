//! A task host that lived through the chats store move writes `created_by: "manager"` once more
//! when it execs the new binary. The record must still load, as the move names that creator,
//! and the next save must write the new shape. Real files in a temp state folder, through the
//! store's own read and write. ref: docs/tasks.md "Chats"

use serde_json::{json, Value};
use std::fs;
use wsx_core::task::store::{self, TaskDir, RECORD_FILE};
use wsx_core::task::{Actor, TaskRecord, TaskState};

const PROJECT: &str = "demo-0a1b2c3d";

/// A record as the host before the move wrote it.
fn old_record(id: u32, created_by: &str) -> Value {
    json!({
        "id": id, "project": "demo", "project_id": PROJECT, "project_path": "/p",
        "default_branch": "main", "base": "abc", "branch": format!("wsx/t{id}"),
        "working_copy": "/p-t", "pane_id": null, "adapter": "claude", "model": "m",
        "checks": [], "agent_session": null, "state": "working", "revision": 0,
        "created_by": created_by, "created_unix_ms": 1, "updated_unix_ms": 2,
    })
}

fn load(created_by: &str) -> (TaskDir, TaskRecord, Value) {
    let dir = TaskDir::allocate(PROJECT).unwrap();
    let id = dir.id;
    fs::write(
        dir.file(RECORD_FILE),
        serde_json::to_vec_pretty(&old_record(id, created_by)).unwrap(),
    )
    .unwrap();
    let record = dir.read_record().unwrap();
    dir.write_record(&record).unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(dir.file(RECORD_FILE)).unwrap()).unwrap();
    (dir, record, saved)
}

#[test]
fn a_record_an_older_host_wrote_loads_and_saves_in_the_new_shape() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.work")
        .join(format!("legacy-creator-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    // SAFETY: this file has one test; nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    // The repo's main chat is named in home.json, as the move left it.
    store::set_main_chat(PROJECT, "projk-home").unwrap();
    let (_, record, saved) = load("manager");
    assert_eq!(record.created_by, Actor::Chat("projk-home".into()));
    assert_eq!(record.state, TaskState::Working);
    assert_eq!(saved["created_by"], "chat:projk-home");

    // Another repo's first chat is named for that repo.
    let (_, record, saved) = load("manager@Other Repo");
    assert_eq!(record.created_by, Actor::Chat("other-repo".into()));
    assert_eq!(saved["created_by"], "chat:other-repo");

    // A creator that is not a legacy one still fails to load, with the file named.
    let dir = TaskDir::allocate(PROJECT).unwrap();
    fs::write(
        dir.file(RECORD_FILE),
        serde_json::to_vec(&old_record(dir.id, "robot")).unwrap(),
    )
    .unwrap();
    let error = format!("{:#}", dir.read_record().unwrap_err());
    assert!(
        error.contains("cannot parse") && error.contains("unknown actor robot"),
        "{error}"
    );

    // Without a main chat (home.json) the creator names no chat: the record fails to load.
    store::clear_main_chat(PROJECT).unwrap();
    let dir = TaskDir::allocate(PROJECT).unwrap();
    fs::write(
        dir.file(RECORD_FILE),
        serde_json::to_vec(&old_record(dir.id, "manager")).unwrap(),
    )
    .unwrap();
    assert!(dir.read_record().is_err());

    let _ = fs::remove_dir_all(&state);
}
