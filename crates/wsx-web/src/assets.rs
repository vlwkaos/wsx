//! Browser files embedded in the binary. No build step: `web/` is served as written.
//! ^ Only four documents are filled in at serve time, from the validated site file:
//! index.html (base path, name), the manifest, the service worker and theme.css.
use crate::{devices::hex, site::Site};
use serde_json::json;
use sha2::{Digest, Sha256};

pub const ICON_192: &[u8] = include_bytes!("../web/icon-192.png");
pub const ICON_512: &[u8] = include_bytes!("../web/icon-512.png");

const JS: &str = "text/javascript; charset=utf-8";
const FILES: &[(&str, &str, &str)] = &[
    (
        "app.css",
        "text/css; charset=utf-8",
        include_str!("../web/app.css"),
    ),
    ("main.js", JS, include_str!("../web/main.js")),
    ("api.js", JS, include_str!("../web/api.js")),
    ("dom.js", JS, include_str!("../web/dom.js")),
    ("pair.js", JS, include_str!("../web/pair.js")),
    ("chat.js", JS, include_str!("../web/chat.js")),
    ("files.js", JS, include_str!("../web/files.js")),
    ("terminal.js", JS, include_str!("../web/terminal.js")),
    ("activity.js", JS, include_str!("../web/activity.js")),
    ("settings.js", JS, include_str!("../web/settings.js")),
];

pub fn find(name: &str) -> Option<(&'static str, &'static str)> {
    FILES
        .iter()
        .find(|(file, _, _)| *file == name)
        .map(|(_, content_type, body)| (*content_type, *body))
}

pub struct Assets {
    pub index: String,
    pub manifest: String,
    pub service_worker: String,
    pub theme: String,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl Assets {
    pub fn build(site: &Site) -> Self {
        let index = include_str!("../web/index.html")
            .replace("__WSX_BASE__", &site.base)
            .replace("__WSX_NAME__", &escape(&site.name));
        // Values were validated as #rgb or #rrggbb, so they are safe inside CSS.
        let block = |tokens: &std::collections::BTreeMap<String, String>| {
            tokens
                .iter()
                .map(|(token, value)| format!("--{}:{value};", token.replace('_', "-")))
                .collect::<String>()
        };
        // `[theme]` restyles the light scheme only. A light accent applied to the dark
        // scheme would pair with dark-scheme text tokens and lose contrast.
        let mut theme = format!(
            "@media (prefers-color-scheme: light){{:root{{{}}}}}",
            block(&site.theme)
        );
        if !site.theme_dark.is_empty() {
            theme.push_str(&format!(
                "@media (prefers-color-scheme: dark){{:root{{{}}}}}",
                block(&site.theme_dark)
            ));
        }
        let scope = site.scope();
        let color = |token: &str, fallback: &str| {
            site.theme
                .get(token)
                .cloned()
                .unwrap_or_else(|| fallback.to_owned())
        };
        let manifest = json!({
            "id": scope, "scope": scope, "start_url": scope,
            "name": site.name, "short_name": site.name,
            "display": "standalone",
            "background_color": color("background", "#f6f5f1"),
            "theme_color": color("accent", "#1f5f8b"),
            "icons": [
                { "src": "icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any" },
                { "src": "icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any" },
            ],
        })
        .to_string();
        let mut shell = vec![
            "./".to_owned(),
            "theme.css".to_owned(),
            "manifest.webmanifest".to_owned(),
            "icon-192.png".to_owned(),
        ];
        shell.extend(FILES.iter().map(|(name, _, _)| format!("assets/{name}")));
        // The cache name changes whenever any shell byte changes, so an old cache is dropped.
        let mut digest = Sha256::new();
        for part in [&index, &theme, &manifest] {
            digest.update(part.as_bytes());
        }
        for (_, _, body) in FILES {
            digest.update(body.as_bytes());
        }
        let version = hex(&digest.finalize()[..6]);
        let service_worker = include_str!("../web/sw.js")
            .replace("__WSX_CACHE__", &format!("wsx-{}-{version}", site.id))
            .replace("__WSX_SHELL__", &json!(shell).to_string());
        Self {
            index,
            manifest,
            service_worker,
            theme,
        }
    }
}
