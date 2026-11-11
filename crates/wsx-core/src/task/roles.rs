//! Roles: the manager host with a name, a persistent conversation with its own brief and
//! notes in `tasks/<project-id>/roles/<name>/`. wsx guards their number, not the prompt.
//! ref: docs/tasks.md "Roles"

use super::manager::{ApprovalAction, ApprovalLine};
use super::now_unix_ms;
use super::store::{self, ManagerDir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The role's instructions, besides the manager's; a new brief starts a fresh conversation.
pub const BRIEF_FILE: &str = "brief.md";
/// Written by `role close` and by wsx after `tasks.roles.closeDays` without a turn; the
/// notes and the log stay.
pub const CLOSED_FILE: &str = "closed";
/// Who opened the role and the agent it runs on: `{by, adapter?, model?}`.
pub const ROLE_FILE: &str = "role.json";
pub const MAX_BRIEF_BYTES: usize = 8 * 1024;
const MAX_NAME_CHARS: usize = 24;
/// After the owner denies a role, `role new` of that name refuses for this long.
pub const DENIED_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// How much of a role's log is read for its last turn.
const TAIL_BYTES: u64 = 64 * 1024;

/// A role's name: a lowercase letter, then lowercase letters, digits and `-`, as the end of
/// its session label `task-<name>`; never `manager`.
pub fn checked_name(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.len() <= MAX_NAME_CHARS;
    if !valid {
        bail!("a role's name is a lowercase letter, then letters, digits or -, at most {MAX_NAME_CHARS}: {name:?}");
    }
    if name == "manager" {
        bail!("the manager is not a role to create");
    }
    Ok(name)
}

/// The names of the project's role folders, open or closed, sorted.
pub fn names(project_id: &str) -> Vec<String> {
    let Ok(entries) = store::roles_dir(project_id).and_then(|dir| Ok(std::fs::read_dir(dir)?))
    else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| checked_name(name).is_ok())
        .collect();
    names.sort();
    names
}

/// A role with a brief that is not closed.
pub fn is_open(dir: &ManagerDir) -> bool {
    dir.role.is_some()
        && dir.path.join(BRIEF_FILE).is_file()
        && !dir.path.join(CLOSED_FILE).exists()
}

/// The open roles' folders, by name.
pub fn open_roles(project_id: &str) -> Vec<ManagerDir> {
    names(project_id)
        .iter()
        .filter_map(|name| {
            ManagerDir::existing_as(project_id, Some(name))
                .ok()
                .flatten()
        })
        .filter(is_open)
        .collect()
}

/// The open role of that name, or an error that says why there is none.
pub fn open_role(project_id: &str, name: &str) -> Result<ManagerDir> {
    match ManagerDir::existing_as(project_id, Some(name))? {
        Some(dir) if is_open(&dir) => Ok(dir),
        _ => bail!("the project has no open role {name}; `wsx task role list` names them"),
    }
}

pub fn brief(dir: &ManagerDir) -> String {
    store::read_bounded_if_exists(&dir.path.join(BRIEF_FILE), MAX_BRIEF_BYTES as u64)
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// A brief as given: UTF-8 text, not empty, at most 8 KiB.
pub fn checked_brief(brief: &str) -> Result<&str> {
    if brief.trim().is_empty() {
        bail!("the role's brief is empty");
    }
    if brief.len() > MAX_BRIEF_BYTES {
        bail!("the role's brief is larger than {MAX_BRIEF_BYTES} bytes");
    }
    Ok(brief)
}

/// Who opened a role. The owner's own `role new` opens a role that wsx never closes by
/// itself; the manager's request opens one that `tasks.roles.closeDays` closes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Opener {
    Owner,
    #[default]
    Manager,
}

/// A role's own agent, `role new --adapter --model`; a role with none runs the manager's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleAgent {
    pub adapter: String,
    pub model: String,
}

