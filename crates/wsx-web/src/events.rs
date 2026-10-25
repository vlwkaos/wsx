//! One daemon observer per server. It feeds every page's single event stream and
//! decides when a consented device gets a push notification.
//! ^ The browser never polls. This thread uses wsxd's long-poll and, while a page is
//! connected, compares transcript file stamps so new history reaches the page.
use crate::{
    chat::{self, exchange_view, session_view},
    devices::now_unix,
    push::{Subscription, Switch},
    App,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use wsx_core::runtime::{self, AgentState, Request, Response, TuiClientPresence};

// Below the runtime client's 5 s read timeout.
const POLL_MS: u64 = 2_000;
// While an agent works and a page is open: a new tool line shows within about a second.
const WORKING_POLL_MS: u64 = 800;
// How often the server reads whether an upgrade of the running wsxd failed.
const UPGRADE_CHECK: Duration = Duration::from_secs(30);
// How often the server looks whether a device's daily digest is due.
const DIGEST_CHECK: Duration = Duration::from_secs(5);

/// A named server-sent event with a JSON body.
#[derive(Clone)]
pub struct Message {
    pub event: &'static str,
    pub data: Arc<str>,
}

fn message(event: &'static str, data: Value) -> Message {
    Message {
        event,
        data: data.to_string().into(),
    }
}

pub fn spawn(app: Arc<App>) {
    thread::Builder::new()
        .name("wsx-web-events".into())
        .spawn(move || observe(app))
        .expect("spawn event observer");
}

fn observe(app: Arc<App>) {
    let app = &app;
    let mut revision = 0;
    let mut first = true;
    let mut available = true;
    let mut last_upgrade_check = Instant::now();
    let mut states: HashMap<u64, AgentState> = HashMap::new();
    // ^ Whether each session's agent was attached at the last look, for the conversation lines.
    let mut attached: HashMap<u64, bool> = HashMap::new();
    let mut watching = false;
    let mut stamps: HashMap<u64, Option<(std::path::PathBuf, u64, i64)>> = HashMap::new();
    let mut exchange_revisions: HashMap<u64, u64> = HashMap::new();
    let mut workspace = String::new();
    let mut tasks = crate::tasks::Watch::default();
    // What each working session does now, read again only when its transcript changed.
    let mut activities: HashMap<u64, String> = HashMap::new();
    // The screen stamp of each blocked session's question, while a page is open.
    let mut questions: HashMap<u64, Option<String>> = HashMap::new();
    // Sessions whose agent began to need the owner, for "An agent needs me".
    let mut agents: Pending<u64> = Pending::default();
    let mut last_digest_check = Instant::now();
    let mut alive_before: Vec<u64> = Vec::new();
    // ^ docs/daemon-live-upgrade.md: wsxd takes a poll without presence for an old TUI
    // and defers every upgrade. This server reconnects by itself, so it says at which
    // protocol. The identity is this executable's; the server never replaces wsxd.
    let presence = std::env::current_exe()
        .and_then(|path| runtime::binary_identity(&path))
        .ok()
        .map(|target_binary_id| TuiClientPresence {
            instance_id: runtime::new_client_id(),
            version: runtime::WSX_VERSION.to_string(),
            target_binary_id,
            target_daemon_revision: runtime::DAEMON_REVISION,
            max_protocol: runtime::PROTOCOL_VERSION,
            // It cannot re-execute into an installed wsx, and needs no exemption:
            // it reconnects at the daemon's own protocol.
            follows: false,
        });
    loop {
        let changed = match app.client.call(&Request::Poll {
            after_revision: revision,
            // A message that waits for an editor to turn idle is tried again soon.
            timeout_ms: if watching || app.queue.waiting() {
                WORKING_POLL_MS
            } else {
                POLL_MS
            },
            tui: presence.clone(),
        }) {
            Ok(Response::Events {
                revision: next,
                events,
            }) => {
                let changed = first || next != revision || !events.is_empty();
                revision = next;
                changed
            }
            _ => {
                // wsxd is stopped or being replaced. Say so once, then wait for it.
                if available {
                    available = false;
                    workspace.clear();
                    // A page that opens now must not start from the last session list.
                    *crate::locked(&app.workspace) = None;
                    publish(app, message("daemon", json!({ "available": false })));
                }
                revision = 0;
                first = true;
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        // ^ docs/daemon-live-upgrade.md "A failed automatic upgrade": another wsx tried to
        // upgrade the running wsxd into this build and it rolled back. Read on each connect
        // and every 30 s; pages hear of a change.
        let mut failure_changed = false;
        if first || last_upgrade_check.elapsed() >= UPGRADE_CHECK {
            last_upgrade_check = Instant::now();
            let failed = runtime::upgrade_failure(&app.client);
            let mut known = crate::locked(&app.upgrade_failed);
            if *known != failed {
                *known = failed;
                failure_changed = true;
            }
        }
        if !available || failure_changed {
            available = true;
            let failed = crate::locked(&app.upgrade_failed).clone();
            publish(
                app,
                message(
                    "daemon",
                    json!({ "available": true, "upgrade_failed": failed }),
                ),
            );
        }
        let Ok(snapshot) = chat::snapshot(&app.client) else {
            continue;
        };
        let targets = chat::targets(&snapshot, &app.site.access);
        // ^ What needs the owner across projects, as the page counts it: each push carries it
        // for the app badge (docs/web-client.md "PWA and push").
        crate::tasks::watch(app, &snapshot, &mut tasks);
        let badge = chat::blocked_count(&targets) + tasks.needs();
        crate::tasks::flush(app, &mut tasks, badge);
        if changed {
            if let Ok(exchanges) = chat::exchanges(&app.client, None) {
                for exchange in exchanges.iter().filter(|exchange| {
                    targets
                        .iter()
                        .any(|target| target.pane.id == exchange.pane_id)
                }) {
                    if exchange_revisions.insert(exchange.id.0, exchange.revision)
                        != Some(exchange.revision)
                        && !first
                    {
                        publish(app, message("exchange", exchange_view(exchange)));
                    }
                }
            }
        }
        for target in &targets {
            let session = target.session_id.0;
            let state = target
                .pane
                .agent
                .as_ref()
                .filter(|agent| agent.attached)
                .map_or(AgentState::Unknown, |agent| agent.state);
            let transition = states
                .insert(session, state)
                .is_some_and(|previous| previous != state);
            // A transition is the only trigger: the first sight of a session, or a
            // server restart, never notifies. "Finished" goes to the daily digest only.
            if transition && !first && chat::needs_owner(target).is_some() {
                agents.add(session);
            }
            let mut history_changed = transition;
            history_changed |= app
                .chat
                .follow_conversation(app, target, &mut attached, first);
            if app.events.receiver_count() > 0 {
                let stamp = chat::history_stamp(target.pane);
                history_changed |= stamps
                    .insert(session, stamp.clone())
                    .is_some_and(|previous| previous != stamp);
            }
            // A dialog that changed on the screen is a changed question for the page.
            if state == AgentState::Blocked
                && app.events.receiver_count() > 0
                && app.site.allows(crate::site::Capability::ChatAnswer)
            {
                let stamp = crate::question::stamp(&app.client, target.pane.id);
                history_changed |= questions
                    .insert(session, stamp.clone())
                    .is_some_and(|previous| previous != stamp);
            } else {
                questions.remove(&session);
            }
            if state != AgentState::Working {
                activities.remove(&session);
            } else if history_changed || !activities.contains_key(&session) {
                activities.insert(session, chat::activity(target));
            }
            // The agent reached a wait boundary: the oldest waiting message goes next.
            history_changed |= app.chat.deliver_queued(app, target);
            if history_changed {
                self::history_changed(app, session);
            }
        }
        // The session list, with what a working agent does.
        let sessions: Vec<Value> = targets
            .iter()
            .map(|target| {
                let mut view = session_view(target);
                // ^ The chat outlives the agent while its conversation is remembered.
                view["conversation"] =
                    json!(!app.queue.conversations(target.session_id.0).is_empty());
                view["activity"] = json!(activities.get(&target.session_id.0));
                view
            })
            .collect();
        // The build lets a page of an older build offer a reload (docs/web-client.md "Build").
        let view = json!({
            "projects": chat::projects_view(&snapshot, &app.site.access),
            "sessions": sessions,
            "build": app.assets.build,
        });
        let text = view.to_string();
        if text != workspace {
            workspace = text;
            publish(app, message("workspace", view));
        }
        if let Some(new) = agents.due(Duration::from_secs(app.site.notify.agents.merge_seconds)) {
            notify_agents(app, &targets, &new, badge);
        }
        if last_digest_check.elapsed() >= DIGEST_CHECK || first {
            last_digest_check = Instant::now();
            daily_digest(app, &snapshot, &targets, badge);
        }
        let alive: Vec<u64> = targets.iter().map(|target| target.session_id.0).collect();
        if alive != alive_before {
            if let Some(push) = &app.push {
                push.retain_muted(&alive);
            }
            alive_before = alive.clone();
        }
        app.queue.retain(&alive);
        activities.retain(|session, _| alive.contains(session));
        states.retain(|session, _| targets.iter().any(|target| target.session_id.0 == *session));
        watching = app.events.receiver_count() > 0
            && states
                .values()
                .any(|state| matches!(state, AgentState::Working | AgentState::Blocked));
        stamps.retain(|session, _| states.contains_key(session));
        attached.retain(|session, _| states.contains_key(session));
        questions.retain(|session, _| states.contains_key(session));
        first = false;
    }
}

/// Tell every open page that a session's timeline, including its queue, changed.
pub fn history_changed(app: &App, session: u64) {
    publish(app, message("history", json!({ "session_id": session })));
}

/// Send a named event with a JSON body to every open page.
pub fn publish_json(app: &App, event: &'static str, data: Value) {
    publish(app, message(event, data));
}

fn publish(app: &App, message: Message) {
    if message.event == "workspace" {
        *crate::locked(&app.workspace) = Some(message.clone());
    }
    // No receiver is not an error: no page is open.
    let _ = app.events.send(message);
}

/// New items of one switch that wait for its merge window (docs/web-client.md "Notifications").
pub struct Pending<K> {
    first: Option<Instant>,
    new: Vec<K>,
}

impl<K> Default for Pending<K> {
    fn default() -> Self {
        Self {
            first: None,
            new: Vec::new(),
        }
    }
}

impl<K: PartialEq> Pending<K> {
    pub fn add(&mut self, key: K) {
        self.first.get_or_insert_with(Instant::now);
        if !self.new.contains(&key) {
            self.new.push(key);
        }
    }

    /// The new items once `merge` passed since the first of them; they are taken.
    pub fn due(&mut self, merge: Duration) -> Option<Vec<K>> {
        if self.first?.elapsed() < merge {
            return None;
        }
        self.first = None;
        Some(std::mem::take(&mut self.new))
    }
}

/// A push body stays far below the push service's 4 KiB with the rest of the payload.
const NOTICE_BODY_BYTES: usize = 1500;

/// One line per item; a body that would pass 1500 bytes ends `and <n> more`.
pub fn notice_body(lines: &[String]) -> String {
    let mut body = String::new();
    for (index, line) in lines.iter().enumerate() {
        if body.len() + line.len() + 1 > NOTICE_BODY_BYTES {
            body.push_str(&format!("\nand {} more", lines.len() - index));
            break;
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    body
}

/// The "An agent needs me" push: every session that needs input or stopped with an error,
/// except the device's muted ones, sent when one of them is new to it. One session reads
/// `<project> › <branch>: <label>` over its state and one line of its turn
/// (docs/web-client.md "Notifications").
fn notify_agents(app: &Arc<App>, targets: &[chat::Target], new: &[u64], badge: usize) {
    if app.push.is_none() {
        return;
    }
    let clip = |text: &str, chars| wsx_core::task::view::clip(text, chars);
    // (session, blocked, name, title, body) of each open item; the notice reads the saved
    // conversation once per session.
    let open: Vec<(u64, bool, String, String, String)> = targets
        .iter()
        .filter_map(|target| {
            let state = chat::needs_owner(target)?;
            let words = match state {
                AgentState::Blocked => "Needs your input",
                _ => "Stopped with an error",
            };
            let body = match chat::notice_line(target, state) {
                Some(line) => format!("{words}: {line}"),
                None => words.to_owned(),
            };
            Some((
                target.session_id.0,
                state == AgentState::Blocked,
                clip(target.label, 30),
                format!(
                    "{} › {}: {}",
                    clip(&target.project.name, 60),
                    clip(target.branch, 60),
                    clip(target.label, 60)
                ),
                body,
            ))
        })
        .collect();
    let list = if app.site.allows(crate::site::Capability::TaskView) {
        "#/sessions"
    } else {
        "#/"
    };
    let tag = format!("wsx-{}-agents", app.site.id);
    deliver(app, Switch::Agents, |device| {
        let shown: Vec<_> = open
            .iter()
            .filter(|(session, ..)| !device.muted.contains(session))
            .collect();
        if !shown.iter().any(|(session, ..)| new.contains(session)) {
            return None;
        }
        if let [(session, _, _, title, body)] = shown.as_slice() {
            return Some(json!({
                "kind": "agents", "title": title, "body": body, "session_id": session,
                "tag": tag, "badge": badge,
            }));
        }
        let group = |blocked: bool, one: &str, many: &str| {
            let names: Vec<&str> = shown
                .iter()
                .filter(|item| item.1 == blocked)
                .map(|item| item.2.as_str())
                .collect();
            (!names.is_empty()).then(|| counted(&names, one, many))
        };
        let title: Vec<String> = [
            group(true, "agent needs input", "agents need input"),
            group(
                false,
                "agent stopped with an error",
                "agents stopped with an error",
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        let lines: Vec<String> = shown
            .iter()
            .map(|(_, _, name, _, body)| clip(&format!("{name}: {body}"), 160))
            .collect();
        Some(json!({
            "kind": "agents", "title": title.join("; "), "body": notice_body(&lines), "url": list,
            "tag": tag, "badge": badge,
        }))
    });
}

/// Each device's daily digest, once its time came that day: what the two switches leave
/// out since the last one. Nothing to say sends nothing (docs/web-client.md "Notifications").
fn daily_digest(
    app: &Arc<App>,
    snapshot: &runtime::Snapshot,
    targets: &[chat::Target],
    badge: usize,
) {
    let Some(push) = &app.push else {
        return;
    };
    let Some(paired) = paired(app) else {
        return;
    };
    let now = now_unix();
    let scopes = if app.site.allows(crate::site::Capability::TaskView) {
        crate::tasks::scopes(snapshot, &app.site.access)
    } else {
        Vec::new()
    };
    for device in push.devices(&paired) {
        let Some(time) = device.choices.digest else {
            continue;
        };
        let (day, passed) = time.day(now);
        let (last, since) = device.digest_sent.unwrap_or((day - 1, now));
        if !passed || last >= day {
            continue;
        }
        push.digest_sent(&device.device_id, day, now);
        let projects: Vec<(&str, Vec<String>)> = scopes
            .iter()
            .map(|scope| {
                (
                    scope.name.as_str(),
                    crate::tasks::digest_lines(scope, since * 1000),
                )
            })
            .filter(|(_, lines)| !lines.is_empty())
            .collect();
        let several = projects.len() > 1;
        let mut lines: Vec<String> = projects
            .into_iter()
            .flat_map(|(name, lines)| {
                lines.into_iter().map(move |line| match several {
                    true => format!("{name}: {line}"),
                    false => line,
                })
            })
            .collect();
        let finished: Vec<String> = targets
            .iter()
            .filter(|target| {
                !chat::is_task_session(target)
                    && !device.muted.contains(&target.session_id.0)
                    && target.pane.agent.as_ref().is_some_and(|agent| {
                        agent.attached
                            && agent.state == AgentState::Done
                            && agent
                                .state_since_unix_ms
                                .is_some_and(|ms| ms > since * 1000)
                    })
            })
            .map(|target| wsx_core::task::view::clip(target.label, 30))
            .collect();
        if !finished.is_empty() {
            lines.push(counted(&finished, "agent finished", "agents finished"));
        }
        if lines.is_empty() {
            continue;
        }
        let payload = json!({
            "kind": "digest",
            "title": format!("{}: daily digest", app.site.name),
            "body": notice_body(&lines),
            "url": "#/",
            "tag": format!("wsx-{}-digest", app.site.id),
            "badge": badge,
        });
        send(app, vec![(device.device_id, payload)]);
    }
}

/// `2 agents finished: a, b`.
fn counted(names: &[impl AsRef<str>], one: &str, many: &str) -> String {
    let words = if names.len() == 1 { one } else { many };
    let names: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    format!("{} {words}: {}", names.len(), names.join(", "))
}

/// The ids of the paired devices: a revoked device gets nothing.
fn paired(app: &App) -> Option<Vec<String>> {
    let devices = app.devices.list().ok()?;
    Some(devices.into_iter().map(|device| device.id).collect())
}

/// Push to each paired device with `switch` on what `payload_for` builds for it, if anything.
pub fn deliver(
    app: &Arc<App>,
    switch: Switch,
    payload_for: impl Fn(&Subscription) -> Option<Value>,
) {
    let Some(push) = &app.push else {
        return;
    };
    let Some(paired) = paired(app) else {
        return;
    };
    let sends = push
        .devices(&paired)
        .iter()
        .filter(|device| device.wants(switch))
        .filter_map(|device| Some((device.device_id.clone(), payload_for(device)?)))
        .collect();
    send(app, sends);
}

fn send(app: &Arc<App>, sends: Vec<(String, Value)>) {
    if sends.is_empty() {
        return;
    }
    // A slow push service must not hold back the event stream.
    let app = app.clone();
    thread::spawn(move || {
        if let Some(push) = &app.push {
            for (device, payload) in sends {
                push.notify(&device, &payload);
            }
        }
    });
}
