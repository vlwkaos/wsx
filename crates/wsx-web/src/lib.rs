//! The wsx hosted web client: one paired site per process.
//! Contract: docs/web-client.md. Deployment: docs/web-deploy.md.
//! ^ This crate is a wsxd client like the TUI. It never starts, stops or replaces
//! the daemon.

mod assets;
mod chat;
mod commands;
mod devices;
mod events;
mod http;
mod media;
mod push;
mod question;
mod queue;
mod sandbox;
mod site;
mod tasks;
mod terminal;
mod theme;

use anyhow::{Context, Result};
use std::future::IntoFuture;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

/// How long open event streams and terminals may keep a stopping server.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct App {
    site: site::Site,
    devices: devices::Devices,
    media: media::Media,
    push: Option<push::Push>,
    /// One pinned root per granted project that was opened, by canonical path.
    roots: Mutex<HashMap<std::path::PathBuf, Arc<sandbox::Root>>>,
    chat: chat::Chat,
    queue: queue::Queue,
    client: wsx_core::runtime::Client,
    assets: assets::Assets,
    limiter: http::Limiter,
    events: tokio::sync::broadcast::Sender<events::Message>,
    /// Latest workspace event, replayed first on every new stream.
    workspace: Mutex<Option<events::Message>>,
    streams: Mutex<HashMap<String, usize>>,
    /// Each open page by its id: its device, its event streams and the project it shows.
    pages: Mutex<HashMap<String, Page>>,
    /// Approvals whose action runs now, as `<store>/<id>`; a second tap is refused.
    approving: Mutex<std::collections::HashSet<String>>,
    /// When this server started, shown in Settings with the build.
    started_unix: u64,
    /// Why the upgrade of the running wsxd into this build failed, while it is still that
    /// daemon (docs/daemon-live-upgrade.md "A failed automatic upgrade").
    upgrade_failed: Mutex<Option<String>>,
}

/// A page of a device, for "no push for the project a page of that device shows".
pub(crate) struct Page {
    pub(crate) device: String,
    pub(crate) streams: usize,
    pub(crate) project: Option<u64>,
}

impl App {
    /// The devices with an open page that shows `project` now.
    pub(crate) fn showing(&self, project: u64) -> Vec<String> {
        let pages = self.pages.lock().unwrap_or_else(|lock| lock.into_inner());
        pages
            .values()
            .filter(|page| page.streams > 0 && page.project == Some(project))
            .map(|page| page.device.clone())
            .collect()
    }

    /// The pinned root of a granted project. The caller checked the grant.
    fn root(&self, project: &Path) -> std::io::Result<Arc<sandbox::Root>> {
        let path = std::fs::canonicalize(project)?;
        let mut roots = self.roots.lock().unwrap_or_else(|lock| lock.into_inner());
        if let Some(root) = roots.get(&path) {
            return Ok(root.clone());
        }
        let root = Arc::new(sandbox::Root::pin(&path)?);
        roots.insert(path, root.clone());
        Ok(root)
    }
}

