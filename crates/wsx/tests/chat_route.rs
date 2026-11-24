//! The router: `/new <name>` forks the owner's message into a new chat and `@<chat>` gives it to
//! an open chat; the chat it was written in takes no turn on it. Through the real CLI and the
//! fake manager agent. ref: docs/tasks.md "Router"
mod common;

use common::Fixture;
use serde_json::Value;
use std::time::{Duration, Instant};

/// The raw log entries of chat `chat`; none while it does not exist yet (a fork goes on from
/// its host's own thread).
fn entries(fixture: &Fixture, chat: &str) -> Vec<Value> {
    let out = fixture.wsx(&["chat", "read", "--chat", chat, "--json"]);
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .and_then(|v| v["entries"].as_array().cloned())
        .unwrap_or_default()
}

fn kinds(entries: &[Value], kind: &str) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| e["kind"] == kind)
        .cloned()
        .collect()
}

/// Poll chat `chat` for up to 30 s until `test` finds something.
fn wait_for<T>(fixture: &Fixture, chat: &str, mut test: impl FnMut(&[Value]) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = entries(fixture, chat);
        if let Some(found) = test(&log) {
            return found;
        }
        assert!(Instant::now() < deadline, "{chat}: timeout: {log:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The owner's message `text` to chat `to` (none: the main chat `demo`); its id there.
fn say(fixture: &Fixture, to: Option<&str>, text: &str) -> u64 {
    let mut args = vec!["chat", "say"];
    if let Some(to) = to {
        args.extend(["--to", to]);
    }
    args.extend([text, "--json"]);
    let said = fixture.json(&args);
    said["id"]
        .as_u64()
        .unwrap_or_else(|| panic!("no message id: {said}"))
}

/// The id of the first owner message in chat `chat` whose text is `text`, once its agent replied.
fn taken(fixture: &Fixture, chat: &str, text: &str) -> u64 {
    let id = wait_for(fixture, chat, |log| {
        log.iter()
            .find(|e| e["kind"] == "message" && e["from"] == "owner" && e["text"] == text)
            .and_then(|e| e["id"].as_u64())
    });
    wait_for(fixture, chat, |log| {
        log.iter()
            .any(|e| {
                e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
            })
            .then_some(())
    });
    id
}

/// No reply in chat `chat` answers its message `id`.
fn untaken(fixture: &Fixture, chat: &str, id: u64) {
    let log = entries(fixture, chat);
    let answered = log.iter().any(|e| {
        e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
    });
    assert!(!answered, "{chat} took a turn on {id}: {log:?}");
    // ^ The chat view leaves out `routed`; the log file has it before the message, so a
    // restarted host does not take the message.
    let raw = std::fs::read_to_string(
        fixture
            .root
            .join(format!("state/wsx/tasks/chats/{chat}/log.jsonl")),
    )
    .unwrap_or_default();
    let routed = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|e| e["kind"] == "routed" && e["message"] == id);
    assert!(routed, "{chat} did not log {id} as routed: {raw}");
}

#[test]
fn the_router_forks_and_moves_the_owners_message() {
    let fixture = Fixture::new();

    // ^ `/new`: the new chat starts on the words after the name, its goal their first line; the
    // main chat logs the message, takes no turn on it and notes the fork.
    let forked = say(
        &fixture,
        None,
        "/new fees-csv CSV export for fees\nwith totals",
    );
    let brief = taken(&fixture, "fees-csv", "CSV export for fees\nwith totals");
    let chats = fixture.json(&["chat", "list", "--json"]);
    let row = chats["chats"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "fees-csv"))
        .cloned()
        .unwrap_or_else(|| panic!("no chat fees-csv: {chats}"));
    assert_eq!(row["goal"], "CSV export for fees", "{row}");
    let from = kinds(&entries(&fixture, "fees-csv"), "forked_from");
    assert_eq!(
        (&from[0]["from"], &from[0]["message"]),
        (&"demo".into(), &forked.into())
    );
    untaken(&fixture, "demo", forked);
    let forks = kinds(&entries(&fixture, "demo"), "forked");
    assert_eq!(
        (&forks[0]["to"], &forks[0]["message"]),
        (&"fees-csv".into(), &forked.into())
    );
    assert!(brief > 0);

    // ^ `@<chat>`: the open chat takes the words after it; the chat it was written in does not.
    let moved = say(&fixture, None, "@fees-csv add a column for VAT");
    taken(&fixture, "fees-csv", "add a column for VAT");
    untaken(&fixture, "demo", moved);
    let notes = kinds(&entries(&fixture, "demo"), "moved");
    assert_eq!(
        (&notes[0]["to"], &notes[0]["message"]),
        (&"fees-csv".into(), &moved.into())
    );
    let back = kinds(&entries(&fixture, "fees-csv"), "moved_from");
    assert_eq!(
        (&back[0]["from"], &back[0]["message"]),
        (&"demo".into(), &moved.into())
    );

    // ^ It works from any chat, to any open chat, the main chat too.
    let home = say(&fixture, Some("fees-csv"), "@demo what else is open?");
    taken(&fixture, "demo", "what else is open?");
    untaken(&fixture, "fees-csv", home);

    // ^ A restart of the main chat's host takes none of the routed messages.
    let socket = fixture.json(&["chat", "lead", "--json"])["socket"]
        .as_str()
        .map(std::path::PathBuf::from)
        .expect("the main chat's socket");
    let stopped = fixture.wsx(&["session", "delete", "task-demo", "-p", "demo"]);
    assert!(stopped.status.success(), "{stopped:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the main chat's host did not stop"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    taken_here(&fixture, "after the restart");
    for id in [forked, moved] {
        untaken(&fixture, "demo", id);
    }

    // ^ Stays: an `@` that names no open chat is only text, and a message with no prefix is the
    // chat's own; both are taken here.
    taken_here(&fixture, "@nobody hello");
    taken_here(&fixture, "plain words");

    // ^ Refused, nothing logged: a used name, a prefix with no words after it.
    for (text, why) in [
        ("/new fees-csv again", "a fork needs a new name"),
        ("/new empty-one", "write the message after `/new empty-one`"),
        ("@fees-csv", "write the message after `@fees-csv`"),
    ] {
        let before = kinds(&entries(&fixture, "demo"), "message").len();
        let out = fixture.wsx(&["chat", "say", text, "--json"]);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(said.contains(why), "{text}: {said}");
        assert_eq!(
            kinds(&entries(&fixture, "demo"), "message").len(),
            before,
            "{text}"
        );
    }
    assert!(!fixture
        .json(&["chat", "list", "--json"])
        .to_string()
        .contains("empty-one"));
}

/// The owner's `text` to the main chat stays there: the main chat takes it.
fn taken_here(fixture: &Fixture, text: &str) {
    say(fixture, None, text);
    taken(fixture, "demo", text);
}

/// The router's calls: it runs in the state folder, so the fake traces one folder up from it.
fn router_traces(fixture: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(fixture.root.join("state/fake-claude.trace"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|t| t["route"] == true)
        .collect()
}

/// How many times the router's model was asked.
fn router_calls(fixture: &Fixture) -> usize {
    router_traces(fixture).len()
}

#[test]
fn the_router_model_picks_a_prefix_and_stays_when_unsure() {
    let fixture = Fixture::with_tasks_config("  router: { model: fake-router }\n");
    let goal = fixture.root.join("goal.md");
    std::fs::write(&goal, "Invoices and VAT\n").unwrap();
    let made = fixture.wsx(&["chat", "new", "billing", "--goal", goal.to_str().unwrap()]);
    assert!(made.status.success(), "{made:?}");

    // ^ The model names an open chat: the message goes there, as `@billing` would send it.
    let moved = say(&fixture, None, "ROUTE_TO billing add VAT to the export");
    taken(
        &fixture,
        "billing",
        "ROUTE_TO billing add VAT to the export",
    );
    untaken(&fixture, "demo", moved);
    let asked = router_traces(&fixture)
        .into_iter()
        .next()
        .expect("a router call");
    assert!(asked["argv"].to_string().contains("fake-router"), "{asked}");
    assert!(
        asked["argv"]
            .to_string()
            .contains("billing: Invoices and VAT"),
        "{asked}"
    );

    // ^ A goal no open chat has: a new chat, as `/new refunds` would make it.
    let forked = say(&fixture, None, "ROUTE_NEW refunds handle refunds by card");
    taken(
        &fixture,
        "refunds",
        "ROUTE_NEW refunds handle refunds by card",
    );
    untaken(&fixture, "demo", forked);

    // ^ Stays: the model unsure, or naming a chat that is not open.
    for text in ["just a question", "ROUTE_TO ghost hello"] {
        say(&fixture, None, text);
        taken(&fixture, "demo", text);
    }

    // ^ A message with its own prefix asks no model.
    let before = router_calls(&fixture);
    say(&fixture, None, "@billing and the totals");
    taken(&fixture, "billing", "and the totals");
    assert_eq!(router_calls(&fixture), before);
}
