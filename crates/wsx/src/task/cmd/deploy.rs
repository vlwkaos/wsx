//! `wsx task push`, `deploy`, `release`, `trial` and `deployed` (docs/tasks.md).

use super::approve::{ask_covering, request_of};
use crate::task::args::{DeployArgs, PushArgs};
use crate::task::open;
use crate::task::{
    acting_as_manager, actor, ask_host, not_for_roles, print_json, project_entry, report_response,
};
use crate::task::{host, hosting, trial};
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};
use wsx_core::config::project::load_task_config;
use wsx_core::git::{output_with_timeout_tail, ssh_agent};
use wsx_core::ops;
use wsx_core::task::checks::{self};
use wsx_core::task::ledger::{default_ref, deploy_status, kind_name};
use wsx_core::task::manager::{ApprovalAction, ApprovalLine};
use wsx_core::task::owner::{self};
use wsx_core::task::view::short_commit;
use wsx_core::task::{
    copy, now_unix_ms,
    store::{self, TaskDir},
    DeployKind, DeployRecord, HostRequest,
};

/// ^ A deploy or release longer than this is killed with its process group and recorded as
/// failed, so a hung command frees `deploy.lock`. wsx's own release takes about 30 minutes.
pub(crate) const DEPLOY_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// The remote a push goes to.
pub(crate) const PUSH_REMOTE: &str = "origin";

