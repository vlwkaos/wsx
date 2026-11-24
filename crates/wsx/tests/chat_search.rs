//! `wsx chat search` over the chat logs and task logs of every project: ranking, the quote a hit
//! carries, Korean words, the digest a rebuild drops, and a task's story (`--task`).
//! ref: docs/tasks.md "Search"

#![cfg(target_os = "macos")]
// ^ The fixture's config is the macOS one (`dirs` reads it from HOME).

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::fs;

const T0: u64 = 1_700_100_000_000;

/// A second project `third` with a chat log and two task logs, written as the host writes them.
struct Third {
    chat_log: std::path::PathBuf,
    landed_log: std::path::PathBuf,
}

fn lines(values: &[Value]) -> String {
    values.iter().map(|value| format!("{value}\n")).collect()
}

fn third(fixture: &Fixture) -> Third {
    let path = fixture.root.join("third");
    fs::create_dir_all(&path).expect("create third");
    let registry = fixture
        .root
        .join("home/Library/Application Support/wsx/config-v2.toml");
    let mut config = fs::read_to_string(&registry).expect("read the config");
    config += &format!(
        "[[projects]]\nname = \"third\"\npath = {:?}\n",
        path.to_str().unwrap()
    );
    fs::write(&registry, config).expect("write the config");
    let id = wsx_core::task::store::project_id("third", &path);
    let tasks = fixture.root.join("state/wsx/tasks");
    let chat = tasks.join("chats/third");
    fs::create_dir_all(&chat).expect("create the chat");
    fs::write(chat.join("chat.json"), json!({"repos": [id]}).to_string()).unwrap();
    let store = tasks.join(&id);
    fs::create_dir_all(&store).expect("create the store");
    fs::write(
        store.join("home.json"),
        json!({"chat": "third"}).to_string(),
    )
    .unwrap();
    let task = |n: u32, state: &str, revision: u32, events: Vec<Value>| {
        let folder = store.join(n.to_string());
        fs::create_dir_all(&folder).unwrap();
        let record = json!({
            "id": n, "project": "third", "project_id": id, "project_path": path,
            "default_branch": "main", "base": "abc", "branch": format!("wsx/t{n}"),
            "working_copy": path, "pane_id": null, "adapter": "claude", "model": "m",
            "checks": [], "agent_session": null, "state": state, "revision": revision,
            "created_unix_ms": T0, "updated_unix_ms": T0,
        });
        fs::write(folder.join("task.json"), record.to_string()).unwrap();
        fs::write(folder.join("log.jsonl"), lines(&events)).unwrap();
        folder.join("log.jsonl")
    };
    let mut landed = vec![
        json!({"unix_ms": T0, "kind": "created", "base": "abc", "branch": "wsx/t1"}),
        json!({"unix_ms": T0 + 1000, "kind": "message", "mode": "prompt", "text": "Fix the quolloquy parser"}),
    ];
    landed.extend((0..3).map(
        |i| json!({"unix_ms": T0 + 2000 + i, "kind": "agent", "text": format!("tool step {i}")}),
    ));
    landed.extend([
        json!({"unix_ms": T0 + 3000, "kind": "result", "status": "done", "summary": "First pass: the parser is fixed."}),
        json!({"unix_ms": T0 + 4000, "kind": "message", "mode": "add", "text": "Also keep the zymurgy case small"}),
        json!({"unix_ms": T0 + 5000, "kind": "checks", "results": [{"command": "cargo test", "exit": 1, "timed_out": false, "output": ""}]}),
        json!({"unix_ms": T0 + 6000, "kind": "revision", "revision": 2, "commit": "cd67238e1f90aaaa"}),
        json!({"unix_ms": T0 + 7000, "kind": "landed", "commit": "cd67238e1f90aaaa"}),
    ]);
    let landed_log = task(1, "landed", 2, landed);
    task(
        2,
        "closed",
        1,
        vec![
            json!({"unix_ms": T0, "kind": "created", "base": "abc", "branch": "wsx/t2"}),
            json!({"unix_ms": T0 + 1000, "kind": "message", "mode": "prompt", "text": "Try the gizmo idea"}),
            json!({"unix_ms": T0 + 2000, "kind": "message", "mode": "steer", "text": "Drop the gizmo, it is a dead end"}),
            json!({"unix_ms": T0 + 3000, "kind": "closed", "discarded": true}),
        ],
    );
    Third {
        chat_log: chat.join("log.jsonl"),
        landed_log,
    }
}

