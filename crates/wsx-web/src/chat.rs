//! Chat over the provider-neutral exchange broker and provider-native history.
//! ^ docs/agent-orchestration.md: PTY delivery is not acceptance, and pane lifecycle
//! is not request-bound completion. This module never reads terminal output; the one
//! reader of a pane's screen is `question.rs`, for a blocked agent's dialog.
use crate::queue::{Round, Settled};
use crate::{
    media::Media,
    sandbox::Root,
    site::{Access, Capability, Site},
    App,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Mutex,
};
use wsx_core::{
    integration::memory::{Item, ItemKind, Reader, ToolStatus},
    runtime::{
        AgentExchange, AgentExchangeAccess, AgentExchangeEvidence, AgentExchangeState,
        AgentSessionRefKind, AgentState, Client, Pane, PaneId, Project, Request, Response,
        SessionId, Snapshot, WorktreeId,
    },
    task::now_unix_ms,
};

const TIMELINE_ITEMS: usize = 400;
const TIMELINE_BYTES: usize = 256 * 1024;
const EXCHANGE_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const MAX_PROMPT_BYTES: usize = 60 * 1024;
const MAX_ATTACHMENTS: usize = 8;
const ATTACHMENT_HEADER: &str = "[attached files]";

/// An HTTP-shaped failure: status, stable code, message for the person.
#[derive(Debug)]
pub struct Fault(pub u16, pub &'static str, pub String);

impl Fault {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self(status, code, message.into())
    }
    pub fn forbidden(what: &str) -> Self {
        Self::new(
            403,
            "capability_denied",
            format!("this site does not allow {what}"),
        )
    }
    pub fn not_found() -> Self {
        Self::new(404, "not_found", "not found in this site's projects")
    }
    /// wsxd answered with a response the request never gets.
    pub fn protocol(what: &str) -> Self {
        Self::new(502, "daemon_protocol", what)
    }
}

pub type Outcome<T> = Result<T, Fault>;

/// One daemon call. Never starts or replaces wsxd.
pub fn call(client: &Client, request: &Request) -> Outcome<Response> {
    match client.call(request) {
        Ok(Response::Error(error)) => {
            let status = match error.code.as_str() {
                "not_found" => 404,
                "invalid_prompt" | "invalid_timeout" | "invalid_claim" => 400,
                _ => 409,
            };
            Err(Fault(
                status,
                "daemon_refused",
                format!("{}: {}", error.code, error.message),
            ))
        }
        Ok(response) => Ok(response),
        Err(error) => Err(Fault::new(
            503,
            "daemon_unavailable",
            format!("wsxd is not reachable: {error}"),
        )),
    }
}

pub fn snapshot(client: &Client) -> Outcome<Snapshot> {
    match call(client, &Request::Snapshot)? {
        Response::Snapshot(snapshot) => Ok(snapshot),
        _ => Err(Fault::protocol("unexpected snapshot response")),
    }
}

/// A session of a granted project, with the pane that carries its agent.
pub struct Target<'a> {
    pub project: &'a Project,
    pub session_id: SessionId,
    pub label: &'a str,
    pub pane: &'a Pane,
    pub worktree: &'a Path,
    pub worktree_id: WorktreeId,
    pub branch: &'a str,
}

/// The projects of wsxd that the site may reach. This and `targets` are the only
/// place that maps daemon state to the grant; nothing else may widen it.
pub fn projects<'a>(snapshot: &'a Snapshot, access: &Access) -> Vec<&'a Project> {
    snapshot
        .projects
        .iter()
        .filter(|project| access.grants(&project.path))
        .collect()
}

pub fn project<'a>(snapshot: &'a Snapshot, access: &Access, id: u64) -> Outcome<&'a Project> {
    projects(snapshot, access)
        .into_iter()
        .find(|project| project.id.0 == id)
        .ok_or_else(Fault::not_found)
}

/// Every session whose worktree belongs to a granted project.
pub fn targets<'a>(snapshot: &'a Snapshot, access: &Access) -> Vec<Target<'a>> {
    let projects = projects(snapshot, access);
    snapshot
        .sessions
        .iter()
        .filter_map(|session| {
            let worktree = snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.id == session.worktree_id)?;
            let project = *projects
                .iter()
                .find(|project| project.id == worktree.project_id)?;
            let panes = || {
                session
                    .panes
                    .iter()
                    .filter_map(|id| snapshot.panes.iter().find(|pane| pane.id == *id))
            };
            let pane = panes()
                .find(|pane| pane.agent.is_some())
                .or_else(|| panes().find(|pane| pane.id == session.primary_pane))?;
            Some(Target {
                project,
                session_id: session.id,
                label: &session.label,
                pane,
                worktree: &worktree.path,
                worktree_id: worktree.id,
                branch: &worktree.branch,
            })
        })
        .collect()
}

pub fn target<'a>(snapshot: &'a Snapshot, access: &Access, session: u64) -> Outcome<Target<'a>> {
    targets(snapshot, access)
        .into_iter()
        .find(|target| target.session_id.0 == session)
        .ok_or_else(Fault::not_found)
}

pub fn session_view(target: &Target) -> Value {
    json!({
        "id": target.session_id,
        "label": target.label,
        "project": { "id": target.project.id, "name": target.project.name },
        "worktree": target.worktree_id,
        "branch": target.branch,
        "pane_id": target.pane.id,
        "exited": target.pane.exited,
        // ^ wsxd's time, so a restart of this server does not reset it (null before revision 28).
        "state_since_unix_ms": target.pane.agent.as_ref().and_then(|agent| agent.state_since_unix_ms),
        "agent": target.pane.agent.as_ref().map(|agent| json!({
            "provider": agent.provider,
            "state": agent.state,
            "attached": agent.attached,
            "prompt": agent.capabilities.prompt,
        })),
    })
}

