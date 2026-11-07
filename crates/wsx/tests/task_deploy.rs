//! A failed deploy keeps the end of its output, and `wsx task deployed` shows it once its
//! detached run ends (docs/tasks.md "Deploy and release"); `wsx task push` moves origin's
//! default branch only as asked, and the manager's waiting requests replace one another and
//! run one at a time (docs/tasks.md "Push"). Drives the public CLI on an isolated
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

    /// `wsx` as the manager runs it: a push or a force push asks the owner.
    fn manager(&self, args: &[&str]) -> Output {
        self.command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .env("WSX_TASK_ACTOR", "manager")
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run wsx {args:?}: {error}"))
    }

    /// Make a commit on top of `parent` with one file, without moving any branch.
    fn commit_on(&self, parent: &str, file: &str) -> String {
        self.git(&["checkout", "-q", "--detach", parent]);
        fs::write(self.root.join("project").join(file), file).expect("write the file");
        self.git(&["add", file]);
        self.git(&["commit", "-q", "-m", file]);
        let commit = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "main"]);
        commit
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
        assert!(
            stderr.to_lowercase().contains("nothing was pushed"),
            "{stderr}"
        );
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

fn approvals_file(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|name| name == "approvals.jsonl")
        {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = approvals_file(&path) {
                return Some(found);
            }
        }
    }
    None
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn waiting_pushes_replace_one_another_and_run_one_at_a_time() {
    let fixture = Fixture::new("true");
    let origin = fixture.root.join("origin.git");
    let origin = origin.to_str().expect("UTF-8 path");
    fixture.git(&["init", "-q", "--bare", "--template=", "-b", "main", origin]);
    fixture.git(&["remote", "add", "origin", origin]);
    fixture.git(&["push", "-q", "origin", "main"]);
    let base = fixture.git(&["rev-parse", "main"]);
    let origin_tip = || fixture.git(&["--git-dir", origin, "rev-parse", "main"]);
    // ^ Commits that exist in the repository only: main stays at `base` until a push moves it.
    let older = fixture.commit_on(&base, "older.txt");
    let newer = fixture.commit_on(&older, "newer.txt");
    let other = fixture.commit_on(&base, "other.txt");
    let ask = |commit: &str, force: bool| {
        let mut args = vec!["task", "push", "-p", "demo", "--commit", commit, "--json"];
        if force {
            args.push("--force");
        }
        fixture.manager(&args)
    };
    let id_of = |output: &Output| -> String {
        assert!(output.status.success(), "{}", text(output));
        let answer: Value = serde_json::from_slice(&output.stdout).expect("request JSON");
        answer["approval"]
            .as_str()
            .expect("approval id")
            .to_string()
    };

    // ^ The newer request contains the older one: the older is dropped, and approving it
    // answers so and runs nothing.
    let first = id_of(&ask(&older, false));
    let second = id_of(&ask(&newer, false));
    let replaced = fixture.wsx(&["task", "approve", &first, "-p", "demo", "--json"]);
    assert!(!replaced.status.success(), "{}", text(&replaced));
    assert!(
        text(&replaced).contains(&format!("replaced by {second}; nothing ran")),
        "{}",
        text(&replaced)
    );
    assert_eq!(origin_tip(), base);

    // ^ Another line of history stays and the new request is refused, naming it; a rewrite may wait beside it.
    let refused = ask(&other, false);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains(&format!("push {second}")),
        "{}",
        text(&refused)
    );
    assert_eq!(origin_tip(), base);
    id_of(&ask(&other, true));

    // ^ Two requests that both wait, as an older wsx left them, run one at a time. A receive
    // hook that takes a second makes the overlap certain without the lock.
    let hook = Path::new(origin).join("hooks/pre-receive");
    fs::create_dir_all(hook.parent().unwrap()).expect("hooks folder");
    fs::write(&hook, "#!/bin/sh\nsleep 1\n").expect("write the hook");
    fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let file = approvals_file(&fixture.root.join("state")).expect("approvals.jsonl");
    let line = |id: &str, commit: &str| {
        format!(
            "{{\"kind\":\"request\",\"id\":\"{id}\",\"action\":\"push\",\"commit\":\"{commit}\",\"unix_ms\":1}}\n"
        )
    };
    fixture.git(&["--git-dir", origin, "update-ref", "refs/heads/main", &base]);
    let mut lines = fs::read_to_string(&file).expect("read approvals");
    lines += &line("a90", &older);
    lines += &line("a91", &newer);
    fs::write(&file, lines).expect("write approvals");
    let run = |id: &str| {
        fixture
            .command(Path::new(env!("CARGO_BIN_EXE_wsx")))
            .args(["task", "approve", id, "-p", "demo", "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn approve")
    };
    let (one, two) = (run("a90"), run("a91"));
    let (one, two) = (
        one.wait_with_output().unwrap(),
        two.wait_with_output().unwrap(),
    );
    for output in [&one, &two] {
        assert!(
            !text(output).contains("cannot lock ref"),
            "{}",
            text(output)
        );
    }
    assert_eq!(origin_tip(), newer, "{}{}", text(&one), text(&two));
}
