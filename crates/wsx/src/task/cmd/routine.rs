//! `digest`, `checkin` and `post`: what a routine does for the chat (docs/tasks.md "Routines").

use crate::cli::RoutineCmd;
use crate::task::{acting_as_manager, need, project_entry};
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use wsx_core::task::manager::{Author, Level, ManagerEntry, ManagerEvent};
use wsx_core::task::owner::{self};
use wsx_core::task::policy::Capability;
use wsx_core::task::store::ManagerDir;
use wsx_core::task::view::digest_lines;
use wsx_core::task::{
    now_unix_ms,
    store::{self},
};

pub(crate) const DAY_MS: u64 = 24 * 3600 * 1000;

/// The events message of a digest in the chat.
pub(crate) const DIGEST_EVENT: &str = "daily digest";

/// How long a digest waits for a manager turn to end, and how often it asks again.
pub(crate) const DIGEST_TURN_WAIT: Duration = Duration::from_secs(30 * 60);

pub(crate) const DIGEST_TURN_POLL: Duration = Duration::from_secs(5);

/// When the newest digest in `log` was posted: the next one starts there. A run with nothing
/// to say posts nothing, so the next one covers its time too.
pub(crate) fn last_digest_ms(log: &[ManagerEntry]) -> Option<u64> {
    log.iter().rev().find_map(|entry| match &entry.event {
        ManagerEvent::Message {
            from: Author::Events,
            text,
            ..
        } if text == DIGEST_EVENT => Some(entry.unix_ms),
        _ => None,
    })
}

/// The routine a check-in writes; one per project, so a manager cannot pile them up.
pub(crate) const CHECKIN: &str = "checkin";

/// The furthest ahead a check-in may be.
pub(crate) const CHECKIN_MAX_SECS: i64 = 7 * 24 * 3600;

pub(crate) const CHECKIN_WHY_CHARS: usize = 500;

/// Write routine `checkin`: one run at the chosen minute whose argv wsx fixes (`printf` of
/// `why`), so it needs no `--owner-said`; its output reaches the manager as the run's event.
/// ref: docs/tasks.md "Routines"
pub(crate) fn checkin(
    project: Option<&str>,
    after: Option<&str>,
    at: Option<&str>,
    why: &str,
) -> Result<()> {
    use wsx_core::routine::{Action, Routine, Trigger};
    need(Capability::Routine, "a check-in")?;
    let why = why.trim();
    if why.is_empty() || why.chars().count() > CHECKIN_WHY_CHARS {
        bail!("--why must say what to look at, in at most {CHECKIN_WHY_CHARS} characters");
    }
    let now = (now_unix_ms() / 1000) as i64;
    let at_secs = match (after, at) {
        (Some(after), _) => now + duration_secs(after)?,
        (None, Some(at)) => {
            wsx_core::routine::local::local_minute(at, now).map_err(anyhow::Error::msg)? * 60
        }
        (None, None) => bail!("pass --in or --at"),
    };
    if at_secs <= now || at_secs - now > CHECKIN_MAX_SECS {
        bail!("a check-in is between 1 minute and 7 days ahead");
    }
    // ^ The tick runs a minute that has begun: round up, so it never runs early.
    let epoch_minute = (at_secs + 59) / 60;
    let path = crate::cli::routine_project(project)?;
    let (revision, routines) = crate::cli::fetch_routines(&path)?;
    let routine = Routine {
        name: CHECKIN.into(),
        trigger: Trigger::At { epoch_minute },
        command: vec!["printf".into(), "%s\\n".into(), why.into()],
        prompt: String::new(),
        enabled: true,
        // ^ The why is for the manager, as the run's event; the owner asked for none.
        post: false,
        findings_exit: Vec::new(),
    }
    .validated()?;
    let action = match routines.iter().any(|view| view.routine.name == CHECKIN) {
        true => Action::Edit {
            revision,
            old_name: CHECKIN.into(),
            routine,
        },
        false => Action::Add { revision, routine },
    };
    crate::cli::send_routine(&path, action)?;
    println!(
        "check-in {}",
        wsx_core::routine::local::trigger_text(&Trigger::At { epoch_minute })
    );
    Ok(())
}

/// `40m`, `2h`, `1d` in seconds.
pub(crate) fn duration_secs(text: &str) -> Result<i64> {
    let text = text.trim();
    let unit = match text.chars().last() {
        Some('m') => 60,
        Some('h') => 3600,
        Some('d') => 24 * 3600,
        _ => bail!("--in takes minutes, hours or days: 40m, 2h, 1d"),
    };
    let count: i64 = text[..text.len() - 1]
        .parse()
        .with_context(|| format!("--in takes minutes, hours or days: {text}"))?;
    Ok(count.saturating_mul(unit))
}