/// Plain words for each evidence level, so the page never overstates delivery.
pub fn exchange_view(exchange: &AgentExchange) -> Value {
    let delivery = match exchange.evidence {
        AgentExchangeEvidence::IntentPersisted => "saved, not delivered to the agent",
        AgentExchangeEvidence::PtyDelivery => {
            "typed into the agent's terminal; the agent has not confirmed it"
        }
        AgentExchangeEvidence::PaneLifecycle => {
            "the agent changed state after delivery; not confirmed for this message"
        }
        AgentExchangeEvidence::RequestBound => "the agent confirmed this message",
    };
    json!({
        "id": exchange.id,
        "pane_id": exchange.pane_id,
        "session_id": exchange.session_id,
        "round": exchange.round,
        "state": exchange.state,
        "evidence": exchange.evidence,
        "delivery": delivery,
        "access": exchange.access,
        "active": !exchange.is_terminal(),
        "failure": exchange.failure,
        "updated_unix_ms": exchange.updated_unix_ms,
    })
}

pub fn exchanges(client: &Client, pane: Option<PaneId>) -> Outcome<Vec<AgentExchange>> {
    match call(client, &Request::AgentExchangeList { pane_id: pane })? {
        Response::AgentExchanges(list) => Ok(list),
        _ => Err(Fault::protocol("unexpected exchange list")),
    }
}

fn latest_exchange(client: &Client, pane: PaneId) -> Outcome<Option<AgentExchange>> {
    Ok(exchanges(client, Some(pane))?
        .into_iter()
        .max_by_key(|exchange| exchange.updated_unix_ms))
}

#[derive(Serialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub image: bool,
    /// Relative to the site base: `media/<sha256>.<ext>`.
    pub url: String,
    pub hash: String,
}

impl From<crate::media::Entry> for Attachment {
    fn from(entry: crate::media::Entry) -> Self {
        Self {
            url: format!("media/{}", entry.file_name()),
            image: entry.is_image(),
            name: entry.name,
            mime: entry.mime,
            size: entry.size,
            hash: entry.hash,
        }
    }
}

pub struct Chat {
    /// (path, mtime, size) of a project file already copied into the media store.
    /// `None` records a file the store refused, so it is not copied again on every read.
    produced: Mutex<HashMap<(PathBuf, i64, u64), Option<String>>>,
    /// One delivery at a time: the page's send and the event observer must not both send
    /// the same waiting message.
    delivering: Mutex<()>,
    /// Sessions whose agent this server started again for a message, and since when.
    resuming: Mutex<HashMap<u64, u64>>,
}

impl Chat {
    pub fn new() -> Self {
        Self {
            produced: Mutex::new(HashMap::new()),
            delivering: Mutex::new(()),
            resuming: Mutex::new(HashMap::new()),
        }
    }

    /// The typed timeline of the provider's saved conversation, the latest exchange
    /// for the pane, and what the agent is doing now (docs/web-client-chat.md "Timeline").
    /// `from` is the id and time of an item the page has. When that item is still in
    /// the timeline the answer starts there and names it in `from`: everything before
    /// it is unchanged on the page. A record without its own id is named by position,
    /// so the time must match too.
    pub fn timeline(&self, app: &App, session: u64, from: Option<(&str, u64)>) -> Outcome<Value> {
        let (client, site, media) = (&app.client, &app.site, &app.media);
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        // Agent output is looked up in the session's own worktree, which wsxd lists
        // under the granted project and which may live outside the main checkout.
        let root = std::fs::canonicalize(target.worktree)
            .ok()
            .and_then(|path| Root::pin(&path).ok());
        // ^ The pane's agent, or the conversation it last had when the agent is gone, so the chat
        // outlives the agent (docs/web-client-chat.md "Conversations").
        let conversations = app.queue.conversations(session);
        let remembered = conversations.last();
        let (provider, session_ref) = match target.pane.agent.as_ref() {
            Some(agent) if agent.session_ref.is_some() || remembered.is_none() => {
                (Some(agent.provider.clone()), agent.session_ref.clone())
            }
            _ => (
                remembered.map(|known| known.provider.clone()),
                remembered.map(|known| known.session_ref.clone()),
            ),
        };
        let timeline = provider.as_deref().map(|provider| {
            Reader::default().read_timeline(
                provider,
                session_ref.as_ref(),
                TIMELINE_ITEMS,
                TIMELINE_BYTES,
            )
        });
        let start = from.and_then(|(id, at)| {
            timeline
                .iter()
                .flat_map(|timeline| &timeline.items)
                .position(|item| item.id == id && item.at_unix_ms == Some(at))
        });
        let item_view = |item: &wsx_core::integration::memory::Item| {
            let (text, attachments) = match (&item.text, item.kind) {
                (Some(text), ItemKind::User) => {
                    let (text, attachments) = sent_message(text, media);
                    (Some(text), attachments)
                }
                (Some(text), ItemKind::Summary) => (Some(text.clone()), Vec::new()),
                (Some(text), _) => {
                    let files = root
                        .as_ref()
                        .filter(|_| site.allows(Capability::MediaDownload))
                        .map(|root| self.produced_files(text, root, media))
                        .unwrap_or_default();
                    (Some(text.clone()), files)
                }
                (None, _) => (None, Vec::new()),
            };
            json!({
                "id": item.id, "kind": item.kind, "at_unix_ms": item.at_unix_ms,
                "text": text, "attachments": attachments, "tool": item.tool,
            })
        };
        let mut items: Vec<Value> = timeline
            .iter()
            .flat_map(|timeline| &timeline.items)
            .skip(start.unwrap_or(0))
            .map(item_view)
            .collect();
        // ^ A full answer starts with the end of the conversation before this one, so the
        // earlier one stays readable above its "New conversation" line.
        let earlier = conversations.iter().rev().find(|known| {
            session_ref
                .as_ref()
                .is_some_and(|current| current.value != known.session_ref.value)
        });
        if let (None, Some(earlier)) = (start, earlier) {
            let read = Reader::default().read_timeline(
                &earlier.provider,
                Some(&earlier.session_ref),
                EARLIER_ITEMS,
                TIMELINE_BYTES,
            );
            let mut before: Vec<Value> = read
                .items
                .iter()
                .map(item_view)
                .map(|mut item| {
                    item["id"] = json!(format!(
                        "earlier-{}",
                        item["id"].as_str().unwrap_or_default()
                    ));
                    item
                })
                .collect();
            before.append(&mut items);
            items = before;
        }
        let items = with_notices(items, app.queue.notices(session), start.is_some());
        // Only wsxd decides that the agent works; the transcript says at what.
        let working = target
            .pane
            .agent
            .as_ref()
            .filter(|agent| agent.attached && agent.state == AgentState::Working)
            .map(|agent| {
                let activity =
                    activity_of(agent, timeline.iter().flat_map(|timeline| &timeline.items));
                json!({
                    "since_unix_ms": agent.state_since_unix_ms,
                    "activity": activity,
                })
            });
        let exchange = latest_exchange(client, target.pane.id)?;
        // What a blocked agent asks, for a site that may answer it from the chat.
        let question = target
            .pane
            .agent
            .as_ref()
            .filter(|agent| {
                agent.attached
                    && agent.state == AgentState::Blocked
                    && site.allows(Capability::ChatAnswer)
            })
            .and_then(|_| {
                let last = timeline.as_ref().and_then(|timeline| timeline.items.last());
                crate::question::view(client, target.pane.id, last)
            });
        // ^ A hold the chat cannot name shows the end of the screen: a blocked agent without a
        // readable question, or a message that waits for more than HELD_MS.
        let blocked = target
            .pane
            .agent
            .as_ref()
            .is_some_and(|agent| agent.attached && agent.state == AgentState::Blocked);
        let now = now_unix_ms();
        let held = app.queue.list(session).iter().any(|queued| {
            queued.failed.is_none()
                && queued.delivered_unix_ms.is_none()
                && now.saturating_sub(queued.at_unix_ms) > HELD_MS
        });
        let screen = (((blocked && question.is_none()) || held)
            && site.allows(Capability::TerminalView))
        .then(|| crate::question::tail(client, target.pane.id, HELD_ROWS))
        .flatten();
        // A delivered queued message leaves the queue once the conversation shows it.
        let shown: Vec<(&str, Option<u64>)> = items
            .iter()
            .filter(|item| item["kind"] == "user")
            .filter_map(|item| Some((item["text"].as_str()?.trim(), item["at_unix_ms"].as_u64())))
            .collect();
        app.queue.settle(session, &shown);
        let mut session_value = session_view(&target);
        session_value["conversation"] = json!(!conversations.is_empty());
        Ok(json!({
            "session": session_value,
            "status": timeline.as_ref().map_or("no_agent", |timeline| timeline.status),
            "truncated": timeline.as_ref().is_some_and(|timeline| timeline.truncated),
            "model": timeline.as_ref().and_then(|timeline| timeline.model.as_deref()),
            "items": items,
            "from": start.and(from).map(|(id, _)| id),
            "exchange": exchange.as_ref().map(exchange_view),
            "queue": app.queue.list(session).iter().map(|queued| queued_view(queued, media)).collect::<Vec<_>>(),
            "working": working,
            "question": question,
            "screen": screen,
        }))
    }

