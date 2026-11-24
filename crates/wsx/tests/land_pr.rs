//! `tasks.land.mode: pr`: a land pushes the task's branch to `origin` and opens its pull request
//! with `gh`; the default branch does not move. A `gh` that fails leaves the task ready, and the
//! next land opens it. Through the real CLI, a local bare remote and a fake `gh`.
//! ref: docs/tasks.md "Landing"
mod common;

use common::Fixture;
use std::fs;
use std::os::unix::fs::PermissionsExt;

const PR: &str = "https://github.com/owner/demo/pull/7";

/// A fake `gh` that logs its arguments; `pr create` fails while the file `gh-fails` exists.
fn fake_gh(fixture: &Fixture) {
    let root = fixture.root.display();
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> {root}/gh.log\ncase \"$1 $2\" in\n  'pr list') exit 0 ;;\n  'pr create') [ -e {root}/gh-fails ] && {{ echo 'not signed in' >&2; exit 1; }}; echo 'Creating pull request'; echo {PR} ;;\nesac\n"
    );
    let gh = fixture.root.join("bin/gh");
    fs::write(&gh, script).expect("write the fake gh");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod the fake gh");
}

#[test]
fn a_pr_project_lands_into_a_pull_request() {
    let fixture = Fixture::new();
    fake_gh(&fixture);
    let remote = fixture.root.join("remote.git");
    let made = std::process::Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()
        .expect("git init --bare");
    assert!(made.success());
    fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    let config = fixture.root.join("project/wsx.config.yml");
    let text = fs::read_to_string(&config).expect("read the project config");
    fs::write(
        &config,
        text.replace("land: { auto: false }", "land: { auto: false, mode: pr }"),
    )
    .expect("write the project config");
    fixture.git(&["commit", "-qam", "land into pull requests"]);
    fixture.git(&["push", "-q", "origin", "main"]);
    let main_before = fixture.git(&["rev-parse", "main"]);

    fixture.json(&[
        "task",
        "new",
        "WORK a.txt",
        "--title",
        "Add a.txt",
        "--json",
    ]);
    fixture.wait("the task is ready", |task| {
        (task["state"] == "ready").then_some(())
    });

    // ^ `gh` fails: the land fails with its words, the task stays ready, main stays.
    fs::write(fixture.root.join("gh-fails"), "").unwrap();
    let failed = fixture.wsx(&["task", "land", "1", "--json"]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(said.contains("not signed in"), "{said}");
    fixture.wait("the task is ready again", |task| {
        (task["state"] == "ready").then_some(())
    });
    assert_eq!(fixture.git(&["rev-parse", "main"]), main_before);

    // ^ The next land opens it: the branch on the remote holds the task's head, the task is
    // landed with the pull request in its log, and main has not moved.
    fs::remove_file(fixture.root.join("gh-fails")).unwrap();
    let landed = fixture.json(&["task", "land", "1", "--json"]);
    assert!(landed.to_string().contains(PR), "{landed}");
    let task = fixture.wait("the task is landed", |task| {
        (task["state"] == "landed").then(|| task.clone())
    });
    // ^ The commit the land pushed, as the task's log records it.
    let log = fixture.log(&task);
    let head = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|entry| {
            (entry.to_string().contains("pr_opened"))
                .then(|| entry.to_string())
                .and_then(|text| {
                    let at = text.find("\"commit\":\"")? + 10;
                    Some(text[at..at + 40].to_string())
                })
        })
        .unwrap_or_else(|| panic!("no pr_opened entry: {log}"));
    let pushed = String::from_utf8(
        std::process::Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/wsx/t1",
            ])
            .output()
            .expect("read the remote branch")
            .stdout,
    )
    .unwrap();
    assert!(!pushed.trim().is_empty(), "no branch wsx/t1 on the remote");
    assert_eq!(pushed.trim(), head, "{task}");
    assert!(
        fixture.log(&task).contains(&format!("\"url\":\"{PR}\"")),
        "{}",
        fixture.log(&task)
    );
    assert_eq!(
        fixture.git(&["rev-parse", "main"]),
        main_before,
        "main moved"
    );
    let gh = fs::read_to_string(fixture.root.join("gh.log")).unwrap();
    assert!(
        gh.contains("pr create --base main --head wsx/t1 --title Add a.txt"),
        "{gh}"
    );
}
