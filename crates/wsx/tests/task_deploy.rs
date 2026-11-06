//! A failed deploy keeps the end of its output, and `wsx task deployed` shows it once its
//! detached run ends (docs/tasks.md "Deploy and release"); `wsx task push` moves origin's
//! default branch only as asked (docs/tasks.md "Push"). Drives the public CLI on an isolated
//! HOME and state, with a local bare repository as origin; neither needs wsxd.

use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(deploy_command: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("resolve the repository root")
            .join("target")
            .join(format!("deploy-it-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // ^ dirs::config_dir(): macOS ignores XDG_CONFIG_HOME, Linux uses it.
        let config = if cfg!(target_os = "macos") {
            root.join("home/Library/Application Support/wsx")
        } else {
            root.join("config/wsx")
        };
        let project = root.join("project");
        fs::create_dir_all(&config).expect("create the config folder");
        fs::create_dir_all(&project).expect("create the project");
        fs::write(
            config.join("config.toml"),
            format!(
                "[[projects]]\nname = \"demo\"\npath = {:?}\n",
                project.to_str().expect("UTF-8 path")
            ),
        )
        .expect("write the wsx config");
        fs::write(
            project.join("wsx.config.yml"),
            format!("tasks:\n  deploy:\n    command: {deploy_command:?}\n"),
        )
        .expect("write the project config");
        let fixture = Self { root };
        fixture.git(&["init", "-q", "--template=", "-b", "main"]);
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-q", "-m", "deploy config"]);
        fixture
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.root.join("project"))
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

    fn git(&self, args: &[&str]) -> String {
        let output = self
            .command(Path::new("git"))
            .args(args)
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn wsx(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run wsx {args:?}: {error}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_failed_deploy_shows_the_end_of_its_output() {
    let fixture = Fixture::new("seq 1 60; echo deploy broke here >&2; exit 3");
    let tip = fixture.git(&["rev-parse", "main"]);

    // ^ A deploy by hand answers that it started; its detached run records the result.
    let deploy = fixture.wsx(&["task", "deploy", "-p", "demo", "--json"]);
    assert!(deploy.status.success(), "{deploy:?}");
    let started: Value = serde_json::from_slice(&deploy.stdout).expect("deploy JSON");
    assert_eq!(started["started"], true, "{started}");
    assert_eq!(started["commit"], tip.as_str(), "{started}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let deployed = fixture.wsx(&["task", "deployed", "-p", "demo", "--json"]);
        assert!(deployed.status.success(), "{deployed:?}");
        let status: Value = serde_json::from_slice(&deployed.stdout).expect("deployed JSON");
        if status["deploy"]["failed_since"] == 1 {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the deploy was not recorded: {status}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let deploy = &status["deploy"];
    assert_eq!(deploy["failed"]["commit"], tip.as_str(), "{status}");
    assert_eq!(deploy["failed"]["exit"], 3, "{status}");
    // ^ The last 40 lines: stdout's end, then stderr.
    let expected: Vec<String> = (22..=60)
        .map(|n| n.to_string())
        .chain(["deploy broke here".to_string()])
        .collect();
    assert_eq!(
        deploy["failed"]["output"]
            .as_str()
            .map(|text| text.lines().map(str::to_string).collect::<Vec<_>>()),
        Some(expected),
        "{status}"
    );
    // ^ The temporary copy is gone; only the main checkout stays.
    assert_eq!(
        fixture
            .git(&["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
fn a_push_moves_origin_only_as_asked() {
    let fixture = Fixture::new("true");
    let origin = fixture.root.join("origin.git");
    let origin = origin.to_str().expect("UTF-8 path");
    fixture.git(&["init", "-q", "--bare", "--template=", "-b", "main", origin]);
    fixture.git(&["remote", "add", "origin", origin]);
    fixture.git(&["push", "-q", "origin", "main"]);
    let first = fixture.git(&["rev-parse", "main"]);
    let origin_tip = || fixture.git(&["--git-dir", origin, "rev-parse", "main"]);
    let answer = |output: &Output| -> Value {
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).expect("push JSON")
    };
    let refused = |args: &[&str], words: &str| {
        let output = fixture.wsx(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(words),
            "{args:?}: {output:?}"
        );
        assert!(stderr.contains("Nothing was pushed"), "{stderr}");
    };

    // ^ The owner's push of a new commit is a fast-forward of origin.
    fs::write(fixture.root.join("project/a.txt"), "a\n").expect("write a.txt");
    fixture.git(&["add", "a.txt"]);
    fixture.git(&["commit", "-q", "-m", "a"]);
    let tip = fixture.git(&["rev-parse", "main"]);
    let pushed = answer(&fixture.wsx(&["task", "push", "-p", "demo", "--json"]));
    assert_eq!(pushed["commit"], tip.as_str(), "{pushed}");
    assert_eq!(pushed["forced"], false, "{pushed}");
    assert_eq!(origin_tip(), tip);

    // ^ A squash: one commit with main's tree and no parent. Without --force it refuses.
    let squash = fixture.git(&["commit-tree", "main^{tree}", "-m", "squash"]);
    let rewrite = ["task", "push", "-p", "demo", "--commit", &squash];
    refused(&rewrite, "a rewrite needs --force");
    // ^ A lease on a tip origin no longer has: the owner approved another state.
    let stale_lease = format!("--lease={first}");
    refused(
        &[&rewrite[..], &["--force", &stale_lease]].concat(),
        "since the push was asked",
    );
    assert_eq!(origin_tip(), tip);
    assert_eq!(fixture.git(&["rev-parse", "main"]), tip);

    // ^ The owner's uncommitted change to a file the move rewrites refuses it, before origin.
    fs::write(fixture.root.join("project/a.txt"), "mine\n").expect("edit a.txt");
    let other = fixture.git(&["commit-tree", &format!("{first}^{{tree}}"), "-m", "other"]);
    refused(
        &["task", "push", "-p", "demo", "--commit", &other, "--force"],
        "uncommitted changes in a.txt",
    );
    assert_eq!(origin_tip(), tip);
    fixture.git(&["checkout", "-q", "--", "a.txt"]);

    // ^ With the lease seen, both origin's and the local main are rewritten; the working tree
    // stays (git reset --keep).
    let lease = format!("--lease={tip}");
    let forced = answer(&fixture.wsx(&[&rewrite[..], &["--force", &lease, "--json"]].concat()));
    assert_eq!(forced["forced"], true, "{forced}");
    let message = forced["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&format!("forced over {}", &tip[..12])),
        "{forced}"
    );
    assert_eq!(origin_tip(), squash);
    assert_eq!(fixture.git(&["rev-parse", "main"]), squash);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
}
