//! A task that lands by itself and its "To check" items: the review job lands a ready task, the
//! worker's Human Verify items join the project's list with their captures, the owner passes,
//! fails or skips them. Real host binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const CHECKS: &str = "  checks:\n    - \"test ! -e broken.txt\"\n";

/// A project that lands a ready task by itself: the fixture's `land: { auto: false }` line goes.
fn auto_land(extra: &str) -> Fixture {
    let fixture = Fixture::with_tasks_config(extra);
    let config = fixture.root.join("project/wsx.config.yml");
    let text = fs::read_to_string(&config).expect("read the project config");
    fs::write(&config, text.replace("  land: { auto: false }\n", "")).expect("turn auto land on");
    fixture.git(&["commit", "-qam", "auto land"]);
    fixture
}

fn count(entries: &[Value], kind: &str) -> usize {
    entries.iter().filter(|entry| entry["kind"] == kind).count()
}

/// Wait for the review job to land the task; the last verdict is `land`.
fn reviewed(fixture: &Fixture, id: &str) -> Value {
    let task = fixture.wait_task(id, "the review lands the task", |task| {
        matches!(task["state"].as_str(), Some("landed" | "failed")).then(|| task.clone())
    });
    assert_eq!(task["state"], "landed", "{}", fixture.log(&task));
    let verdicts: Vec<Value> = fixture
        .entries(&task)
        .into_iter()
        .filter(|entry| entry["kind"] == "review")
        .map(|entry| entry["verdict"].clone())
        .collect();
    assert_eq!(verdicts.last(), Some(&json!("land")), "{verdicts:?}");
    task
}

/// A landed task closes itself and stays `landed`: its copy and branch go, its folder stays.
fn closed(fixture: &Fixture, id: &str) {
    fixture.wait_task(id, "the landed task closes", |task| {
        let copy = Path::new(task["record"]["working_copy"].as_str()?);
        let last = fixture.log(task).lines().last().map(str::to_string);
        // ^ Its copy and its branch go; the folder and the log, which ends `closed`, stay.
        (task["state"] == "landed"
            && !copy.exists()
            && last.is_some_and(|line| line.contains("\"kind\":\"closed\""))
            && fixture
                .git(&["branch", "--list", &format!("wsx/t{id}")])
                .is_empty())
        .then_some(())
    });
}

fn open_items(fixture: &Fixture) -> Vec<Value> {
    fixture.json(&["project", "check", "--json"])["open"]
        .as_array()
        .expect("open items")
        .clone()
}

fn items_of(fixture: &Fixture, id: &str) -> Vec<Value> {
    open_items(fixture)
        .into_iter()
        .filter(|item| item["task"].as_u64() == id.parse().ok())
        .collect()
}

fn on_main(fixture: &Fixture, path: &str) -> bool {
    fixture
        .command(Path::new("git"))
        .args(["cat-file", "-e", &format!("main:{path}")])
        .status()
        .expect("run git")
        .success()
}

/// `wsx` as the lead runs it.
fn as_chat(fixture: &Fixture, args: &[&str]) -> std::process::Output {
    fixture
        .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
        .env("WSX_TASK_ACTOR", "chat:demo")
        .args(args)
        .output()
        .expect("run wsx")
}

fn task_folder(task: &Value) -> PathBuf {
    Path::new(task["report"].as_str().expect("report"))
        .parent()
        .expect("task folder")
        .to_path_buf()
}

fn refusal(output: &std::process::Output) -> String {
    assert!(!output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stderr).to_string()
}

const PNG: &[u8] = b"\x89PNG";

// ref: docs/tasks.md "Review"
#[test]
fn an_auto_landed_task_puts_its_human_verify_items_on_the_to_check_list() {
    let fixture = auto_land(CHECKS);
    // ^ The task records main as its default branch at `new`; main is left during the turn.
    let hv = fixture.new_task("SLOW WORK z.txt HV");
    fixture.git(&["switch", "-q", "-c", "parking"]);
    let task = reviewed(&fixture, &hv);
    assert!(on_main(&fixture, "z.txt"));
    assert_eq!(
        fixture.git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        "parking"
    );
    assert_eq!(count(&fixture.entries(&task), "landed"), 1);
    let items = open_items(&fixture);
    assert_eq!(items.len(), 1, "{items:?}");
    let item = &items[0];
    assert_eq!(item["id"], "c1");
    assert_eq!(item["task"].to_string(), hv);
    assert_eq!(item["check"], "open the file");
    assert_eq!(item["pass_if"], "it has text");
    assert_eq!(item["state"], "open");
    assert_eq!(item["commit"], fixture.git(&["rev-parse", "main"]).as_str());
    assert_eq!(item["revision"], 1);
    assert_eq!(
        fixture.json(&["task", "show", &hv, "--json"])["to_check"][0]["id"],
        "c1"
    );
    let report = fs::read_to_string(task["report"].as_str().unwrap()).expect("report");
    assert!(
        report.contains("## To check\n\n- [ ] open the file (pass if: it has text)")
            && report.contains("State `landed`"),
        "{report}"
    );
    fixture.git(&["switch", "-q", "main"]);

    // ^ Two tasks on one file: the second land conflicts, the worker resolves it, and it lands by
    // itself without a second review.
    let a1 = fixture.new_task("WORK x.txt alpha HV");
    let a2 = fixture.new_task("WORK x.txt beta HV");
    let landed: Vec<Value> = [&a1, &a2].iter().map(|id| reviewed(&fixture, id)).collect();
    let conflicted: Vec<&Value> = landed
        .iter()
        .filter(|task| {
            fixture
                .entries(task)
                .iter()
                .any(|e| e["kind"] == "conflict")
        })
        .collect();
    assert_eq!(conflicted.len(), 1);
    assert_eq!(fixture.git(&["show", "main:x.txt"]), "resolved");
    assert_eq!(count(&fixture.entries(conflicted[0]), "review"), 1);
    assert_eq!(open_items(&fixture).len(), 3);
}

