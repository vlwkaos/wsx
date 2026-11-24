//! Roles (`tasks.roles`): `chat new <name> --role <role>` opens a chat whose purpose names the
//! role's paths and purpose, before the owner's own goal, and its agent reads them in its first
//! turn; a role the config does not name is refused. ref: docs/tasks.md "Roles"
mod common;

use common::Fixture;
use std::time::{Duration, Instant};

#[test]
fn a_chat_opened_at_a_role_owns_its_paths() {
    let fixture = Fixture::with_tasks_config(
        "  roles:\n    web:\n      paths: [\"crates/web/**\", \"docs/web*.md\"]\n      purpose: Keep the page fast.\n",
    );
    let goal = fixture.root.join("goal.md");
    std::fs::write(&goal, "Ship the dark theme.\n").unwrap();
    let made = fixture.wsx(&[
        "chat",
        "new",
        "web-desk",
        "--role",
        "web",
        "--goal",
        goal.to_str().unwrap(),
    ]);
    assert!(made.status.success(), "{made:?}");
    let chats = fixture.json(&["chat", "list", "--json"]);
    let row = chats["chats"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "web-desk"))
        .cloned()
        .unwrap_or_else(|| panic!("no chat web-desk: {chats}"));
    // ^ The list shows the goal's first line: the role and its paths.
    let line = row["goal"].as_str().unwrap_or_default();
    assert!(
        line.starts_with("Role web: you own crates/web/**, docs/web*.md."),
        "{line}"
    );

    // ^ Its agent reads the whole goal: the role, its purpose, then the owner's goal.
    fixture.json(&["chat", "say", "--to", "web-desk", "hello", "--json"]);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let read = fixture.traces().iter().any(|trace| {
            trace["instructions"].as_str().is_some_and(|text| {
                let role = text.find("Role web: you own crates/web/**");
                let purpose = text.find("Keep the page fast.");
                let goal = text.find("Ship the dark theme.");
                matches!((role, purpose, goal), (Some(r), Some(p), Some(g)) if r < p && p < g)
            })
        });
        if read {
            break;
        }
        assert!(Instant::now() < deadline, "the agent never read the role");
        std::thread::sleep(Duration::from_millis(200));
    }

    // ^ A role the config does not name: refused with the names it has, no chat made.
    let refused = fixture.wsx(&["chat", "new", "api-desk", "--role", "api"]);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && said.contains("no role api in tasks.roles (it has: web)"),
        "{said}"
    );
    assert!(!fixture
        .json(&["chat", "list", "--json"])
        .to_string()
        .contains("api-desk"));
}