/// `wsx web serve [<site>]`: run until SIGINT or SIGTERM.
pub fn serve(named: Option<&str>) -> Result<()> {
    let site = site::Site::load(&site::Site::resolve(named)?)?;
    let app = Arc::new(App {
        devices: devices::Devices::open(&site.state_dir)?,
        media: media::Media::open(&site.state_dir, site.media.clone())?,
        push: site
            .push
            .as_ref()
            .map(|settings| push::Push::open(&site.state_dir, settings))
            .transpose()?,
        roots: Mutex::new(HashMap::new()),
        chat: chat::Chat::new(),
        queue: queue::Queue::open(&site.state_dir)?,
        // Never `Client::local()`: a web server must not start wsxd.
        client: wsx_core::runtime::Client::new(wsx_core::runtime::default_socket_path()),
        assets: assets::Assets::build(&site),
        limiter: http::Limiter::new(),
        events: tokio::sync::broadcast::channel(256).0,
        workspace: Mutex::new(None),
        streams: Mutex::new(HashMap::new()),
        approving: Mutex::new(Default::default()),
        pages: Mutex::new(HashMap::new()),
        started_unix: devices::now_unix(),
        upgrade_failed: Mutex::new(None),
        site,
    });
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            // Loopback only: the tailnet proxy is the single public entry.
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, app.site.port));
            let listener = tokio::net::TcpListener::bind(address)
                .await
                .with_context(|| format!("cannot listen on {address}"))?;
            eprintln!(
                "wsx web: site '{}' on http://{address}{} for {}{}",
                app.site.id,
                app.site.scope(),
                app.site.public.origin,
                app.site.scope()
            );
            // A listed project must be readable now; "all" pins each project on first use.
            if let site::Access::Projects(paths) = &app.site.access {
                for path in paths {
                    app.root(path)
                        .with_context(|| format!("cannot open project {}", path.display()))?;
                }
            }
            events::spawn(app.clone());
            let sweeper = app.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    tokio::task::block_in_place(|| sweeper.media.sweep());
                }
            });
            // ^ Graceful shutdown waits for every open connection, and an event stream or a
            // terminal never ends by itself: after the signal they get a short grace period.
            let (stopping, mut stopped) = tokio::sync::watch::channel(false);
            let serve =
                axum::serve(listener, http::router(app)).with_graceful_shutdown(async move {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("install SIGTERM handler");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = terminate.recv() => {}
                    }
                    let _ = stopping.send(true);
                });
            let grace = async move {
                let _ = stopped.wait_for(|stopping| *stopping).await;
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            };
            tokio::select! {
                result = serve.into_future() => result?,
                _ = grace => eprintln!("wsx web: closed the open streams at shutdown"),
            }
            Ok(())
        })
}

/// `wsx web pair [<site>]`: print a one-time pairing code.
pub fn pair(named: Option<&str>) -> Result<()> {
    let file = site::Site::resolve(named)?;
    let site = site::Site::load(&file)?;
    let code = devices::Devices::open(&site.state_dir)?.create_pairing()?;
    println!("pairing code for '{}': {code}", site.id);
    println!("site file {}", file.display());
    println!(
        "open {}{} on the device and enter it within 10 minutes",
        site.public.origin,
        site.scope()
    );
    Ok(())
}

/// `wsx web devices [<site>] [--revoke <id>] [--json]`.
pub fn devices(named: Option<&str>, revoke: Option<&str>, json: bool) -> Result<()> {
    let site = site::Site::load(&site::Site::resolve(named)?)?;
    let devices = devices::Devices::open(&site.state_dir)?;
    if let Some(id) = revoke {
        anyhow::ensure!(devices.revoke(id)?, "no paired device has id {id}");
        println!("revoked device {id} from '{}'", site.id);
        return Ok(());
    }
    let list = devices.list()?;
    if json {
        let rows: Vec<_> = list
            .iter()
            .map(|device| {
                serde_json::json!({
                    "id": device.id, "name": device.name, "created_unix": device.created_unix,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else if list.is_empty() {
        println!("no device is paired with '{}'", site.id);
    } else {
        println!("{} paired with '{}'", list.len(), site.id);
        println!("{:<10}{:<12}NAME", "ID", "PAIRED");
        for device in list {
            let days = devices::now_unix().saturating_sub(device.created_unix) / 86_400;
            println!(
                "{:<10}{:<12}{}",
                device.id,
                format!("{days}d ago"),
                device.name
            );
        }
    }
    Ok(())
}

/// `wsx web vapid`: print a new key pair for a site file's `[push]` section.
pub fn vapid() -> Result<()> {
    let (private, public) = push::generate_vapid()?;
    println!("# add to the site file; keep the file owner-only");
    println!("[push]\nsubject = \"mailto:you@example.com\"\nprivate_key = \"{private}\"");
    println!("# public key (the server derives it; shown for reference): {public}");
    Ok(())
}
