//! Batch land: several ready tasks rebase together and their combined tree is checked once.
//! A failed batch splits in halves until the failing task lands alone, through its own checks.
//! ref: docs/tasks.md "Batch land"

use super::host::run_check;
use super::slot::{self, Priority};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::task::store::{self, TaskDir};
use wsx_core::task::{call_host_waiting, copy, HostRequest, TaskRecord, TaskState};

/// A land rebases before it answers; one that runs its own checks answers sooner.
const LAND_TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(1);

/// Land `dirs` in their order: the first lands first. Runs detached (`task land-batch`),
/// its lines in the project's follow-up log.
pub(super) fn run(dirs: Vec<TaskDir>) -> Result<()> {
    let started = Instant::now();
    let first = dirs
        .first()
        .context("a batch land needs tasks")?
        .read_record()?;
    let main = first.project_path.clone();
    for orphan in copy::remove_orphans(&main, &["batch"]) {
        println!(
            "removed {}: the batch land that made it is gone",
            orphan.display()
        );
    }
    let scratch = copy::working_copy_path_named(&main, &format!("batch-{}", std::process::id()))?;
    let mut groups = VecDeque::from([dirs]);
    let mut runs = 0;
    let outcome = (|| -> Result<()> {
        while let Some(group) = groups.pop_front() {
            if let [dir] = group.as_slice() {
                land(dir, None)?;
                continue;
            }
            let tip = copy::resolve_commit(&main, &format!("refs/heads/{}", first.default_branch))?;
            if !scratch.exists() {
                copy::create_detached(&main, &tip, &scratch)?;
            }
            let (stacked, left) = stack(&scratch, &tip, &group)?;
            // ^ A task that does not stack lands alone after the others.
            groups.extend(left.into_iter().map(|dir| vec![dir]));
            if stacked.len() < 2 {
                groups.extend(stacked.into_iter().map(|(dir, _)| vec![dir]));
                continue;
            }
            runs += 1;
            if checks_pass(&scratch, &first, &stacked)? {
                for (dir, tree) in &stacked {
                    land(dir, Some(tree.clone()))?;
                }
                continue;
            }
            let half = stacked.len() / 2;
            let mut dirs: Vec<TaskDir> = stacked.into_iter().map(|(dir, _)| dir).collect();
            let upper = dirs.split_off(half);
            println!(
                "the checks failed on the batch: it splits into {} and {}",
                ids(&dirs),
                ids(&upper)
            );
            groups.push_front(upper);
            groups.push_front(dirs);
        }
        Ok(())
    })();
    if scratch.exists() {
        wsx_core::ops::cargo_clean(&scratch);
        if let Err(error) = copy::remove(&main, &scratch, None) {
            println!("cannot remove {}: {error:#}", scratch.display());
        }
    }
    println!(
        "batch land ended after {} ms and {runs} batch check run(s)",
        started.elapsed().as_millis()
    );
    outcome
}

fn ids(dirs: &[TaskDir]) -> String {
    let ids: Vec<String> = dirs.iter().map(|dir| format!("#{}", dir.id)).collect();
    ids.join(", ")
}

/// The tasks stacked with the tree after each, and the ones that did not stack.
type Stacked = (Vec<(TaskDir, String)>, Vec<TaskDir>);

/// Put each task's ready revision on `tip` in `scratch`, in order, and keep each one's tree.
/// A task whose revision does not apply on the ones before it comes back in the second list.
fn stack(scratch: &Path, tip: &str, group: &[TaskDir]) -> Result<Stacked> {
    git(
        scratch,
        &["checkout", "--quiet", "--force", "--detach", tip],
    )?;
    git(scratch, &["clean", "-fdq"])?;
    let (mut stacked, mut left) = (Vec::new(), Vec::new());
    for dir in group {
        let record = dir.read_record()?;
        let revision = format!("refs/wsx/task/{}/r{}", record.id, record.revision);
        let head = copy::resolve_commit(scratch, &revision)?;
        let base = copy::merge_base(scratch, &head, tip)?;
        if base == head {
            println!(
                "#{} has nothing that is not on the default branch",
                record.id
            );
            left.push(dir.clone());
            continue;
        }
        let range = format!("{base}..{head}");
        if git(scratch, &["cherry-pick", "--allow-empty", &range]).is_err() {
            let _ = git(scratch, &["cherry-pick", "--abort"]);
            println!("#{} does not apply on the batch: it lands alone", record.id);
            left.push(dir.clone());
            continue;
        }
        stacked.push((dir.clone(), copy::tree(scratch, "HEAD")?));
    }
    Ok((stacked, left))
}

/// Run the project's checks once on the stacked tree, in the check slot, as a land's.
fn checks_pass(scratch: &Path, first: &TaskRecord, stacked: &[(TaskDir, String)]) -> Result<bool> {
    let settings = load_task_config(&first.project_path).map_err(|error| anyhow::anyhow!(error))?;
    let dirs: Vec<TaskDir> = stacked.iter().map(|(dir, _)| dir.clone()).collect();
    let label = format!("{} batch {}", first.project, ids(&dirs));
    let queued = Instant::now();
    let _slot = slot::take(Priority::Land, &label, |reason| println!("{reason}"));
    let queued_ms = queued.elapsed().as_millis() as u64;
    let timeout = Duration::from_secs(settings.check_timeout_secs);
    let beside = store::follow_up_log_path(&first.project_id)?;
    let mut passed = true;
    for (index, command) in settings.checks.iter().enumerate() {
        let log = beside.with_file_name(format!("batch-check-{}.log", index + 1));
        let check = run_check(scratch, command, &settings.env, timeout, &log);
        println!(
            "batch {}: check `{command}`: {} in {} ms, queued {queued_ms} ms",
            ids(&dirs),
            check
                .exit
                .map_or("no exit code".into(), |code| format!("exit {code}")),
            check.ms
        );
        if check.exit != Some(0) {
            println!("{}", check.output);
            passed = false;
            break;
        }
    }
    Ok(passed)
}

/// Land one task and wait while its land's own checks run.
fn land(dir: &TaskDir, batch_tree: Option<String>) -> Result<()> {
    let request = HostRequest::Land {
        tried: false,
        batch_tree,
    };
    let response = match call_host_waiting(&dir.socket_path(), &request, LAND_TIMEOUT) {
        Ok(response) => response,
        Err(error) => {
            println!("#{}: no host answered: {error:#}", dir.id);
            return Ok(());
        }
    };
    println!("#{}: {}", dir.id, response.message);
    if !response.ok {
        return Ok(());
    }
    loop {
        let record = dir.read_record()?;
        if record.state != TaskState::Checking {
            println!("#{} is {}", dir.id, record.state.as_str());
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<()> {
    let output = wsx_core::git::git_cmd(repo)
        .args(args)
        .output()
        .context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