    /// Find project files named in an assistant message and copy allowed ones into
    /// the media store. Text comes from native history, never from the terminal.
    fn produced_files(&self, text: &str, root: &Root, media: &Media) -> Vec<Attachment> {
        let mut found: Vec<Attachment> = Vec::new();
        let separators =
            |character: char| character.is_whitespace() || "`()[]<>\"'*,;".contains(character);
        for token in text.split(separators) {
            let token = token.trim_end_matches(['.', ':', '!', '?']);
            let named = Path::new(token)
                .extension()
                .is_some_and(|extension| (1..=5).contains(&extension.len()));
            if !named || token.len() > 1024 || token.contains("://") {
                continue;
            }
            let Some(relative) = root.relative(Path::new(token)) else {
                continue;
            };
            if private_name(&relative) {
                continue;
            }
            let Ok(file) = root.open_file(&relative) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            // An old file would restart its TTL on every read; only fresh output is offered.
            let age = crate::devices::now_unix().saturating_sub(metadata.mtime().max(0) as u64);
            if age >= media.ttl_seconds() || metadata.len() > media.max_file_bytes() {
                continue;
            }
            let key = (
                root.path().join(&relative),
                metadata.mtime(),
                metadata.len(),
            );
            let known = crate::locked(&self.produced).get(&key).cloned();
            let entry = match known {
                Some(None) => continue,
                Some(Some(hash)) => media.by_hash(&hash),
                None => None,
            };
            let entry = match entry {
                Some(entry) => entry,
                None => {
                    let name = relative.file_name().unwrap_or_default().to_string_lossy();
                    let entry = media.ingest(file, &name).ok();
                    crate::locked(&self.produced)
                        .insert(key, entry.as_ref().map(|entry| entry.hash.clone()));
                    let Some(entry) = entry else {
                        continue;
                    };
                    entry
                }
            };
            if !found.iter().any(|existing| existing.hash == entry.hash) {
                found.push(entry.into());
            }
            if found.len() == MAX_ATTACHMENTS {
                break;
            }
        }
        found
    }

    /// Take one message from the page into the session's queue, and deliver the oldest
    /// waiting one now when the agent can take it. Every message goes through the queue, so
    /// the server keeps it until the conversation shows it and sends it again once when the
    /// agent did not take it (docs/web-client-chat.md "Queue and Stop").
    pub fn submit(
        &self,
        app: &App,
        session: u64,
        text: &str,
        attachments: &[String],
    ) -> Outcome<Value> {
        let (client, site, media) = (&app.client, &app.site, &app.media);
        prompt_of(site, media, text, attachments)?;
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        match attached_agent(&target) {
            Err(fault) if fault.1 == "no_agent" => self.resume(app, &snapshot, &target)?,
            attached => drop(attached?),
        }
        let queued = app.queue.push(session, text, attachments).ok_or_else(|| {
            Fault::new(
                409,
                "queue_full",
                "five messages already wait for this agent; remove one first",
            )
        })?;
        self.deliver_queued(app, &target);
        crate::events::history_changed(app, session);
        let now = app
            .queue
            .list(session)
            .into_iter()
            .find(|current| current.id == queued.id)
            .unwrap_or(queued);
        // ^ Delivered now: the exchange it went out in, with its evidence level.
        let exchange = now.round.and_then(|sent| {
            exchanges(client, Some(target.pane.id))
                .ok()?
                .into_iter()
                .find(|exchange| exchange.id.0 == sent.exchange)
        });
        Ok(json!({
            "queued": queued_view(&now, media),
            "exchange": exchange.as_ref().map(exchange_view),
        }))
    }

