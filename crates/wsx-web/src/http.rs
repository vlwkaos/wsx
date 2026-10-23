//! Routes, request checks and response headers. See docs/web-client.md "Security".
//! ^ Every route is mounted twice: at the site base and at the root, because
//! `tailscale serve --set-path` may strip the prefix before forwarding.
use crate::{
    assets,
    chat::{self, Fault, Outcome},
    devices::Device,
    events::Message,
    media::MediaError,
    push::{SubscriptionInput, Topic},
    sandbox::{SandboxError, MAX_TEXT_BYTES},
    site::{Capability, Panel},
    terminal, App,
};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::WebSocketUpgrade, DefaultBodyLimit, FromRequestParts, Path as UrlPath, Query, Request,
        State,
    },
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use wsx_core::runtime::{Request as Daemon, Response as DaemonResponse};

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; \
media-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'self'; \
form-action 'none'; frame-ancestors 'none'";
const MAX_STREAMS_PER_DEVICE: usize = 4;

type Shared = Arc<App>;

pub fn router(app: Shared) -> Router {
    let routes = || {
        Router::new()
            .route("/", get(index))
            .route("/manifest.webmanifest", get(manifest))
            .route("/sw.js", get(service_worker))
            .route("/theme/{file}", get(theme_file))
            .route("/assets/{name}", get(asset))
            .route("/assets/{build}/{name}", get(built_asset))
            .route("/media/{file}", get(media_get))
            .route("/api/pair", post(pair))
            .route("/api/session", get(session))
            .route("/api/logout", post(logout))
            .route("/api/events", get(events))
            .route("/api/presence", post(presence))
            .route("/api/sessions", post(session_start))
            .route("/api/sessions/{session}", delete(session_remove))
            .route("/api/chat/{session}/timeline", get(timeline))
            .route("/api/chat/{session}/send", post(send))
            .route("/api/chat/{session}/commands", get(commands))
            .route("/api/chat/{session}/cancel", post(cancel))
            .route("/api/chat/{session}/answer", post(answer))
            .route("/api/chat/{session}/queue/{id}", delete(queue_remove))
            .route("/api/chat/{session}/queue/{id}/retry", post(queue_retry))
            .route("/api/chat/{session}/queue/{id}/steer", post(queue_steer))
            .route("/api/tasks", get(tasks_all))
            .route("/api/tasks/{project}", get(tasks_overview))
            .route("/api/tasks/{project}/chat", get(tasks_chat))
            .route("/api/tasks/{project}/since", get(tasks_since))
            .route("/api/tasks/{project}/brief", get(tasks_brief))
            .route("/api/tasks/{project}/page", get(tasks_page))
            .route("/api/tasks/{project}/say", post(tasks_say))
            .route("/api/tasks/{project}/t/{task}", get(task_detail))
            .route("/api/tasks/{project}/t/{task}/{action}", post(task_act))
            .route(
                "/api/tasks/{project}/approvals/{id}/{decision}",
                post(task_decide),
            )
            .route("/api/tasks/{project}/checks", get(tasks_checks))
            .route(
                "/api/tasks/{project}/checks/{id}/{decision}",
                post(task_check).get(check_capture),
            )
            .route("/api/media", put(media_put))
            .route("/api/files", get(files_list))
            // A text file is at most 1 MiB and a write carries the old and the new text.
            .route(
                "/api/file",
                get(file_read)
                    .put(file_write)
                    .layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
            )
            .route("/api/terminal/{session}/ws", get(terminal_socket))
            .route(
                "/api/push",
                get(push_get).post(push_set).delete(push_delete),
            )
            .layer(middleware::from_fn_with_state(app.clone(), guard))
            .with_state(app.clone())
    };
    let router = Router::new().merge(routes());
    if app.site.base.is_empty() {
        router
    } else {
        // `nest` serves "<base>" but not "<base>/", which is the page's canonical URL.
        router.nest(&app.site.base, routes()).merge(
            Router::new()
                .route(&app.site.scope(), get(index))
                .layer(middleware::from_fn_with_state(app.clone(), guard))
                .with_state(app.clone()),
        )
    }
}

impl IntoResponse for Fault {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({ "error": { "code": self.1, "message": self.2 } })),
        )
            .into_response()
    }
}

