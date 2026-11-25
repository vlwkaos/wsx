//! A deploy or release by hand runs in a temporary copy of a default-branch commit and leaves a
//! record; open To check items stop only public output (docs/tasks.md "Deploy and release",
//! "To check"). Real host binary, fake Claude (`scripts/fake-claude-stream.py`) for the tasks
//! that land and give the items.

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A deploy that logs its commit and folder beside the project, and a release that fails.
fn deploy_fixture() -> Fixture {
    Fixture::with_tasks_config(
        "  deploy:\n    command: 'printf \"%s %s\\n\" \"$WSX_DEPLOY_COMMIT\" \"$(pwd)\" >> {root}/deploy.log'\n\
         \x20 release:\n    command: 'seq 1 60; echo \"release broke here: Permission denied (publickey).\" >&2; exit 3'\n",
    )
}

fn until<T>(label: &str, mut test: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = test() {
            return value;
        }
        assert!(Instant::now() < deadline, "{label} timeout");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A task that lands by hand; with `HV` in its prompt it leaves one To check item.
fn land_by_hand(fixture: &Fixture, prompt: &str) -> String {
    let id = fixture.new_task(prompt);
    fixture.wait_task(&id, "ready", |task| {
        (task["state"] == "ready").then_some(())
    });
    let landed = fixture.json(&["task", "land", &id, "--json"]);
    assert_eq!(landed["state"], "landed", "{landed}");
    id
}

fn deployed(fixture: &Fixture) -> Value {
    fixture.json(&["project", "deployed", "--json"])
}

/// demo's store folder (docs/tasks.md "Store"), once wsx made it.
fn store(fixture: &Fixture) -> Option<PathBuf> {
    fs::read_dir(fixture.root.join("state/wsx/tasks"))
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("demo-"))
        })
}