// ref: docs/tasks.md "To check"
#[test]
fn an_item_keeps_its_capture_and_a_bare_item_goes_back_to_the_worker_once() {
    let fixture = auto_land(CHECKS);
    // ^ An item that names a command and one about a page without a capture go back to the worker
    // once, in one message; its next result has none.
    let shot = fixture.new_task("WORK cap.txt HV CAPTURE");
    let command = fixture.new_task("WORK cmd.txt CMDHV BAREHV");
    reviewed(&fixture, &shot);
    reviewed(&fixture, &command);
    // ^ command branched before shot landed: its land rebases, reruns the checks and lands when
    // they pass (docs/tasks.md "Landing" 3).
    let command_log = fixture.entries(&fixture.json(&["task", "show", &command, "--json"]));
    assert!(count(&command_log, "revision") >= 2, "{command_log:?}");
    closed(&fixture, &shot);
    closed(&fixture, &command);
    let command_log = fixture.entries(&fixture.json(&["task", "show", &command, "--json"]));
    assert_eq!(count(&command_log, "result"), 2);
    // ^ A review job's message quotes the task's messages: not a send-back of its own.
    let sent: Vec<String> = fixture
        .traces()
        .iter()
        .filter_map(|trace| trace["message"].as_str().map(str::to_string))
        .filter(|text| text.contains("yours to prove") && !text.starts_with("Review round"))
        .collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].contains("wsx task list") && sent[0].contains("the page reads well"));
    let kept: Vec<Value> = open_items(&fixture)
        .into_iter()
        .filter(|item| [&shot, &command].contains(&&item["task"].to_string()))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["task"].to_string(), shot);
    assert_eq!(kept[0]["where"], "/#/x");
    assert_eq!(kept[0]["captures"], json!(["captures/r1-1-1.png"]));
    let folder = task_folder(&fixture.json(&["task", "show", &shot, "--json"]));
    let stored = folder.join("captures/r1-1-1.png");
    assert!(fs::read(&stored).unwrap().starts_with(PNG), "{stored:?}");
    let mode = fs::metadata(stored.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "{mode:o}");
}

// ref: docs/tasks.md "To check"
#[test]
fn video_and_several_captures_are_kept_as_files_and_big_or_fake_ones_are_dropped() {
    let fixture = auto_land(CHECKS);
    // ^ A video capture (MP4 by its `ftyp` box) and its poster are kept as r<k>-<n>.mp4 and
    // r<k>-<n>.poster.png; a video over 25 MiB and a text file named .mp4 are dropped, their
    // items kept without a capture.
    let video = fixture.new_task("WORK v.txt HV VIDEO BIGVIDEO NOTVIDEO");
    reviewed(&fixture, &video);
    closed(&fixture, &video);
    let folder = task_folder(&fixture.json(&["task", "show", &video, "--json"]));
    let clip = fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/fixtures/check-clip.mp4"),
    )
    .expect("read the clip");
    assert_eq!(fs::read(folder.join("captures/r1-1-1.mp4")).unwrap(), clip);
    assert!(fs::read(folder.join("captures/r1-1.poster.png"))
        .unwrap()
        .starts_with(PNG));
    let names = |folder: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder.join("captures"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&folder), ["r1-1-1.mp4", "r1-1.poster.png"]);
    let rows: Vec<Value> = fs::read_to_string(folder.parent().unwrap().join("checks.jsonl"))
        .expect("checks.jsonl")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["task"].as_u64() == video.parse().ok())
        .map(|row| json!({"check": row["check"], "captures": row["captures"], "poster": row["poster"]}))
        .collect();
    assert_eq!(
        rows,
        [
            json!({"check": "open the file", "captures": ["captures/r1-1-1.mp4"], "poster": "captures/r1-1.poster.png"}),
            json!({"check": "the long video plays", "captures": null, "poster": null}),
            json!({"check": "the text video plays", "captures": null, "poster": null}),
        ]
    );

    // ^ Several captures: one item keeps three files as r<k>-<n>-<m>.png and lists three in
    // `wsx project check --json`. An item whose words name an image file it has no capture of is
    // reported in the log.
    let multi = fixture.new_task("WORK m.txt CAPTURES3 NAMEDHV");
    let task = reviewed(&fixture, &multi);
    let said = "a To check item names tasks-390x844-home-dark.png but has no capture of it";
    assert!(
        fixture
            .entries(&task)
            .iter()
            .any(|entry| entry.to_string().contains(said)),
        "{}",
        fixture.log(&task)
    );
    closed(&fixture, &multi);
    let three: Vec<Value> = items_of(&fixture, &multi)
        .into_iter()
        .filter(|item| item["check"] == "the three sizes")
        .collect();
    assert_eq!(three.len(), 1);
    assert_eq!(
        three[0]["captures"],
        json!([
            "captures/r1-1-1.png",
            "captures/r1-1-2.png",
            "captures/r1-1-3.png"
        ])
    );
    let folder = task_folder(&fixture.json(&["task", "show", &multi, "--json"]));
    assert_eq!(names(&folder), ["r1-1-1.png", "r1-1-2.png", "r1-1-3.png"]);
}