fn search(fixture: &Fixture, args: &[&str]) -> Value {
    let mut argv = vec!["chat", "search", "--json"];
    argv.extend(args);
    fixture.json(&argv)
}

fn references(found: &Value) -> Vec<String> {
    found["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| hit["reference"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn search_ranks_quotes_and_drops_what_a_rebuild_no_longer_finds() {
    let fixture = Fixture::new();
    let third = third(&fixture);
    fs::write(&third.chat_log, lines(&[
        json!({"unix_ms": 1_700_000_000_000u64, "kind": "message", "id": 1, "from": "owner",
               "text": "> an older line\n\nThe zebra ledger rule is decided: marmalade stays out of it."}),
        json!({"unix_ms": 1_700_000_001_000u64, "kind": "reply", "id": 2, "to": [1], "text": "Noted: the zebra ledger rule."}),
        json!({"unix_ms": 1_700_000_002_000u64, "kind": "reply", "id": 3, "to": [1], "text": "zebra quiet", "quiet": true}),
        json!({"unix_ms": 1_700_000_003_000u64, "kind": "message", "id": 4, "from": "events", "text": "zebra event"}),
        json!({"unix_ms": 1_700_000_004_000u64, "kind": "agent", "text": "zebra tool step"}),
    ])).unwrap();

    // ^ The owner's message ranks first and quotes by id; a reply is found but has no quote; a
    // quiet reply, an event and a tool step are not searched.
    let found = search(&fixture, &["--all", "zebra", "marmalade"]);
    let hits = found["hits"].as_array().expect("hits");
    assert_eq!(hits[0]["reference"], "third:1", "{found}");
    assert_eq!(
        hits[0]["line"],
        "The zebra ledger rule is decided: marmalade stays out of it."
    );
    assert!(
        hits.iter()
            .any(|hit| hit["reference"] == "third:2" && hit.get("quote").is_none()),
        "{found}"
    );
    assert!(
        !references(&found)
            .iter()
            .any(|r| r == "third:3" || r == "third:4"),
        "{found}"
    );
    let top = &hits[0];
    assert_eq!(top["slug"], top["reference"]);
    assert_eq!(top["status"], "active");
    assert!(top["excerpt"].as_str().unwrap().contains("zebra ledger"));
    assert!(top["path"].as_str().unwrap().ends_with("log.jsonl"));
    assert_eq!(found["projects"], 2, "{found}");
    // ^ A quoted line is not the message's own words; case does not matter.
    assert!(!search(&fixture, &["--all", "older"])
        .to_string()
        .contains("an older line"));
    assert!(references(&search(&fixture, &["--all", "ZEBRA"])).contains(&"third:1".into()));

    // ^ The log is replaced (a roll deleted the old file): the digest still holds the old words, a
    // rebuild drops them. Pairs of characters are Korean words: a particle still matches.
    fs::write(
        &third.chat_log,
        lines(&[json!({"unix_ms": 1_700_000_005_000u64, "kind": "message",
        "id": 5, "from": "owner", "text": "배포 결정은 월요일로 합니다"})]),
    )
    .unwrap();
    assert_eq!(
        search(&fixture, &["--all", "결정을"])["hits"][0]["reference"],
        "third:5"
    );
    assert!(references(&search(&fixture, &["--all", "zebra"])).contains(&"third:1".into()));
    assert!(
        !references(&search(&fixture, &["--all", "--reindex", "zebra"]))
            .contains(&"third:1".into())
    );
    assert_eq!(
        search(&fixture, &["--all", "결정을"])["hits"][0]["reference"],
        "third:5"
    );

    let refused = fixture.wsx(&["chat", "search", "!!"]);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("needs some words"),
        "{refused:?}"
    );
}