/// Push the default branch tip, or `commit`, to origin's default branch, then move the local
/// default branch there. A fast-forward of both, unless `force`: then it may rewrite them, with
/// `--force-with-lease` on origin's tip seen when it was asked (`lease`). The manager always
/// asks the owner (or names the owner's message); its broker runs this in the manager host,
/// outside the boundary, so the owner's ssh setup applies. ref: docs/tasks.md "Push"
pub(crate) fn push(args: PushArgs) -> Result<()> {
    let PushArgs {
        commit,
        force,
        lease,
        said,
        project,
        json,
    } = args;
    let (project, commit, lease, owner_said) = (
        project.as_deref(),
        commit.as_deref(),
        lease.as_deref(),
        said.owner_said,
    );
    not_for_roles("push")?;
    let (project_name, main) = project_entry(project)?;
    let project_id = store::project_id(&project_name, &main);
    let default = default_ref(&main);
    let branch = default.trim_start_matches("refs/heads/").to_string();
    let refuse = |error: anyhow::Error| anyhow::anyhow!("{error:#}; nothing was pushed");
    // ^ One approved push of a project at a time, and origin's tip is read under the lock: two
    // approved pushes ran together once and the older won the ref. A request waits for nothing.
    let asks = acting_as_manager() && owner_said.is_none();
    let _one_at_a_time = if asks {
        None
    } else {
        Some(store::lock_pushes(&project_id).map_err(refuse)?)
    };
    let local = copy::resolve_commit(&main, &default).map_err(refuse)?;
    let target = copy::resolve_commit(&main, commit.unwrap_or(&local)).map_err(refuse)?;
    let remote = copy::remote_tip(&main, PUSH_REMOTE, &branch).map_err(refuse)?;
    let contains =
        |ancestor: &str, commit: &str| copy::is_ancestor(&main, ancestor, commit).unwrap_or(false);
    // ^ A request leases what it saw; when origin moved since, the owner approved another state.
    if let Some(seen) = lease.filter(|seen| *seen != remote.as_deref().unwrap_or_default()) {
        bail!(
            "{PUSH_REMOTE} {branch} moved from {} to {} since the push was asked; ask again. Nothing was pushed",
            if seen.is_empty() { "none" } else { short_commit(seen) },
            remote.as_deref().map_or("none", short_commit)
        );
    }
    let remote_ff = remote.as_deref().is_none_or(|tip| contains(tip, &target));
    if let Some(tip) = remote
        .as_deref()
        .filter(|tip| !force && *tip != target && contains(&target, tip))
    {
        bail!(
            "{} is already on {PUSH_REMOTE} {branch}, which is at {}. Nothing was pushed",
            short_commit(&target),
            short_commit(tip)
        );
    }
    if !force && !remote_ff {
        bail!(
            "{} is not a fast-forward of {PUSH_REMOTE} {branch} ({}); a rewrite needs --force. Nothing was pushed",
            short_commit(&target),
            remote.as_deref().map_or("", short_commit)
        );
    }
    // ^ Local main moves to the pushed commit unless it has it already (lands since the ask).
    let local_moves = !contains(&target, &local);
    let local_ff = contains(&local, &target);
    let rewrite = local_moves && !local_ff;
    if rewrite && !force {
        bail!(
            "local {branch} ({}) is not an ancestor of {}; a rewrite needs --force. Nothing was pushed",
            short_commit(&local),
            short_commit(&target)
        );
    }
    if local_moves {
        copy::check_move(&main, &branch, &target, "push").map_err(refuse)?;
    }
    // ^ A lease that is not origin's tip was refused above, so the push leases what it saw now.
    let lease = force.then(|| remote.clone().unwrap_or_default());
    if acting_as_manager() {
        // ^ The request says what it carries: the landed tasks and the commits origin lacks.
        let mut request = request_of(
            ApprovalAction::Push,
            None,
            Some(&target),
            None,
            lease.as_deref(),
        );
        if let (ApprovalLine::Request { tasks, commits, .. }, Ok((found, count))) = (
            &mut request,
            owner::carried(&project_id, &main, remote.as_deref(), &target),
        ) {
            *tasks = found;
            *commits = Some(count);
        }
        if ask_covering(&main, &project_id, request, owner_said, json)? {
            return Ok(());
        }
    }
    // ^ A rewrite leaves open tasks on commits the new branch lacks: name them.
    let mut old_base = Vec::new();
    if rewrite {
        for id in store::task_numbers(&project_id)? {
            let Ok(record) = TaskDir::open(&project_id, id).and_then(|dir| dir.read_record())
            else {
                continue;
            };
            if record.state.is_open()
                && record.default_branch == branch
                && !contains(&record.base, &target)
            {
                old_base.push(id);
            }
        }
    }
    copy::push(&main, PUSH_REMOTE, &branch, &target, lease.as_deref()).map_err(refuse)?;
    if local_moves {
        copy::move_branch(&main, &branch, &local, &target, rewrite).map_err(|error| {
            anyhow::anyhow!(
                "pushed {} to {PUSH_REMOTE} {branch}, but local {branch} stayed at {}: {error:#}",
                short_commit(&target),
                short_commit(&local)
            )
        })?;
    }
    let mut message = format!(
        "pushed {branch} to {PUSH_REMOTE}: {}",
        short_commit(&target)
    );
    if let Some(was) = remote.as_deref().filter(|was| !contains(was, &target)) {
        message += &format!(" (forced over {})", short_commit(was));
    }
    if rewrite {
        message += &format!("; local {branch} was {}", short_commit(&local));
    }
    if !old_base.is_empty() {
        let tasks: Vec<String> = old_base.iter().map(|id| format!("#{id}")).collect();
        message += &format!("; open tasks on the old {branch}: {}", tasks.join(", "));
    }
    if json {
        return print_json(&json!({
            "pushed": true,
            "remote": PUSH_REMOTE,
            "branch": branch,
            "commit": target,
            "remote_was": remote,
            "local_was": local,
            "forced": force,
            "old_base_tasks": old_base,
            "message": message,
        }));
    }
    println!("{message}");
    Ok(())
}