/// Exact Host and Origin checks, then the fixed response headers.
async fn guard(State(app): State<Shared>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let text = |name: header::HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
    let path = request.uri().path();
    let refusal = if text(header::HOST) != Some(app.site.public.host.as_str()) {
        Some(Fault::new(
            421,
            "wrong_host",
            "this server answers one public host only",
        ))
    } else if text(header::ORIGIN).is_some_and(|origin| origin != app.site.public.origin) {
        Some(Fault::new(
            403,
            "wrong_origin",
            "cross-origin requests are refused",
        ))
    } else if !matches!(*request.method(), Method::GET | Method::HEAD)
        && text(header::ORIGIN).is_none()
    {
        Some(Fault::new(
            403,
            "wrong_origin",
            "state-changing requests need an Origin",
        ))
    } else if (path.starts_with("/api/") || path.starts_with("/media/"))
        && headers
            .get("sec-fetch-site")
            .is_some_and(|site| site != "same-origin")
    {
        Some(Fault::new(
            403,
            "wrong_origin",
            "cross-site requests are refused",
        ))
    } else {
        None
    };
    let api = path.starts_with("/api/");
    let mut response = match refusal {
        Some(fault) => fault.into_response(),
        None => next.run(request).await,
    };
    let headers = response.headers_mut();
    for (name, value) in [
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Token bucket per key. A device key limits one device; "pair" limits guessing.
pub struct Limiter(Mutex<HashMap<String, (f64, Instant)>>);

impl Limiter {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
    fn allow(&self, key: &str, burst: f64, per_minute: f64) -> bool {
        let mut buckets = crate::locked(&self.0);
        let now = Instant::now();
        let (tokens, seen) = buckets.entry(key.to_owned()).or_insert((burst, now));
        *tokens =
            (*tokens + now.duration_since(*seen).as_secs_f64() * per_minute / 60.0).min(burst);
        *seen = now;
        if *tokens < 1.0 {
            return false;
        }
        *tokens -= 1.0;
        true
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

pub(crate) fn authenticate(app: &App, headers: &HeaderMap) -> Outcome<Device> {
    let unpaired = || Fault::new(401, "unpaired", "this device is not paired");
    let credential = cookie(headers, &app.site.cookie_name()).ok_or_else(unpaired)?;
    app.devices
        .authenticate(credential)
        .map_err(|error| Fault::new(500, "device_store", format!("{error:#}")))?
        .ok_or_else(unpaired)
}

/// Extractor: a paired device that is inside its request budget.
struct Paired(Device);

impl FromRequestParts<Shared> for Paired {
    type Rejection = Fault;
    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Outcome<Self> {
        let device = blocking(|| authenticate(app, &parts.headers))?;
        if !app.limiter.allow(&device.id, 120.0, 240.0) {
            return Err(Fault::new(
                429,
                "rate_limited",
                "too many requests from this device",
            ));
        }
        Ok(Self(device))
    }
}

fn need(app: &App, panel: Panel) -> Outcome<()> {
    app.site
        .shows(panel)
        .then_some(())
        .ok_or_else(|| Fault::forbidden("this panel"))
}

fn allow(app: &App, capability: Capability, what: &str) -> Outcome<()> {
    app.site
        .allows(capability)
        .then_some(())
        .ok_or_else(|| Fault::forbidden(what))
}

/// Daemon and file calls are short blocking I/O on a multi-thread runtime.
pub(crate) fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}

fn typed(content_type: &'static str, body: impl Into<Body>) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body.into(),
    )
        .into_response()
}

async fn index(State(app): State<Shared>) -> Response {
    typed("text/html; charset=utf-8", app.assets.index.clone())
}
async fn manifest(State(app): State<Shared>) -> Response {
    typed("application/manifest+json", app.assets.manifest.clone())
}
async fn service_worker(State(app): State<Shared>) -> Response {
    typed(
        "text/javascript; charset=utf-8",
        app.assets.service_worker.clone(),
    )
}
/// Theme stylesheet, fonts and icons. The name is the content hash, so the
/// response never changes and may be cached for good.
async fn theme_file(State(app): State<Shared>, UrlPath(file): UrlPath<String>) -> Response {
    match app.site.theme.files.iter().find(|entry| entry.name == file) {
        Some(entry) => (
            [
                (header::CONTENT_TYPE, entry.content_type),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            entry.bytes.clone(),
        )
            .into_response(),
        None => Fault::not_found().into_response(),
    }
}
/// `assets/<name>`: what a page of a build before the build path asks for.
async fn asset(UrlPath(name): UrlPath<String>, headers: HeaderMap) -> Response {
    let Some(asset) = assets::find(&name) else {
        return Fault::not_found().into_response();
    };
    if !asset.pinned {
        return typed(asset.content_type, asset.body);
    }
    immutable(asset, &headers)
}

/// `assets/<build>/<name>`: the files of this build, which never change under that path,
/// and the pinned files under any build. An open page of an older build gets 404 for a
/// module it loads late: its "New version" control reloads it (docs/web-client.md "Build").
async fn built_asset(
    State(app): State<Shared>,
    UrlPath((build, name)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match assets::find(&name) {
        Some(asset) if asset.pinned || build == app.assets.build => immutable(asset, &headers),
        Some(_) => {
            Fault::new(404, "old_build", "this page is an older version; reload it").into_response()
        }
        None => Fault::not_found().into_response(),
    }
}

fn immutable(asset: assets::Asset, headers: &HeaderMap) -> Response {
    let mut response = (
        [
            (header::CONTENT_TYPE, asset.content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        asset.body,
    )
        .into_response();
    if asset.gzip {
        // Stored compressed only. Every browser that runs the page accepts gzip.
        let accepted = headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|coding| coding.trim().starts_with("gzip"))
            });
        if !accepted {
            return Fault::new(
                406,
                "gzip_required",
                "this file is served gzip-compressed only",
            )
            .into_response();
        }
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairInput {
    code: String,
    #[serde(default)]
    name: String,
}

async fn pair(State(app): State<Shared>, Json(input): Json<PairInput>) -> Outcome<Response> {
    // The proxy hides the client address, so guessing is limited for the whole site.
    if !app.limiter.allow("pair", 5.0, 5.0) {
        return Err(Fault::new(
            429,
            "rate_limited",
            "too many pairing attempts; wait a minute",
        ));
    }
    let credential = blocking(|| app.devices.redeem(&input.code, &input.name))
        .map_err(|error| Fault::new(409, "pairing_refused", format!("{error:#}")))?
        .ok_or_else(|| Fault::new(403, "wrong_code", "the code is wrong or expired"))?;
    let cookie = format!(
        "{}={credential}; Path={}; HttpOnly; SameSite=Strict; Max-Age=31536000{}",
        app.site.cookie_name(),
        app.site.scope(),
        if app.site.public.secure {
            "; Secure"
        } else {
            ""
        }
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "paired": true })),
    )
        .into_response())
}

async fn logout(State(app): State<Shared>, Paired(device): Paired) -> Outcome<Response> {
    blocking(|| app.devices.revoke(&device.id))
        .map_err(|error| Fault::new(500, "device_store", format!("{error:#}")))?;
    if let Some(push) = &app.push {
        push.unsubscribe(&device.id);
    }
    let cookie = format!(
        "{}=; Path={}; HttpOnly; SameSite=Strict; Max-Age=0",
        app.site.cookie_name(),
        app.site.scope()
    );
    Ok((
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "paired": false })),
    )
        .into_response())
}