    /// Answer the dialog of a blocked agent with one key or one line of text.
    pub fn answer(
        &self,
        app: &App,
        session: u64,
        answer: &crate::question::Answer,
    ) -> Outcome<Value> {
        if !app.site.allows(Capability::ChatAnswer) {
            return Err(Fault::forbidden("answering the agent from the chat"));
        }
        let snapshot = snapshot(&app.client)?;
        let target = target(&snapshot, &app.site.access, session)?;
        let blocked = target
            .pane
            .agent
            .as_ref()
            .is_some_and(|agent| agent.attached && agent.state == AgentState::Blocked);
        if !blocked {
            return Err(Fault::new(
                409,
                "not_blocked",
                "the agent is not waiting for an answer",
            ));
        }
        let sent = crate::question::answer(&app.client, target.pane.id, answer)?;
        crate::events::history_changed(app, session);
        Ok(sent)
    }

    /// Remove a waiting message, or let a failed one be delivered again.
    pub fn edit_queue(&self, app: &App, session: u64, id: u64, retry: bool) -> Outcome<Value> {
        if !app.site.allows(Capability::ChatSend) {
            return Err(Fault::forbidden("sending messages"));
        }
        let snapshot = snapshot(&app.client)?;
        let target = target(&snapshot, &app.site.access, session)?;
        let changed = if retry {
            app.queue.retry(session, id)
        } else {
            app.queue.remove(session, id)
        };
        if !changed {
            return Err(Fault::new(
                404,
                "not_found",
                "this message is no longer waiting",
            ));
        }
        // ^ Sent again at once when the agent can take it, as a new message is.
        if retry {
            self.deliver_queued(app, &target);
        }
        crate::events::history_changed(app, session);
        Ok(
            json!({ "queue": app.queue.list(session).iter().map(|queued| queued_view(queued, &app.media)).collect::<Vec<_>>() }),
        )
    }

    /// Type a waiting message into the working Claude now: it takes the message at its next
    /// step (docs/web-client-chat.md "Queue and Stop"). It then leaves the queue as a
    /// delivered message does without a round, when the conversation shows its words.
    pub fn steer(&self, app: &App, session: u64, id: u64) -> Outcome<Value> {
        let _one = crate::locked(&self.delivering);
        let snapshot = snapshot(&app.client)?;
        if !snapshot.capabilities.agent_steer {
            return Err(Fault::new(
                409,
                "daemon_upgrade_required",
                "wsxd must be upgraded before a message can be sent during a turn",
            ));
        }
        let target = target(&snapshot, &app.site.access, session)?;
        let queued = app
            .queue
            .list(session)
            .into_iter()
            .find(|queued| {
                queued.id == id && queued.failed.is_none() && queued.delivered_unix_ms.is_none()
            })
            .ok_or_else(|| Fault::new(404, "not_found", "this message is no longer waiting"))?;
        let text = prompt_of(&app.site, &app.media, &queued.text, &queued.attachments)?;
        match call(
            &app.client,
            &Request::AgentSteer {
                pane_id: target.pane.id,
                text,
                takeover: true,
            },
        )? {
            Response::Ack { .. } => app.queue.delivered(session, id, None),
            _ => return Err(Fault::protocol("unexpected steer response")),
        };
        crate::events::history_changed(app, session);
        Ok(
            json!({ "queue": app.queue.list(session).iter().map(|queued| queued_view(queued, &app.media)).collect::<Vec<_>>() }),
        )
    }

    /// Settle the rounds that carried delivered messages once each round is over. A taken one
    /// leaves the queue when the agent reports prompt receipts (its words may never show, as
    /// after a compaction), else it stops being watched and waits for the conversation to show
    /// it. One the agent did not take sends its message again once, then keeps it failed.
    /// Runs before every delivery, while the round is still the exchange's current one.
    /// Returns whether the queue changed. ref: docs/agent-orchestration.md "Taken, not typed"
    fn settle_rounds(&self, app: &App, target: &Target) -> bool {
        let session = target.session_id.0;
        let sent: Vec<_> = app
            .queue
            .list(session)
            .into_iter()
            .filter_map(|queued| Some((queued.id, queued.round?)))
            .collect();
        if sent.is_empty() {
            return false;
        }
        let Ok(exchanges) = exchanges(&app.client, Some(target.pane.id)) else {
            return false;
        };
        let agent = target.pane.agent.as_ref();
        let provider = agent.map_or("The agent".to_string(), |agent| {
            provider_name(&agent.provider)
        });
        let receipts = agent.is_some_and(|agent| agent.capabilities.prompt_receipts);
        let mut changed = false;
        for (id, sent) in sent {
            let Some(exchange) = exchanges
                .iter()
                .find(|exchange| exchange.id.0 == sent.exchange)
            else {
                continue;
            };
            // ^ A round that still runs keeps its message as sent, also once the agent took it:
            // Claude's prompt hook reports the turn before Claude saves the prompt, and the
            // message leaves when the conversation shows it (`Queue::settle`). A later round of
            // the exchange means this one was over.
            if exchange.round == sent.round && !exchange.is_terminal() {
                continue;
            }
            let not_taken = exchange.round == sent.round
                && exchange.state == AgentExchangeState::DeliveryFailed
                && exchange
                    .failure
                    .as_deref()
                    .is_some_and(|failure| failure.starts_with("not_taken:"));
            let outcome = if not_taken {
                Round::NotTaken {
                    reason: format!("not_taken: {provider} did not take this message"),
                }
            } else {
                Round::Taken { remove: receipts }
            };
            let settled = app.queue.settle_round(session, id, sent, outcome);
            if let Some(Settled::SentAgain) = settled {
                app.queue.notice(
                    session,
                    format!("Sent again: {provider} did not take it the first time."),
                );
            }
            changed |= settled.is_some();
        }
        changed
    }