impl RoleAgent {
    /// `--adapter` (default `claude`) and `--model`, as `task new --model` checks them. The
    /// adapter needs its model: a claude model name does not fit pi.
    pub fn checked(adapter: Option<&str>, model: Option<&str>) -> Result<Option<Self>> {
        let Some(model) = model else {
            if adapter.is_some() {
                bail!("--adapter needs --model: a model of the manager's adapter does not fit another");
            }
            return Ok(None);
        };
        let adapter = adapter.unwrap_or("claude");
        if !crate::config::global::TASK_ADAPTERS.contains(&adapter) {
            bail!(
                "--adapter must be one of: {}",
                crate::config::global::TASK_ADAPTERS.join(", ")
            );
        }
        Ok(Some(Self {
            adapter: adapter.to_string(),
            model: super::view::checked_model(model)?,
        }))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RoleFile {
    #[serde(default)]
    by: Opener,
    #[serde(default, flatten, skip_serializing_if = "Option::is_none")]
    agent: Option<RoleAgent>,
}

fn role_file(dir: &ManagerDir) -> RoleFile {
    store::read_bounded_if_exists(&dir.path.join(ROLE_FILE), 4096)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The role's own agent, none for a role that runs the manager's.
pub fn agent(dir: &ManagerDir) -> Option<RoleAgent> {
    role_file(dir).agent
}

/// Open the role with this brief: a new folder, or a closed one again with its notes.
pub fn create(
    project_id: &str,
    name: &str,
    brief: &str,
    by: Opener,
    agent: Option<RoleAgent>,
) -> Result<ManagerDir> {
    let dir = ManagerDir::open_as(project_id, Some(checked_name(name)?))?;
    store::write_atomic(&dir.path.join(BRIEF_FILE), checked_brief(brief)?.as_bytes())?;
    store::write_atomic(
        &dir.path.join(ROLE_FILE),
        &serde_json::to_vec(&RoleFile { by, agent })?,
    )?;
    match std::fs::remove_file(dir.path.join(CLOSED_FILE)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("cannot open the closed role again")
        }
        _ => {}
    }
    Ok(dir)
}

/// Close a role; its host sees the file and leaves. The notes and the log stay.
pub fn close(dir: &ManagerDir, why: &str) -> Result<()> {
    store::write_atomic(&dir.path.join(CLOSED_FILE), why.as_bytes())
}

/// When the role last had anything in its conversation, else when its brief was written.
pub fn last_active_ms(dir: &ManagerDir) -> u64 {
    let logged = dir
        .read_tail(TAIL_BYTES)
        .ok()
        .and_then(|entries| entries.last().map(|entry| entry.unix_ms));
    logged.unwrap_or_else(|| modified_ms(&dir.path.join(BRIEF_FILE)))
}

fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as u64)
}

/// Close each open role the manager opened with no turn for `close_secs`; their names. A role
/// the owner opened stays open until `role close`. ref: docs/tasks.md "Roles"
pub fn close_idle(project_id: &str, close_secs: u64) -> Vec<String> {
    let now = now_unix_ms();
    open_roles(project_id)
        .into_iter()
        .filter(|dir| role_file(dir).by == Opener::Manager)
        .filter(|dir| last_active_ms(dir) + close_secs * 1000 <= now)
        .filter(|dir| close(dir, "closed by wsx: no turn for tasks.roles.closeDays").is_ok())
        .map(|dir| dir.name().to_string())
        .collect()
}

/// The pending requests to create a role, by name.
pub fn asked(lines: &[ApprovalLine]) -> Vec<(&str, &str)> {
    super::manager::pending_approvals(lines)
        .into_iter()
        .filter_map(|line| match line {
            ApprovalLine::Request {
                id,
                action: ApprovalAction::Role,
                role: Some(role),
                ..
            } => Some((id.as_str(), role.as_str())),
            _ => None,
        })
        .collect()
}