async fn session(State(app): State<Shared>, Paired(device): Paired) -> Json<Value> {
    Json(json!({
        "site": {
            "id": app.site.id,
            "name": app.site.name,
            "panels": app.site.panels,
            "capabilities": app.site.capabilities,
            "max_file_bytes": app.site.media.max_file_bytes,
            "terminal": { "ligatures": app.site.terminal_ligatures },
        },
        "device": { "id": device.id, "name": device.name },
        "build": {
            "version": env!("CARGO_PKG_VERSION"),
            "commit": assets::COMMIT,
            "id": app.assets.build,
            "started_unix": app.started_unix,
        },
        "upgrade_failed": crate::locked(&app.upgrade_failed).clone(),
    }))
}

/// One of a device's long-lived connections of one kind: event streams or terminals.
struct StreamSlot(Shared, String);
impl StreamSlot {
    fn take(app: &Shared, key: String, what: &str) -> Outcome<Self> {
        let mut streams = crate::locked(&app.streams);
        let count = streams.entry(key.clone()).or_insert(0);
        if *count >= MAX_STREAMS_PER_DEVICE {
            return Err(Fault::new(
                429,
                "too_many_streams",
                format!("too many open {what} on this device"),
            ));
        }
        *count += 1;
        Ok(Self(app.clone(), key))
    }
}
impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut streams = crate::locked(&self.0.streams);
        if let Some(count) = streams.get_mut(&self.1) {
            *count = count.saturating_sub(1);
        }
    }
}

/// `page`: an id the page chose for itself, so it can say which project it shows.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsQuery {
    page: Option<String>,
}