#[test]
fn a_task_story_lists_its_steps_and_finds_later_lines() {
    let fixture = Fixture::new();
    let third = third(&fixture);
    // ^ T0 + 7 s is 2023-11-16 02:00:07 UTC; the outcome shows the day in UTC.
    let day = "11-16";
    let outcome = format!("landed {day} cd67238e1f90");

    // ^ A word of a later add finds the task with its outcome; a steer finds the closed one.
    let found = search(&fixture, &["--all", "zymurgy"]);
    let hit = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["reference"] == "third#1")
        .expect("third#1");
    assert_eq!(
        (
            hit["kind"].as_str(),
            hit["by"].as_str(),
            hit["outcome"].as_str()
        ),
        (Some("history"), Some("add"), Some(outcome.as_str())),
        "{hit}"
    );
    let found = search(&fixture, &["--all", "dead", "end"]);
    let hit = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["reference"] == "third#2")
        .expect("third#2");
    assert_eq!(
        (hit["by"].as_str(), hit["outcome"].as_str()),
        (Some("steer"), Some("closed: discarded"))
    );

    // ^ `--task` lists start, prompt, steps, result, add, checks, revision and landing in order.
    let story = search(&fixture, &["-p", "third", "--task", "1"]);
    let labels: Vec<_> = story["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        ["start", "prompt", "steps", "result", "add", "checks", "revision", "landed"],
        "{story}"
    );
    let texts: Vec<_> = story["entries"].as_array().unwrap()[2..]
        .iter()
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        texts,
        [
            "3 steps, last: tool step 2",
            "First pass: the parser is fixed.",
            "Also keep the zymurgy case small",
            "1 failed: cargo test",
            "r2 cd67238e1f90",
            "cd67238e1f90"
        ]
    );
    assert_eq!(
        (
            story["outcome"].as_str(),
            story["title"].as_str(),
            story["skipped"].as_u64()
        ),
        (
            Some(outcome.as_str()),
            Some("Fix the quolloquy parser"),
            Some(0)
        )
    );
    let printed = fixture.wsx(&["chat", "search", "-p", "third", "--task", "1"]);
    let printed = String::from_utf8_lossy(&printed.stdout);
    assert_eq!(
        printed.lines().next(),
        Some(format!("t1 [{outcome}]  Fix the quolloquy parser").as_str())
    );
    assert_eq!(
        search(&fixture, &["-p", "third", "--task", "1", "--limit", "2"])["skipped"],
        6
    );

    // ^ Words with --task keep that task's hits; a line the log gains later is found without a
    // rebuild, and lines read before are not found twice.
    assert_eq!(
        references(&search(
            &fixture,
            &["-p", "third", "--task", "1", "gizmo", "zymurgy"]
        )),
        ["third#1"]
    );
    let mut log = fs::read_to_string(&third.landed_log).unwrap();
    log += &format!(
        "{}\n",
        json!({"unix_ms": T0 + 9000, "kind": "message", "mode": "add", "text": "A tumbleweed note"})
    );
    fs::write(&third.landed_log, log).unwrap();
    assert_eq!(
        references(&search(
            &fixture,
            &["-p", "third", "--task", "1", "tumbleweed"]
        )),
        ["third#1"]
    );
    assert_eq!(
        search(&fixture, &["-p", "third", "--task", "1", "zymurgy"])["hits"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        search(
            &fixture,
            &["-p", "third", "--task", "1", "--reindex", "tumbleweed"]
        )["docs"],
        8
    );
}