fn records(fixture: &Fixture) -> Vec<Value> {
    store(fixture)
        .and_then(|store| fs::read_to_string(store.join("deploys.jsonl")).ok())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// No deploy or release holds `deploy.lock`: its run has ended.
fn lock_free(fixture: &Fixture) -> bool {
    store(fixture)
        .and_then(|store| {
            fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(store.join("deploy.lock"))
                .ok()
        })
        .is_some_and(|lock| lock.try_lock().is_ok())
}

/// Start a deploy or release by hand and wait for its detached run to end: its record is
/// written and its lock is free, so no run outlives the test. The record has the commit the
/// answer named.
fn started(fixture: &Fixture, args: &[&str]) -> Value {
    let before = records(fixture).len();
    let mut all = vec!["project"];
    all.extend(args);
    all.push("--json");
    let answer = fixture.json(&all);
    assert!(
        answer["started"] == true
            && answer["message"]
                .as_str()
                .is_some_and(|m| m.contains(" started; ")),
        "{answer}"
    );
    let record = until("the detached run ends", || {
        let records = records(fixture);
        (records.len() > before && lock_free(fixture)).then(|| records[records.len() - 1].clone())
    });
    assert_eq!(answer["commit"], record["commit"], "{answer} {record}");
    answer
}

fn deploy_log(fixture: &Fixture) -> PathBuf {
    fixture.root.join("deploy.log")
}

/// The copies of a deploy or release that are still on disk.
fn copies(fixture: &Fixture) -> Vec<String> {
    fixture
        .git(&["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| path.contains("project-deploy-") || path.contains("project-release-"))
        .map(str::to_string)
        .collect()
}

// ref: docs/tasks.md "Deploy and release"
#[test]
fn a_deploy_without_a_command_is_refused_and_runs_nothing() {
    let fixture = Fixture::new();
    let said = fixture.refusal(&["project", "deploy"]);
    assert!(
        said.contains("tasks.deploy.command is not set") && said.contains("nothing was run"),
        "{said}"
    );
}

// ref: docs/tasks.md "To check"
#[test]
fn open_items_stop_a_release_and_a_public_deploy_but_not_a_plain_deploy() {
    let fixture = deploy_fixture();
    land_by_hand(&fixture, "WORK w0.txt HV");
    assert!(deployed(&fixture)["deploy"]["commit"].is_null());
    let said = fixture.refusal(&["project", "release", "--json"]);
    assert!(
        said.contains("1 item(s) to check are open") && said.contains("Nothing was run"),
        "{said}"
    );
    // ^ A deploy marked public stops too; the marker goes before the commit that does not have it.
    let config = fixture.root.join("project/wsx.config.yml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("  deploy:\n", "  deploy:\n    public: true\n"),
    )
    .unwrap();
    assert!(fixture
        .refusal(&["project", "deploy"])
        .contains("to check are open"));
    fixture.git(&["checkout", "--", "wsx.config.yml"]);
    assert!(!deploy_log(&fixture).exists());
    let plain = started(&fixture, &["deploy"]);
    assert!(
        plain["message"]
            .as_str()
            .unwrap()
            .ends_with("; 1 to check on the deployed site"),
        "{plain}"
    );
}

// ref: docs/tasks.md "To check"
#[test]
fn a_skip_records_skipped_starts_no_task_and_is_the_owners_alone() {
    let fixture = deploy_fixture();
    land_by_hand(&fixture, "WORK w0.txt HV");
    let tasks = |f: &Fixture| {
        f.json(&["task", "list", "--all", "--json"])["tasks"]
            .as_array()
            .unwrap()
            .len()
    };
    let before = tasks(&fixture);
    let as_chat = |args: &[&str]| {
        fixture
            .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .env("WSX_TASK_ACTOR", "chat:demo")
            .args(args)
            .output()
            .expect("run wsx")
    };
    let skip = as_chat(&["project", "check", "skip", "--all"]);
    assert!(!skip.status.success());
    assert!(String::from_utf8_lossy(&skip.stderr).contains("check skip is for the owner"));
    // ^ A dismiss of a question is the owner's too (docs/tasks.md "Questions" under "Chat host").
    let dismiss = as_chat(&["chat", "dismiss", "1"]);
    assert!(!dismiss.status.success());
    assert!(String::from_utf8_lossy(&dismiss.stderr).contains("dismiss is for the owner"));
    let skipped = fixture.json(&["project", "check", "skip", "--all", "--json"]);
    let states: Vec<&Value> = skipped["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["state"])
        .collect();
    assert_eq!(states, ["skipped"], "{skipped}");
    assert_eq!(skipped["open"], 0);
    assert_eq!(skipped["message"], "1 skipped. 0 to check");
    let item = &skipped["changed"][0];
    assert!(
        item["fix_task"].is_null() && item["note"].is_null(),
        "{item}"
    );
    let id = item["id"].as_str().unwrap();
    assert!(fixture
        .refusal(&["project", "check", "pass", id])
        .contains("already skipped"));
    let shown = fixture.json(&["task", "show", &item["task"].to_string(), "--json"]);
    let row = shown["to_check"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap();
    assert_eq!(row["state"], "skipped");
    assert_eq!(tasks(&fixture), before);
    // ^ With no item open, a release runs (and fails, as this command does).
    started(&fixture, &["release"]);
    until("the release is recorded", || {
        (deployed(&fixture)["release"]["failed"]["exit"] == 3).then_some(())
    });
}

// ref: docs/tasks.md "Deploy and release"
#[test]
fn a_deploy_runs_in_a_copy_of_the_tip_and_the_next_one_removes_an_orphaned_copy() {
    let fixture = deploy_fixture();
    // ^ A release whose process was killed left its copy; the next deploy removes it. A pid
    // above macOS's 99998 is never alive.
    let orphan = fixture.root.join("project-release-999999");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        "--detach",
        orphan.to_str().unwrap(),
    ]);
    // ^ A worktree on a branch with the same name shape is someone's work: it stays.
    let owned = fixture.root.join("project-deploy-999998");
    fixture.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "owned-999998",
        owned.to_str().unwrap(),
    ]);
    let tip = fixture.git(&["rev-parse", "main"]);
    started(&fixture, &["deploy"]);
    until("the deploy is recorded", || {
        (deployed(&fixture)["deploy"]["commit"] == tip.as_str()).then_some(())
    });
    let listed = fixture.git(&["worktree", "list"]);
    assert!(
        !orphan.exists() && !listed.contains(orphan.to_str().unwrap()),
        "{listed}"
    );
    assert!(
        owned.is_dir() && listed.contains(owned.to_str().unwrap()),
        "{listed}"
    );
    fixture.git(&["worktree", "remove", "--force", owned.to_str().unwrap()]);
    fixture.git(&["branch", "-D", "owned-999998"]);
    let line = fs::read_to_string(deploy_log(&fixture)).expect("the deploy command's log");
    let (commit, copy) = line.trim().split_once(' ').expect("commit and folder");
    assert_eq!(commit, tip);
    assert!(Path::new(copy) != fixture.root.join("project"), "{copy}");
    // ^ The temporary copy is gone once the run ends; only the main checkout stays.
    until("the copy is removed", || {
        (!Path::new(copy).exists() && copies(&fixture).is_empty()).then_some(())
    });
    assert_eq!(
        fixture
            .git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    assert_eq!(deployed(&fixture)["deploy"]["behind"], 0);

    // ^ Each land puts main one commit ahead of the deployed one; a deploy of an older commit,
    // not the deployed one, records that commit.
    land_by_hand(&fixture, "WORK w1.txt");
    assert_eq!(deployed(&fixture)["deploy"]["behind"], 1);
    land_by_hand(&fixture, "WORK w2.txt");
    land_by_hand(&fixture, "WORK w3.txt");
    let older = fixture.git(&["rev-parse", "main~2"]);
    assert_ne!(older, tip);
    started(&fixture, &["deploy", "--commit", &older]);
    let status = deployed(&fixture);
    assert_eq!(status["deploy"]["commit"], older.as_str(), "{status}");
    assert_eq!(status["deploy"]["behind"], 2, "{status}");
}

// ref: docs/tasks.md "Deploy and release"
#[test]
fn a_failed_release_is_recorded_with_the_end_of_its_output_and_the_ledger_shows_it() {
    let fixture = deploy_fixture();
    let tip = fixture.git(&["rev-parse", "main"]);
    started(&fixture, &["release"]);
    let status = until("the release is recorded", || {
        let status = deployed(&fixture);
        (status["release"]["failed"]["exit"] == 3).then_some(status)
    });
    let mut release = status["release"].clone();
    let failed = release.as_object_mut().unwrap().remove("failed").unwrap();
    assert_eq!(
        release,
        serde_json::json!({"commit": null, "failed_since": 1, "configured": true}),
        "{status}"
    );
    assert_eq!(failed["commit"], tip.as_str());
    // ^ The last 40 lines: the command's own end, then a line that says which ssh agent was asked
    // (or why none) as a refused push does.
    let lines: Vec<&str> = failed["output"].as_str().unwrap().lines().collect();
    let (agent_words, printed) = lines.split_last().unwrap();
    let expected: Vec<String> = (23..=60)
        .map(|n| n.to_string())
        .chain(["release broke here: Permission denied (publickey).".to_string()])
        .collect();
    assert_eq!(printed, expected, "{failed}");
    assert!(agent_words.starts_with("ssh "), "{agent_words}");
    let log = fs::read_dir(fixture.root.join("state/wsx/tasks"))
        .unwrap()
        .filter_map(|dir| fs::read_to_string(dir.unwrap().path().join("follow-up.log")).ok())
        .collect::<String>();
    assert!(log.contains("release broke here"), "{log}");
    assert_eq!(
        fixture.json(&["chat", "ledger", "--json"])["deploys"]["release"]["failed"],
        failed
    );
}
