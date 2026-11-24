//! `wsx project add <git url>`: a URL alone clones into `<root>/<repo>` and registers it, the root
//! being the folder of the project it runs from (`ops::clone_place`). ref: docs/tasks.md "Approvals"

#![cfg(target_os = "macos")]

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::Path;

/// `wsx <args>` with file URLs allowed, as the scenarios allow them (`git::ALLOW_FILE_CLONE_ENV`).
fn add(fixture: &Fixture, args: &[&str]) -> std::process::Output {
    fixture
        .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
        .env("WSX_TEST_ALLOW_FILE_CLONE", "1")
        .args(args)
        .output()
        .expect("run wsx")
}

#[test]
fn a_git_url_alone_clones_beside_the_current_project() {
    let fixture = Fixture::new();
    let home = fixture.root.join("home");
    let source = fixture.root.join("src");
    // ^ Two remote repositories: `alpha` is registered at a folder named, `beta` by its URL alone.
    for name in ["alpha", "beta"] {
        let bare = source.join(format!("{name}.git"));
        fixture.git(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    }
    let url = |name: &str| format!("file://{}/{name}.git", source.display());
    let alpha = home.join("ws-a/alpha");
    let added = add(
        &fixture,
        &[
            "project",
            "add",
            alpha.to_str().unwrap(),
            "--clone",
            &url("alpha"),
            "--json",
        ],
    );
    assert!(added.status.success(), "{added:?}");

    // ^ From `alpha`, `beta`'s URL clones into alpha's folder, named by the URL.
    let added = add(
        &fixture,
        &["project", "add", &url("beta"), "-p", "alpha", "--json"],
    );
    assert!(added.status.success(), "{added:?}");
    let answer: Value = serde_json::from_slice(&added.stdout).expect("JSON answer");
    let beta = home.join("ws-a/beta");
    assert_eq!(answer["project"], "beta", "{answer}");
    assert_eq!(answer["path"], beta.to_str().unwrap(), "{answer}");
    assert!(beta.join(".git").exists() && beta.join("wsx.config.yml").exists());
    let ledger = fixture.json(&["chat", "ledger", "-p", "beta", "--json"]);
    assert_eq!(ledger["project"], "beta", "{ledger}");

    // ^ The same URL again is refused by name, and clones nothing more.
    let again = add(&fixture, &["project", "add", &url("beta"), "-p", "alpha"]);
    let said = String::from_utf8_lossy(&again.stderr);
    assert!(
        !again.status.success() && said.contains("already registered"),
        "{again:?}"
    );
    assert!(!home.join("ws-a/beta-1").exists());

    // ^ A `-p` that names no project is refused, not dropped for another root.
    let typo = add(&fixture, &["project", "add", &url("beta"), "-p", "nope"]);
    assert!(!typo.status.success(), "{typo:?}");
    assert!(
        String::from_utf8_lossy(&typo.stderr).contains("nope"),
        "{typo:?}"
    );
    // ^ A folder is never read as a URL, though `/…/a@b:c` has the scp shape.
    let folder = home.join("ws-a/a@b:c");
    let refused = add(&fixture, &["project", "add", folder.to_str().unwrap()]);
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("does not exist"),
        "{refused:?}"
    );
}
