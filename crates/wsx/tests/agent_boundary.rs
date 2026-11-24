//! What the lead's host runs for its agent (`broker::allowed`, `broker::reach`): a host command is
//! refused wherever its flags sit, and the reach into another project covers the `wsx task` verbs
//! the role lists, not a `wsx chat` verb of the same name. Real host, fake Claude.
//! ref: docs/tasks.md "Lead tools", docs/design/orchestration.md "Role"

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use std::time::{Duration, Instant};

/// The main chat's reply to the owner's message `text`, which runs commands in its turn.
fn chat_runs(fixture: &Fixture, text: &str) -> String {
    let id = fixture.json(&["chat", "say", text, "--json"])["id"]
        .as_u64()
        .expect("message id");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = fixture.json(&["chat", "read", "--json"])["entries"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(reply) = log.iter().find(|e| {
            e["kind"] == "reply" && e["to"].as_array().is_some_and(|to| to.contains(&id.into()))
        }) {
            return reply["text"].as_str().unwrap_or_default().to_string();
        }
        assert!(Instant::now() < deadline, "no reply to {text}: {log:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The line of `ran` that reports `command`.
fn ran<'a>(reply: &'a str, command: &str) -> &'a str {
    reply
        .lines()
        .find(|line| line.contains(&format!("ran: {command}")))
        .unwrap_or_else(|| panic!("no run of {command}: {reply}"))
}

#[test]
fn the_lead_reaches_other_projects_only_through_task_verbs() {
    let fixture = Fixture::new();
    let other = fixture.root.join("home/other");
    std::fs::create_dir_all(&other).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "start"],
    ] {
        let done = fixture
            .command(std::path::Path::new("git"))
            .current_dir(&other)
            .args(args)
            .output()
            .expect("run git");
        assert!(done.status.success(), "{done:?}");
    }
    let added = fixture.wsx(&["project", "add", other.to_str().unwrap()]);
    assert!(added.status.success(), "{added:?}");

    let reply = chat_runs(
        &fixture,
        "RUN wsx task list -p other --json\nRUN wsx chat new spy -p other --json\nRUN wsx task -p demo host x 1",
    );
    // ^ `list` is on the lead's reach list: it runs in the other project.
    assert!(!ran(&reply, "wsx task list").contains("-> 2"), "{reply}");
    // ^ `chat new` shares the word `new` with `task new` and is refused there.
    assert!(ran(&reply, "wsx chat new").contains("-> 2"), "{reply}");
    // ^ A host command after a flag is still refused.
    assert!(
        ran(&reply, "wsx task -p demo host").contains("is not for an agent"),
        "{reply}"
    );
    let chats = fixture.json(&["chat", "list", "-p", "other", "--json"]);
    assert!(!chats.to_string().contains("\"spy\""), "{chats}");
}