    /// The pane's agent is gone: type the provider's resume command for the conversation the
    /// pane last had into its idle shell, so a message goes to the same conversation once the
    /// agent reports again. At most once a minute per session. ref:
    /// docs/web-client-chat.md "Conversations"
    fn resume(&self, app: &App, snapshot: &Snapshot, target: &Target) -> Outcome<()> {
        let session = target.session_id.0;
        let refuse = |message: &str| Fault::new(409, "no_agent", message.to_string());
        let (provider, session_ref) = target
            .pane
            .agent
            .as_ref()
            .and_then(|agent| Some((agent.provider.clone(), agent.session_ref.clone()?)))
            .or_else(|| {
                app.queue
                    .conversations(session)
                    .last()
                    .map(|known| (known.provider.clone(), known.session_ref.clone()))
            })
            .ok_or_else(|| refuse("no agent is attached to this session"))?;
        let plan =
            wsx_core::integration::resume::plan(&provider, &session_ref).ok_or_else(|| {
                refuse("no agent is attached, and its conversation cannot be resumed")
            })?;
        if target.pane.exited {
            return Err(refuse("the session's program ended; start it again in wsx"));
        }
        if snapshot
            .pane_activity
            .iter()
            .any(|activity| activity.pane_id == target.pane.id && activity.foreground_job)
        {
            return Err(refuse(
                "the terminal runs another program; open the terminal",
            ));
        }
        let now = now_unix_ms();
        {
            let mut resuming = crate::locked(&self.resuming);
            if resuming
                .get(&session)
                .is_some_and(|since| now.saturating_sub(*since) < RESUME_WAIT_MS)
            {
                return Ok(());
            }
            resuming.insert(session, now);
        }
        // ^ Ctrl+U first: a line half typed at the prompt must not run with it.
        let line = format!("\u{15}{}\r", shell_words(&plan.argv));
        let typed = crate::question::with_lease(&app.client, target.pane.id, |client_id| {
            call(
                &app.client,
                &Request::TerminalInput {
                    pane_id: target.pane.id,
                    client_id,
                    bytes: line.into_bytes(),
                },
            )
            .map(drop)
        });
        if let Err(fault) = typed {
            crate::locked(&self.resuming).remove(&session);
            return Err(fault);
        }
        app.queue.notice(
            session,
            format!(
                "Resumed {} in the same conversation.",
                provider_name(&provider)
            ),
        );
        Ok(())
    }

    /// Remember the conversation of the session's agent and say in the chat when it ends or
    /// changes: "Claude exited.", "Same conversation, continued", "New conversation". A server
    /// that just started (`first`) only remembers. Returns whether the chat changed.
    /// ref: docs/web-client-chat.md "Conversations"
    pub fn follow_conversation(
        &self,
        app: &App,
        target: &Target,
        attached: &mut HashMap<u64, bool>,
        first: bool,
    ) -> bool {
        let session = target.session_id.0;
        let agent = target.pane.agent.as_ref();
        let now = agent.is_some_and(|agent| agent.attached);
        let before = attached.insert(session, now);
        let word = agent.map_or("The agent".to_string(), |agent| {
            provider_name(&agent.provider)
        });
        let Some((agent, session_ref)) = agent
            .filter(|agent| agent.attached)
            .and_then(|agent| Some((agent, agent.session_ref.as_ref()?)))
        else {
            if before == Some(true) && !first && !app.queue.conversations(session).is_empty() {
                app.queue.notice(session, format!("{word} exited."));
                return true;
            }
            return false;
        };
        let resumed = crate::locked(&self.resuming).remove(&session).is_some();
        let line = match app
            .queue
            .see_conversation(session, &agent.provider, session_ref)
        {
            _ if first => None,
            crate::queue::Seen::New => Some(NEW_CONVERSATION.to_string()),
            crate::queue::Seen::Same if before == Some(false) && !resumed => {
                Some("Same conversation, continued".to_string())
            }
            _ => None,
        };
        line.map(|line| app.queue.notice(session, line)).is_some()
    }

    /// Deliver the oldest waiting message of a session when the agent can take it.
    /// Returns whether the queue changed.
    pub fn deliver_queued(&self, app: &App, target: &Target) -> bool {
        let _one = crate::locked(&self.delivering);
        let session = target.session_id.0;
        let settled = self.settle_rounds(app, target);
        let Some(queued) = app.queue.next(session) else {
            return settled;
        };
        let ready = target.pane.agent.as_ref().is_some_and(|agent| {
            agent.attached && !matches!(agent.state, AgentState::Working | AgentState::Blocked)
        });
        if !ready {
            // ^ An agent started again for this message that never reported fails it, with why.
            let mut resuming = crate::locked(&self.resuming);
            let stale = resuming
                .get(&session)
                .is_some_and(|since| now_unix_ms().saturating_sub(*since) >= RESUME_WAIT_MS);
            if stale {
                resuming.remove(&session);
                app.queue.fail(
                    session,
                    queued.id,
                    "resume_failed: the agent did not start again within 60 s",
                );
                return true;
            }
            return settled;
        }
        match self.send(
            &app.client,
            &app.site,
            &app.media,
            session,
            &queued.text,
            &queued.attachments,
        ) {
            // ^ wsxd answers a failed delivery with the exchange, not an error. The
            // message must stay, failed with the reason, or it is lost without a word.
            Ok(exchange) if exchange["state"] == "delivery_failed" => {
                let reason = exchange["failure"]
                    .as_str()
                    .unwrap_or("not delivered to the agent");
                app.queue.fail(session, queued.id, reason);
                true
            }
            Ok(exchange) => {
                let round = exchange["id"].as_u64().zip(exchange["round"].as_u64()).map(
                    |(exchange, round)| crate::queue::SentRound {
                        exchange,
                        round: round as u32,
                    },
                );
                app.queue.delivered(session, queued.id, round)
            }
            // Not at a wait boundary yet, such as Claude's editor that is drawn idle a moment
            // after the turn ends: try again soon, and fail with the reason after a while.
            Err(fault) if busy(&fault) => {
                let since = app.queue.not_yet(session, queued.id);
                if now_unix_ms().saturating_sub(since) < NOT_YET_MS {
                    return settled;
                }
                app.queue.fail(session, queued.id, &fault.2);
                true
            }
            Err(fault) => {
                app.queue.fail(session, queued.id, &fault.2);
                true
            }
        }
    }

