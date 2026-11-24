//! What `wsx chat ledger` reports from git and the task logs alone: the project's other
//! worktrees with their flags, one parse per task log, and a note when git does not answer.
//! Real host binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn ledger(fixture: &Fixture) -> Value {
    fixture.json(&["chat", "ledger", "--json"])
}

/// One settled task, so the ledger has an open row and a log.
fn a_ready_task(fixture: &Fixture) {
    fixture.json(&["task", "new", "NOOP", "--json"]);
    fixture.wait("the task settles", |task| {
        (task["state"] == "ready").then_some(())
    });
}

// ref: docs/tasks.md "Ledger"
#[test]
fn the_ledger_lists_stray_worktrees_with_flags_from_git_alone() {
    let fixture = Fixture::new();
    let at = |name: &str| fixture.root.join(name);
    let path = |name: &str| at(name).to_str().unwrap().to_string();
    for name in ["stray-merged", "stray-dirty", "stray-gone"] {
        fixture.git(&["worktree", "add", "-q", "-b", name, &path(name)]);
    }
    let dirty = path("stray-dirty");
    fs::write(at("stray-dirty/new.txt"), "ahead\n").unwrap();
    fixture.git(&["-C", &dirty, "add", "new.txt"]);
    fixture.git(&["-C", &dirty, "commit", "-q", "-m", "ahead"]);
    fs::write(at("stray-dirty/README"), "edited\n").unwrap();
    fs::remove_dir_all(at("stray-gone")).unwrap();

    let answer = ledger(&fixture);
    let strays = answer["stray_worktrees"].as_array().expect("stray_worktrees");
    // ^ The sweep's rule (stale::loss): uncommitted files and commits neither merged nor pushed.
    let flags = |name: &str| {
        let row = strays
            .iter()
            .find(|row| Path::new(row["path"].as_str().unwrap()).file_name().unwrap() == name)
            .unwrap_or_else(|| panic!("no stray {name} in {strays:?}"));
        ["branch", "dirty", "unmerged", "missing", "sessions"].map(|key| row[key].clone())
    };
    assert_eq!(strays.len(), 3, "{strays:?}");
    assert_eq!(flags("stray-merged"), [json!("stray-merged"), json!(0), json!(0), json!(false), json!(0)]);
    assert_eq!(flags("stray-dirty"), [json!("stray-dirty"), json!(1), json!(1), json!(false), json!(0)]);
    assert_eq!(flags("stray-gone"), [json!("stray-gone"), json!(0), json!(0), json!(true), json!(0)]);
    assert_eq!(answer["stray_worktrees_omitted"], 0);

    for name in ["stray-merged", "stray-dirty"] {
        fixture.git(&["worktree", "remove", "--force", &path(name)]);
    }
    fixture.git(&["worktree", "prune"]);
    fixture.git(&["branch", "-D", "stray-merged", "stray-dirty", "stray-gone"]);
    assert_eq!(ledger(&fixture)["stray_worktrees"], json!([]));
}

// ref: docs/tasks.md "Agents"
#[test]
fn a_ledger_build_parses_each_task_log_once() {
    let fixture = Fixture::new();
    a_ready_task(&fixture);
    let traced = fixture
        .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
        .env("WSX_TRACE_READ_LOG", "1")
        .args(["chat", "ledger", "--json"])
        .output()
        .expect("run wsx");
    assert!(traced.status.success(), "{traced:?}");
    let stderr = String::from_utf8_lossy(&traced.stderr);
    let logs: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("wsx-trace read_log "))
        .filter(|path| path.ends_with("/log.jsonl") && !path.contains("/chats/"))
        .collect();
    let mut unique = logs.clone();
    unique.sort();
    unique.dedup();
    assert!(!logs.is_empty() && logs.len() == unique.len(), "{logs:?}");
}

/// The first directory of `PATH` that holds `name`.
fn on_path(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|file| file.is_file())
        .unwrap_or_else(|| panic!("no {name} on PATH"))
}

// ref: docs/tasks.md "Ledger"
#[test]
fn a_git_that_hangs_leaves_a_note_and_keeps_the_rows() {
    let fixture = Fixture::new();
    a_ready_task(&fixture);
    let hang = fixture.root.join("hang-git");
    fs::create_dir(&hang).unwrap();
    // ^ `git status` sleeps; every other git call goes to the real one.
    let shim = hang.join("git");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" status \"*) exec sleep 60;; esac\nexec {} \"$@\"\n",
            on_path("git").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        hang.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let started = Instant::now();
    let hung = fixture
        .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
        .env("PATH", format!("{}:{path}", fixture.root.join("bin").display()))
        .args(["chat", "ledger", "--json"])
        .output()
        .expect("run wsx");
    let took = started.elapsed();
    assert!(hung.status.success(), "{hung:?}");
    assert!(took > Duration::from_secs(9) && took < Duration::from_secs(40), "{took:?}");
    let hung: Value = serde_json::from_slice(&hung.stdout).expect("ledger JSON");
    let notes = hung["notes"].as_array().expect("notes");
    assert!(
        notes.len() == 1 && notes[0].as_str().unwrap().contains("git did not answer in 10 s"),
        "{notes:?}"
    );
    assert_eq!(hung["main"]["branch"], "main");
    assert!(hung["main"]["changed_files"].is_null(), "{}", hung["main"]);
    let normal = ledger(&fixture);
    let ids = |answer: &Value| -> Vec<Value> {
        answer["open"].as_array().unwrap().iter().map(|row| row["id"].clone()).collect()
    };
    assert_eq!(ids(&hung), ids(&normal));
    assert!(!ids(&normal).is_empty());
    assert!(normal.get("notes").is_none(), "{normal}");
}
