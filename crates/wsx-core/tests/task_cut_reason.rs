//! A task that waits on the owner with the reason an older host cut to the first line of the
//! worker's summary gets the whole summary back when its host loads it, once; a reason that is
//! not that cut line stays. The status keeps the whole question for the page and one line for
//! its sentence. Real files in a temp state folder, through the store.

use serde_json::{json, Value};
use std::fs;
use wsx_core::task::status;
use wsx_core::task::store::{TaskDir, RECORD_FILE};
use wsx_core::task::{LogEvent, TaskRecord};

const PROJECT: &str = "demo-0a1b2c3d";
const SUMMARY: &str = "Three questions for you:\n1. Which type?\n2. Keep the name?\n3. Land it?";

fn task(on: &str, reason: &str, summary: &str) -> (TaskDir, TaskRecord) {
    let dir = TaskDir::allocate(PROJECT).unwrap();
    let record = json!({
        "id": dir.id, "project": "demo", "project_id": PROJECT, "project_path": "/p",
        "default_branch": "main", "base": "abc", "branch": format!("wsx/t{}", dir.id),
        "working_copy": "/p-t", "pane_id": null, "adapter": "claude", "model": "m",
        "checks": [], "agent_session": null, "state": "waiting", "revision": 0,
        "created_by": "owner", "created_unix_ms": 1, "updated_unix_ms": 2,
        "waiting": { "on": on, "reason": reason },
    });
    fs::write(dir.file(RECORD_FILE), serde_json::to_vec(&record).unwrap()).unwrap();
    dir.append(LogEvent::Result {
        status: "blocked".into(),
        summary: summary.into(),
        usage: None,
        denials: vec![],
        step: None,
        trigger: None,
        reexec: false,
    })
    .unwrap();
    let record = dir.read_record().unwrap();
    (dir, record)
}

#[test]
fn a_cut_question_is_restored_once_from_the_result() {
    let state = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.work")
        .join(format!("cut-reason-{}", std::process::id()));
    let _ = fs::remove_dir_all(&state);
    fs::create_dir_all(&state).unwrap();
    let state = state.canonicalize().unwrap();
    // SAFETY: this file has one test; nothing else reads the environment meanwhile.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };

    // The older host's record: the first line only.
    let (dir, mut record) = task("owner", "Three questions for you:", SUMMARY);
    assert!(dir.restore_question(&mut record));
    assert_eq!(record.waiting.as_ref().unwrap().reason, SUMMARY);
    // Written back, the record loads whole and a second load changes nothing.
    dir.write_record(&record).unwrap();
    let mut again = dir.read_record().unwrap();
    assert!(!dir.restore_question(&mut again));
    assert_eq!(again.waiting.as_ref().unwrap().reason, SUMMARY);

    // The sentence has one line; the question for the page has all of them.
    let shown = status::status(&dir, &record, &Default::default(), 10_000);
    assert!(!shown.sentence.contains('\n'), "{}", shown.sentence);
    assert_eq!(shown.ask().as_deref(), Some(SUMMARY));
    assert!(
        shown.rest().starts_with("Answer the manager"),
        "{}",
        shown.rest()
    );
    let saved: Value = serde_json::from_slice(&fs::read(dir.file(RECORD_FILE)).unwrap()).unwrap();
    assert_eq!(saved["waiting"]["reason"], SUMMARY);

    // A pause is restored the same way.
    let (dir, mut record) = task("paused", "Three questions for you:", SUMMARY);
    assert!(dir.restore_question(&mut record));

    // Neither a reason the worker did not say (a stop), nor a one-line summary, nor another wait.
    let (dir, mut record) = task("owner", "stopped by request", SUMMARY);
    assert!(!dir.restore_question(&mut record));
    let (dir, mut record) = task("owner", "Which layout?", "Which layout?");
    assert!(!dir.restore_question(&mut record));
    let (dir, mut record) = task("disk", "Three questions for you:", SUMMARY);
    assert!(!dir.restore_question(&mut record));
}