    /// Deliver one message. Returns the exchange with its truthful evidence level.
    pub fn send(
        &self,
        client: &Client,
        site: &Site,
        media: &Media,
        session: u64,
        text: &str,
        attachments: &[String],
    ) -> Outcome<Value> {
        let prompt = prompt_of(site, media, text, attachments)?;
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        let agent = attached_agent(&target)?;
        // ^ docs/web-client-chat.md "Commands": a message that is one of the agent's commands
        // is typed as written, because an agent runs a command only at the start of its input.
        // A `!` line is Claude's shell mode, which also starts only at the start of the input.
        let shell =
            attachments.is_empty() && crate::commands::is_shell_line(&agent.provider, &prompt);
        let command = shell
            || attachments.is_empty()
                && prompt.starts_with('/')
                && crate::commands::list(&agent.provider, target.worktree).has_command(&prompt);
        if command && !snapshot.capabilities.exchange_command
            || shell && !snapshot.capabilities.exchange_shell
        {
            return Err(Fault::new(
                409,
                "daemon_upgrade_required",
                "wsxd must be upgraded before the chat can run a command",
            ));
        }
        let previous = latest_exchange(client, target.pane.id)?;
        let access = if site.allows(Capability::AgentWrite) {
            AgentExchangeAccess::Writer
        } else {
            AgentExchangeAccess::ReadOnly
        };
        // docs/agent-orchestration.md: Claude's documented stash keeps an unsent draft
        // in the owner's editor from being merged into this message.
        let stash = agent.provider == "claude"
            && matches!(agent.state, AgentState::Idle | AgentState::Done);
        // Continue only the same pane and agent identity; wsxd rechecks the generation.
        // An exchange keeps the access it was created with, so one that another site
        // or the owner's CLI opened with different access is not continued from here.
        // ^ A person sent this message, so every request takes the pane from a terminal
        // or a wsx window that holds it (`takeover`). The CLI does not.
        let request = match previous.filter(|exchange| {
            // wsxd does not continue a cancelled exchange or one whose pane is gone:
            // the next message starts a new one.
            exchange.can_continue()
                && !matches!(
                    exchange.state,
                    AgentExchangeState::Cancelled
                        | AgentExchangeState::TargetExited
                        | AgentExchangeState::TargetReplaced
                )
                && exchange.pane_id == target.pane.id
                && exchange.agent_id == agent.id
                && exchange.access == access
        }) {
            Some(exchange) if stash => Request::AgentExchangeContinueStashingDraft {
                exchange_id: exchange.id,
                expected_revision: exchange.revision,
                prompt,
                timeout_ms: EXCHANGE_TIMEOUT_MS,
                command,
                takeover: true,
            },
            Some(exchange) => Request::AgentExchangeContinue {
                exchange_id: exchange.id,
                expected_revision: exchange.revision,
                prompt,
                timeout_ms: EXCHANGE_TIMEOUT_MS,
                command,
                takeover: true,
            },
            None => {
                if stash {
                    Request::AgentExchangeCreateStashingDraft {
                        pane_id: target.pane.id,
                        prompt,
                        timeout_ms: EXCHANGE_TIMEOUT_MS,
                        access,
                        write_claims: Vec::new(),
                        command,
                        takeover: true,
                    }
                } else {
                    Request::AgentExchangeCreate {
                        pane_id: target.pane.id,
                        prompt,
                        timeout_ms: EXCHANGE_TIMEOUT_MS,
                        access,
                        write_claims: Vec::new(),
                        command,
                        takeover: true,
                    }
                }
            }
        };
        match call(client, &request)? {
            Response::AgentExchange { exchange, .. } => Ok(exchange_view(&exchange)),
            _ => Err(Fault::protocol("unexpected exchange response")),
        }
    }

    /// The `/` list of a session's agent: names and one-line descriptions only.
    pub fn commands(&self, client: &Client, site: &Site, session: u64) -> Outcome<Value> {
        if !site.allows(Capability::ChatSend) {
            return Err(Fault::forbidden("sending messages"));
        }
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        let provider = target
            .pane
            .agent
            .as_ref()
            .map_or("", |agent| agent.provider.as_str());
        let listing = crate::commands::list(provider, target.worktree);
        Ok(json!({
            "provider": provider,
            "listed": listing.listed,
            "commands": listing.commands,
            "truncated": listing.truncated,
            "skipped": listing.skipped,
            "runs": snapshot.capabilities.exchange_command,
        }))
    }

    pub fn cancel(&self, client: &Client, site: &Site, session: u64) -> Outcome<Value> {
        if !site.allows(Capability::ChatSend) {
            return Err(Fault::forbidden("cancelling messages"));
        }
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        let exchange = latest_exchange(client, target.pane.id)?
            .filter(|exchange| !exchange.is_terminal())
            .ok_or_else(|| Fault::new(409, "nothing_to_cancel", "no message is in progress"))?;
        match call(
            client,
            &Request::AgentExchangeCancel {
                exchange_id: exchange.id,
                expected_revision: exchange.revision,
                takeover: true,
            },
        )? {
            Response::AgentExchange { exchange, .. } => Ok(exchange_view(&exchange)),
            _ => Err(Fault::protocol("unexpected exchange response")),
        }
    }
}

/// What a working agent does now: compacting, else from the end of its saved
/// conversation the title of the running tool, else "Thinking", else "Working".
fn activity_of<'a>(
    agent: &wsx_core::runtime::AgentInfo,
    items: impl Iterator<Item = &'a Item>,
) -> String {
    if agent.compacting {
        return "Compacting the conversation".into();
    }
    match items.last() {
        Some(Item {
            tool: Some(tool), ..
        }) if tool.status == ToolStatus::Running => tool.title.clone(),
        Some(Item {
            kind: ItemKind::Thinking,
            ..
        }) => "Thinking".into(),
        _ => "Working".to_owned(),
    }
}