/// Post the project's digest. Each try reads the lines again and starts from the chat log, so
/// a run that fails leaves its time to the next one. ref: docs/tasks.md "Routines"
pub(crate) fn digest(project: Option<&str>, since: Option<u64>) -> Result<()> {
    let (name, main) = project_entry(project)?;
    let project_id = store::project_id(&name, &main);
    let after_ms = since.map_or_else(
        || {
            let log = ManagerDir::existing(&project_id)
                .ok()
                .flatten()
                .and_then(|dir| dir.read_log().ok())
                .unwrap_or_default();
            last_digest_ms(&log).unwrap_or_else(|| now_unix_ms().saturating_sub(DAY_MS))
        },
        |seconds| seconds.saturating_mul(1000),
    );
    post_when_free(&project_id, &name, DIGEST_EVENT, None, || {
        digest_lines(&project_id, after_ms)
    })
}

/// The most of stdin `post` reads.
pub(crate) const POST_BYTES: u64 = 16 * 1024;

/// A post's `--key` is 1-64 characters of `[A-Za-z0-9._-]`.
pub(crate) fn post_key(key: &str) -> Result<String, String> {
    let fits = (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    match fits {
        true => Ok(key.to_string()),
        false => Err("a key is 1-64 characters of A-Z a-z 0-9 . _ -".into()),
    }
}

/// The same key within this long of the last kept post is dropped.
pub(crate) const POST_KEY_WINDOW_SECS: u64 = 10 * 60;

/// `wsx task post`: stdin's lines to the chat, as the digest posts its own. It needs only the
/// state folder (`HOME` or `XDG_STATE_HOME`) and the running manager host, no `WSX_*`
/// variable. ref: docs/tasks.md "Routines"
pub(crate) fn post(
    project: Option<&str>,
    title: &str,
    level: &str,
    key: Option<&str>,
) -> Result<()> {
    use std::io::Read;
    let (name, main) = project_entry(project)?;
    let title = title.trim();
    if title.is_empty() || title.contains('\n') {
        bail!("--title must be one line");
    }
    let level = match level {
        "warn" => Some(Level::Warn),
        "bad" => Some(Level::Bad),
        _ => None,
    };
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(POST_BYTES)
        .read_to_end(&mut bytes)
        .context("cannot read stdin")?;
    // ^ The cut at POST_BYTES may split a character.
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.trim().lines().map(str::to_string).collect();
    let project_id = store::project_id(&name, &main);
    let send = || post_when_free(&project_id, &name, title, level, || lines.clone());
    match key {
        Some(key) if !lines.is_empty() => {
            let now = now_unix_ms() / 1000;
            if !store::post_once(&project_id, key, now, POST_KEY_WINDOW_SECS, send)? {
                println!("deduped: {key}");
            }
            Ok(())
        }
        _ => send(),
    }
}

/// Post `lines()` as wsx's `event` message to the chat, with no manager turn: none when it is
/// empty. A turn that runs refuses it, so this tries again for up to 30 minutes, at once
/// failing when the manager itself runs it inside that turn. ref: docs/tasks.md "Routines"
pub(crate) fn post_when_free(
    project_id: &str,
    name: &str,
    event: &str,
    level: Option<Level>,
    mut lines: impl FnMut() -> Vec<String>,
) -> Result<()> {
    // ^ The manager's own command runs inside the turn it would wait for.
    let wait = match acting_as_manager() {
        true => Duration::ZERO,
        false => DIGEST_TURN_WAIT,
    };
    let deadline = Instant::now() + wait;
    loop {
        let lines = lines();
        if lines.is_empty() {
            return Ok(());
        }
        let response = owner::post(project_id, event.into(), lines.join("\n"), level)
            .with_context(|| format!("the manager host of {name} is not running"))?;
        if response.ok {
            return Ok(());
        }
        if !response.busy || Instant::now() >= deadline {
            bail!("the {event} was not posted: {}", response.message);
        }
        std::thread::sleep(DIGEST_TURN_POLL);
    }
}

/// `wsx task routine`: the project's routines. A role changes nothing of the project's own, and
/// the manager writes one only with the owner's word (docs/tasks.md "Roles", "Routines").
pub(crate) fn routine_command(action: RoutineCmd, owner_said: Option<u64>) -> Result<()> {
    use RoutineCmd::{Add, Delete, Disable, Edit, Enable};
    // ^ A role changes nothing of the project's own (docs/tasks.md "Roles").
    if let Add { .. } | Edit { .. } | Delete { .. } | Enable { .. } | Disable { .. } = &action {
        need(Capability::Routine, "routine changes")?;
    }
    // ^ wsxd runs a routine outside the agent boundary: only the owner writes one.
    if let Add { project, .. } | Edit { project, .. } | Delete { project, .. } = &action {
        if acting_as_manager() {
            let said = crate::task::owner_said(owner_said).context(
                "the owner adds, edits and deletes routines: pass --owner-said <id> \
                 when the owner's message of this turn asks for it; nothing was run",
            )?;
            let (name, main) = project_entry(project.as_deref())?;
            owner::check_owner_said(&store::project_id(&name, &main), said)?;
        }
    }
    crate::cli::cmd_routine(action)
}