/// Run the project's deploy or release command in a temporary copy of one commit.
/// ^ Never from a task's working copy: what runs is always a default-branch commit.
/// `if_new`: skip a commit that is already deployed, so the deploys that follow quick lands
/// run once for the newest one (docs/tasks.md "Deploy and release").
/// One by hand checks, takes the lock and answers at once: a detached `wsx task` holding the
/// lock (`run_locked`) runs and records it, so no caller's time limit (the manager's broker)
/// cuts a long release.
pub(crate) fn deploy(kind: DeployKind, args: DeployArgs) -> Result<()> {
    let DeployArgs {
        commit,
        if_new,
        trial,
        run,
        said,
        project,
        json,
    } = args;
    let (project, commit) = (project.as_deref(), commit.as_deref());
    let (run_locked, owner_said) = (run.run_locked, said.owner_said);
    let by_hand = !if_new && trial.is_none() && run_locked.is_none();
    let name = kind_name(kind);
    not_for_roles(name)?;
    let (project_name, main) = project_entry(project)?;
    let settings =
        load_task_config(&main).map_err(|error| anyhow::anyhow!("{error}; nothing was run"))?;
    let (command, public, approve, action) = match kind {
        DeployKind::Deploy => (
            settings.deploy_command.clone(),
            settings.deploy_public,
            settings.deploy_approve,
            ApprovalAction::Deploy,
        ),
        DeployKind::Release => (
            settings.release_command.clone(),
            true,
            settings.release_approve,
            ApprovalAction::Release,
        ),
    };
    let command = command.with_context(|| {
        format!("tasks.{name}.command is not set in wsx.config.yml; nothing was run")
    })?;
    let project_id = store::project_id(&project_name, &main);
    // ^ Only public output waits for what the owner has to check, and a release the owner's
    // message asked for runs: some items can be checked only on the released build.
    // ref: docs/tasks.md "Deploy and release"
    let open = checks::open_items(&project_id)?;
    let to_check = open.len();
    if public && to_check > 0 && run_locked.is_none() {
        match (kind, owner_said) {
            (DeployKind::Release, Some(said)) => owner::check_owner_said(&project_id, said)?,
            _ => bail!(
                "{to_check} item(s) to check are open; the owner checks them first (`wsx task check`). Nothing was run"
            ),
        }
    }
    if approve
        && run_locked.is_none()
        && acting_as_manager()
        && ask_covering(
            &main,
            &project_id,
            request_of(action, None, commit, None, None),
            owner_said,
            json,
        )?
    {
        return Ok(());
    }
    // ^ One deploy or release at a time per project: one by hand refuses while another runs,
    // and one that a land or a trial started waits here.
    let running = match run_locked {
        Some(fd) => store::adopt_deploy_lock(&project_id, fd)?,
        None if by_hand => store::try_lock_deploys(&project_id)?.with_context(|| {
            format!("a deploy or release of {project_name} runs; its result arrives in `wsx task deployed`. Nothing was run")
        })?,
        None => store::lock_deploys(&project_id)?,
    };
    // ^ While a trial holds the live site, only its own deploy runs; the default branch
    // deploys when it ends. ref: docs/tasks.md "Hold and trial"
    let live_trial = store::read_trial(&project_id)?;
    // ^ Each answer is JSON with `--json`, else its message.
    let answer = |value: serde_json::Value| -> Result<()> {
        if json {
            return print_json(&value);
        }
        println!("{}", value["message"].as_str().unwrap_or_default());
        Ok(())
    };
    let skip =
        |message: String| answer(json!({ "kind": kind, "skipped": true, "message": message }));
    let revision = match (trial, &live_trial) {
        (Some(task), Some(live)) if live.task == task && live.commit.is_some() => {
            live.commit.clone().unwrap_or_default()
        }
        (Some(task), _) => return skip(format!("the trial of #{task} ended; nothing was run")),
        (None, Some(live)) if if_new => {
            return skip(format!(
                "the live site runs the trial of #{}; the default branch deploys when it ends",
                live.task
            ))
        }
        (None, Some(live)) => bail!(
            "the live site runs the trial of #{task}; end it first (`wsx task trial {task} --end`). Nothing was run",
            task = live.task
        ),
        (None, None) => match commit {
            Some(commit) => commit.to_string(),
            None => default_ref(&main),
        },
    };
    if if_new {
        let tip = copy::resolve_commit(&main, &revision)?;
        let deployed = store::read_deploys(&project_id)?
            .into_iter()
            .rev()
            .find(|record| record.kind == kind && record.exit == Some(0))
            .is_some_and(|record| record.commit == tip);
        if deployed {
            let message = format!("{name} of {tip:.12}: already deployed");
            return answer(
                json!({ "kind": kind, "commit": tip, "skipped": true, "message": message }),
            );
        }
    }
    let check_suffix = match kind {
        _ if to_check == 0 => String::new(),
        DeployKind::Deploy => format!("; {to_check} to check on the deployed site"),
        DeployKind::Release => format!(
            "; still to check on the release: {}",
            open.iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    if by_hand {
        let commit = copy::resolve_commit(&main, &revision)?;
        let log = store::follow_up_log(&project_id)?;
        let fd = running.as_raw_fd().to_string();
        let args = [
            "task",
            name,
            "-p",
            &project_name,
            "--commit",
            &commit,
            "--run-locked",
            &fd,
            "--json",
        ];
        hosting::spawn_wsx(&args, log, Some(&running))?;
        let message = format!(
            "{name} of {commit:.12} started; its output goes to {}, its result to `wsx task deployed`{}",
            store::follow_up_log_path(&project_id)?.display(),
            check_suffix
        );
        return answer(
            json!({ "kind": kind, "commit": commit, "started": true, "message": message }),
        );
    }
    for orphan in copy::remove_orphans(&main, &["deploy", "release"]) {
        eprintln!("removed {}: the run that made it is gone", orphan.display());
    }
    let path = copy::working_copy_path_named(&main, &format!("{name}-{}", std::process::id()))?;
    let commit = copy::create_detached(&main, &revision, &path)
        .map_err(|error| error.context(format!("nothing was run or recorded for this {name}")))?;
    let started = Instant::now();
    // ^ The output's tail is kept like a check's: a failed deploy records it for the ledger.
    // ^ Like wsx's own push, the command's git and ssh reach the login session's agent when
    // this host's environment has none that answers. ref: docs/tasks.md "Push"
    let (agent, no_key) = ssh_agent();
    let mut process = std::process::Command::new("/bin/sh");
    process
        .args(["-c", &command])
        .current_dir(&path)
        .env("WSX_DEPLOY_COMMIT", &commit)
        .stdin(std::process::Stdio::null());
    if let Some(socket) = agent {
        process.env("SSH_AUTH_SOCK", socket);
    }
    let ran = output_with_timeout_tail(&mut process, DEPLOY_TIMEOUT, host::CHECK_OUTPUT_BYTES);
    let duration_ms = started.elapsed().as_millis() as u64;
    ops::cargo_clean(&path);
    let removed = copy::remove(&main, &path, None);
    let (status, mut output) = match &ran {
        Ok(output) => (Some(output.status), host::output_text(output)),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => (
            None,
            format!("timed out after {} s", DEPLOY_TIMEOUT.as_secs()),
        ),
        Err(error) => (None, format!("cannot run tasks.{name}.command: {error}")),
    };
    if status.is_some_and(|status| !status.success())
        && output.contains("Permission denied (publickey")
    {
        output = format!("{output}\n{no_key}");
    }
    // ^ JSON callers parse stdout, so the command's output goes to stderr then.
    if !output.is_empty() {
        if json {
            eprintln!("{output}");
        } else {
            println!("{output}");
        }
    }
    let exit = status.and_then(|status| status.code());
    let succeeded = status.is_some_and(|status| status.success());
    let record = DeployRecord {
        kind,
        commit: commit.clone(),
        exit,
        unix_ms: now_unix_ms(),
        duration_ms,
        trial,
        output: (!succeeded).then(|| output.clone()),
    };
    store::append_deploy(&project_id, &record)?;
    // ^ A trial whose deploy failed ends, and the default branch deploys again.
    if let (Some(task), false) = (trial, succeeded) {
        trial::end(
            &project_id,
            &project_name,
            Some(task),
            "its deploy failed",
            false,
        )?;
    }
    removed.with_context(|| {
        format!(
            "the {name} was recorded, but its copy {} remains",
            path.display()
        )
    })?;
    let Some(status) = status else {
        bail!("tasks.{name}.command: {output}; recorded with no exit code");
    };
    let mut message = format!(
        "{name} of {commit:.12}: exit {}",
        exit.map_or("none".into(), |code| code.to_string())
    );
    if status.success() {
        message.push_str(&check_suffix);
    }
    let mut value = serde_json::to_value(&record)?;
    value["message"] = json!(message);
    answer(value)?;
    if !status.success() {
        bail!("tasks.{name}.command failed ({status}); the failure is recorded and nothing else changed");
    }
    Ok(())
}

/// `wsx task trial <n>`: take the project's trial slot and have the host put the ready
/// revision on the live site; `--end` frees the slot and deploys the default branch again.
/// ref: docs/tasks.md "Hold and trial"
pub(crate) fn trial_command(
    task: &str,
    end: bool,
    project: Option<&str>,
    json: bool,
) -> Result<()> {
    not_for_roles("trial")?;
    let dir = open(project, task)?;
    let record = dir.read_record()?;
    if end {
        let ended = trial::end(
            &dir.project_id,
            &record.project,
            Some(record.id),
            "ended without a decision",
            false,
        )?;
        let message = match ended {
            Some(_) => "the trial ended; the default branch deploys again",
            None => "no trial of this task runs",
        };
        if json {
            return print_json(&json!({ "ended": ended.is_some(), "message": message }));
        }
        println!("t{} {message}", record.id);
        return Ok(());
    }
    let settings = load_task_config(&record.project_path)
        .map_err(|error| anyhow::anyhow!("{error}; nothing was tried"))?;
    if settings.deploy_command.is_none() {
        bail!("tasks.deploy.command is not set: there is no live site to try it on");
    }
    if settings.deploy_public {
        bail!("tasks.deploy.public is on: unlanded work does not go public; try it on a preview");
    }
    // ^ A trial changes the live site: a manager that may not deploy alone asks the owner
    // to start it.
    if acting_as_manager() && settings.deploy_approve {
        bail!("tasks.deploy.approve is on: ask the owner to start the trial (Try live)");
    }
    if !trial::claim(&record, actor())? {
        let trial = store::read_trial(&dir.project_id)?;
        let message = format!(
            "the trial of t{} already runs{}",
            record.id,
            trial
                .and_then(|trial| trial.commit)
                .map_or(String::new(), |commit| format!(" at {commit:.12}"))
        );
        if json {
            return print_json(&json!({ "ok": true, "state": record.state, "message": message }));
        }
        println!("{message}");
        return Ok(());
    }
    let response = ask_host(&dir, &HostRequest::Trial);
    // ^ A host that refused or did not answer leaves the slot free again.
    if !response.as_ref().is_ok_and(|response| response.ok) {
        let _ = store::change_trial(&dir.project_id, |current| match current {
            Some(trial) if trial.task == record.id && trial.commit.is_none() => (None, ()),
            other => (other, ()),
        });
    }
    report_response(&dir, response?, json)
}

/// The last successful deploy and release and how many default-branch commits each lacks.
pub(crate) fn deployed(project: Option<&str>, json: bool) -> Result<()> {
    let (project_name, main) = project_entry(project)?;
    let settings = load_task_config(&main).map_err(|error| anyhow::anyhow!(error))?;
    let mut out = deploy_status(&project_name, &main, &settings)?;
    let default = out["default_branch"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    out["project"] = json!(project_name);
    if json {
        return print_json(&out);
    }
    for kind in [DeployKind::Deploy, DeployKind::Release] {
        let entry = &out[kind_name(kind)];
        match entry["commit"].as_str() {
            Some(commit) => println!(
                "{:<8} {:.12}  {} commit(s) behind {default}",
                kind_name(kind),
                commit,
                entry["behind"]
            ),
            None => println!("{:<8} none", kind_name(kind)),
        }
    }
    Ok(())
}