/// The activity line of a working session, for the session list.
pub fn activity(target: &Target) -> String {
    let Some(agent) = target.pane.agent.as_ref() else {
        return "Working".to_owned();
    };
    let timeline = Reader::default().read_timeline(
        &agent.provider,
        agent.session_ref.as_ref(),
        TIMELINE_ITEMS,
        TIMELINE_BYTES,
    );
    activity_of(agent, timeline.items.iter())
}

/// One line of the agent's current turn for a notification (docs/web-client.md "PWA and
/// push"): the call a blocked agent waits on (a question names itself), the call that
/// failed, else its last reply. Only items after the last user message count, so an
/// earlier turn's reply is never said again. Tool titles are one line built from the
/// call's input, never its output.
pub fn notice_line(target: &Target, state: AgentState) -> Option<String> {
    let agent = target.pane.agent.as_ref()?;
    let timeline = Reader::default().read_timeline(
        &agent.provider,
        agent.session_ref.as_ref(),
        TIMELINE_ITEMS,
        TIMELINE_BYTES,
    );
    let start = timeline
        .items
        .iter()
        .rposition(|item| item.kind == ItemKind::User)
        .map_or(0, |index| index + 1);
    let turn = &timeline.items[start..];
    let tool = |status: ToolStatus| {
        turn.iter()
            .rev()
            .find_map(|item| item.tool.as_ref().filter(|tool| tool.status == status))
            .map(|tool| tool.title.clone())
    };
    let reply = || {
        turn.iter()
            .rev()
            .filter(|item| item.kind == ItemKind::Assistant)
            .find_map(|item| item.text.as_deref())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    match state {
        AgentState::Blocked => tool(ToolStatus::Running).or_else(reply),
        AgentState::Error => tool(ToolStatus::Error).or_else(reply),
        _ => reply(),
    }
    .map(|line| wsx_core::task::view::clip(&line, NOTICE_CHARS))
}
const NOTICE_CHARS: usize = 120;

/// A task worker or the manager: its task or the project chat shows it, so the lists leave
/// it out (store.js `listedSessions`).
pub fn is_task_session(target: &Target) -> bool {
    matches!(
        target
            .pane
            .agent
            .as_ref()
            .map(|agent| agent.provider.as_str()),
        Some("task" | "manager")
    ) || target
        .label
        .strip_prefix("task-")
        .is_some_and(|rest| rest == "manager" || rest.parse::<u32>().is_ok())
}

/// The state of a listed session's attached agent when it needs the owner: blocked or error.
pub fn needs_owner(target: &Target) -> Option<AgentState> {
    let agent = target.pane.agent.as_ref()?;
    (!is_task_session(target)
        && !target.pane.exited
        && agent.attached
        && matches!(agent.state, AgentState::Blocked | AgentState::Error))
    .then_some(agent.state)
}

/// How many sessions wait for the owner's input, as the page counts them: an attached
/// agent that is blocked, in a session the lists show.
pub fn blocked_count(targets: &[Target]) -> usize {
    targets
        .iter()
        .filter(|target| needs_owner(target) == Some(AgentState::Blocked))
        .count()
}

/// How long a queued message waits before the timeline shows the end of the screen.
const HELD_MS: u64 = 10_000;
const HELD_ROWS: usize = 8;

/// How long a queued message waits for an agent that is not at a wait boundary yet.
const NOT_YET_MS: u64 = 30_000;

/// Whether wsxd refused because the agent is not at a wait boundary yet: still in a turn,
/// an editor not drawn idle yet, or wsxd being replaced. The same message may go later.
fn busy(fault: &Fault) -> bool {
    fault.1 == "daemon_refused"
        && [
            "agent_busy",
            "exchange_busy",
            "editor_unavailable",
            "daemon_unavailable",
            "daemon_replacing",
            // ^ The exchange changed between the read and the continue, as a receipt does.
            "revision_conflict",
        ]
        .iter()
        .any(|code| fault.2.starts_with(&format!("{code}:")))
}

/// The agent a message may go to. A blocked agent shows a provider dialog: pasted
/// text plus Enter could pick an option nobody chose, so that answer stays in the terminal.
fn attached_agent<'a>(target: &Target<'a>) -> Outcome<&'a wsx_core::runtime::AgentInfo> {
    let agent = target
        .pane
        .agent
        .as_ref()
        .filter(|agent| agent.attached)
        .ok_or_else(|| Fault::new(409, "no_agent", "no agent is attached to this session"))?;
    if agent.state == AgentState::Blocked {
        return Err(Fault::new(
            409,
            "agent_blocked",
            "the agent is waiting for an answer in its terminal",
        ));
    }
    Ok(agent)
}

/// The text the agent receives for a message and its attachments, or why the
/// message cannot be sent at all.
fn prompt_of(site: &Site, media: &Media, text: &str, attachments: &[String]) -> Outcome<String> {
    if !site.allows(Capability::ChatSend) {
        return Err(Fault::forbidden("sending messages"));
    }
    with_attachments(site, media, text, attachments)
}

/// The text with its attachments' paths under the attachment header, as an agent reads
/// them (the session chat and the manager chat).
pub fn with_attachments(
    site: &Site,
    media: &Media,
    text: &str,
    attachments: &[String],
) -> Outcome<String> {
    if !attachments.is_empty() && !site.allows(Capability::MediaUpload) {
        return Err(Fault::forbidden("attachments"));
    }
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(Fault::new(
            400,
            "too_many_attachments",
            "at most 8 attachments",
        ));
    }
    let mut prompt = agent_text(text);
    if !attachments.is_empty() {
        prompt.push_str(&format!("\n\n{ATTACHMENT_HEADER}"));
        for hash in attachments {
            let entry = media.by_hash(hash).ok_or_else(|| {
                Fault::new(
                    400,
                    "attachment_missing",
                    "an attachment expired; upload it again",
                )
            })?;
            prompt.push_str(&format!(
                "\n{} ({})",
                media.path(&entry).display(),
                entry.name
            ));
        }
    }
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(Fault::new(
            400,
            "invalid_message",
            "message must be 1 byte to 60 KiB",
        ));
    }
    Ok(prompt)
}