// ref: docs/tasks.md "Review"
#[test]
fn a_project_with_no_checks_leaves_a_ready_task_for_a_land_by_hand() {
    // ^ Nothing proves the work, so no review job runs: a ready task waits.
    let fixture = auto_land("");
    let id = fixture.new_task("WORK n.txt");
    fixture.wait_task(&id, "ready", |task| {
        (task["state"] == "ready").then_some(())
    });
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(
        fixture.json(&["task", "show", &id, "--json"])["state"],
        "ready"
    );
    let landed = fixture.json(&["task", "land", &id, "--json"]);
    assert_eq!(landed["state"], "landed");
    assert!(on_main(&fixture, "n.txt"));
}

// ref: docs/tasks.md "To check"
#[test]
fn the_owner_passes_and_fails_items_and_a_fail_starts_a_linked_fix_task() {
    let fixture = auto_land("");
    // ^ Landed by hand one after the other, so the items are c1, c2 and c3.
    for file in ["a", "b", "c"] {
        let id = fixture.new_task(&format!("WORK {file}.txt HV"));
        fixture.wait_task(&id, "ready", |task| {
            (task["state"] == "ready").then_some(())
        });
        let landed = fixture.json(&["task", "land", &id, "--json"]);
        assert_eq!(landed["state"], "landed", "{landed}");
    }
    assert_eq!(open_items(&fixture).len(), 3);
    let passed = fixture.json(&["project", "check", "pass", "c1", "--json"]);
    let states: Vec<&Value> = passed["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["state"])
        .collect();
    assert_eq!(states, ["passed"]);
    assert_eq!(passed["open"], 2);
    let again = fixture.wsx(&["project", "check", "pass", "c1", "--json"]);
    assert!(refusal(&again).contains("already passed"));
    let missing = fixture.wsx(&["project", "check", "pass", "c99"]);
    assert!(refusal(&missing).contains("no item c99"));
    // ^ The lead may not decide an item for the owner.
    let by_chat = as_chat(&fixture, &["project", "check", "pass", "--all"]);
    assert!(refusal(&by_chat).contains("for the owner"));
    let blank = fixture.wsx(&["project", "check", "fail", "c2", "--note", " "]);
    assert!(refusal(&blank).contains("needs --note"));

    let failed = fixture.json(&[
        "project",
        "check",
        "fail",
        "c2",
        "--note",
        "the text is cut off",
        "--json",
    ]);
    let item = &failed["changed"][0];
    assert_eq!(item["state"], "failed");
    assert_eq!(item["note"], "the text is cut off");
    let fix = item["fix_task"].as_u64().expect("a fix task").to_string();
    let fixing = fixture.json(&["task", "show", &fix, "--json"]);
    assert_eq!(fixing["record"]["fixes"], "c2");
    assert_eq!(fixing["title"], "Fix: open the file");
    let log = fixture.entries(&fixing);
    let prompt = log
        .iter()
        .find(|entry| entry["mode"] == "prompt")
        .and_then(|entry| entry["text"].as_str())
        .expect("the fix prompt");
    let origin = fixture.json(&["task", "show", &item["task"].to_string(), "--json"]);
    assert!(
        prompt.contains("the text is cut off")
            && prompt.contains(origin["report"].as_str().unwrap())
            && prompt.contains("Pass if: it has text"),
        "{prompt}"
    );
    fixture.wait_task(&fix, "the fix task settles", |task| {
        matches!(task["state"].as_str(), Some("ready" | "landed" | "failed")).then_some(())
    });
    let open: Vec<String> = open_items(&fixture)
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(open, ["c3"]);
    // ^ No checks prove the fix, so it waits ready; it has no commits and closes plainly.
    let closed = fixture.wsx(&["task", "close", &fix]);
    assert!(closed.status.success(), "{closed:?}");
}
