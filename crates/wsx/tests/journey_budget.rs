//! The end-to-end journeys stay small. A new check goes in a crate's integration tests
//! (`crates/*/tests`, `common::Fixture`), which run in seconds; a journey under `scripts/` holds
//! only what needs the whole stack (wsxd with its hosts and fake agents, the TUI in a PTY, a
//! browser). Each journey has a line ceiling: past it this test fails, and well under it this
//! test asks for the ceiling to come down, so the ceilings only fall.
//! ref: AGENTS.md "Test placement"

use std::path::Path;

/// Each journey and its ceiling in lines.
const JOURNEYS: [(&str, usize); 5] = [
    ("scripts/test-task.py", 7751),
    ("scripts/test-web-client.py", 5560),
    ("scripts/web-tasks-check.mjs", 4509),
    ("scripts/runtime-smoke.py", 3410),
    ("scripts/web-browser-check.mjs", 1339),
];

/// How far under its ceiling a journey may fall before the ceiling must come down.
const SLACK: usize = 200;

#[test]
fn each_journey_stays_under_its_ceiling() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut wrong = Vec::new();
    for (path, ceiling) in JOURNEYS {
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        let lines = text.lines().count();
        if lines > ceiling {
            wrong.push(format!(
                "{path} has {lines} lines, over its ceiling {ceiling}: put the new check in the \
                 crate's integration tests (crates/*/tests) unless it needs the whole stack, and \
                 delete the checks the change made stale"
            ));
        } else if lines + SLACK < ceiling {
            wrong.push(format!(
                "{path} has {lines} lines, {} under its ceiling {ceiling}: lower the ceiling in \
                 crates/wsx/tests/journey_budget.rs to {lines}",
                ceiling - lines
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