fn page_id(id: &str) -> Outcome<()> {
    let valid = (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    valid.then_some(()).ok_or_else(|| {
        Fault::new(
            400,
            "invalid_page",
            "a page id is 8-64 of a-z, A-Z, 0-9, - and _",
        )
    })
}

/// A page's open stream. The page and what it shows are forgotten with its last stream.
struct PageSlot(Shared, String);
impl PageSlot {
    fn take(app: &Shared, id: String, device: &str) -> Outcome<Self> {
        let mut pages = crate::locked(&app.pages);
        let page = pages.entry(id.clone()).or_insert_with(|| crate::Page {
            device: device.to_string(),
            streams: 0,
            project: None,
        });
        // ^ One device cannot speak for another device's page.
        if page.device != device {
            return Err(Fault::new(
                409,
                "invalid_page",
                "this page id belongs to another device",
            ));
        }
        page.streams += 1;
        Ok(Self(app.clone(), id))
    }
}
impl Drop for PageSlot {
    fn drop(&mut self) {
        let mut pages = crate::locked(&self.0.pages);
        if let Some(page) = pages.get_mut(&self.1) {
            page.streams = page.streams.saturating_sub(1);
            if page.streams == 0 {
                pages.remove(&self.1);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresenceInput {
    page: String,
    /// The project the page shows while it is visible, or null.
    project: Option<u64>,
}

/// What an open page shows, so a task push for that project skips its device.
async fn presence(
    State(app): State<Shared>,
    Paired(device): Paired,
    Json(input): Json<PresenceInput>,
) -> Outcome<Json<Value>> {
    page_id(&input.page)?;
    let mut pages = crate::locked(&app.pages);
    let page = pages
        .get_mut(&input.page)
        .filter(|page| page.device == device.id)
        .ok_or_else(|| Fault::new(404, "unknown_page", "this page has no open event stream"))?;
    page.project = input.project;
    Ok(Json(json!({ "ok": true })))
}

/// The page's one event stream. It ends when the device is revoked.
async fn events(
    State(app): State<Shared>,
    Paired(device): Paired,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Outcome<Response> {
    let page = match query.page {
        Some(id) => {
            page_id(&id)?;
            Some(PageSlot::take(&app, id, &device.id)?)
        }
        None => None,
    };
    // The slot is released when the stream task ends, however it ends.
    let slot = StreamSlot::take(&app, device.id, "pages")?;
    let mut source = app.events.subscribe();
    let first = crate::locked(&app.workspace).clone();
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        let _page = page;
        let app = &slot.0;
        let event =
            |message: Message| Ok(Event::default().event(message.event).data(&*message.data));
        if sender
            .send(event(first.unwrap_or(Message {
                event: "daemon",
                data: r#"{"available":false}"#.into(),
            })))
            .await
            .is_err()
        {
            return;
        }
        loop {
            let next = tokio::select! {
                // The page went away: free its stream slot now, not at the next event.
                _ = sender.closed() => return,
                next = tokio::time::timeout(Duration::from_secs(20), source.recv()) => next,
            };
            // Revocation is checked on every event and at least every 20 seconds.
            if blocking(|| authenticate(app, &headers)).is_err() {
                let _ = sender
                    .send(Ok(Event::default().event("revoked").data("{}")))
                    .await;
                return;
            }
            let message = match next {
                Err(_) => continue,
                Ok(Ok(message)) => message,
                // The page missed events: tell it to reload its state.
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => Message {
                    event: "resync",
                    data: "{}".into(),
                },
                Ok(Err(_)) => return,
            };
            if sender.send(event(message)).await.is_err() {
                return;
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(receiver))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
        .into_response())
}

/// Closes a session the site reaches, as `wsx session delete` does (docs/web-client.md
/// "Screens"). A device that may start sessions may remove them. A task's
/// worker and the manager belong to their host and close with it.
async fn session_remove(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(session): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    allow(&app, Capability::SessionStart, "removing a session")?;
    blocking(|| {
        let snapshot = chat::snapshot(&app.client)?;
        let target = chat::target(&snapshot, &app.site.access, session)?;
        let provider = target
            .pane
            .agent
            .as_ref()
            .map(|agent| agent.provider.as_str());
        // ^ `task-<n>`, `task-manager` and the owner's `task-<n>-hand` (docs/tasks.md "By hand").
        let task_label = target.label.strip_prefix("task-").is_some_and(|rest| {
            let number = rest.strip_suffix("-hand").unwrap_or(rest);
            rest == "manager" || number.parse::<u32>().is_ok()
        });
        if matches!(provider, Some("task" | "manager")) || task_label {
            return Err(Fault::new(
                409,
                "task_session",
                "this session belongs to a task; close the task instead",
            ));
        }
        let revision = snapshot
            .sessions
            .iter()
            .find(|candidate| candidate.id == target.session_id)
            .map(|candidate| candidate.revision)
            .ok_or_else(Fault::not_found)?;
        match chat::call(
            &app.client,
            &Daemon::SessionClose {
                session_id: target.session_id,
                expected_revision: revision,
            },
        )? {
            DaemonResponse::Ack { .. } => Ok(Json(json!({ "removed": session }))),
            _ => Err(Fault::protocol("unexpected close response")),
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    project: u64,
    /// A worktree of that project; the main checkout when absent.
    #[serde(default)]
    worktree: Option<u64>,
    #[serde(default)]
    label: String,
}

async fn session_start(
    State(app): State<Shared>,
    Paired(device): Paired,
    Json(input): Json<StartInput>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    allow(&app, Capability::SessionStart, "starting an agent session")?;
    blocking(|| {
        let snapshot = chat::snapshot(&app.client)?;
        let project = chat::project(&snapshot, &app.site.access, input.project)?;
        let worktrees = || {
            snapshot
                .worktrees
                .iter()
                .filter(|worktree| worktree.project_id == project.id)
        };
        let worktree = match input.worktree {
            Some(id) => worktrees().find(|worktree| worktree.id.0 == id),
            None => worktrees()
                .find(|worktree| worktree.path == project.path)
                .or_else(|| worktrees().next()),
        }
        .ok_or_else(Fault::not_found)?;
        // Each start spawns a process on the host.
        if !app.limiter.allow(&format!("start:{}", device.id), 3.0, 6.0) {
            return Err(Fault::new(
                429,
                "rate_limited",
                "too many new sessions; wait a moment",
            ));
        }
        let label: String = input
            .label
            .chars()
            .filter(|character| !character.is_control())
            .take(40)
            .collect();
        match chat::call(
            &app.client,
            &Daemon::SessionCreate {
                worktree_id: worktree.id,
                label: if label.trim().is_empty() {
                    "chat".into()
                } else {
                    label.trim().into()
                },
                command: app.site.agent_command.clone(),
                initial_input: None,
                rows: 40,
                cols: 120,
            },
        )? {
            DaemonResponse::Created { id, .. } => Ok(Json(json!({ "session_id": id }))),
            _ => Err(Fault::protocol("unexpected create response")),
        }
    })
}

/// `from` and `at`: the id and time of an item the page has. The answer then starts at
/// that item when it is still there (docs/web-client-chat.md "Timeline").
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineQuery {
    from: Option<String>,
    at: Option<u64>,
}

async fn timeline(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(session): UrlPath<u64>,
    Query(query): Query<TimelineQuery>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    let from = query.from.zip(query.at);
    blocking(|| {
        app.chat
            .timeline(
                &app,
                session,
                from.as_ref().map(|(id, at)| (id.as_str(), *at)),
            )
            .map(Json)
    })
}

async fn commands(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(session): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    blocking(|| app.chat.commands(&app.client, &app.site, session).map(Json))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    text: String,
    #[serde(default)]
    attachments: Vec<String>,
}

async fn send(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath(session): UrlPath<u64>,
    Json(input): Json<SendInput>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    act_budget(&app, &device)?;
    blocking(|| {
        app.chat
            .submit(&app, session, &input.text, &input.attachments)
            .map(Json)
    })
}

async fn queue_remove(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath((session, id)): UrlPath<(u64, u64)>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    blocking(|| app.chat.edit_queue(&app, session, id, false).map(Json))
}

async fn queue_retry(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath((session, id)): UrlPath<(u64, u64)>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    blocking(|| app.chat.edit_queue(&app, session, id, true).map(Json))
}

// The same budget as messages: a steer is typed into the agent.
async fn queue_steer(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath((session, id)): UrlPath<(u64, u64)>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    act_budget(&app, &device)?;
    blocking(|| app.chat.steer(&app, session, id).map(Json))
}

async fn cancel(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(session): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    blocking(|| app.chat.cancel(&app.client, &app.site, session).map(Json))
}

async fn answer(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath(session): UrlPath<u64>,
    Json(input): Json<crate::question::Answer>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Chat)?;
    // The same budget as messages: an answer is also typed into the agent.
    act_budget(&app, &device)?;
    blocking(|| app.chat.answer(&app, session, &input).map(Json))
}

/// Task routes need the chat panel and `task_view`; actions also `task_act`.
fn tasks_allowed(app: &App, act: bool) -> Outcome<()> {
    need(app, Panel::Chat)?;
    allow(app, Capability::TaskView, "tasks")?;
    if act {
        allow(app, Capability::TaskAct, "acting on tasks")?;
    }
    Ok(())
}

/// Task actions share the message budget: each one reaches an agent or runs a command.
fn act_budget(app: &App, device: &Device) -> Outcome<()> {
    if app
        .limiter
        .allow(&format!("send:{}", device.id), 10.0, 20.0)
    {
        Ok(())
    } else {
        Err(Fault::new(
            429,
            "rate_limited",
            "too many messages; wait a moment",
        ))
    }
}

async fn tasks_all(State(app): State<Shared>, Paired(_): Paired) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| crate::tasks::overviews(&app).map(Json))
}

async fn tasks_overview(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| {
        Ok(Json(crate::tasks::overview(&crate::tasks::scope(
            &app, project,
        )?)))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AfterQuery {
    after: Option<u64>,
}

async fn tasks_chat(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
    Query(query): Query<AfterQuery>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| {
        let scopes = crate::tasks::scopes(&crate::chat::snapshot(&app.client)?, &app.site.access);
        let scope = scopes
            .iter()
            .find(|scope| scope.project == project)
            .ok_or_else(Fault::not_found)?;
        crate::tasks::chat(scope, &scopes, query.after, &app.media).map(Json)
    })
}

async fn tasks_since(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
    Query(query): Query<AfterQuery>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    let after = query
        .after
        .ok_or_else(|| Fault::new(400, "after_required", "since needs ?after=<unix ms>"))?;
    blocking(|| {
        Ok(Json(crate::tasks::since(
            &crate::tasks::scope(&app, project)?,
            after,
        )))
    })
}

async fn tasks_brief(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| crate::tasks::brief(&crate::tasks::scope(&app, project)?).map(Json))
}

async fn tasks_page(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| crate::tasks::page(&crate::tasks::scope(&app, project)?).map(Json))
}

async fn task_detail(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath((project, task)): UrlPath<(u64, u32)>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    let can_act = app.site.allows(Capability::TaskAct);
    blocking(|| crate::tasks::detail(&crate::tasks::scope(&app, project)?, task, can_act).map(Json))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SayInput {
    text: String,
    /// Media hashes from `PUT /api/media`.
    #[serde(default)]
    attachments: Vec<String>,
}

async fn tasks_say(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath(project): UrlPath<u64>,
    Json(input): Json<SayInput>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, true)?;
    act_budget(&app, &device)?;
    blocking(|| {
        let text =
            crate::chat::with_attachments(&app.site, &app.media, &input.text, &input.attachments)?;
        crate::tasks::say(&app, &crate::tasks::scope(&app, project)?, &text).map(Json)
    })
}

/// The body of a task action; each action reads only its own fields.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct ActInput {
    text: Option<String>,
    /// `allow`: exactly one of host and path; `project` keeps it for every task.
    host: Option<String>,
    path: Option<String>,
    project: bool,
}

async fn task_act(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath((project, task, action)): UrlPath<(u64, u32, String)>,
    Json(input): Json<ActInput>,
) -> Outcome<Json<Value>> {
    use wsx_core::task::HostRequest;
    tasks_allowed(&app, true)?;
    if matches!(action.as_str(), "preview" | "hand" | "handback") {
        act_budget(&app, &device)?;
        let scope = blocking(|| crate::tasks::scope(&app, project))?;
        // ^ A preview waits for its command to answer, up to minutes.
        return tokio::task::spawn_blocking(move || crate::tasks::run_task(&scope, &action, task))
            .await
            .map_err(|_| Fault::new(500, "task_action", "the action stopped unexpectedly"))?
            .map(Json);
    }
    let missing = |field: &str| Fault::new(400, "invalid_body", format!("{action} needs {field}"));
    let request = match action.as_str() {
        "add" => HostRequest::Add {
            text: input.text.ok_or_else(|| missing("text"))?,
        },
        "steer" => HostRequest::Steer {
            text: input.text.ok_or_else(|| missing("text"))?,
        },
        // ^ The web grants hosts and paths only; tool rules stay with the CLI.
        "allow" => HostRequest::Grant {
            access: match (input.host, input.path) {
                (Some(host), None) => wsx_core::task::Access::Network { host },
                (None, Some(path)) => wsx_core::task::Access::Write { path },
                _ => return Err(missing("one of host and path")),
            },
            project: input.project,
            by: wsx_core::task::Actor::Owner,
        },
        "land" => HostRequest::Land {},
        _ => return Err(Fault::not_found()),
    };
    act_budget(&app, &device)?;
    let scope = blocking(|| crate::tasks::scope(&app, project))?;
    // A landing may take minutes; it must not hold a runtime worker.
    tokio::task::spawn_blocking(move || crate::tasks::act(&scope, task, request))
        .await
        .map_err(|_| Fault::new(500, "task_action", "the action stopped unexpectedly"))?
        .map(Json)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct DecideInput {
    note: Option<String>,
}

async fn task_decide(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath((project, id, decision)): UrlPath<(u64, String, String)>,
    Json(input): Json<DecideInput>,
) -> Outcome<Response> {
    tasks_allowed(&app, true)?;
    act_budget(&app, &device)?;
    blocking(|| {
        let scope = crate::tasks::scope(&app, project)?;
        match decision.as_str() {
            "approve" => Ok((
                StatusCode::ACCEPTED,
                Json(crate::tasks::approve(&app, scope, &id)?),
            )
                .into_response()),
            "deny" => Ok(Json(crate::tasks::deny(&scope, &id, input.note)?).into_response()),
            _ => Err(Fault::not_found()),
        }
    })
}

async fn tasks_checks(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(project): UrlPath<u64>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, false)?;
    blocking(|| crate::tasks::checks(&crate::tasks::scope(&app, project)?)).map(Json)
}

/// The capture kept with a "To check" item: `GET .../checks/<id>/capture`.
async fn check_capture(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath((project, id, part)): UrlPath<(u64, String, String)>,
) -> Outcome<Response> {
    tasks_allowed(&app, false)?;
    if part != "capture" {
        return Err(Fault::not_found());
    }
    let (bytes, media) =
        blocking(|| crate::tasks::check_capture(&crate::tasks::scope(&app, project)?, &id))?;
    Ok(typed(media, bytes))
}

/// Pass or fail one "To check" item, or pass them all (`all/pass`).
async fn task_check(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath((project, id, decision)): UrlPath<(u64, String, String)>,
    Json(input): Json<DecideInput>,
) -> Outcome<Json<Value>> {
    tasks_allowed(&app, true)?;
    act_budget(&app, &device)?;
    let scope = blocking(|| crate::tasks::scope(&app, project))?;
    // ^ A fail starts a task, which waits for its host for a few seconds.
    tokio::task::spawn_blocking(move || crate::tasks::check(&scope, &id, &decision, input.note))
        .await
        .map_err(|_| Fault::new(500, "task_action", "the action stopped unexpectedly"))?
        .map(Json)
}

fn media_fault(error: MediaError) -> Fault {
    match error {
        MediaError::TooLarge => {
            Fault::new(413, "too_large", "the file is larger than this site allows")
        }
        MediaError::UnsupportedType => {
            Fault::new(415, "unsupported_type", "this file type is not allowed")
        }
        MediaError::Io => Fault::new(500, "media_store", "the media store failed"),
    }
}

async fn media_put(
    State(app): State<Shared>,
    Paired(_): Paired,
    Query(query): Query<HashMap<String, String>>,
    body: Body,
) -> Outcome<Json<Value>> {
    allow(&app, Capability::MediaUpload, "uploads")?;
    let mut upload = app.media.begin().map_err(media_fault)?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Fault::new(400, "upload_aborted", "the upload stopped"))?;
        if let Err(error) = blocking(|| upload.write(&chunk)) {
            // Read a bounded rest of the body so the client gets the answer, not a reset.
            let mut drained = 0;
            while let Some(Ok(rest)) = stream.next().await {
                drained += rest.len();
                if drained > 16 * 1024 * 1024 {
                    break;
                }
            }
            return Err(media_fault(error));
        }
    }
    let name = query.get("name").map_or("file", String::as_str);
    let entry = blocking(|| app.media.finish(upload, name)).map_err(media_fault)?;
    Ok(Json(json!(chat::Attachment::from(entry))))
}

/// Immutable by construction: the URL names the content hash, and the cache
/// lifetime is the time the entry has left.
async fn media_get(
    State(app): State<Shared>,
    Paired(_): Paired,
    UrlPath(file): UrlPath<String>,
    headers: HeaderMap,
) -> Outcome<Response> {
    if !app.site.allows(Capability::MediaDownload) && !app.site.allows(Capability::MediaUpload) {
        return Err(Fault::forbidden("media"));
    }
    let (entry, remaining) = app.media.get(&file).ok_or_else(Fault::not_found)?;
    // One `bytes=a-b` range. Several ranges are answered with the whole file.
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .filter(|value| !value.contains(','))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok())));
    let range = match range {
        Some((start, end)) => {
            let end = end.unwrap_or(entry.size - 1).min(entry.size - 1);
            if start > end {
                return Ok((
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{}", entry.size))],
                )
                    .into_response());
            }
            Some((start, end))
        }
        None => None,
    };
    let (start, end) = range.unwrap_or((0, entry.size - 1));
    let mut source = std::fs::File::open(app.media.path(&entry)).map_err(|_| Fault::not_found())?;
    source
        .seek(SeekFrom::Start(start))
        .map_err(|_| Fault::not_found())?;
    let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);
    tokio::task::spawn_blocking(move || {
        let mut left = end - start + 1;
        let mut buffer = vec![0u8; 64 * 1024];
        while left > 0 {
            let want = left.min(buffer.len() as u64) as usize;
            match source.read(&mut buffer[..want]) {
                Ok(0) => break,
                Ok(read) => {
                    left -= read as u64;
                    if sender
                        .blocking_send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.blocking_send(Err(error));
                    break;
                }
            }
        }
    });
    let disposition =
        if entry.is_image() || entry.mime.starts_with("video/") || entry.mime.starts_with("audio/")
        {
            "inline".to_owned()
        } else {
            // The stored name has no quote or control character (media::clean_name).
            // RFC 6266: an ASCII fallback plus the exact UTF-8 name.
            let ascii: String = entry
                .name
                .chars()
                .map(|c| if c.is_ascii() && c != '\\' { c } else { '_' })
                .collect();
            let exact: String = entry
                .name
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect();
            format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{exact}")
        };
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, entry.mime.as_str())
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CACHE_CONTROL,
            format!("private, max-age={remaining}, immutable"),
        )
        .header(header::ETAG, format!("\"{}\"", entry.hash));
    if range.is_some() {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", entry.size),
        );
    }
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response = response.header(header::CONTENT_DISPOSITION, value);
    }
    response
        .body(Body::from_stream(ReceiverStream::new(receiver)))
        .map_err(|_| Fault::new(500, "media_store", "cannot build the response"))
}

