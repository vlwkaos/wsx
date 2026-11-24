//! `wsx worktree delete` refuses uncommitted work before it closes anything, ignores build
//! output, and keeps a branch with unmerged commits. Real wsxd and its worktree session.
//! ref: docs/cli.md "worktree"

mod common;

use common::Fixture;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn sessions(fixture: &Fixture) -> usize {
    let listed = fixture.json(&["session", "list", "--json"]);
    listed
        .as_array()
        .or_else(|| listed["sessions"].as_array())
        .map_or(0, |rows| {
            rows.iter()
                .filter(|row| row.to_string().contains("feat/keep"))
                .count()
        })
}

fn delete(fixture: &Fixture) -> std::process::Output {
    fixture.wsx(&["worktree", "delete", "feat/keep"])
}

fn said(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn delete_refuses_uncommitted_work_and_keeps_an_unmerged_branch() {
    let fixture = Fixture::new();
    // ^ Ignored build output does not count as work.
    fs::write(fixture.root.join("project/.gitignore"), "target/\n").unwrap();
    fs::write(fixture.root.join("project/README"), "fixture\n").unwrap();
    fixture.git(&["add", ".gitignore", "README"]);
    fixture.git(&["commit", "-q", "-m", "ignore"]);
    let created = fixture.json(&["worktree", "create", "feat/keep", "--json"]);
    let keep: PathBuf = created["worktree"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .into();
    let git = |args: &[&str]| {
        let mut argv = vec!["-C", keep.to_str().unwrap()];
        argv.extend(args);
        fixture.git(&argv)
    };
    fs::create_dir(keep.join("target")).unwrap();
    fs::write(keep.join("target/build"), "ignored output").unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    while sessions(&fixture) == 0 {
        assert!(Instant::now() < deadline, "the worktree session timeout");
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::write(keep.join("README"), "changed\n").unwrap();
    let refused = delete(&fixture);
    assert!(
        !refused.status.success()
            && said(&refused).contains("uncommitted changes: README")
            && said(&refused).contains("Nothing was removed"),
        "{refused:?}"
    );
    assert!(keep.exists() && sessions(&fixture) > 0);
    git(&["checkout", "--", "README"]);
    fs::write(keep.join("notes.txt"), "draft\n").unwrap();
    let refused = delete(&fixture);
    assert!(
        !refused.status.success() && said(&refused).contains("untracked files: notes.txt"),
        "{refused:?}"
    );
    assert!(keep.exists() && sessions(&fixture) > 0);
    git(&["add", "notes.txt"]);
    git(&["commit", "-q", "-m", "unmerged"]);
    let deleted = delete(&fixture);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(
        String::from_utf8_lossy(&deleted.stdout).contains("kept branch feat/keep"),
        "{deleted:?}"
    );
    assert!(!Path::new(&keep).exists() && sessions(&fixture) == 0);
    assert!(!fixture.git(&["branch", "--list", "feat/keep"]).is_empty());
}
