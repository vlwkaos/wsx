//! Which worktrees and sessions of a project are stale, and the one safe action for each.
//! The manager host's prune, the web's and the TUI's worktree delete read this rule alone.
//! ref: docs/tasks.md "Stale worktrees"
//!
//! A worktree is never judged when it is the main checkout (its shell session stays), an open
//! task's working copy, or holds a task's session, a session with a live agent turn or a
//! program in the foreground, or a clock wsxd does not give (an older daemon, or git that did
//! not answer): not knowing is not stale.

use crate::git::{git_cmd, output_with_timeout};
use crate::ops;
use crate::runtime::{AgentState, Client, Request, Response, SessionId, Snapshot};
use crate::task::{copy::READ_TIMEOUT, ledger::default_ref, now_unix_ms, session_news};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DAY_MS: u64 = 86_400_000;

/// The most dirty paths a loss names.
const NAMED_PATHS: usize = 5;

/// How long a session or a worktree may stand idle, in ms; 0 turns that action off.
/// `tasks.prune.idleDays` closes idle sessions, `tasks.prune.removeDays` removes the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    pub idle_ms: u64,
    pub remove_ms: u64,
}

impl Rules {
    pub fn off(&self) -> bool {
        self.idle_ms == 0 && self.remove_ms == 0
    }
}

/// One session of a worktree as the rule sees it.
#[derive(Debug, Clone)]
pub struct SessionFact {
    pub id: SessionId,
    pub revision: u64,
    /// Ms since a pane of it last printed or got input; none when wsxd does not say.
    pub idle_ms: Option<u64>,
    /// An agent works or waits for an answer, or a program runs in the foreground.
    pub busy: bool,
    pub task: bool,
}

/// What the rule needs to know of one worktree.
#[derive(Debug, Clone)]
pub struct Facts {
    pub path: PathBuf,
    pub branch: String,
    pub sessions: Vec<SessionFact>,
    /// Ms since git last moved the worktree's HEAD (it was made, committed on, switched).
    pub git_idle_ms: Option<u64>,
}

/// What a delete of the worktree loses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loss {
    /// The first dirty paths, tracked or not.
    pub dirty: Vec<String>,
    pub dirty_total: usize,
    /// Commits of the branch that are neither in the default branch nor on any remote.
    pub unmerged: usize,
}

impl Loss {
    pub fn is_empty(&self) -> bool {
        self.dirty_total == 0 && self.unmerged == 0
    }