fn sandbox_fault(error: SandboxError) -> Fault {
    match error {
        SandboxError::Forbidden => Fault::new(403, "path_refused", "this path is not allowed"),
        SandboxError::InvalidPath => Fault::new(
            400,
            "invalid_path",
            "use a relative path inside the project",
        ),
        SandboxError::NotFound => Fault::not_found(),
        SandboxError::TooLarge => Fault::new(413, "too_large", "the file is larger than 1 MiB"),
        SandboxError::Conflict => {
            Fault::new(409, "file_changed", "the file changed since you opened it")
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathQuery {
    project: u64,
    #[serde(default)]
    path: String,
}

/// The pinned root of one granted project.
fn project_root(app: &App, project: u64) -> Outcome<Arc<crate::sandbox::Root>> {
    let snapshot = chat::snapshot(&app.client)?;
    let project = chat::project(&snapshot, &app.site.access, project)?;
    app.root(&project.path).map_err(|_| Fault::not_found())
}

async fn files_list(
    State(app): State<Shared>,
    Paired(_): Paired,
    Query(query): Query<PathQuery>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Files)?;
    allow(&app, Capability::FilesRead, "reading files")?;
    let (entries, truncated) = blocking(|| {
        project_root(&app, query.project)?
            .list(Path::new(&query.path))
            .map_err(sandbox_fault)
    })?;
    Ok(Json(
        json!({ "path": query.path, "entries": entries, "truncated": truncated }),
    ))
}

async fn file_read(
    State(app): State<Shared>,
    Paired(_): Paired,
    Query(query): Query<PathQuery>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Files)?;
    allow(&app, Capability::FilesRead, "reading files")?;
    let bytes = blocking(|| {
        project_root(&app, query.project)?
            .read(Path::new(&query.path), MAX_TEXT_BYTES)
            .map_err(sandbox_fault)
    })?;
    Ok(Json(match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => json!({ "path": query.path, "text": text }),
        _ => json!({ "path": query.path, "binary": true }),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    project: u64,
    path: String,
    original: String,
    content: String,
}

async fn file_write(
    State(app): State<Shared>,
    Paired(_): Paired,
    Json(input): Json<WriteInput>,
) -> Outcome<Json<Value>> {
    need(&app, Panel::Files)?;
    allow(&app, Capability::FilesWrite, "editing files")?;
    blocking(|| {
        project_root(&app, input.project)?
            .write_text(Path::new(&input.path), &input.original, &input.content)
            .map_err(sandbox_fault)
    })?;
    Ok(Json(json!({ "saved": true })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachQuery {
    #[serde(default)]
    takeover: bool,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    cols: u16,
    /// Chosen by the page, stable while it stays open.
    #[serde(default)]
    client: String,
}

/// A live terminal on one session (docs/web-client-terminal.md).
/// ^ A WebSocket upgrade is not covered by CORS and may carry no Origin that the
/// guard would refuse, so the exact Origin is required here.
async fn terminal_socket(
    State(app): State<Shared>,
    Paired(device): Paired,
    UrlPath(session): UrlPath<u64>,
    Query(query): Query<AttachQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Outcome<Response> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin != Some(app.site.public.origin.as_str()) {
        return Err(Fault::new(
            403,
            "wrong_origin",
            "a terminal connection needs this site's Origin",
        ));
    }
    need(&app, Panel::Terminal)?;
    allow(&app, Capability::TerminalView, "viewing the terminal")?;
    // Without terminal_input a device only watches: it cannot type, resize the
    // pane, or take the terminal from another controller.
    let input = app.site.allows(Capability::TerminalInput);
    if query.takeover && !input {
        return Err(Fault::forbidden("taking control of the terminal"));
    }
    let (pane_id, raw_stream) = blocking(|| {
        let snapshot = chat::snapshot(&app.client)?;
        let pane_id = chat::target(&snapshot, &app.site.access, session)?.pane.id;
        Ok((pane_id, snapshot.capabilities.raw_terminal_stream))
    })?;
    let slot = StreamSlot::take(&app, format!("terminal:{}", device.id), "terminals")?;
    if query.client.len() > 32 || !query.client.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Fault::new(400, "invalid_client", "client must be digits"));
    }
    let attach = terminal::Attach {
        pane_id,
        client_id: terminal::client_id(&device.id, &query.client),
        takeover: query.takeover,
        size: (input && query.rows > 0 && query.cols > 0).then_some((query.rows, query.cols)),
        input,
        raw_stream,
    };
    Ok(upgrade
        .max_message_size(4 * terminal::MAX_INPUT_BYTES)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            terminal::bridge(app, socket, attach, headers).await;
        }))
}

async fn push_get(State(app): State<Shared>, Paired(device): Paired) -> Outcome<Json<Value>> {
    allow(&app, Capability::Push, "notifications")?;
    let push = app
        .push
        .as_ref()
        .ok_or_else(|| Fault::forbidden("notifications"))?;
    Ok(Json(
        json!({ "public_key": push.public_key(), "topics": push.topics(&device.id) }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushInput {
    subscription: SubscriptionInput,
    topics: Vec<Topic>,
}

async fn push_set(
    State(app): State<Shared>,
    Paired(device): Paired,
    Json(input): Json<PushInput>,
) -> Outcome<Json<Value>> {
    allow(&app, Capability::Push, "notifications")?;
    if input.topics.contains(&Topic::Tasks) {
        allow(&app, Capability::TaskView, "task notifications")?;
    }
    let push = app
        .push
        .as_ref()
        .ok_or_else(|| Fault::forbidden("notifications"))?;
    blocking(|| push.subscribe(&device.id, input.subscription, input.topics))
        .map_err(|error| Fault::new(400, "invalid_subscription", format!("{error:#}")))?;
    Ok(Json(json!({ "topics": push.topics(&device.id) })))
}

async fn push_delete(State(app): State<Shared>, Paired(device): Paired) -> Outcome<Json<Value>> {
    allow(&app, Capability::Push, "notifications")?;
    if let Some(push) = &app.push {
        blocking(|| push.unsubscribe(&device.id));
    }
    Ok(Json(json!({ "topics": [] })))
}
