//! One daemon observer per server. It feeds every page's single event stream and
//! decides when a consented device gets a push notification.
//! ^ The browser never polls. This thread uses wsxd's long-poll and, while a page is
//! connected, compares transcript file stamps so new history reaches the page.
use crate::{
    chat::{self, exchange_view, session_view},
    push::Topic,
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
        crate::tasks::watch(app, &snapshot, &mut tasks);
        let targets = chat::targets(&snapshot, &app.site.access);
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
            let mut history_changed = states
                .insert(session, state)
                .is_some_and(|previous| previous != state);
            history_changed |= app
                .chat
                .follow_conversation(app, target, &mut attached, first);
            // A transition is the only trigger: the first sight of a session, or a
            // server restart, never notifies.
            if history_changed && !first {
                let topic = match state {
                    AgentState::Blocked => Some((Topic::Blocked, "needs your input")),
                    AgentState::Done => Some((Topic::Done, "finished")),
                    AgentState::Error => Some((Topic::Error, "stopped with an error")),
                    _ => None,
                };
                if let Some((topic, words)) = topic {
                    notify(app, topic, session, target.label, words);
                }
            }
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
        let alive: Vec<u64> = targets.iter().map(|target| target.session_id.0).collect();
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

fn notify(app: &Arc<App>, topic: Topic, session: u64, label: &str, words: &str) {
    let label: String = label.chars().take(60).collect();
    let payload = json!({
        "kind": topic,
        "title": app.site.name,
        "body": format!("{label} {words}"),
        "session_id": session,
        "tag": format!("wsx-{}-{session}", app.site.id),
    });
    deliver(app, topic, payload, Vec::new());
}

/// Push `payload` to every paired device that consented to `topic`, except `except`.
pub fn deliver(app: &Arc<App>, topic: Topic, payload: Value, except: Vec<String>) {
    if app.push.is_none() {
        return;
    }
    let paired: Vec<String> = match app.devices.list() {
        Ok(devices) => devices.into_iter().map(|device| device.id).collect(),
        Err(_) => return,
    };
    // A slow push service must not hold back the event stream.
    let app = app.clone();
    thread::spawn(move || {
        if let Some(push) = &app.push {
            push.notify(topic, &payload, &paired, &except);
        }
    });
}
