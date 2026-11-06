//! A project's HTML file shown as a page in a sandboxed frame (docs/web-client.md "Files").
//! The frame has an opaque origin, so its requests carry no session cookie: a short-lived
//! token in the URL is the right, scoped to one device, one project and the page's folder.
use crate::devices;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

const LIFETIME: Duration = Duration::from_secs(30 * 60);
const MAX_VIEWS: usize = 256;
/// A page or one of its files.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct View {
    pub device: String,
    pub project: u64,
    /// The page's folder, relative to the project root; the token reaches only below it.
    pub folder: PathBuf,
    expires: Instant,
}

pub struct Views(Mutex<HashMap<String, View>>);

impl Views {
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    /// A new token. Expired ones go first; at the bound the one that ends soonest goes.
    pub fn mint(&self, device: &str, project: u64, folder: &Path) -> anyhow::Result<String> {
        let token = devices::hex(&devices::random_bytes::<32>()?);
        let now = Instant::now();
        let mut views = crate::locked(&self.0);
        views.retain(|_, view| view.expires > now);
        if views.len() >= MAX_VIEWS {
            if let Some(oldest) = views
                .iter()
                .min_by_key(|(_, view)| view.expires)
                .map(|(token, _)| token.clone())
            {
                views.remove(&oldest);
            }
        }
        views.insert(
            token.clone(),
            View {
                device: device.to_owned(),
                project,
                folder: folder.to_owned(),
                expires: now + LIFETIME,
            },
        );
        Ok(token)
    }

    pub fn get(&self, token: &str) -> Option<View> {
        crate::locked(&self.0)
            .get(token)
            .filter(|view| view.expires > Instant::now())
            .cloned()
    }
}

pub fn is_page(path: &Path) -> bool {
    matches!(extension(path).as_deref(), Some("html" | "htm"))
}

/// The only types a page may load; any other file of the folder answers 404.
pub fn content_type(path: &Path) -> Option<&'static str> {
    Some(match extension(path)?.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => return None,
    })
}

/// No script, no request off the token's folder, framed only by the site itself. Opening the
/// URL directly is sandboxed too. `prefix` is the absolute URL of the token's folder.
pub fn policy(origin: &str, prefix: &str) -> String {
    format!(
        "sandbox allow-popups allow-popups-to-escape-sandbox; default-src 'none'; \
         img-src {prefix} data:; style-src {prefix} 'unsafe-inline'; font-src {prefix} data:; \
         media-src {prefix}; frame-src {prefix}; form-action 'none'; base-uri 'none'; \
         frame-ancestors {origin}"
    )
}

fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}
