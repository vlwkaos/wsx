//! `wsx task sync` moves the local default branch to origin's tip after a merge there, with no
//! approval and no `--force`, and refuses, naming what it names, when a commit or a file
//! could be lost (docs/tasks.md "Sync"). Drives the public CLI on an isolated HOME and state,
//! with real git repositories and a bare repository as origin; it needs no wsxd.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    other: PathBuf,
}

impl Fixture {
    /// A project on `main` with two pushed commits (`a.txt`, `b.txt`), and a second clone
    /// where origin's side of the story is written.
    fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("resolve the repository root")
            .join("target")
            .join(format!("sync-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // ^ dirs::config_dir(): macOS ignores XDG_CONFIG_HOME, Linux uses it.
        let config = if cfg!(target_os = "macos") {
            root.join("home/Library/Application Support/wsx")
        } else {
            root.join("config/wsx")
        };
        fs::create_dir_all(&config).expect("create the config folder");
        let fixture = Self {
            project: root.join("project"),
            other: root.join("other"),
            root,
        };
        fs::write(
            config.join("config.toml"),
            format!(
                "[[projects]]\nname = \"demo\"\npath = {:?}\n",
                fixture.project.to_str().expect("UTF-8 path")
            ),
        )
        .expect("write the wsx config");
        let origin = fixture.root.join("origin.git");
        fixture.git(
            &fixture.root,
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        fixture.git(&fixture.root, &["clone", "-q", "origin.git", "project"]);
        fixture.git(&fixture.project, &["checkout", "-q", "-b", "main"]);
        fixture.commit(&fixture.project, "a.txt", "1\n");
        fixture.commit(&fixture.project, "b.txt", "1\n");
        fixture.git(&fixture.project, &["push", "-q", "origin", "main"]);
        fixture.git(
            &fixture.root,
            &["clone", "-q", origin.to_str().unwrap(), "other"],
        );
        fixture
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.project)
            .env("HOME", self.root.join("home"))
            .env("WSX_EXIT_WITH_PID", std::process::id().to_string())
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com");
        // ^ A worker's broker or actor would route or refuse the commands.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("WSX_") {
                command.env_remove(name);
            }
        }
        command
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self
            .command(Path::new("git"))
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(&self, dir: &Path, file: &str, text: &str) {
        fs::write(dir.join(file), text).expect("write the file");
        self.git(dir, &["add", "."]);
        self.git(dir, &["commit", "-q", "-m", &format!("{file}: {text:?}")]);
    }

    /// Two commits (`c.txt`, `d.txt`) on the project's local `main`, not on origin.
    fn local_work(&self) {
        self.commit(&self.project, "c.txt", "c\n");
        self.commit(&self.project, "d.txt", "d\n");
    }

    fn tip(&self) -> String {
        self.git(&self.project, &["rev-parse", "main"])
    }

    fn origin_tip(&self) -> String {
        self.git(&self.other, &["rev-parse", "HEAD"])
    }

    /// What a merge on GitHub leaves on origin: `c.txt` and `d.txt` as the pull request had
    /// them, as one squash commit, or as the same commits again (a rebase merge).
    fn merge_on_origin(&self, squash: bool) {
        if squash {
            fs::write(self.other.join("c.txt"), "c\n").unwrap();
            fs::write(self.other.join("d.txt"), "d\n").unwrap();
            self.git(&self.other, &["add", "."]);
            self.git(&self.other, &["commit", "-q", "-m", "squash"]);
        } else {
            self.git(
                &self.other,
                &["fetch", "-q", self.project.to_str().unwrap(), "main"],
            );
            self.git(&self.other, &["cherry-pick", "FETCH_HEAD~1", "FETCH_HEAD"]);
        }
        self.commit(&self.other, "e.txt", "e\n");
        self.git(&self.other, &["push", "-q", "origin", "main"]);
    }

