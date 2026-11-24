//! The lead forks the owner's message into a new chat that starts on it; closing that chat
//! before it owns a task undoes the fork. Through the real CLI and the fake manager agent.
//! ref: docs/tasks.md "Lead and forks"
mod common;

use common::Fixture;
use serde_json::Value;
use std::time::{Duration, Instant};

/// The raw log entries of chat `chat`.
fn entries(fixture: &Fixture, chat: &str) -> Vec<Value> {
    fixture.json(&["task", "chat", "--chat", chat, "--json"])["entries"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The text of chat `chat`'s reply to its message `id`, polled for up to 30 s. The fixture's
/// own wait reads a task; this one reads a chat.
fn reply_to(fixture: &Fixture, chat: &str, id: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let log = entries(fixture, chat);
        let reply = log.iter().find_map(|entry| {
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
            "{chat} did not reply to {id}: {log:?}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn kinds(entries: &[Value], kind: &str) -> Vec<Value> {
    entries
        .iter()
        .filter(|e| e["kind"] == kind)
        .cloned()
        .collect()
}

/// The owner's message to the project: the goal on its first line, then the fork the lead (the
/// main chat `demo`) runs, then the `more` lines.
fn ask_the_lead(fixture: &Fixture, name: &str, goal: &str, more: &str) -> u64 {
    let text = format!(
        "{goal}\nRUN wsx task chat new {name} --brief --owner-said OWNER_SAID -p demo --json{more}"
    );
    fixture.json(&["task", "say", &text, "--json"])["id"]
        .as_u64()
        .expect("a message id")
}

/// The id of the forked chat's first message, after its first turn ended.
fn brief_taken(fixture: &Fixture, chat: &str) -> u64 {
    let first = kinds(&entries(fixture, chat), "message")[0]["id"]
        .as_u64()
        .expect("the brief's id");
    reply_to(fixture, chat, first);
    first
}

fn chat_names(fixture: &Fixture) -> String {
    fixture
        .json(&["task", "chat", "list", "--json"])
        .to_string()
}

#[test]
fn the_lead_forks_a_goal_into_a_new_chat_that_starts_on_it() {
    let fixture = Fixture::new();
    // ^ The same message forked a second time, by its second RUN line: refused.
    let again = "\nRUN wsx task chat new fees-twice --brief --owner-said OWNER_SAID -p demo --json";
    let asked = ask_the_lead(&fixture, "fees-csv", "CSV export for fees", again);
    let answer = reply_to(&fixture, "demo", asked);
    assert!(answer.contains("forked message"), "{answer}");
    assert!(
        answer.contains(&format!("message {asked} was forked already")),
        "{answer}"
    );
    assert!(!chat_names(&fixture).contains("fees-twice"));

    // ^ The new chat: open, its goal the owner's first line, its first message the owner's own
    // words, marked as forked from the lead; the lead's log notes the fork.
    let chats = fixture.json(&["task", "chat", "list", "--json"]);
    let row = chats["chats"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "fees-csv"))
        .cloned()
        .unwrap_or_else(|| panic!("no chat fees-csv: {chats}"));
    assert_eq!(row["state"], "open", "{row}");
    assert_eq!(row["goal"], "CSV export for fees", "{row}");
    let thread = entries(&fixture, "fees-csv");
    let from = kinds(&thread, "forked_from");
    assert_eq!(from.len(), 1, "{thread:?}");
    assert_eq!(
        (&from[0]["from"], &from[0]["message"]),
        (&"demo".into(), &asked.into())
    );
    let original = kinds(&entries(&fixture, "demo"), "message")
        .into_iter()
        .find(|m| m["id"] == asked)
        .expect("the owner's message in the lead");
    let brief = kinds(&thread, "message");
    assert_eq!(brief.len(), 1, "{thread:?}");
    assert_eq!(
        (&brief[0]["from"], &brief[0]["text"]),
        (&"owner".into(), &original["text"])
    );
    let lead = entries(&fixture, "demo");
    let forked = kinds(&lead, "forked");
    assert_eq!(forked.len(), 1, "{lead:?}");
    assert_eq!(
        (&forked[0]["to"], &forked[0]["message"]),
        (&"fees-csv".into(), &asked.into())
    );

    // ^ The new chat took a turn on the brief and ran the same lines: only the lead forks, so
    // both refuse and nothing else changed.
    let rerun = reply_to(&fixture, "fees-csv", brief[0]["id"].as_u64().unwrap());
    assert!(rerun.contains("only the lead forks"), "{rerun}");
    assert_eq!(kinds(&entries(&fixture, "fees-csv"), "forked").len(), 0);

    // ^ A name used before is refused: a fork never reopens a chat's log.
    let reused = ask_the_lead(&fixture, "fees-csv", "Again", "");
    let answer = reply_to(&fixture, "demo", reused);
    assert!(
        answer.contains("was opened before: a fork needs a new name"),
        "{answer}"
    );
    assert_eq!(kinds(&entries(&fixture, "demo"), "forked").len(), 1);

    // ^ A chat with no goal that is not the main chat is no lead either.
    let opened = fixture.wsx(&["task", "chat", "new", "side"]);
    assert!(opened.status.success(), "{opened:?}");
    let side = fixture.json(&[
        "task",
        "say",
        "--to",
        "side",
        "Side work\nRUN wsx task chat new from-side --brief --owner-said OWNER_SAID -p demo --json",
        "--json",
    ])["id"]
        .as_u64()
        .expect("a message id");
    let answer = reply_to(&fixture, "side", side);
    assert!(answer.contains("only the lead forks"), "{answer}");
    assert!(!chat_names(&fixture).contains("from-side"));

    // ^ Only the lead's agent forks: the owner's own --brief is refused and makes no chat.
    let owner = fixture.wsx(&[
        "task",
        "chat",
        "new",
        "by-hand",
        "--brief",
        "--owner-said",
        &asked.to_string(),
    ]);
    let said = String::from_utf8_lossy(&owner.stderr);
    assert!(
        !owner.status.success() && said.contains("the lead's agent passes it"),
        "{owner:?}"
    );
    assert!(!chat_names(&fixture).contains("by-hand"));
}

#[test]
fn closing_a_fork_before_it_owns_a_task_undoes_it_and_no_later() {
    let fixture = Fixture::new();
    let asked = ask_the_lead(&fixture, "docs-fix", "Fix the docs", "");
    reply_to(&fixture, "demo", asked);
    brief_taken(&fixture, "docs-fix");
    let turns_before = kinds(&entries(&fixture, "demo"), "delivered").len();

    // ^ Undo: the owner closes it with no task. The lead gets a note no turn reads, and no
    // hand-back message whose goal it would fork again.
    let closed = fixture.wsx(&["task", "chat", "close", "docs-fix"]);
    let said = String::from_utf8_lossy(&closed.stdout);
    assert!(
        closed.status.success() && said.contains("the fork is undone"),
        "{closed:?}"
    );
    let lead = entries(&fixture, "demo");
    let undone = kinds(&lead, "fork_undone");
    assert_eq!(undone.len(), 1, "{lead:?}");
    assert_eq!(undone[0]["to"], "docs-fix");
    let handed = |lead: &[Value], chat: &str| {
        lead.iter().any(|e| {
            e["kind"] == "message"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains(&format!("Chat {chat} is closed")))
        })
    };
    assert!(
        !handed(&lead, "docs-fix"),
        "a hand-back reached the lead: {lead:?}"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        kinds(&entries(&fixture, "demo"), "delivered").len(),
        turns_before
    );

    // ^ Undone once: the name reopened by the owner and closed again is only closed.
    let reopened = fixture.wsx(&["task", "chat", "new", "docs-fix"]);
    assert!(reopened.status.success(), "{reopened:?}");
    let closed = fixture.wsx(&["task", "chat", "close", "docs-fix"]);
    let said = String::from_utf8_lossy(&closed.stdout);
    assert!(
        closed.status.success() && said.contains("open tasks are the main chat's"),
        "{closed:?}"
    );
    let lead = entries(&fixture, "demo");
    assert_eq!(kinds(&lead, "fork_undone").len(), 1, "{lead:?}");
    assert!(
        handed(&lead, "docs-fix"),
        "no hand-back on the second close: {lead:?}"
    );

    // ^ A fork that owned a task is only closed: its hand-back reaches the lead, no undo note.
    let asked = ask_the_lead(&fixture, "worker-fork", "Do the work", "");
    reply_to(&fixture, "demo", asked);
    brief_taken(&fixture, "worker-fork");
    let task = fixture.json(&[
        "task",
        "say",
        "--to",
        "worker-fork",
        "RUN wsx task new 'NOOP the work' -p demo --json",
        "--json",
    ])["id"]
        .as_u64()
        .expect("a message id");
    let made = reply_to(&fixture, "worker-fork", task);
    assert!(made.contains("\"id\""), "{made}");
    let closed = fixture.wsx(&["task", "chat", "close", "worker-fork"]);
    assert!(closed.status.success(), "{closed:?}");
    let lead = entries(&fixture, "demo");
    assert_eq!(kinds(&lead, "fork_undone").len(), 1, "{lead:?}");
    assert!(handed(&lead, "worker-fork"), "no hand-back: {lead:?}");
}

/// The models the fake agent was launched with for the chats' turns, in launch order.
fn launched_models(fixture: &Fixture) -> Vec<String> {
    fixture
        .traces()
        .iter()
        .filter(|t| t["launch"] == true && t["schema"] != true)
        .filter_map(|t| t["model"].as_str().map(str::to_string))
        .collect()
}

/// The lead answers and forks goals, so it runs on `tasks.lead.model` (unset on Claude:
/// sonnet) while a chat with a goal keeps the manager's model (opus); `tasks.lead.model: same` gives
/// the lead the manager's model again. ref: docs/tasks.md "Lead and forks"
#[test]
fn the_lead_runs_on_its_own_model_and_goal_chats_on_the_managers() {
    let fixture = Fixture::new();
    let id = fixture.json(&["task", "say", "hello", "--json"])["id"]
        .as_u64()
        .unwrap();
    reply_to(&fixture, "demo", id);
    assert_eq!(launched_models(&fixture), ["sonnet"]);
    let goal = fixture.root.join("goal.md");
    std::fs::write(&goal, "Invoices and VAT\n").unwrap();
    let made = fixture.wsx(&[
        "task",
        "chat",
        "new",
        "billing",
        "--goal",
        goal.to_str().unwrap(),
    ]);
    assert!(made.status.success(), "{made:?}");
    let id = fixture.json(&["task", "say", "--to", "billing", "hi", "--json"])["id"]
        .as_u64()
        .unwrap();
    reply_to(&fixture, "billing", id);
    assert_eq!(launched_models(&fixture), ["sonnet", "opus"]);
    // ^ A goal set on the main chat ends its lead role: its agent starts again on the manager's model.
    let set = fixture.wsx(&[
        "task",
        "chat",
        "goal",
        "demo",
        "--goal",
        goal.to_str().unwrap(),
    ]);
    assert!(set.status.success(), "{set:?}");
    let id = fixture.json(&["task", "say", "again", "--json"])["id"]
        .as_u64()
        .unwrap();
    reply_to(&fixture, "demo", id);
    assert_eq!(launched_models(&fixture), ["sonnet", "opus", "opus"]);

    let same = Fixture::with_tasks_config("  lead:\n    model: same\n");
    let id = same.json(&["task", "say", "hello", "--json"])["id"]
        .as_u64()
        .unwrap();
    reply_to(&same, "demo", id);
    assert_eq!(launched_models(&same), ["opus"]);
}
