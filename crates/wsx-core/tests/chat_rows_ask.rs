//! An approval the manager asked shows once in a chat: as the tag of the manager's reply
//! (`chat_rows`), never as a second row. A log from a host that also posted "Asked you: ..." after
//! the turn shows no extra row either. ref: docs/tasks.md "Approvals"

use serde_json::json;
use wsx_core::task::chat::{chat_rows, Names};
use wsx_core::task::manager::{ApprovalLine, ChatEntry};

fn entries(lines: &[serde_json::Value]) -> Vec<ChatEntry> {
    lines
        .iter()
        .map(|line| serde_json::from_value(line.clone()).unwrap())
        .collect()
}

#[test]
fn an_asked_approval_is_one_row_with_one_tag() {
    let owner =
        json!({ "unix_ms": 100, "kind": "message", "id": 1, "from": "owner", "text": "push it" });
    // ^ The agent asked in its turn (the request is older than its reply) and said so in its reply.
    let reply = json!({ "unix_ms": 300, "kind": "reply", "id": 2, "to": [1], "text": "Asked: force push." });
    // ^ What an older host added 5 ms after the reply: an events message and a post reply to it.
    let old_event = json!({ "unix_ms": 305, "kind": "message", "id": 3, "from": "events",
        "text": "asked the owner: Approve force push (a3)" });
    let old_post = json!({ "unix_ms": 305, "kind": "reply", "id": 4, "to": [3], "text": "Asked you: Approve force push (a3).",
        "post": true, "approval": "a3" });
    let request: ApprovalLine = serde_json::from_value(
        json!({ "kind": "request", "id": "a3", "action": "push", "lease": "abc", "unix_ms": 250 }),
    )
    .unwrap();
    let names = Names {
        own: "manager",
        manager: "manager",
    };
    for (what, log) in [
        ("one reply", vec![owner.clone(), reply.clone()]),
        (
            "an old ask-line pair after it",
            vec![owner, reply, old_event, old_post],
        ),
    ] {
        let log = entries(&log);
        let refs: Vec<&ChatEntry> = log.iter().collect();
        let rows = chat_rows(&refs, &[&request], &[], &[], &names);
        assert_eq!(rows.len(), 2, "{what}: {rows:?}");
        assert_eq!(rows[1].approvals, ["a3"], "{what}: {rows:?}");
        assert!(rows.iter().all(|row| !row.post), "{what}: {rows:?}");
        assert!(
            rows.iter().all(|row| !row.text.contains("Asked you")),
            "{what}: {rows:?}"
        );
    }
}