    /// `3 uncommitted files (a, b, c) and 2 commits that are not merged or pushed`.
    pub fn words(&self) -> String {
        let mut parts = Vec::new();
        if self.dirty_total > 0 {
            let more = self.dirty_total.saturating_sub(self.dirty.len());
            parts.push(format!(
                "{} uncommitted file{} ({}{})",
                self.dirty_total,
                plural(self.dirty_total),
                self.dirty.join(", "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            ));
        }
        if self.unmerged > 0 {
            parts.push(format!(
                "{} commit{} that {} not merged or pushed",
                self.unmerged,
                plural(self.unmerged),
                if self.unmerged == 1 { "is" } else { "are" }
            ));
        }
        parts.join(" and ")
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// The sessions to close: idle for `idle_ms`, none of them busy or a task's. A worktree that
/// holds a task's session, or a session with no clock, is left whole.
pub fn idle_sessions(facts: &Facts, rules: &Rules) -> Vec<SessionFact> {
    if rules.idle_ms == 0 || facts.sessions.iter().any(|session| session.task) {
        return Vec::new();
    }
    facts
        .sessions
        .iter()
        .filter(|session| {
            !session.busy && session.idle_ms.is_some_and(|idle| idle >= rules.idle_ms)
        })
        .cloned()
        .collect()
}

/// Ms the worktree has stood idle: its newest session, else git's last move. None: not known.
pub fn worktree_idle(facts: &Facts) -> Option<u64> {
    if facts.sessions.is_empty() {
        return facts.git_idle_ms;
    }
    facts
        .sessions
        .iter()
        .map(|session| session.idle_ms)
        .collect::<Option<Vec<u64>>>()?
        .into_iter()
        .min()
}

/// What happens to the worktree itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Keep,
    /// Clean, merged or pushed, and idle past `removeDays`.
    Remove,
    /// Idle past `removeDays` but not safe to remove: the owner decides, with this loss.
    Flag(Loss),
}

/// The end of a worktree. `loss` is asked only when the worktree is idle enough to matter.
pub fn end(facts: &Facts, rules: &Rules, loss: impl FnOnce() -> Option<Loss>) -> End {
    if rules.remove_ms == 0 || facts.sessions.iter().any(|s| s.task || s.busy) {
        return End::Keep;
    }
    let past = rules.remove_ms.max(rules.idle_ms);
    if worktree_idle(facts).is_none_or(|idle| idle < past) {
        return End::Keep;
    }
    match loss() {
        Some(loss) if loss.is_empty() => End::Remove,
        Some(loss) => End::Flag(loss),
        None => End::Keep,
    }
}

/// What a delete of `path` on `branch` loses; none when git did not answer. `branch` `HEAD`
/// is a detached worktree, judged by its own HEAD.
pub fn loss(main: &Path, path: &Path, branch: &str) -> Option<Loss> {
    let read = |dir: &Path, args: &[&str]| {
        output_with_timeout(git_cmd(dir).args(args), READ_TIMEOUT)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    let status = read(path, &["status", "--porcelain"])?;
    let paths: Vec<String> = status
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| line[3..].to_string())
        .collect();
    let default = default_ref(main);
    let counted = if branch == "HEAD" {
        read(
            path,
            &[
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                &default,
                "--remotes",
            ],
        )
    } else {
        let tip = format!("refs/heads/{branch}");
        read(
            main,
            &["rev-list", "--count", &tip, "--not", &default, "--remotes"],
        )
    }?;
    Some(Loss {
        dirty_total: paths.len(),
        dirty: paths.into_iter().take(NAMED_PATHS).collect(),
        unmerged: counted.trim().parse().ok()?,
    })
}

/// Ms since git last moved the worktree's HEAD, from its own git folder.
fn git_idle(path: &Path, now_ms: u64) -> Option<u64> {
    let output = output_with_timeout(
        git_cmd(path).args(["rev-parse", "--absolute-git-dir"]),
        READ_TIMEOUT,
    )
    .ok()?;
    let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    ["HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok()?.modified().ok())
        .filter_map(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_millis() as u64)
        .max()
        .map(|newest| now_ms.saturating_sub(newest))
}

/// The facts of one worktree from wsxd's snapshot and git.
pub fn facts(snapshot: &Snapshot, path: &Path, branch: &str, now_ms: u64) -> Facts {
    let worktree = snapshot.worktrees.iter().find(|w| same(&w.path, path));
    let sessions = snapshot
        .sessions
        .iter()
        .filter(|session| worktree.is_some_and(|w| w.id == session.worktree_id))
        .map(|session| {
            let panes: Vec<_> = snapshot
                .panes
                .iter()
                .filter(|pane| pane.session_id == session.id)
                .collect();
            let activity = |pane: &crate::runtime::Pane| {
                snapshot.pane_activity.iter().find(|a| a.pane_id == pane.id)
            };
            // ^ A session is as new as its newest pane; a pane without a clock gives no idle.
            let last = panes
                .iter()
                .map(|pane| activity(pane).map_or(0, |a| a.last_io_unix_ms))
                .collect::<Vec<_>>();
            let idle_ms = (!last.is_empty() && last.iter().all(|ms| *ms > 0))
                .then(|| now_ms.saturating_sub(last.into_iter().max().unwrap_or(0)));
            let busy = panes.iter().any(|pane| {
                !pane.exited
                    && (pane.agent.as_ref().is_some_and(|agent| {
                        matches!(agent.state, AgentState::Working | AgentState::Blocked)
                    }) || activity(pane).is_some_and(|a| a.foreground_job))
            });
            let provider = panes
                .iter()
                .find_map(|pane| pane.agent.as_ref())
                .map(|agent| agent.provider.as_str());
            SessionFact {
                id: session.id,
                revision: session.revision,
                idle_ms,
                busy,
                task: session_news::is_task_session(&session.label, provider),
            }
        })
        .collect();
    Facts {
        path: path.to_path_buf(),
        branch: branch.to_string(),
        sessions,
        git_idle_ms: git_idle(path, now_ms),
    }
}

fn same(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// One line for the manager's chat and whether it needs the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub path: PathBuf,
    pub text: String,
    /// Left for the owner (not done): posted again only when its text changes.
    pub flag: bool,
}

/// Apply the safe actions to the project's worktrees other than its main checkout and the
/// open task copies `copies`: close idle sessions, remove a clean merged or pushed worktree,
/// flag the rest. Nothing is forced. Empty without wsxd.
pub fn sweep(client: &Client, main: &Path, copies: &[PathBuf], rules: &Rules) -> Result<Vec<Note>> {
    let Ok(snapshot) = ops::runtime_snapshot(client) else {
        return Ok(Vec::new());
    };
    let now = now_unix_ms();
    let mut notes = Vec::new();
    for entry in crate::git::worktree::list_worktrees(main)? {
        if entry.is_main || same(&entry.path, main) || copies.iter().any(|c| same(c, &entry.path)) {
            continue;
        }
        let facts = facts(&snapshot, &entry.path, &entry.branch, now);
        let name = if entry.branch == "HEAD" {
            entry.path.display().to_string()
        } else {
            entry.branch.clone()
        };
        let verdict = end(&facts, rules, || loss(main, &entry.path, &entry.branch));
        let idle_days = worktree_idle(&facts).map_or(0, |idle| idle / DAY_MS);
        if verdict == End::Remove {
            match ops::delete_worktree(client, main, &entry.path, &entry.branch, false) {
                Ok(_) => notes.push(Note {
                    path: entry.path,
                    text: format!(
                        "removed worktree {name} (clean, merged or pushed, idle {idle_days} days)"
                    ),
                    flag: false,
                }),
                Err(error) => notes.push(Note {
                    path: entry.path,
                    text: format!("could not remove worktree {name}: {error:#}"),
                    flag: true,
                }),
            }
            continue;
        }
        let idle = idle_sessions(&facts, rules);
        let mut closed = 0;
        for session in &idle {
            let sent = client.call(&Request::SessionClose {
                session_id: session.id,
                expected_revision: session.revision,
            });
            // ^ A conflict means the session moved since the snapshot: it is not idle now.
            closed += usize::from(matches!(sent, Ok(Response::Ack { .. })));
        }
        if closed > 0 {
            notes.push(Note {
                path: entry.path.clone(),
                text: format!("closed {closed} idle session{} in {name}", plural(closed)),
                flag: false,
            });
        }
        if let End::Flag(loss) = verdict {
            notes.push(Note {
                path: entry.path,
                text: format!(
                    "worktree {name} is idle and kept: it has {}. Delete it in the web Sessions tab or the TUI if it is not needed",
                    loss.words()
                ),
                flag: true,
            });
        }
    }
    Ok(notes)
}

/// `Rules` from the project's task settings.
pub fn rules(settings: &crate::model::workspace::TaskConfig) -> Rules {
    Rules {
        idle_ms: settings.prune_idle_secs * 1000,
        remove_ms: settings.prune_remove_secs * 1000,
    }
}
