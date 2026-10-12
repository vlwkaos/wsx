//! Chat over the provider-neutral exchange broker and provider-native history.
//! ^ docs/agent-orchestration.md: PTY delivery is not acceptance, and pane lifecycle
//! is not request-bound completion. This module never reads terminal output.
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
        AgentExchange, AgentExchangeAccess, AgentExchangeEvidence, AgentSessionRefKind, AgentState,
        Client, Pane, PaneId, Project, Request, Response, SessionId, Snapshot,
    },
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
        _ => Err(Fault::new(
            502,
            "daemon_protocol",
            "unexpected snapshot response",
        )),
    }
}

/// A session of a granted project, with the pane that carries its agent.
pub struct Target<'a> {
    pub project: &'a Project,
    pub session_id: SessionId,
    pub label: &'a str,
    pub pane: &'a Pane,
    pub worktree: &'a Path,
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
        "branch": target.branch,
        "pane_id": target.pane.id,
        "exited": target.pane.exited,
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
        "updated_unix_ms": exchange.updated_unix_ms,
    })
}

pub fn exchanges(client: &Client, pane: Option<PaneId>) -> Outcome<Vec<AgentExchange>> {
    match call(client, &Request::AgentExchangeList { pane_id: pane })? {
        Response::AgentExchanges(list) => Ok(list),
        _ => Err(Fault::new(
            502,
            "daemon_protocol",
            "unexpected exchange list",
        )),
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
}

impl Chat {
    pub fn new() -> Self {
        Self {
            produced: Mutex::new(HashMap::new()),
        }
    }

    /// The typed timeline of the provider's saved conversation, the latest exchange
    /// for the pane, and what the agent is doing now (docs/web-client.md "Timeline").
    pub fn timeline(&self, app: &App, session: u64) -> Outcome<Value> {
        let (client, site, media) = (&app.client, &app.site, &app.media);
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        // Agent output is looked up in the session's own worktree, which wsxd lists
        // under the granted project and which may live outside the main checkout.
        let root = std::fs::canonicalize(target.worktree)
            .ok()
            .and_then(|path| Root::pin(&path).ok());
        let timeline = target.pane.agent.as_ref().map(|agent| {
            Reader::default().read_timeline(
                &agent.provider,
                agent.session_ref.as_ref(),
                TIMELINE_ITEMS,
                TIMELINE_BYTES,
            )
        });
        let items: Vec<Value> = timeline
            .iter()
            .flat_map(|timeline| &timeline.items)
            .map(|item| {
                let (text, attachments) = match (&item.text, item.kind) {
                    (Some(text), ItemKind::User) => {
                        let (text, attachments) = sent_message(text, media);
                        (Some(text), attachments)
                    }
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
            })
            .collect();
        // Only wsxd decides that the agent works; the transcript says at what.
        let working = target
            .pane
            .agent
            .as_ref()
            .filter(|agent| agent.attached && agent.state == AgentState::Working)
            .map(|_| {
                let steps = timeline.iter().flat_map(|timeline| &timeline.items);
                let activity = match steps.last() {
                    Some(Item {
                        tool: Some(tool), ..
                    }) if tool.status == ToolStatus::Running => tool.title.clone(),
                    Some(Item {
                        kind: ItemKind::Thinking,
                        ..
                    }) => "Thinking".into(),
                    _ => "Working".to_owned(),
                };
                json!({
                    "since_unix_ms": app.state_since(session, AgentState::Working),
                    "activity": activity,
                })
            });
        let exchange = latest_exchange(client, target.pane.id)?;
        Ok(json!({
            "session": session_view(&target),
            "status": timeline.as_ref().map_or("no_agent", |timeline| timeline.status),
            "truncated": timeline.as_ref().is_some_and(|timeline| timeline.truncated),
            "checkpoint": timeline.and_then(|timeline| timeline.checkpoint),
            "items": items,
            "exchange": exchange.as_ref().map(exchange_view),
            "queue": [],
            "working": working,
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
            let known = self
                .produced
                .lock()
                .unwrap_or_else(|lock| lock.into_inner())
                .get(&key)
                .cloned();
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
                    self.produced
                        .lock()
                        .unwrap_or_else(|lock| lock.into_inner())
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
        if !site.allows(Capability::ChatSend) {
            return Err(Fault::forbidden("sending messages"));
        }
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
        // The daemon rejects terminal control characters; drop them here so a pasted
        // carriage return does not fail the whole message.
        let mut prompt: String = text
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect::<String>()
            .trim()
            .to_owned();
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
        let snapshot = snapshot(client)?;
        let target = target(&snapshot, &site.access, session)?;
        let agent = target.pane.agent.as_ref().filter(|agent| agent.attached);
        let Some(agent) = agent else {
            return Err(Fault::new(
                409,
                "no_agent",
                "no agent is attached to this session",
            ));
        };
        // A blocked agent shows a provider dialog. Pasted text plus Enter could
        // pick an option nobody chose, so that answer stays in the terminal.
        if agent.state == AgentState::Blocked {
            return Err(Fault::new(
                409,
                "agent_blocked",
                "the agent is waiting for an answer in its terminal",
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
        let request = match previous.filter(|exchange| {
            exchange.can_continue()
                && exchange.pane_id == target.pane.id
                && exchange.agent_id == agent.id
                && exchange.access == access
        }) {
            Some(exchange) if stash => Request::AgentExchangeContinueStashingDraft {
                exchange_id: exchange.id,
                expected_revision: exchange.revision,
                prompt,
                timeout_ms: EXCHANGE_TIMEOUT_MS,
            },
            Some(exchange) => Request::AgentExchangeContinue {
                exchange_id: exchange.id,
                expected_revision: exchange.revision,
                prompt,
                timeout_ms: EXCHANGE_TIMEOUT_MS,
            },
            None => {
                if stash {
                    Request::AgentExchangeCreateStashingDraft {
                        pane_id: target.pane.id,
                        prompt,
                        timeout_ms: EXCHANGE_TIMEOUT_MS,
                        access,
                        write_claims: Vec::new(),
                    }
                } else {
                    Request::AgentExchangeCreate {
                        pane_id: target.pane.id,
                        prompt,
                        timeout_ms: EXCHANGE_TIMEOUT_MS,
                        access,
                        write_claims: Vec::new(),
                    }
                }
            }
        };
        match call(client, &request)? {
            Response::AgentExchange { exchange, .. } => Ok(exchange_view(&exchange)),
            _ => Err(Fault::new(
                502,
                "daemon_protocol",
                "unexpected exchange response",
            )),
        }
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
            },
        )? {
            Response::AgentExchange { exchange, .. } => Ok(exchange_view(&exchange)),
            _ => Err(Fault::new(
                502,
                "daemon_protocol",
                "unexpected exchange response",
            )),
        }
    }
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

/// Undo what `send` and the broker added to a user message, for display only.
fn sent_message(text: &str, media: &Media) -> (String, Vec<Attachment>) {
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