/// How long an agent started again for a message may take to report.
const RESUME_WAIT_MS: u64 = 60_000;

/// An argv as one shell line: each word as written when it is plain, else in single quotes.
fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_./=:@".contains(&byte));
            if plain {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The most items of the conversation before the current one a full timeline begins with.
const EARLIER_ITEMS: usize = 60;

const NEW_CONVERSATION: &str = "New conversation";

/// The server's notices among the conversation's items, each before the first item saved
/// after it. A partial answer (`from`) gets only the notices from its first item on.
fn with_notices(
    mut items: Vec<Value>,
    notices: Vec<crate::queue::Notice>,
    partial: bool,
) -> Vec<Value> {
    let first = items.first().and_then(|item| item["at_unix_ms"].as_u64());
    // ^ The line is stamped when this server saw the new conversation, which can be after
    // its first message: in a full read after the earlier conversation it opens the current one.
    let earlier = |item: &Value| {
        item["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("earlier-"))
    };
    let opens = (!partial && items.first().is_some_and(earlier))
        .then(|| {
            notices
                .iter()
                .rposition(|notice| notice.text == NEW_CONVERSATION)
        })
        .flatten();
    for (number, notice) in notices.into_iter().enumerate() {
        let at = notice.at_unix_ms;
        if partial && first.is_none_or(|first| at < first) {
            continue;
        }
        let mut index = items
            .iter()
            .position(|item| {
                item["at_unix_ms"]
                    .as_u64()
                    .is_some_and(|item_at| item_at > at)
            })
            .unwrap_or(items.len());
        if Some(number) == opens {
            let current = items
                .iter()
                .position(|item| !earlier(item) && item["kind"] != "notice")
                .unwrap_or(items.len());
            index = index.min(current);
        }
        items.insert(
            index,
            json!({ "id": format!("notice-{at}"), "kind": "notice", "at_unix_ms": at, "text": notice.text, "attachments": [] }),
        );
    }
    items
}

/// A message as the agent gets it. The daemon rejects terminal control characters; they
/// are dropped so a pasted carriage return does not fail the whole message.
pub fn agent_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A provider as a person reads it: `claude` is Claude.
fn provider_name(provider: &str) -> String {
    let mut letters = provider.chars();
    letters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(letters).collect()
    })
}

fn queued_view(queued: &crate::queue::Queued, media: &Media) -> Value {
    let attachments: Vec<Attachment> = queued
        .attachments
        .iter()
        .filter_map(|hash| media.by_hash(hash))
        .map(Attachment::from)
        .collect();
    json!({
        "id": queued.id, "text": queued.text, "attachments": attachments,
        "at_unix_ms": queued.at_unix_ms, "failed": queued.failed,
        "delivered_unix_ms": queued.delivered_unix_ms, "resent": queued.resent,
    })
}

/// Files an agent may name but the web client must not hand out: hidden paths
/// (`.env`, `.git/`, `.ssh/`) and names that usually hold credentials.
fn private_name(relative: &Path) -> bool {
    let hidden = relative
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'));
    let name = relative
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let extension = name.rsplit('.').next().unwrap_or("");
    hidden
        || [
            "secret",
            "credential",
            "password",
            "passwd",
            "token",
            "id_rsa",
            "id_ed25519",
        ]
        .iter()
        .any(|word| name.contains(word))
        || ["pem", "key", "p12", "pfx", "env", "jks"].contains(&extension)
}

/// Undo what `send` and the broker added to a user message, for display only. The manager
/// chat reads its owner messages through it too.
pub fn sent_message(text: &str, media: &Media) -> (String, Vec<Attachment>) {
    let text = unwrap_paste(text);
    let body = match text.split_once('\n') {
        Some((first, rest)) if first.starts_with("[wsx exchange ") && first.ends_with(']') => rest,
        _ => text,
    };
    let Some((before, list)) = body.rsplit_once(&format!("\n\n{ATTACHMENT_HEADER}\n")) else {
        return (body.to_owned(), Vec::new());
    };
    let attachments = list
        .lines()
        .filter_map(|line| {
            let path = line.rsplit_once(" (").map_or(line, |(path, _)| path);
            media.by_path(Path::new(path)).map(Attachment::from)
        })
        .collect();
    (before.to_owned(), attachments)
}

/// Claude Code can save a pasted prompt as `<pasted_content id="N">\n…\n</pasted_content id="N">`;
/// the broker's prompt is always a paste, so a wrapper around the whole message is undone.
fn unwrap_paste(text: &str) -> &str {
    let trimmed = text.trim_matches('\n');
    let Some(rest) = trimmed.strip_prefix("<pasted_content id=\"") else {
        return text;
    };
    let Some((id, inner)) = rest.split_once("\">\n") else {
        return text;
    };
    let close = format!("\n</pasted_content id=\"{id}\">");
    match inner.strip_suffix(close.as_str()) {
        Some(inner) if !inner.contains("<pasted_content id=\"") => inner,
        _ => text,
    }
}

/// Transcript identity and size, used only to notice that history changed.
pub fn history_stamp(pane: &Pane) -> Option<(PathBuf, u64, i64)> {
    let reference = pane.agent.as_ref()?.session_ref.as_ref()?;
    let path = reference.transcript_path.clone().or_else(|| {
        (reference.kind == AgentSessionRefKind::Path).then(|| PathBuf::from(&reference.value))
    })?;
    let metadata = std::fs::metadata(&path).ok()?;
    Some((
        path,
        metadata.len(),
        metadata
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec()),
    ))
}

/// The granted projects and their worktrees, for the page's session list and
/// its "new session" and files actions.
pub fn projects_view(snapshot: &Snapshot, access: &Access) -> Vec<Value> {
    projects(snapshot, access)
        .into_iter()
        .map(|project| {
            let worktrees: Vec<Value> = snapshot
                .worktrees
                .iter()
                .filter(|worktree| worktree.project_id == project.id)
                .map(|worktree| {
                    json!({
                        "id": worktree.id, "branch": worktree.branch,
                        "main": worktree.path == project.path,
                    })
                })
                .collect();
            json!({ "id": project.id, "name": project.name, "worktrees": worktrees })
        })
        .collect()
}
