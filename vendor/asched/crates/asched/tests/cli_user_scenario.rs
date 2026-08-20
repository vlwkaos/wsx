use serde_json::Value;
use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scenario {
    root: PathBuf,
    alpha: PathBuf,
    beta: PathBuf,
    daemon_may_have_started: Cell<bool>,
    cleanup_verified: Cell<bool>,
}

impl Scenario {
    fn new() -> Result<Self, String> {
        // ^ Keep the root short enough for the Unix-domain socket path limit.
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cu")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let alpha = root.join("working/alpha");
        let beta = root.join("working/beta");
        fs::create_dir_all(&alpha).map_err(|error| error.to_string())?;
        fs::create_dir_all(&beta).map_err(|error| error.to_string())?;
        Ok(Self {
            root,
            alpha,
            beta,
            daemon_may_have_started: Cell::new(false),
            cleanup_verified: Cell::new(false),
        })
    }

    fn run_json(&self, arguments: Vec<OsString>) -> Result<Value, String> {
        let output = Command::new(env!("CARGO_BIN_EXE_asched"))
            .args(arguments)
            .env("ASCHED_ROOT", &self.root)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "asched failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "invalid JSON output ({error}): {}",
                String::from_utf8_lossy(&output.stdout).trim()
            )
        })
    }

    fn verify_cleanup(&self) -> Result<(), String> {
        if !self.daemon_may_have_started.get() {
            self.cleanup_verified.set(true);
            return Ok(());
        }
        let stopped = self.run_json(args(&["daemon", "stop", "--json"]))?;
        require(stopped["result"] == "ok", "daemon stop")?;
        let socket = self.root.join("daemon-v1.sock");
        require(
            poll_bounded(40, || Ok(!socket.exists()))?,
            "daemon socket cleanup",
        )?;
        self.cleanup_verified.set(true);
        Ok(())
    }

    fn attempt_cleanup(&self) {
        if !self.daemon_may_have_started.get() || self.cleanup_verified.get() {
            return;
        }
        let _ = Command::new(env!("CARGO_BIN_EXE_asched"))
            .args(["daemon", "stop", "--json"])
            .env("ASCHED_ROOT", &self.root)
            .output();
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        self.attempt_cleanup();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn add_project(scenario: &Scenario, name: &str, path: &Path) -> Result<Value, String> {
    scenario.run_json(vec![
        "project".into(),
        "add".into(),
        name.into(),
        path.as_os_str().to_owned(),
        "--json".into(),
    ])
}

fn require(condition: bool, boundary: &str) -> Result<(), String> {
    condition
        .then_some(())
        .ok_or_else(|| format!("{boundary} returned an unexpected response"))
}

fn poll_bounded(
    attempts: usize,
    mut ready: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    let mut delay_ms = 5;
    for attempt in 0..attempts {
        if ready()? {
            return Ok(true);
        }
        if attempt + 1 < attempts {
            std::thread::sleep(Duration::from_millis(delay_ms));
            delay_ms = (delay_ms * 2).min(80);
        }
    }
    Ok(false)
}

fn run_boundaries(scenario: &Scenario) -> Result<(), String> {
    let added_alpha = add_project(scenario, "alpha", &scenario.alpha)?;
    require(
        added_alpha["projects"]
            .as_array()
            .is_some_and(|projects| projects.len() == 1),
        "project add",
    )?;
    add_project(scenario, "beta", &scenario.beta)?;

    let projects = scenario.run_json(args(&["project", "list", "--json"]))?;
    require(
        projects["projects"].as_array().is_some_and(|projects| {
            projects.len() == 2
                && projects.iter().any(|project| project["name"] == "alpha")
                && projects.iter().any(|project| project["name"] == "beta")
        }),
        "project list",
    )?;

    scenario.daemon_may_have_started.set(true);
    let added_routine = scenario.run_json(args(&[
        "routine",
        "add",
        "echo-once",
        "--project",
        "alpha",
        "--cron",
        "0 0 1 1 *",
        "--arg",
        "/bin/echo",
        "--arg",
        "contract-output",
        "--json",
    ]))?;
    require(
        added_routine["result"] == "ok" && added_routine["revision"] == 1,
        "routine add",
    )?;

    let routines = scenario.run_json(args(&["routine", "list", "--project", "alpha", "--json"]))?;
    require(
        routines.as_array().is_some_and(|rows| {
            rows.len() == 1
                && rows[0]["project"]["name"] == "alpha"
                && rows[0]["routines"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1)
                && rows[0]["routines"][0]["routine"]["name"] == "echo-once"
        }),
        "project-filtered routine list",
    )?;

    let added_event = scenario.run_json(args(&[
        "routine",
        "add",
        "event-once",
        "--project",
        "alpha",
        "--event",
        "filesystem.changed",
        "--arg",
        "/bin/echo",
        "--arg",
        "event-contract-output",
        "--json",
    ]))?;
    require(
        added_event["result"] == "ok" && added_event["revision"] == 2,
        "event routine add",
    )?;

    let event_routines = scenario.run_json(args(&[
        "routine",
        "list",
        "--project",
        "alpha",
        "--trigger",
        "event",
        "--event-kind",
        "filesystem.changed",
        "--json",
    ]))?;
    require(
        event_routines.as_array().is_some_and(|rows| {
            rows.len() == 1
                && rows[0]["routines"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1)
                && rows[0]["routines"][0]["routine"]["name"] == "event-once"
        }),
        "event-filtered routine list",
    )?;

    let fired = scenario.run_json(args(&[
        "routine",
        "fire",
        "--project",
        "alpha",
        "--kind",
        "filesystem.changed",
        "--event-id",
        "scenario-delivery",
        "--payload",
        "{\"path\":\"src/main.rs\"}",
        "--json",
    ]))?;
    require(
        fired["result"] == "fire"
            && fired["outcome"]["handled"]["routines"][0]["started"]["name"] == "event-once",
        "event routine fire",
    )?;

    let completed_event = poll_bounded(80, || {
        let logs = scenario.run_json(args(&[
            "routine",
            "logs",
            "event-once",
            "--project",
            "alpha",
            "--json",
        ]))?;
        Ok(logs["runs"].as_array().is_some_and(|runs| {
            runs.iter().any(|record| {
                record["status"] == "succeeded"
                    && record["cause"]["event"]["kind"] == "filesystem.changed"
                    && record["cause"]["event"]["event_id"] == "scenario-delivery"
            })
        }))
    })?;
    require(completed_event, "event run logs")?;

    let run = scenario.run_json(args(&[
        "routine",
        "run",
        "echo-once",
        "--project",
        "alpha",
        "--json",
    ]))?;
    require(run["result"] == "ok", "manual routine run")?;

    let completed_logs = poll_bounded(80, || {
        let logs = scenario.run_json(args(&[
            "routine",
            "logs",
            "echo-once",
            "--project",
            "alpha",
            "--json",
        ]))?;
        Ok(logs["runs"].as_array().is_some_and(|runs| {
            runs.iter().any(|record| {
                record["status"] == "succeeded"
                    && record["final_output"]
                        .as_str()
                        .is_some_and(|output| output.contains("contract-output"))
            })
        }))
    })?;
    require(completed_logs, "manual run logs")?;

    let disabled = scenario.run_json(args(&[
        "routine",
        "disable",
        "echo-once",
        "--project",
        "alpha",
        "--json",
    ]))?;
    require(
        disabled["result"] == "ok" && disabled["revision"] == 3,
        "routine disable",
    )?;

    let status = scenario.run_json(args(&["daemon", "status", "--json"]))?;
    require(
        status["result"] == "daemon"
            && status["protocol"].as_u64().is_some()
            && status["pid"].as_u64().is_some(),
        "daemon status",
    )?;

    Ok(())
}

fn run_user_scenario() -> Result<(), String> {
    let scenario = Scenario::new()?;
    let boundaries = run_boundaries(&scenario);
    let cleanup = scenario.verify_cleanup();
    match (boundaries, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(boundary), Ok(())) => Err(boundary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(boundary), Err(cleanup)) => Err(format!(
            "{boundary}; cleanup verification also failed: {cleanup}"
        )),
    }
}

#[test]
fn given_isolated_root_when_cli_lifecycle_runs_then_all_user_boundaries_succeed() {
    // ^ This scenario needs permission to spawn a daemon and create its Unix socket.
    assert_eq!(run_user_scenario(), Ok(()));
}