/// The asked roles as rows; the agent the request names shows as an open role's does.
fn asked_rows(lines: &[ApprovalLine]) -> Vec<serde_json::Value> {
    super::manager::pending_approvals(lines)
        .into_iter()
        .filter_map(|line| match line {
            ApprovalLine::Request {
                id,
                action: ApprovalAction::Role,
                role: Some(role),
                adapter,
                model,
                ..
            } => {
                let mut row = serde_json::json!({ "name": role, "state": "asked", "approval": id });
                if let (Some(adapter), Some(model)) = (adapter, model) {
                    row["adapter"] = adapter.as_str().into();
                    row["model"] = model.as_str().into();
                }
                Some(row)
            }
            _ => None,
        })
        .collect()
}

/// The roles as rows for the ledger, `role list` and the views: each folder with a brief,
/// open or closed, by name, then each one asked for: `{name, state, brief, last_unix_ms}` or
/// `{name, state: "asked", approval, adapter?, model?}`. `brief` is the brief's first line;
/// `by` is `owner` or `manager`; `adapter` and `model` are the role's own agent, left out for
/// a role on the manager's.
pub fn rows(project_id: &str, lines: &[ApprovalLine]) -> Vec<serde_json::Value> {
    let mut rows: Vec<serde_json::Value> = names(project_id)
        .iter()
        .filter_map(|name| ManagerDir::existing_as(project_id, Some(name)).ok()?)
        .filter(|dir| dir.path.join(BRIEF_FILE).is_file())
        .map(|dir| {
            let brief = brief(&dir);
            let file = role_file(&dir);
            let mut row = serde_json::json!({
                "name": dir.name(),
                "state": if is_open(&dir) { "open" } else { "closed" },
                "brief": brief.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default(),
                "last_unix_ms": last_active_ms(&dir),
                "by": file.by,
            });
            if let Some(agent) = file.agent {
                row["adapter"] = agent.adapter.into();
                row["model"] = agent.model.into();
            }
            row
        })
        .collect();
    rows.extend(asked_rows(lines));
    rows
}

/// Until when the owner's last denial of a role of this name refuses a new one.
fn denied_until(lines: &[ApprovalLine], name: &str, now: u64) -> Option<u64> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| match line {
            ApprovalLine::Decision {
                approved: false,
                outcome,
                unix_ms,
                ..
            } if !outcome.starts_with(super::owner::DROPPED) => Some((at, *unix_ms)),
            _ => None,
        })
        .filter(|(at, _)| {
            matches!(super::manager::decided_request(lines, *at), Some(ApprovalLine::Request {
                action: ApprovalAction::Role, role: Some(role), ..
            }) if role == name)
        })
        .map(|(_, unix_ms)| unix_ms + DENIED_MS)
        .filter(|until| *until > now)
        .max()
}

/// Whether `role new <name>` may go on: wsx's guard against too many roles, enforced here
/// and not by the prompt. Refuses a role that is open or asked for, a name the owner denied
/// in the last 7 days, and a project at its `tasks.roles.max` (open roles and pending
/// requests count). ref: docs/tasks.md "Roles"
pub fn may_create(project_id: &str, name: &str, max: usize) -> Result<()> {
    checked_name(name)?;
    let lines = store::read_approvals(project_id)?;
    let asked = asked(&lines);
    if let Some((id, _)) = asked.iter().find(|(_, role)| *role == name) {
        bail!("role {name} is already asked for ({id}); the owner decides it");
    }
    let open = open_roles(project_id);
    if open.iter().any(|dir| dir.name() == name) {
        bail!("role {name} is already open");
    }
    let now = now_unix_ms();
    if let Some(until) = denied_until(&lines, name, now) {
        bail!(
            "the owner denied role {name}; it cannot be asked for again for {} more hour(s)",
            (until - now).div_ceil(3_600_000)
        );
    }
    let count = open.len() + asked.len();
    if count >= max {
        let names: Vec<String> = open
            .iter()
            .map(|dir| dir.name().to_string())
            .chain(
                asked
                    .iter()
                    .map(|(id, role)| format!("{role} ({id}, asked)")),
            )
            .collect();
        bail!(
            "the project has {count} of tasks.roles.max {max} roles ({}); close one first with `wsx task role close <name>`",
            match names.is_empty() {
                true => "none".to_string(),
                false => names.join(", "),
            }
        );
    }
    Ok(())
}