    fn sync(&self) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .args(["task", "sync", "-p", "demo", "--json"])
            .output()
            .expect("run wsx task sync")
    }

    fn text(output: &Output) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn moved(fixture: &Fixture) -> Value {
    let before = fixture.tip();
    let output = fixture.sync();
    assert!(output.status.success(), "{}", Fixture::text(&output));
    let answer: Value = serde_json::from_slice(&output.stdout).expect("sync JSON");
    assert_eq!(answer["moved"], true, "{answer}");
    assert_eq!(answer["from"], before.as_str(), "{answer}");
    assert_eq!(fixture.tip(), fixture.origin_tip());
    assert_eq!(fixture.git(&fixture.project, &["status", "--short"]), "");
    answer
}

#[test]
fn a_rebase_merge_on_origin_moves_the_local_branch_without_force() {
    let fixture = Fixture::new("rebase");
    fixture.local_work();
    fixture.merge_on_origin(false);
    assert_ne!(fixture.tip(), fixture.origin_tip());
    moved(&fixture);
}

#[test]
fn a_squash_merge_on_origin_moves_the_local_branch() {
    let fixture = Fixture::new("squash");
    fixture.local_work();
    fixture.merge_on_origin(true);
    moved(&fixture);
}

#[test]
fn a_plain_fast_forward_moves_and_a_second_run_has_nothing_to_do() {
    let fixture = Fixture::new("ff");
    fixture.commit(&fixture.other, "e.txt", "e\n");
    fixture.git(&fixture.other, &["push", "-q", "origin", "main"]);
    moved(&fixture);
    let output = fixture.sync();
    assert!(output.status.success(), "{}", Fixture::text(&output));
    let answer: Value = serde_json::from_slice(&output.stdout).expect("sync JSON");
    assert_eq!(answer["moved"], false, "{answer}");
}

#[test]
fn a_local_commit_origin_lacks_refuses_and_is_named() {
    let fixture = Fixture::new("local");
    fixture.local_work();
    fixture.commit(&fixture.other, "z.txt", "z\n");
    fixture.git(&fixture.other, &["push", "-q", "origin", "main"]);
    let before = fixture.tip();
    let output = fixture.sync();
    let text = Fixture::text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("c.txt:") && text.contains("d.txt:"), "{text}");
    assert!(text.contains("nothing was moved"), "{text}");
    assert_eq!(fixture.tip(), before);
}

#[test]
fn a_checkout_a_reset_left_half_moved_moves() {
    // ^ ir #19: `git reset --hard origin/main` ended at the ref, which the boundary refused,
    // after the index and the files were at origin's tip already.
    let fixture = Fixture::new("half");
    fixture.local_work();
    fixture.merge_on_origin(true);
    fixture.git(&fixture.project, &["fetch", "-q", "origin"]);
    fixture.git(&fixture.project, &["read-tree", "-um", "origin/main"]);
    assert!(!fixture
        .git(&fixture.project, &["status", "--short"])
        .is_empty());
    moved(&fixture);
}

#[test]
fn a_file_that_has_the_targets_content_does_not_block_the_move() {
    let fixture = Fixture::new("equal");
    fixture.commit(&fixture.other, "a.txt", "2\n");
    fixture.git(&fixture.other, &["push", "-q", "origin", "main"]);
    fs::write(fixture.project.join("a.txt"), "2\n").unwrap();
    moved(&fixture);
}

#[test]
fn a_file_that_differs_from_the_target_refuses_and_stays() {
    let fixture = Fixture::new("differs");
    fixture.commit(&fixture.other, "a.txt", "2\n");
    fixture.git(&fixture.other, &["push", "-q", "origin", "main"]);
    fs::write(fixture.project.join("a.txt"), "mine\n").unwrap();
    let before = fixture.tip();
    let output = fixture.sync();
    let text = Fixture::text(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("a.txt"), "{text}");
    assert_eq!(fixture.tip(), before);
    assert_eq!(
        fs::read_to_string(fixture.project.join("a.txt")).unwrap(),
        "mine\n"
    );
}
