//! A chat's purpose (docs/tasks.md "Chats"): `chat goal` shows, sets and clears it, for the main
//! chat too; the agent's instructions read it at the next turn; a chat's agent changes it only on
//! the owner's word. Real host binary, fake Claude (`scripts/fake-claude-stream.py`).

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::{
    fs,
    time::{Duration, Instant},
};

/// The text of chat `chat`'s reply to message `id`, polled for up to 30 s.
fn reply_to(fixture: &Fixture, chat: &str, id: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = fixture.json(&["chat", "read", "--chat", chat, "--json"]);
        let reply = log["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|entry| {
                let answers = entry["kind"] == "reply"
                    && entry["to"]
                        .as_array()
                        .is_some_and(|to| to.contains(&id.into()));
                answers.then(|| entry["text"].as_str().unwrap_or_default().to_string())
            });
        if let Some(reply) = reply {
            return reply;
        }
        assert!(
            Instant::now() < deadline,
            "{chat} did not reply to {id}: {log}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Say `text` to `chat` and wait for its answer.
fn say(fixture: &Fixture, chat: &str, text: &str) -> String {
    let said: Value = fixture.json(&["chat", "say", "--to", chat, text, "--json"]);
    reply_to(fixture, chat, said["id"].as_u64().expect("message id"))
}

/// The instructions of the chat agent's latest launch.
fn last_instructions(fixture: &Fixture, chat: &str) -> String {
    fixture
        .traces()
        .iter()
        .rev()
        .find(|trace| {
            !trace["launch"].is_null() && trace["actor"] == format!("chat:{chat}").as_str()
        })
        .and_then(|trace| trace["instructions"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no launch of chat {chat}"))
}

// ref: docs/tasks.md "Chats"
#[test]
fn a_chats_purpose_reaches_its_instructions_and_only_the_owner_changes_it() {
    let fixture = Fixture::new();
    let goal_of = |name: &str, text: &str| {
        let path = fixture.root.join(name);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_string()
    };
    let first = goal_of("purpose-1.md", "Keep the invoices exact.\n");
    let second = goal_of("purpose-2.md", "Own the refunds.\n");
    let none = goal_of("purpose-none.md", "");
    let goal_file = fixture.root.join("state/wsx/tasks/chats/purpose/goal.md");

    let made = fixture.wsx(&["chat", "new", "purpose", "--goal", &first]);
    assert!(made.status.success(), "{made:?}");
    assert_eq!(
        fixture.json(&["chat", "goal", "purpose", "--json"])["goal"],
        "Keep the invoices exact.\n"
    );
    say(&fixture, "purpose", "FIRST TURN");
    assert!(last_instructions(&fixture, "purpose")
        .contains("The owner's purpose for this chat:\nKeep the invoices exact."));

    // ^ The chat's own agent may not change it ...
    let asked = say(
        &fixture,
        "purpose",
        &format!("RUN wsx chat goal purpose --goal {second} -p demo --json"),
    );
    assert!(asked.contains("chat goal is the owner's"), "{asked}");
    assert_eq!(
        fs::read_to_string(&goal_file).unwrap(),
        "Keep the invoices exact.\n"
    );
    // ^ ... until the owner's message says so.
    let allowed = say(
        &fixture,
        "purpose",
        &format!(
            "change your purpose\nRUN wsx chat goal purpose --goal {second} --owner-said OWNER_SAID -p demo --json"
        ),
    );
    assert!(allowed.contains("-> 0 "), "{allowed}");
    assert_eq!(
        fs::read_to_string(&goal_file).unwrap(),
        "Own the refunds.\n"
    );
    say(&fixture, "purpose", "SECOND TURN");
    let instructions = last_instructions(&fixture, "purpose");
    assert!(
        instructions.contains("The owner's purpose for this chat:\nOwn the refunds.")
            && !instructions.contains("invoices"),
        "{instructions}"
    );
    // ^ An empty goal clears it.
    let cleared = fixture.wsx(&["chat", "goal", "purpose", "--goal", &none]);
    assert!(cleared.status.success(), "{cleared:?}");
    say(&fixture, "purpose", "THIRD TURN");
    let instructions = last_instructions(&fixture, "purpose");
    assert!(!instructions.contains("purpose for this chat") && !goal_file.exists());
    assert!(
        instructions.starts_with("You are purpose, a chat of the project demo.\n\n"),
        "{instructions}"
    );

    // ^ The main chat has none by default: it opens as the lead (docs/tasks.md "Lead and forks");
    // with one, its instructions open with the purpose and no lead preamble.
    say(&fixture, "demo", "HOME TURN 0");
    let lead = last_instructions(&fixture, "demo");
    assert!(
        !lead.contains("purpose for this chat")
            && lead.starts_with("You are the lead of the project demo:"),
        "{lead}"
    );
    let set = fixture.wsx(&["chat", "goal", "demo", "--goal", &first]);
    assert!(set.status.success(), "{set:?}");
    say(&fixture, "demo", "HOME TURN");
    let home = last_instructions(&fixture, "demo");
    assert!(
        home.starts_with("The owner's purpose for this chat:\nKeep the invoices exact.\n\n")
            && !home.contains("You are the lead"),
        "{home}"
    );
    let cleared = fixture.wsx(&["chat", "goal", "demo", "--goal", &none]);
    assert!(cleared.status.success(), "{cleared:?}");
    say(&fixture, "demo", "HOME TURN 2");
    assert!(!last_instructions(&fixture, "demo").contains("purpose for this chat"));

    // ^ A goal is bounded.
    let big = fixture.root.join("purpose-big.md");
    fs::write(&big, "x".repeat(9000)).unwrap();
    let refused = fixture.wsx(&["chat", "goal", "purpose", "--goal", big.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("larger than 8192 bytes"));
    let closed = fixture.wsx(&["chat", "close", "purpose"]);
    assert!(closed.status.success(), "{closed:?}");
}
